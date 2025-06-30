use std::io;
use std::net::SocketAddr;

use socket2::{Domain, Socket, Type};
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
        SocketAddr::V4(_) => TcpSocket::new_v4()?,
        SocketAddr::V6(_) => TcpSocket::new_v6()?,
    };
    for iface in OUTBOUND_INTERFACES.iter() {
        set_ip_bound_if(&socket, addr, iface)?;
    }
    Ok(socket)
}
