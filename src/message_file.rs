extern crate serde_json;
extern crate twox_hash;
extern crate base32;
extern crate time;
extern crate byteorder;

use serde_json::{Value, Map};
use serde_json::Error as JsonError;
use serde_json::error::ErrorCode;
use self::twox_hash::XxHash;
use self::base32::Alphabet;
use self::byteorder::{LittleEndian, ReadBytesExt, WriteBytesExt};

use std::io::Cursor;
use std::hash::Hasher;
use std::error::Error;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write, ErrorKind, Seek, SeekFrom};
use std::io::Error as IOError;

// this is kinda clunky :-\
fn make_json_error(msg: &str) -> JsonError {
    return JsonError::syntax(ErrorCode::Message(String::from(msg).into_boxed_str()), 0, 0);
}

fn u64_bytes(x: u64) -> [u8; 8] {
    return [
        ((x >> 56) & 0xff) as u8,
        ((x >> 48) & 0xff) as u8,
        ((x >> 40) & 0xff) as u8,
        ((x >> 32) & 0xff) as u8,
        ((x >> 24) & 0xff) as u8,
        ((x >> 16) & 0xff) as u8,
        ((x >> 8) & 0xff) as u8,
        (x & 0xff) as u8
    ];
}

fn u32_bytes(x: u32) -> [u8; 4] {
    return [
        ((x >> 24) & 0xff) as u8,
        ((x >> 16) & 0xff) as u8,
        ((x >> 8) & 0xff) as u8,
        (x & 0xff) as u8
    ];
}

fn get_ts() -> (i64) {
    let ts = time::get_time();

    return (ts.sec * 1000) + (ts.nsec as i64 / 1000000);
}

///
/// The message file that holds all of the log messages
///
pub struct MessageFile {
    fd: File, // the actual message file
    num_messages: u32, // the number of messages in the file
    //TODO: Need to add a destructor as this value is never saved
}

/// This struct represents the on-disk format of the MessageFile
/// The messages follow the VV, where VV is the version number and
/// 0xTT is the if the message has been tombstoned:
/// |-------------------------------------------|
/// | 0x4C 0x4F 0x47 0x53 | 0x54 0x4F 0x52 0xVV |
/// | L    O    G    S    | T    O    R    0xVV |
/// |-------------------------------------------|
/// | num msgs in file    | 0xTT | msg size ... |
/// |-------------------------------------------|
/// | ... | message  in JSON ...                |
/// |-------------------------------------------|
/// | ... message in JSON                       |
/// |-------------------------------------------|
/// | ...                                       |
/// |-------------------------------------------|

const VERSION: u8 = 0x1;

impl MessageFile {
    pub fn new(file_path: &str) -> Result<MessageFile, Box<Error>> {
        let mut msg_file_path = String::from(file_path);

        if !msg_file_path.ends_with("/") {
            msg_file_path.push_str("/messages.data");
        } else {
            msg_file_path.push_str("messages.data");
        }

        trace!("Attempting to open message file: {}", msg_file_path);

        // open the file, and return if an error is encountered
        let mut msg_file = OpenOptions::new().read(true).append(true).create(true).open(&msg_file_path)?;
        let mut num_messages = 0;

        // if we're opening a new file, write the header info
        if msg_file.metadata()?.len() == 0 {
            msg_file.write(b"LOGSTOR")?; // write the header/magic
            msg_file.write_u8(VERSION)?;
            msg_file.write_u32::<LittleEndian>(0x00)?;
        } else {
            let mut magic = vec![0; 7];

            msg_file.read_exact(&mut magic)?;

            if "LOGSTOR".as_bytes() != magic.as_slice() {
                return Err(From::from("Invalid magic header for file"));
            }

            let version = msg_file.read_u8()?;

            if VERSION != version {
                return Err(From::from("Wrong version number"));
            }

            num_messages = msg_file.read_u32::<LittleEndian>()?;
        }

        debug!("Created MessageFile with {}", msg_file_path);

        // create our structure and return it wrapped in an Ok()
        return Ok(MessageFile {
            fd: msg_file,
            num_messages: num_messages,
        });
    }

    pub fn add_message(&mut self, message: &str) -> Result<(), Box<Error>> {
        trace!("Attempting to parse JSON");

        let v: Value = serde_json::from_str(message)?;

        // unpack the resulting value, and ensure it's an object
        let mut json: Map<String, Value> = match v {
            Value::Object(x) => Ok(x),
            _ => Err(make_json_error("Messages must be JSON objects"))
        }.unwrap();

        trace!("Checking all fields for __");

        let mut canoncial_json = String::from("{");

        { // this wraps all the immutable borrows
            // check to see if there are any restricted fields
            if json.keys().any(|k| k.starts_with("__")) {
                return Err(From::from("Illegal fields in message; fields cannot start with __: ".to_owned() + message));
            }

            // get a sorted vector of keys to form a canonical representation
            let mut sorted_keys = json.keys().collect::<Vec<&String>>();

            sorted_keys.sort_unstable();

            // go through each sorted key, and convert it into a string based upon type
            // or return an error if a nested JSON object is found
            for key in sorted_keys.into_iter() {
                let value = match json.get(key).unwrap().to_owned() {
                    Value::Object(_) => Err(make_json_error("Nested JSON Objects are not allowed")),
                    Value::Null => Ok(String::from("")),
                    Value::Bool(b) => Ok(b.to_string()),
                    Value::Number(n) => Ok(n.to_string()),
                    Value::String(s) => Ok(s),
                    Value::Array(v) => Ok(v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(","))
                }?;

                // add key: value
                canoncial_json.push_str(&format!("{}:{},", key.to_owned(), value));
            }
        }

        canoncial_json.pop(); // remove trailing comma
        canoncial_json.push_str("}");

        debug!("Canonical: {}", canoncial_json);
        println!("Canonical: {}", canoncial_json);

        // construct our hash function
        let mut hash: XxHash = XxHash::with_seed(0xBEDBEEF);

        hash.write(canoncial_json.as_bytes());

        let id = base32::encode(Alphabet::RFC4648 { padding: false }, &u64_bytes(hash.finish()));

        println!("ID: {}", id);

        // add the ID to the message
        json.insert(String::from("__id"), Value::String(id));

        // add the TS to the message
        json.insert(String::from("__ts"), json!(get_ts()));

        let final_message = serde_json::to_string(&json)?;

        println!("Final Message: {} {}", final_message.len(), final_message);

        // write out that we're not tombstoning this
        self.fd.write(&[0x00 as u8])?;

        // write out the size of the message
        self.fd.write(&u32_bytes(final_message.len() as u32))?;

        // write the data to the file
        self.fd.write(&final_message.as_bytes())?;

        // flush to disk
        self.fd.flush();

        // update the number of messages written
        self.num_messages += 1;

        return Ok(());
    }
}

#[cfg(test)]
mod tests {
    use ::message_file::MessageFile;

    #[test]
    fn new_file_no_slash() {
        let ret = MessageFile::new("/tmp").unwrap();
    }

    #[test]
    fn new_file_with_slash() {
        let ret = MessageFile::new("/tmp/").unwrap();
    }

    #[test]
    fn write_message() {
        let mut ret = MessageFile::new("/tmp").unwrap();

        ret.add_message("{\"b\": 1, \"a\": \"something\"}");
    }
}