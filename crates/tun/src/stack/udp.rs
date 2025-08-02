use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    task::{Poll, ready},
};

use etherparse::PacketBuilder;
use futures::{Sink, SinkExt, Stream};
use log::debug;
use smoltcp::wire::{IpProtocol, UdpPacket as SmolUdpPacket};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio_util::sync::PollSender;

use crate::stack::{StackPacket, ip::IpPacket};

pub struct UdpStack {
    stack_tx: PollSender<StackPacket>,
    udp_rx: Receiver<StackPacket>,
}

impl UdpStack {
    pub fn new(stack_tx: Sender<StackPacket>, udp_rx: Receiver<StackPacket>) -> Self {
        Self {
            stack_tx: PollSender::new(stack_tx),
            udp_rx,
        }
    }

    pub fn split(self) -> (UdpHalfWrite, UdpHalfRead) {
        (UdpHalfWrite(self.stack_tx), UdpHalfRead(self.udp_rx))
    }
}

pub type UdpPacket = (
    Vec<u8>,    /* payload */
    SocketAddr, /* local */
    SocketAddr, /* remote */
);

pub struct UdpHalfRead(Receiver<StackPacket>);

#[derive(Clone)]
pub struct UdpHalfWrite(PollSender<StackPacket>);

impl Stream for UdpHalfRead {
    type Item = (StackPacket, SocketAddr, SocketAddr);

    fn poll_next(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        match self.0.poll_recv(cx) {
            Poll::Ready(Some(data)) => {
                let packet = match IpPacket::new_checked(&data) {
                    Ok(ip) => ip,
                    Err(e) => {
                        debug!("error packet, {e}");
                        return Poll::Pending;
                    }
                };
                let s_addr = packet.src_addr();
                let d_addr = packet.dst_addr();
                let proto = packet.protocol();
                if matches!(proto, IpProtocol::Udp) {
                    let udp = SmolUdpPacket::new_unchecked(packet.payload());
                    let s_addr = SocketAddr::new(s_addr, udp.src_port());
                    let d_addr = SocketAddr::new(d_addr, udp.dst_port());
                    Poll::Ready(Some((udp.payload().to_vec(), s_addr, d_addr)))
                } else {
                    debug!("udp stack not support protocol. {}", proto);
                    Poll::Pending
                }
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Sink<UdpPacket> for UdpHalfWrite {
    type Error = io::Error;

    fn poll_ready(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Result<(), Self::Error>> {
        let rs = ready!(self.0.poll_ready_unpin(cx));
        match rs {
            Ok(s) => Poll::Ready(Ok(s)),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: UdpPacket) -> Result<(), Self::Error> {
        let (palyload, src_addr, dst_addr) = item;
        let builder = match (src_addr, dst_addr) {
            (SocketAddr::V4(src), SocketAddr::V4(dst)) => {
                PacketBuilder::ipv4(src.ip().octets(), dst.ip().octets(), 20)
                    .udp(src_addr.port(), dst_addr.port())
            }
            (SocketAddr::V6(src), SocketAddr::V6(dst)) => {
                PacketBuilder::ipv6(src.ip().octets(), dst.ip().octets(), 20)
                    .udp(src_addr.port(), dst_addr.port())
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "src or destination type unmatch",
                ));
            }
        };
        let mut ip_packet_writer = Vec::with_capacity(builder.size(palyload.len()));
        builder
            .write(&mut ip_packet_writer, &palyload)
            .map_err(|err| io::Error::other(format!("PacketBuilder::write: {err}")))?;
        match self.0.start_send_unpin(ip_packet_writer.clone()) {
            Ok(()) => Ok(()),
            Err(err) => Err(io::Error::other(format!("send error: {err}"))),
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let rs = ready!(self.0.poll_flush_unpin(cx));
        match rs {
            Ok(s) => Poll::Ready(Ok(s)),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        let rs = ready!(self.0.poll_close_unpin(cx));
        match rs {
            Ok(s) => Poll::Ready(Ok(s)),
            Err(e) => Poll::Ready(Err(io::Error::other(e))),
        }
    }
}
