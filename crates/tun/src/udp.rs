use futures::StreamExt;
use log::{debug, error};
use netstack_lwip::UdpSocket;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, io};
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Sender, channel};
use tokio::time;

use std::{net::SocketAddr, pin::Pin};

use crate::{net::create_outbound_udp_socket, option::UDP_RECV_CH_SIZE};

struct UdpPkg {
    payload: Vec<u8>,
}

pub struct UdpHandle {}

impl UdpHandle {
    pub fn new() -> Self {
        Self {}
    }
    pub async fn handle_udp_socket(&self, udp_socket: Pin<Box<UdpSocket>>) -> io::Result<()> {
        let (udp_tx, mut udp_rx) = udp_socket.split();

        let sessions = Arc::new(Mutex::new(HashMap::<SocketAddr, Sender<UdpPkg>>::new()));
        let udp_tx = Arc::new(udp_tx);
        while let Some((data, s_addr, d_addr)) = udp_rx.next().await {
            let mut sesses = sessions.lock().await;
            let entry = sesses.entry(s_addr);
            match entry {
                Entry::Occupied(mut entry) => {
                    if let Err(e) = entry.get_mut().send(UdpPkg { payload: data }).await {
                        error!("send channel is closed, error: {}", e);
                        entry.remove();
                    }
                }
                Entry::Vacant(vacant) => {
                    let (tx, mut rx) = channel::<UdpPkg>(*UDP_RECV_CH_SIZE);
                    tx.send(UdpPkg { payload: data }).await.unwrap();
                    let proxy_udp = create_outbound_udp_socket(&d_addr).unwrap();
                    let proxy_udp = Arc::new(proxy_udp);
                    let proxy_udp_cl = proxy_udp.clone();
                    vacant.insert(tx);
                    tokio::spawn(async move {
                        loop {
                            let pkg = match rx.recv().await {
                                Some(pkg) => pkg,
                                None => {
                                    error!("udp recv channel closed");
                                    return;
                                }
                            };
                            match proxy_udp_cl.send_to(&pkg.payload, d_addr).await {
                                Ok(_) => {}
                                Err(e) => {
                                    error!("send to {} error: {}", d_addr, e);
                                    return;
                                }
                            }
                        }
                    });
                    let udp_tx_cl = udp_tx.clone();
                    let sessions = sessions.clone();
                    tokio::spawn(async move {
                        let mut buf = vec![0u8; 65535];
                        let s_addr = s_addr;
                        loop {
                            let timeout = time::timeout(
                                Duration::from_secs(5),
                                proxy_udp.recv_from(&mut buf),
                            );
                            match timeout.await {
                                Ok(Ok((n, dest_addr))) => {
                                    udp_tx_cl.send_to(&buf[..n], &dest_addr, &s_addr).unwrap();
                                }
                                Ok(Err(e)) => {
                                    error!("proxy recv error: {}", e);
                                    return;
                                }
                                Err(_) => {
                                    let mut sesses = sessions.lock().await;
                                    debug!("session size: {}", sesses.len());
                                    sesses.remove(&s_addr);
                                    return;
                                }
                            }
                        }
                    });
                }
            };
        }
        Ok(())
    }
}
