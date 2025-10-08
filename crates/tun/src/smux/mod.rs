use std::io;

mod cipher;
mod fec;
mod session;
mod stream;

const VERSON_SIZE: usize = 1;
const CMD_SIZE: usize = 1;
const LENGTH_SIZE: usize = 2;
const SID_SIZE: usize = 4;
const HEAD_SIZE: usize = VERSON_SIZE + CMD_SIZE + LENGTH_SIZE + SID_SIZE;

const UPD_HDR_SIZE: usize = 8;

pub(crate) const CMD_SYN: u8 = 0;
pub(crate) const CMD_FIN: u8 = 1;
pub(crate) const CMD_PSH: u8 = 2;
pub(crate) const CMD_NOP: u8 = 3;
pub(crate) const CMD_UDP: u8 = 4;

pub struct FrameHdr {
    pub version: u8,
    pub cmd: u8,
    pub sid: u32,
    pub len: u16,
}

impl FrameHdr {
    #[inline]
    pub fn new() -> Self {
        Self {
            version: 0,
            cmd: 0,
            sid: 0,
            len: 0,
        }
    }

    #[inline]
    pub fn marshal(&self, buf: &mut [u8]) {
        buf[0] = self.version;
        buf[1] = self.cmd;
        buf[2..6].copy_from_slice(&self.sid.to_le_bytes());
        buf[6..8].copy_from_slice(&self.len.to_le_bytes());
    }

    #[inline]
    pub fn unmarshal(&mut self, buf: &[u8]) -> io::Result<()> {
        if buf.len() < HEAD_SIZE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invaild head length",
            ));
        }
        self.version = buf[0];
        self.cmd = buf[1];
        let arr: [u8; 4] = [buf[2], buf[3], buf[4], buf[5]];
        self.sid = u32::from_le_bytes(arr);
        let arr: [u8; 2] = [buf[6], buf[7]];
        self.len = u16::from_le_bytes(arr);
        Ok(())
    }
}
