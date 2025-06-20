use byteorder::{BigEndian, ByteOrder};
use bytes::{BufMut, BytesMut};
use hickory_resolver::proto::{ProtoError, ProtoErrorKind, op::Message};
use std::{
    io::{self, Result as IOResult},
    net::IpAddr,
    os::fd::AsRawFd,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpStream, UdpSocket},
    time,
};

use crate::options::ConnectOptions;

pub enum DnsClient {
    DnsUdp(UdpRemoteClient),
    DnsTcp(TcpRemoteClient),
}

struct UdpRemoteClient {
    udp_sock: UdpSocket,
    remote: (IpAddr, u16),
}

struct TcpRemoteClient {
    tcp_stream: TcpStream,
}

impl UdpRemoteClient {
    /// query the message
    async fn query(&mut self, msg: Message) -> Result<Message, ProtoError> {
        let buf = msg.to_vec()?;
        self.udp_sock.send_to(&buf, self.remote).await?;
        let mut resp_buf = [0u8; 512];
        let readn = self.udp_sock.recv(&mut resp_buf).await?;

        let resp_msg = Message::from_vec(&resp_buf[..readn])?;
        Ok(resp_msg)
    }
}

impl TcpRemoteClient {
    /// query the message, the message parameter must be prepared.
    async fn query(&mut self, msg: Message) -> Result<Message, ProtoError> {
        let mut msg_bytes = msg.to_vec()?;
        let msg_len = msg_bytes.len();
        msg_bytes.resize(msg_len + 2, 0);
        msg_bytes.copy_within(0..msg_len, 2);
        BigEndian::write_u16(&mut msg_bytes[0..2], msg_len as u16);

        self.tcp_stream.write_all(&msg_bytes).await?;
        let mut len_buf = [0u8; 2];
        self.tcp_stream.read_exact(&mut len_buf).await?;
        let msg_len = BigEndian::read_u16(&len_buf) as usize;

        let mut msg_bytes = BytesMut::with_capacity(msg_len);
        // fastest
        unsafe {
            msg_bytes.advance_mut(msg_len);
        }
        self.tcp_stream.read_exact(&mut msg_bytes[..]).await?;
        let resp_message = Message::from_vec(&msg_bytes)?;
        Ok(resp_message)
    }

    #[cfg(unix)]
    pub async fn check_connect(&self) -> bool {
        let fd = self.tcp_stream.as_raw_fd();
        unsafe {
            let mut buf = [0u8; 1];
            let n = libc::recv(
                fd,
                buf.as_mut_ptr() as _,
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            );
            match n.cmp(&0) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Equal => false,
                std::cmp::Ordering::Less => {
                    use std::io::ErrorKind;
                    let err = io::Error::last_os_error();
                    err.kind() == ErrorKind::WouldBlock
                }
            }
        }
    }
}

impl DnsClient {
    pub async fn connect_udp(opts: &ConnectOptions, remote: (IpAddr, u16)) -> IOResult<Self> {
        let udp_sock = UdpSocket::bind(opts.bind_local_addr.unwrap()).await?;
        Ok(DnsClient::DnsUdp(UdpRemoteClient { udp_sock, remote }))
    }

    pub async fn connect_tcp(opts: &ConnectOptions, remote: (IpAddr, u16)) -> IOResult<Self> {
        let tcp_stream = TcpStream::connect(remote).await?;
        Ok(DnsClient::DnsTcp(TcpRemoteClient { tcp_stream }))
    }

    pub async fn check_connect(&self) -> bool {
        match *self {
            DnsClient::DnsUdp(_) => true,
            DnsClient::DnsTcp(ref tcp_remote_client) => tcp_remote_client.check_connect().await,
        }
    }

    pub async fn lookup(&mut self, msg: Message) -> Result<Message, ProtoError> {
        match *self {
            DnsClient::DnsUdp(ref mut udp_remote_client) => udp_remote_client.query(msg).await,
            DnsClient::DnsTcp(ref mut tcp_remote_client) => tcp_remote_client.query(msg).await,
        }
    }

    pub async fn lookup_timeout(
        &mut self,
        msg: Message,
        t: Duration,
    ) -> Result<Message, ProtoError> {
        match time::timeout(t, self.lookup(msg)).await {
            Ok(r) => r,
            Err(_) => Err(ProtoErrorKind::Timeout.into()),
        }
    }
}
