mod net;
mod tcp_tun;
mod virtual_device;

use byte_string::ByteStr;
use ipnet::IpNet;
use log::{debug, error, trace, warn};
use smoltcp::wire::{IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, TcpPacket};
use std::{
    io::{self, Result as IOResult},
    net::{IpAddr, SocketAddr},
};
use tokio::io::AsyncReadExt;
use tokio::io::AsyncWriteExt;
use tun::{
    AbstractDevice, AsyncDevice, Configuration as TunConfiguration, ToAddress, create_as_async,
};

use crate::{tcp_tun::TcpTun, virtual_device::TokenBuffer};

enum IpPacket<T: AsRef<[u8]>> {
    Ipv4(Ipv4Packet<T>),
    Ipv6(Ipv6Packet<T>),
}

impl<'a, T: AsRef<[u8]> + ?Sized> IpPacket<&'a T> {
    /// Return a pointer to the payload.
    #[inline]
    pub fn payload(&self) -> &'a [u8] {
        match *self {
            IpPacket::Ipv4(ref packet) => packet.payload(),
            IpPacket::Ipv6(ref packet) => packet.payload(),
        }
    }
}

impl<T: AsRef<[u8]>> IpPacket<T> {
    pub fn new_checked(packet: T) -> smoltcp::wire::Result<Option<Self>> {
        let buffer = packet.as_ref();
        match IpVersion::of_packet(buffer)? {
            IpVersion::Ipv4 => Ok(Some(Self::Ipv4(Ipv4Packet::new_checked(packet)?))),
            IpVersion::Ipv6 => Ok(Some(Self::Ipv6(Ipv6Packet::new_checked(packet)?))),
        }
    }

    pub fn src_addr(&self) -> IpAddr {
        match *self {
            Self::Ipv4(ref packet) => IpAddr::from(packet.src_addr()),
            Self::Ipv6(ref packet) => IpAddr::from(packet.src_addr()),
        }
    }

    pub fn dst_addr(&self) -> IpAddr {
        match *self {
            Self::Ipv4(ref packet) => IpAddr::from(packet.dst_addr()),
            Self::Ipv6(ref packet) => IpAddr::from(packet.dst_addr()),
        }
    }

    pub fn protocol(&self) -> IpProtocol {
        match *self {
            Self::Ipv4(ref packet) => packet.next_header(),
            Self::Ipv6(ref packet) => packet.next_header(),
        }
    }
}

pub struct TunBuilder {
    tun_config: TunConfiguration,
}

impl TunBuilder {
    pub fn new() -> Self {
        Self {
            tun_config: Default::default(),
        }
    }
    pub fn address<Addr: ToAddress>(&mut self, addr: Addr) -> &mut TunBuilder {
        self.tun_config.address(addr);
        self
    }

    pub fn mtu(&mut self, mtu: u16) -> &mut TunBuilder {
        self.tun_config.mtu(mtu);
        self
    }

    pub fn netmask<Addr: ToAddress>(&mut self, netmask: Addr) -> &mut TunBuilder {
        self.tun_config.netmask(netmask);
        self
    }

    pub fn destination<Addr: ToAddress>(&mut self, destination: Addr) -> &mut TunBuilder {
        self.tun_config.destination(destination);
        self
    }

    pub fn build(&self) -> IOResult<Tun> {
        let device = match create_as_async(&self.tun_config) {
            Ok(device) => Ok(device),
            Err(tun::Error::Io(e)) => Err(e),
            Err(e) => Err(io::Error::new(io::ErrorKind::Other, e.to_string())),
        }?;
        Ok(Tun::new(device))
    }
}

pub struct Tun {
    device: AsyncDevice,
    tcp_tun: TcpTun,
}

impl Tun {
    pub fn new(device: AsyncDevice) -> Self {
        let tcp_tun = TcpTun::new(1500);
        Self { device, tcp_tun }
    }

