use std::{
    fmt, io,
    net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6},
};

use bytes::{BufMut, BytesMut};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

mod tcp_client;
pub use tcp_client::*;

pub const SOCKS5_VERSION: u8 = 0x5;
pub const SOCKS5_AUTH_METHOD_NONE: u8 = 0x00;
pub const SOCKS5_AUTH_METHOD_GSSAPI: u8 = 0x01;
pub const SOCKS5_AUTH_METHOD_PASSWORD: u8 = 0x02;
pub const SOCKS5_AUTH_METHOD_NOT_ACCEPTABLE: u8 = 0xff;

pub const SOCKS5_ADDR_TYPE_IPV4: u8 = 0x01;
pub const SOCKS5_ADDR_TYPE_DOMAIN_NAME: u8 = 0x03;
pub const SOCKS5_ADDR_TYPE_IPV6: u8 = 0x04;

pub const SOCKS5_CMD_TCP_CONNECT: u8 = 0x01;
pub const SOCKS5_CMD_TCP_BIND: u8 = 0x02;
pub const SOCKS5_CMD_UDP_ASSOCIATE: u8 = 0x03;

pub const SOCKS5_REPLY_SUCCEEDED: u8 = 0x00;
pub const SOCKS5_REPLY_GENERAL_FAILURE: u8 = 0x01;
pub const SOCKS5_REPLY_CONNECTION_NOT_ALLOWED: u8 = 0x02;
pub const SOCKS5_REPLY_NETWORK_UNREACHABLE: u8 = 0x03;
pub const SOCKS5_REPLY_HOST_UNREACHABLE: u8 = 0x04;
pub const SOCKS5_REPLY_CONNECTION_REFUSED: u8 = 0x05;
pub const SOCKS5_REPLY_TTL_EXPIRED: u8 = 0x06;
pub const SOCKS5_REPLY_COMMAND_NOT_SUPPORTED: u8 = 0x07;
pub const SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;

/// SOCKS5 reply code
#[derive(Clone, Debug, Copy)]
pub enum Reply {
    Succeeded,
    GeneralFailure,
    ConnectionNotAllowed,
    NetworkUnreachable,
    HostUnreachable,
    ConnectionRefused,
    TtlExpired,
    CommandNotSupported,
    AddressTypeNotSupported,

    OtherReply(u8),
}

impl Reply {
    #[inline]
    pub fn as_u8(self) -> u8 {
        match self {
            Self::Succeeded => SOCKS5_REPLY_SUCCEEDED,
            Self::GeneralFailure => SOCKS5_REPLY_GENERAL_FAILURE,
            Self::ConnectionNotAllowed => SOCKS5_REPLY_CONNECTION_NOT_ALLOWED,
            Self::NetworkUnreachable => SOCKS5_REPLY_NETWORK_UNREACHABLE,
            Self::HostUnreachable => SOCKS5_REPLY_HOST_UNREACHABLE,
            Self::ConnectionRefused => SOCKS5_REPLY_CONNECTION_REFUSED,
            Self::TtlExpired => SOCKS5_REPLY_TTL_EXPIRED,
            Self::CommandNotSupported => SOCKS5_REPLY_COMMAND_NOT_SUPPORTED,
            Self::AddressTypeNotSupported => SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED,
            Self::OtherReply(c) => c,
        }
    }

    #[inline]
    pub fn from_u8(code: u8) -> Self {
        match code {
            SOCKS5_REPLY_SUCCEEDED => Self::Succeeded,
            SOCKS5_REPLY_GENERAL_FAILURE => Self::GeneralFailure,
            SOCKS5_REPLY_CONNECTION_NOT_ALLOWED => Self::ConnectionNotAllowed,
            SOCKS5_REPLY_NETWORK_UNREACHABLE => Self::NetworkUnreachable,
            SOCKS5_REPLY_HOST_UNREACHABLE => Self::HostUnreachable,
            SOCKS5_REPLY_CONNECTION_REFUSED => Self::ConnectionRefused,
            SOCKS5_REPLY_TTL_EXPIRED => Self::TtlExpired,
            SOCKS5_REPLY_COMMAND_NOT_SUPPORTED => Self::CommandNotSupported,
            SOCKS5_REPLY_ADDRESS_TYPE_NOT_SUPPORTED => Self::AddressTypeNotSupported,
            _ => Self::OtherReply(code),
        }
    }
}

impl fmt::Display for Reply {
    #[rustfmt::skip]
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Self::Succeeded               => write!(f, "Succeeded"),
            Self::AddressTypeNotSupported => write!(f, "Address type not supported"),
            Self::CommandNotSupported     => write!(f, "Command not supported"),
            Self::ConnectionNotAllowed    => write!(f, "Connection not allowed"),
            Self::ConnectionRefused       => write!(f, "Connection refused"),
            Self::GeneralFailure          => write!(f, "General failure"),
            Self::HostUnreachable         => write!(f, "Host unreachable"),
            Self::NetworkUnreachable      => write!(f, "Network unreachable"),
            Self::TtlExpired              => write!(f, "TTL expired"),
            Self::OtherReply(u)           => write!(f, "Other reply ({u})"),
        }
    }
}

/// the handshake request
/// ```plan
/// +----+----------+----------+
/// |VER | NMETHODS | METHODS  |
/// +----+----------+----------+
/// | 1  |    1     | 1~255个  |
/// +----+----------+----------+
pub struct HandShakeRequest {
    pub method: Vec<u8>,
}

