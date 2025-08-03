use std::net::SocketAddr;
use std::os::fd::{AsRawFd, FromRawFd};
use std::{io, time::Duration};

use socket2::{Domain, Protocol, Socket, TcpKeepalive, Type};
use tokio::net::{TcpSocket, UdpSocket};

use crate::{net::set_ip_bound_if, option::OUTBOUND_INTERFACES};

pub fn create_outbound_udp_socket(addr: &SocketAddr) -> io::Result<UdpSocket> {
    let socket = match *addr {
        SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::DGRAM, None)?,
        SocketAddr::V6(_) => Socket::new(Domain::IPV6, Type::DGRAM, None)?,
    };

    socket.set_nonblocking(true)?;
    for iface in OUTBOUND_INTERFACES.iter() {
        set_ip_bound_if(&socket, addr, iface)?;
    }
    let socket = UdpSocket::from_std(socket.into())?;
    Ok(socket)
}

pub fn create_outbound_tcp_socket(addr: &SocketAddr) -> io::Result<TcpSocket> {
    let socket = match *addr {
        SocketAddr::V4(_) => Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP))?,
        SocketAddr::V6(_) => Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?,
    };
    for iface in OUTBOUND_INTERFACES.iter() {
        set_ip_bound_if(&socket, addr, iface)?;
    }
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(600))
        .with_interval(Duration::from_secs(60));
    socket.set_keepalive(true)?;
    socket.set_tcp_keepalive(&keepalive)?;
    let socket = unsafe { TcpSocket::from_raw_fd(socket.as_raw_fd()) };
    Ok(socket)
}