    pub async fn run(mut self) -> IOResult<()> {
        let address = match self.device.address() {
            Ok(a) => a,
            Err(err) => {
                error!("[TUN] failed to get device address, error: {}", err);
                return Err(io::Error::other(err));
            }
        };

        let netmask = match self.device.netmask() {
            Ok(n) => n,
            Err(err) => {
                error!("[TUN] failed to get device netmask, error: {}", err);
                return Err(io::Error::other(err));
            }
        };

        let address_net = match IpNet::with_netmask(address, netmask) {
            Ok(n) => n,
            Err(err) => {
                error!(
                    "[TUN] invalid address {}, netmask {}, error: {}",
                    address, netmask, err
                );
                return Err(io::Error::other(err));
            }
        };

        trace!(
            "[TUN] tun device network: {} (address: {}, netmask: {})",
            address_net, address, netmask
        );

        let address_broadcast = address_net.broadcast();
        let create_empty_package = || {
            const PACKET_LEN: usize = 65535;
            let mut packet_buffer = TokenBuffer::with_capacity(PACKET_LEN);
            unsafe { packet_buffer.set_len(PACKET_LEN) };
            packet_buffer
        };
        let mut packet_buffer = create_empty_package();
        loop {
            tokio::select! {
                n = self.device.read(&mut packet_buffer) => {
                    let n = n?;
                    let mut packet = std::mem::replace(& mut packet_buffer, create_empty_package());
                    unsafe {
                        packet.set_len(n);
                    }
                    let _ = self.handle_dev_packet(&address_broadcast, packet);
                },
                packet = self.tcp_tun.recv_packet() => {
                    match self.device.write(&packet).await {
                        Ok(n) => {
                            if n < packet.len() {
                                warn!("[TUN] sent IP packet (TCP), but truncated. sent {} < {}, {:?}", n, packet.len(), ByteStr::new(&packet));
                            } else {
                                trace!("[TUN] sent IP packet (TCP) {:?}", ByteStr::new(&packet));
                            }
                        },
                        Err(e) =>
                            error!("[TUN] failed to set packet information, error: {}, {:?}", e, ByteStr::new(&packet)),
                    }

                }
            }
        }
    }

    pub fn handle_dev_packet(
        &mut self,
        device_broadcast_addr: &IpAddr,
        frame: TokenBuffer,
    ) -> smoltcp::wire::Result<()> {
        let ip_pkg = match IpPacket::new_checked(frame.as_ref())? {
            Some(pkg) => pkg,
            None => {
                return Ok(());
            }
        };
        let src_ip_addr = ip_pkg.src_addr();
        let dst_ip_addr = ip_pkg.dst_addr();
        let src_non_unicast = src_ip_addr == *device_broadcast_addr
            || match src_ip_addr {
                IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_multicast() || v6.is_unspecified(),
            };
        let dst_non_unicast = dst_ip_addr == *device_broadcast_addr
            || match dst_ip_addr {
                IpAddr::V4(v4) => v4.is_broadcast() || v4.is_multicast() || v4.is_unspecified(),
                IpAddr::V6(v6) => v6.is_multicast() || v6.is_unspecified(),
            };
        if src_non_unicast || dst_non_unicast {
            trace!(
                "[TUN] IP packet {} (unicast? {}) -> {} (unicast? {}) throwing away",
                src_ip_addr, !src_non_unicast, dst_ip_addr, !dst_non_unicast
            );
            return Ok(());
        }
        match ip_pkg.protocol() {
            IpProtocol::Tcp => {
                let tcp_packet = match TcpPacket::new_checked(ip_pkg.payload()) {
                    Ok(p) => p,
                    Err(err) => {
                        error!(
                            "invalid TCP packet err: {}, src_ip: {}, dst_ip: {}, payload: {:?}",
                            err,
                            ip_pkg.src_addr(),
                            ip_pkg.dst_addr(),
                            ByteStr::new(ip_pkg.payload())
                        );
                        return Ok(());
                    }
                };
                let src_port = tcp_packet.src_port();
                let dst_port = tcp_packet.dst_port();

                let src_addr = SocketAddr::new(ip_pkg.src_addr(), src_port);
                let dst_addr = SocketAddr::new(ip_pkg.dst_addr(), dst_port);
                trace!(
                    "[TUN] TCP packet {} (unicast? {}) -> {} (unicast? {}) {}",
                    src_addr, !src_non_unicast, dst_addr, !dst_non_unicast, tcp_packet
                );

                // TCP first handshake packet.
                if let Err(err) = self
                    .tcp_tun
                    .intercept_tcp_syn(src_addr, dst_addr, &tcp_packet)
                {
                    error!(
                        "handle TCP packet failed, error: {}, {} <-> {}, packet: {:?}",
                        err, src_addr, dst_addr, tcp_packet
                    );
                }
                // send raw data to tcp stack.
                self.tcp_tun.send_tcp_frame(frame);
            }
            _ => {
                debug!("IP packet ignored (protocol: {:?})", ip_pkg.protocol());

                return Ok(());
            }
        }
        Ok(())
    }
}
