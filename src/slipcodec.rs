pub use tokio_util::codec::{Decoder, Encoder};
pub use std::io::{Error, ErrorKind};
pub struct SlipCodec;
pub struct SlipDecoded {
    item: Vec<u8>,
    debug_info: String,
}

impl SlipDecoded {
    pub fn get_item(&self) -> Vec<u8> {
        self.item.clone()
    }

    pub fn get_debug_info(&self) -> String {
        self.debug_info.clone()
    }

    pub fn new() -> Self {
        SlipDecoded { item: vec![], debug_info: String::new()}
    }
}

use bytes::BytesMut;

impl Decoder for SlipCodec {
    type Item = SlipDecoded;
    type Error = Error;

    fn decode(&mut self, src: &mut BytesMut) -> Result<Option<Self::Item>, Self::Error> {              
        let debug_str = if src[0] == b'[' {
            if let Some(pos) = src[1..].into_iter().position(|b| *b == 0xc0) {
                String::from_utf8_lossy(&src.split_to(pos + 1)).to_string()
            } else {
                return Ok(None);
            }
        } else {
            String::new()
        };

        if src.len() < 2 {
            return Ok(None);
        }

        if src[0] != 0xc0 {            
            return Err(Error::new(ErrorKind::Other, format!("Invalid bytes {:?}", src.to_vec())));
        }

        if let Some(pos) = src[1..].into_iter().position(|b| *b == 0xc0) {            
            let mut frame = src.split_to(pos + 2);            
            let _ = frame.split_to(1);
            let mut result: Vec<u8> = Vec::with_capacity(frame.len());
            for val in frame.into_iter() {
                match val {
                    0xdc|0xdd => {
                                if let Some(last) = result.last_mut() {                                        
                                    if *last == 0xdb {                                        
                                        *last = match last {
                                            0xdc => 0xc0,
                                            0xdd => 0xdb,
                                            _ => {
                                                return Err(Error::new(ErrorKind::Other,
                                                    "Invalid byte in escape sequence"));
                                            },
                                        };
                                    } else {
                                        result.push(val);
                                    }
                                }
                            },
                    0xc0 => {},
                    x => result.push(x),
                }
            }
            return Ok(Some(SlipDecoded { item: result, debug_info: debug_str }));
        }                
        Ok(None)
    }
}

impl<T> Encoder<T> for SlipCodec 
    where T: AsRef<Vec<u8>> {
    type Error = Error;

    fn encode(&mut self, _item: T, _dst: &mut BytesMut) -> Result<(), Self::Error> {
        let mut buffer: Vec<u8> = Vec::with_capacity(_item.as_ref().len() + 2);
        buffer.push(0xc0);
        for b in _item.as_ref().iter() {
            match b {
                0xc0 => buffer.append(&mut vec![ 0xdb as u8, 0xdc as u8]),
                0xdb => buffer.append(&mut vec![ 0xdb as u8, 0xdd as u8]),
                x => buffer.push(*x),
            }
        }
        buffer.push(0xc0);
        _dst.extend_from_slice(buffer.as_slice());
        Ok(())
    }
}
