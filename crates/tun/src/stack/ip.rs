use std::net::IpAddr;

use smoltcp::wire::{IpProtocol, IpVersion, Ipv4Packet, Ipv6Packet, Result};

pub enum IpPacket<T: AsRef<[u8]>> {
    Ipv4(Ipv4Packet<T>),
    Ipv6(Ipv6Packet<T>),
}

impl<T: AsRef<[u8]>> IpPacket<T> {
    #[inline(always)]
    pub fn new_checked(pkt: T) -> Result<IpPacket<T>> {
        let buffer = pkt.as_ref();
        match IpVersion::of_packet(buffer)? {
            IpVersion::Ipv4 => Ok(Self::Ipv4(Ipv4Packet::new_checked(pkt)?)),
            IpVersion::Ipv6 => Ok(Self::Ipv6(Ipv6Packet::new_checked(pkt)?)),
        }
    }

    #[inline(always)]
    pub fn protocol(&self) -> IpProtocol {
        match self {
            IpPacket::Ipv4(packet) => packet.next_header(),
            IpPacket::Ipv6(packet) => packet.next_header(),
        }
    }

    #[inline(always)]
    pub fn src_addr(&self) -> IpAddr {
        match self {
            IpPacket::Ipv4(packet) => packet.src_addr().into(),
            IpPacket::Ipv6(packet) => packet.src_addr().into(),
        }
    }

    #[inline(always)]
    pub fn dst_addr(&self) -> IpAddr {
        match self {
            IpPacket::Ipv4(packet) => packet.dst_addr().into(),
            IpPacket::Ipv6(packet) => packet.dst_addr().into(),
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> IpPacket<&'a T> {
    #[inline(always)]
    pub fn payload(&self) -> &'a [u8] {
        match *self {
            IpPacket::Ipv4(ref packet) => packet.payload(),
            IpPacket::Ipv6(ref packet) => packet.payload(),
        }
    }
}
