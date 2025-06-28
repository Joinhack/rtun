use bytes::{BufMut, Bytes, BytesMut};
use etherparse::PacketBuilder;
use log::{error, info, warn};
use smoltcp::wire::UdpPacket;
use spin::mutex::SpinMutex;

use std::{
    collections::{HashMap, hash_map::Entry},
    net::SocketAddr,
    sync::Arc,
};
use tokio::{
    io,
    net::UdpSocket,
    sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
};

use crate::net::set_ip_bound_if;

type DestChType = Arc<SpinMutex<HashMap<(SocketAddr, SocketAddr), UnboundedSender<Bytes>>>>;

pub struct UdpTun {
    proxy_tx_chs: DestChType,
    recv_ch_rx: UnboundedReceiver<BytesMut>,
    recv_ch_tx: UnboundedSender<BytesMut>,
}

struct UdpInboundWriter {
    src_addr: SocketAddr,
    dest_addr: SocketAddr,
}

impl UdpInboundWriter {
    fn write(&self, data: Bytes) -> io::Result<BytesMut> {
        match (self.src_addr, self.dest_addr) {
            (SocketAddr::V4(s_v4), SocketAddr::V4(d_v4)) => {
                let builder = PacketBuilder::ipv4(d_v4.ip().octets(), s_v4.ip().octets(), 20)
                    .udp(d_v4.port(), s_v4.port());
                let packet = BytesMut::with_capacity(builder.size(data.len()));
                let mut packet_writer = packet.writer();
                builder
                    .write(&mut packet_writer, &data)
                    .expect("packet writer error.");
                return Ok(packet_writer.into_inner());
            }
            (SocketAddr::V6(s_v6), SocketAddr::V6(d_v6)) => {
                let builder = PacketBuilder::ipv6(d_v6.ip().octets(), s_v6.ip().octets(), 20)
                    .udp(d_v6.port(), s_v6.port());
                let packet = BytesMut::with_capacity(builder.size(data.len()));
                let mut packet_writer = packet.writer();
                builder
                    .write(&mut packet_writer, &data)
                    .expect("packet writer error.");
                return Ok(packet_writer.into_inner());
            }
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "source destitition type not match.",
                ));
            }
        };
    }
}

impl UdpTun {
    pub fn new() -> Self {
        let (tx, rx) = unbounded_channel();
        Self {
            proxy_tx_chs: Default::default(),
            recv_ch_rx: rx,
            recv_ch_tx: tx,
        }
    }

    pub async fn recv_packet(&mut self) -> BytesMut {
        match self.recv_ch_rx.recv().await {
            Some(d) => d,
            None => unimplemented!("the recv channel closed."),
        }
    }

    pub fn handle_packet(
        &mut self,
        src_addr: SocketAddr,
        dest_addr: SocketAddr,
        udp: &UdpPacket<&[u8]>,
    ) -> io::Result<()> {
        let bs = Bytes::copy_from_slice(udp.payload());
        if let Entry::Occupied(mut ch) = self.proxy_tx_chs.lock().entry((src_addr, dest_addr)) {
            match ch.get_mut().send(bs) {
                Ok(_) => return Ok(()),
                Err(e) => {
                    error!(
                        "the udp proxy is close {} {} error: {}",
                        src_addr, dest_addr, e
                    );
                    // proxy task is finish, the channel is closed.
                    ch.remove();
                }
            };
        } else {
            let proxy_tx = self.recv_ch_tx.clone();
            let (proxy_ch_tx, mut proxy_ch_rx) = unbounded_channel();
            proxy_ch_tx.send(bs).unwrap();
            info!("udp create proxy source:{src_addr}  destition:{dest_addr}");
            self.proxy_tx_chs
                .lock()
                .insert((src_addr, dest_addr), proxy_ch_tx);
            tokio::spawn(async move {
                let proxy_udp = UdpSocket::bind("192.168.3.73:0").await.unwrap();
                set_ip_bound_if(&proxy_udp, &src_addr, "en0").unwrap();
                let mut buf = BytesMut::with_capacity(65535);
                loop {
                    tokio::select! {
                        data = proxy_udp.recv(&mut buf) => {
                            match data {
                                Ok(n) => {
                                    let writer = UdpInboundWriter {
                                        src_addr,
                                        dest_addr,
                                    };
                                    let data = buf.split_to(n).freeze();
                                    let data = match writer.write(data) {
                                        Ok(d) => d,
                                        Err(e) => {
                                            error!("packat process error, {}", e);
                                            continue;
                                        },
                                    };
                                    // when recv from proxy socket, send back to device through channel.
                                    if let Err(e) = proxy_tx.send(data) {
                                        unimplemented!("proxy send channel can't be closed, {}", e);
                                    };
                                },
                                Err(e) => {
                                    // proxy socket is break.
                                    error!("recv from proxy error {}", e);
                                    return;
                                },
                            }
                        },
                        data = proxy_ch_rx.recv() => {
                            match data {
                                Some(d) => {
                                    match proxy_udp.send_to(&d, dest_addr).await {
                                        Ok(_) => {},
                                        Err(e) => {
                                            error!("udp proxy send error, dest addr {}, {}", dest_addr, e);
                                            return;
                                        }
                                    }
                                },
                                None => {
                                    warn!("proxy channel is closed.");
                                    return;
                                },
                            }
                        }
                    };
                }
            });
        }
        Ok(())
    }
}