impl HandShakeRequest {
    pub fn new(meth: Vec<u8>) -> Self {
        Self { method: meth }
    }

    pub async fn write_to<W: AsyncWrite + Unpin>(&self, mut writer: W) -> io::Result<()> {
        let mut buf = BytesMut::with_capacity(2 + self.method.len());
        buf.put_u8(SOCKS5_VERSION);
        buf.put_u8(self.method.len() as _);
        buf.put_slice(&self.method);
        writer.write_all(&buf).await
    }
}

/// socks5 handshake response
/// ```plain
/// +----+--------+
/// |VER | METHOD |
/// +----+--------+
/// | 1  |   1    |
/// +----+--------+
/// ```
pub struct HandShakeResponse {
    pub choose_method: u8,
}

impl HandShakeResponse {
    pub async fn read_from<R: AsyncRead + Unpin>(mut reader: R) -> io::Result<Self> {
        let mut buf = [0u8; 2];
        reader.read_exact(&mut buf).await?;
        let ver = buf[0];
        let choose_method = buf[1];
        if ver != SOCKS5_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unknown sock5 version",
            ));
        }
        Ok(Self { choose_method })
    }
}

pub enum Socks5Addr {
    SocketAddr(SocketAddr),
    Domain(String, u16),
}

/// socks5 tcp request.
/// ```plan
/// +----+-----+-------+------+----------+----------+
/// |VER | CMD |  RSV  | ATYP | DST.ADDR | DST.PORT |
/// +----+-----+-------+------+----------+----------+
/// | 1  |  1  |  0x00 |  1   | Variable |    2     |
/// +----+-----+-------+------+----------+----------+
/// ```
pub struct Socks5TcpRequest {
    command: u8,
    addr: Socks5Addr,
}

impl Socks5TcpRequest {
    #[inline]
    pub fn new(command: u8, addr: Socks5Addr) -> Self {
        Self { command, addr }
    }
    pub async fn write_to<W: AsyncWrite + Unpin>(&self, mut writer: W) -> io::Result<()> {
        let mut buf = BytesMut::with_capacity(22);
        buf.put_u8(SOCKS5_VERSION);
        buf.put_u8(self.command);
        buf.put_u8(0x00);
        match self.addr {
            Socks5Addr::SocketAddr(SocketAddr::V4(addr)) => {
                buf.put_u8(SOCKS5_ADDR_TYPE_IPV4);
                let addr_octs = addr.ip().octets();
                buf.put_slice(&addr_octs);
                buf.put_u16(addr.port());
            }
            Socks5Addr::SocketAddr(SocketAddr::V6(addr)) => {
                buf.put_u8(SOCKS5_ADDR_TYPE_IPV6);
                let addr_octs = addr.ip().octets();
                buf.put_slice(&addr_octs);
                buf.put_u16(addr.port());
            }
            Socks5Addr::Domain(ref domain, port) => {
                buf.put_u8(SOCKS5_ADDR_TYPE_DOMAIN_NAME);
                buf.put_u8(domain.len() as _);
                buf.put_slice(domain.as_bytes());
                buf.put_u16(port);
            }
        };
        writer.write_all(&buf).await
    }
}

/// socks5 tcp response.
/// ```plan
/// +----+-----+-------+------+----------+----------+
/// |VER | REP |  RSV  | ATYP | BND.ADDR | BND.PORT |
/// +----+-----+-------+------+----------+----------+
/// | 1  |  1  | X'00' |  1   | Variable |    2     |
/// +----+-----+-------+------+----------+----------+
/// ```
pub struct Socks5TcpResponse {
    pub reply: Reply,
    pub addr: SocketAddr,
}

impl Socks5TcpResponse {
    pub async fn read_from<R: AsyncRead + Unpin>(mut reader: R) -> io::Result<Self> {
        let mut buf = [0u8; 4];
        reader.read_exact(&mut buf).await?;
        if buf[0] != SOCKS5_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid version data.",
            ));
        }
        let reply = buf[1];
        let mut addr: SocketAddr;
        macro_rules! read_u16 {
            ($buf:ident, $i: literal) => {
                u16::from_be_bytes([$buf[$i], $buf[$i + 1]])
            };
        }
        match buf[3] {
            SOCKS5_ADDR_TYPE_IPV4 => {
                let mut buf = [0u8; 6];
                reader.read_exact(&mut buf).await?;
                let ip_addr = Ipv4Addr::new(buf[0], buf[1], buf[2], buf[3]);
                let port = read_u16!(buf, 4);
                addr = SocketAddr::V4(SocketAddrV4::new(ip_addr, port));
            }
            SOCKS5_ADDR_TYPE_IPV6 => {
                let mut buf = [0u8; 18];
                reader.read_exact(&mut buf).await?;

                let ip_addr = Ipv6Addr::new(
                    read_u16!(buf, 0),
                    read_u16!(buf, 2),
                    read_u16!(buf, 4),
                    read_u16!(buf, 6),
                    read_u16!(buf, 8),
                    read_u16!(buf, 10),
                    read_u16!(buf, 12),
                    read_u16!(buf, 14),
                );
                let port = read_u16!(buf, 16);
                addr = SocketAddr::V6(SocketAddrV6::new(ip_addr, port, 0, 0));
            }
            SOCKS5_ADDR_TYPE_DOMAIN_NAME => {
                unimplemented!("can be reached!");
            }
            _ => {
                unimplemented!("can be reached!");
            }
        };
        Ok(Self {
            reply: Reply::from_u8(reply),
            addr,
        })
    }
}
