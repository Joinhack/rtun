use std::{
    io,
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::{Sink, SinkExt, Stream};
use log::debug;
use smoltcp::wire::IpProtocol;
use tokio::sync::mpsc;
use tokio_util::sync::PollSender;

use crate::{
    option::{STACK_CHANNEL, TCP_STACK_CHANNEL, UDP_STACK_CHANNEL},
    stack::{
        ip::IpPacket,
        tcp::{TcpListener, TcpRunner, TcpStack},
        udp::UdpStack,
    },
};

mod ip;
pub mod tcp;
pub mod udp;
mod virt_device;

type StackPacket = Vec<u8>;

pub struct StackBuidler {}

impl StackBuidler {
    pub fn new() -> StackBuidler {
        Self {}
    }

    pub fn build(self) -> (Stack, TcpListener, TcpRunner, UdpStack) {
        let (tcp_tx, tcp_rx) = mpsc::channel::<StackPacket>(*TCP_STACK_CHANNEL);
        let (udp_tx, udp_rx) = mpsc::channel::<StackPacket>(*UDP_STACK_CHANNEL);
        let (stack_tx, stack_rx) = mpsc::channel::<StackPacket>(*STACK_CHANNEL);
        let (tcp_runner, listener) = TcpStack::new(1500, tcp_rx, stack_tx.clone());
        let udp_stack = UdpStack::new(stack_tx, udp_rx);
        let stack = Stack {
            stack_rx,
            send_ready: false,
            icmp_tx: PollSender::new(tcp_tx.clone()),
            tcp_tx: PollSender::new(tcp_tx),
            udp_tx: PollSender::new(udp_tx),
            send_pkg: None,
        };
        (stack, listener, tcp_runner, udp_stack)
    }
}

pub struct Stack {
    send_pkg: Option<(StackPacket, IpProtocol)>,
    tcp_tx: PollSender<StackPacket>,
    udp_tx: PollSender<StackPacket>,
    send_ready: bool,
    icmp_tx: PollSender<StackPacket>,
    stack_rx: mpsc::Receiver<StackPacket>,
}

impl Stack {
    fn send(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let item = match self.send_pkg.take() {
            Some(p) => p,
            None => return Poll::Ready(Ok(())),
        };

        let proto: IpProtocol = item.1;
        let ready = self.send_ready;
        if !ready {
            if let Err(e) = ready!(self.ready(cx, proto)) {
                return Poll::Ready(Err(e));
            }
        }
        let Stack {
            tcp_tx,
            udp_tx,
            icmp_tx,
            send_ready,
            ..
        } = &mut *self;
        let rs = match item.1 {
            IpProtocol::Tcp => tcp_tx.start_send_unpin(item.0),
            IpProtocol::Udp => udp_tx.start_send_unpin(item.0),
            IpProtocol::Icmp | IpProtocol::Icmpv6 => icmp_tx.start_send_unpin(item.0),
            _ => unreachable!("not support proto, can't be reachable"),
        };
        match rs {
            Ok(d) => {
                *send_ready = false;
                Poll::Ready(Ok(d))
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e.to_string()))),
        }
    }

    #[inline(always)]
    fn ready(&mut self, cx: &mut Context<'_>, proto: IpProtocol) -> Poll<Result<(), io::Error>> {
        let Stack {
            tcp_tx,
            udp_tx,
            icmp_tx,
            send_ready,
            ..
        } = &mut *self;
        let rs = ready!(match proto {
            IpProtocol::Tcp => tcp_tx.poll_ready_unpin(cx),
            IpProtocol::Udp => udp_tx.poll_ready_unpin(cx),
            IpProtocol::Icmp | IpProtocol::Icmpv6 => icmp_tx.poll_ready_unpin(cx),
            _ => unreachable!("other already filter, can't be reached."),
        });
        match rs {
            Ok(d) => {
                *send_ready = true;
                Poll::Ready(Ok(d))
            }
            Err(e) => Poll::Ready(Err(io::Error::other(e.to_string()))),
        }
    }
}

impl Stream for Stack {
    type Item = Vec<u8>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Stack { stack_rx, .. } = &mut *self;
        stack_rx.poll_recv(cx)
    }
}

impl Sink<StackPacket> for Stack {
    type Error = io::Error;

    fn poll_ready(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let Stack {
            send_pkg,
            send_ready,
            ..
        } = &mut *self;
        // ip packet is exist and the send channel is ready, just send.
        match *send_pkg {
            Some((_, proto)) => {
                if *send_ready {
                    Poll::Ready(Ok(()))
                } else {
                    self.ready(cx, proto)
                }
            }
            None => Poll::Ready(Ok(())),
        }
    }

    fn start_send(mut self: Pin<&mut Self>, item: StackPacket) -> Result<(), Self::Error> {
        if self.send_pkg.is_some() {
            return Ok(());
        }

        let packet = match IpPacket::new_checked(&item) {
            Ok(ip) => ip,
            Err(e) => return Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        };
        let proto = packet.protocol();
        if matches!(
            proto,
            IpProtocol::Tcp | IpProtocol::Icmp | IpProtocol::Icmpv6 | IpProtocol::Udp
        ) {
            // cache the ip packet and protocol
            self.send_pkg.replace((item, proto));
        } else {
            debug!("stack not support protocol. {}", proto);
        }
        Ok(())
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        (&mut *self).send(cx)
    }

    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        let Stack {
            tcp_tx,
            udp_tx,
            stack_rx,
            icmp_tx,
            ..
        } = &mut *self;
        stack_rx.close();
        let _ = ready!(udp_tx.poll_close_unpin(cx));
        let _ = ready!(icmp_tx.poll_close_unpin(cx));
        let _ = ready!(tcp_tx.poll_close_unpin(cx));
        Poll::Ready(Ok(()))
    }
}
