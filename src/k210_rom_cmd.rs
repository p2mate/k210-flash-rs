/*
 *  Copyright 2020 Peter De Schrijver
 *
 *  Licensed under the Apache License, Version 2.0 (the "License");
 *  you may not use this file except in compliance with the License.
 *  You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 *  Unless required by applicable law or agreed to in writing, software
 *  distributed under the License is distributed on an "AS IS" BASIS,
 *  WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 *  See the License for the specific language governing permissions and
 *  limitations under the License.
 */
use super::slipcodec::{Decoder, Error, ErrorKind, SlipCodec, SlipDecoded};
use crc_any::CRC;
use futures::{sink::SinkExt, stream::StreamExt};
use std::path::Path;
use std::rc::Rc;
use tokio::time;
use tokio_serial::{Serial, SerialPort, ClearBuffer};

pub struct K210RomCmd {
    port: Rc<Serial>,
}

pub enum K210FlashType {
    OnChip = 0,
    External = 1,
}

fn encode_cmd<T: AsRef<[u8]>>(cmd: u8, param: [u32; 2], d: &T) -> Result<Vec<u8>, Error> {
    let data = d.as_ref();
    let mut crc = CRC::crc32();
    for x in param.iter() {
        crc.digest(&x.to_le_bytes());
    }    
    crc.digest(data);    
    let mut crc_le = [0 as u8 ;4];
    crc_le.copy_from_slice(&crc.get_crc_vec_le()[..]);
    if data.len() != 0 {
        Ok([
            [
                (cmd as u32).to_le_bytes(),
                crc_le,
                param[0].to_le_bytes(),
                param[1].to_le_bytes(),
            ]
            .concat(),
            data.to_vec(),
        ]
        .concat())
    } else {
        Ok([
            (cmd as u32).to_le_bytes(),
            crc_le,
            param[0].to_le_bytes(),
            param[1].to_le_bytes(),
        ]
        .concat())
    }
}

fn cmd_ok<T: AsRef<[u8]>>(cmd: u8, r: &T) -> Result<(), Error> {
    let response = r.as_ref();
    if response.len() == 2 && response[0] == cmd && response[1] == 0xe0 {
        Ok(())
    } else {
        dbg!(response);
        Err(Error::new(ErrorKind::Other, "Invalid response"))
    }
}

async fn do_reset_target(port: &mut Serial) {
    use time::delay_for;

    port.write_data_terminal_ready(false).unwrap();
    delay_for(time::Duration::from_millis(100 as u64)).await;
    port.write_data_terminal_ready(true).unwrap();
    delay_for(time::Duration::from_millis(100 as u64)).await;
    port.write_data_terminal_ready(false).unwrap();
}

impl K210RomCmd {
    pub fn from_path<P: AsRef<Path>>(path: P) -> Result<Self, Error> {
        let mut settings = tokio_serial::SerialPortSettings::default();
        settings.baud_rate = 115200;
        let mut port = tokio_serial::Serial::from_path(path, &settings).unwrap();

        #[cfg(unix)]
        port.set_exclusive(false)?;
        port.clear(ClearBuffer::All)?;

        Ok(K210RomCmd {
            port: Rc::new(port),
        })
    }

    async fn target_isp_mode(&mut self) {
        use time::delay_for;

        let port = Rc::get_mut(&mut self.port).unwrap();
        port.write_request_to_send(false).unwrap();
        delay_for(time::Duration::from_millis(100 as u64)).await;
        port.write_request_to_send(true).unwrap();

        do_reset_target(port).await;        

        delay_for(time::Duration::from_millis(100 as u64)).await;
        port.write_request_to_send(false).unwrap();
    }

    pub async fn reset_target(&mut self) {
        let port = Rc::get_mut(&mut self.port).unwrap();
        do_reset_target(port).await;
    }

    async fn send_cmd<T: AsRef<[u8]>>(&mut self, b: &T) -> Result<(), Error> {
        let mut writer = SlipCodec.framed(
            Rc::get_mut(&mut self.port)
                .ok_or(Error::new(ErrorKind::Other, "frame codec failed"))?,
        );
        let buffer = b.as_ref().to_vec();
        writer.send(buffer).await
    }

    async fn send_cmd_with_reply<T: AsRef<[u8]>>(
        &mut self,
        b: &T,
        delay: time::Duration,
    ) -> Result<SlipDecoded, Error> {

        self.send_cmd(b).await?;

        let mut reader = SlipCodec.framed(
            Rc::get_mut(&mut self.port)
                .ok_or(Error::new(ErrorKind::Other, "frame codec failed"))?,
        );

        if let Ok(result) = time::timeout(delay, reader.next()).await {
            match result {
                Some(x) => x,
                None => Ok(SlipDecoded::new()),
            }
        } else {
            Err(Error::new(ErrorKind::TimedOut, "Timeout"))
        }
    }

    async fn do_greeting(&mut self, cmd: u8, do_isp: bool) -> Result<String, Error> {
        for _ in 0 as u32..15 {
            if do_isp {
                self.target_isp_mode().await;
            }
            let result = self
                .send_cmd_with_reply(
                    &encode_cmd(cmd, [0x0, 0x0], &[])?,
                    time::Duration::from_millis(500),
                )
                .await;
            match result {
                Ok(x) => {
                    if let Ok(()) = cmd_ok(cmd, &x.get_item()) {
                        return Ok(x.get_debug_info());
                    }
                }
                _ => {}
            }
        }
        Err(Error::new(ErrorKind::TimedOut, "Timeout"))
    }

    pub async fn greet(&mut self) -> Result<String, Error> {
        self.do_greeting(0xc2, true).await      
    }

    pub async fn flash_greet(&mut self) -> Result<String, Error> {
        self.do_greeting(0xd2, false).await
    }

    pub async fn memory_write<T: AsRef<[u8]>>(&mut self, address: u32, d: &T) -> Result<String, Error> {
        let data = d.as_ref().to_vec();
        let result = self
            .send_cmd_with_reply(
                &encode_cmd(0xc3, [address, data.len() as u32], &data)?,
                time::Duration::from_millis(500),
            )
            .await?;
        cmd_ok(0xc3, &result.get_item()).map(|()| result.get_debug_info())
    }

    pub async fn flash_write<T: AsRef<[u8]>>(&mut self, address: u32, d: &T) -> Result<String, Error> {
        let mut data = d.as_ref().to_vec().clone();
        data.resize(16 * 4096, 0);        
        let cmd = encode_cmd(0xd4, [address, data.len() as u32], &data)?;        
        let result = self
            .send_cmd_with_reply(&cmd, time::Duration::from_secs(3))
            .await?;
        cmd_ok(0xd4, &result.get_item()).map(|()| result.get_debug_info())
    }

    pub async fn memory_boot(&mut self, address: u32) -> Result<(), Error> {
        self.send_cmd(&encode_cmd(0xc5, [address, 0 as u32], &[])?)
            .await
    }

    pub async fn select_flash_type(&mut self, ftype: K210FlashType) -> Result<String, Error> {
        let result = self
            .send_cmd_with_reply(
                &encode_cmd(0xd7, [ftype as u32, 0], &Vec::<u8>::new())?,
                time::Duration::from_millis(500),
            )
            .await?;
        cmd_ok(0xd7, &result.get_item()).map(|()| result.get_debug_info())
    }
}

#[tokio::test]
async fn test_reset() {
    let mut rom_cmd = K210RomCmd::from_path("/dev/ttyUSB0").unwrap();
    rom_cmd.reset_target().await;
}
