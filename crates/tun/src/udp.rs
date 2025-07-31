use futures::StreamExt;
use log::{debug, error};
use netstack_lwip::UdpSocket;
use netstack_lwip::udp::SendHalf;
use std::collections::hash_map::Entry;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, io};
use tokio::net::UdpSocket as TokioUdpSocket;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{Receiver, Sender, channel};
use tokio::time;

use std::{net::SocketAddr, pin::Pin};

use crate::dns_proxy::DnsProxy;
use crate::fakedns::{FakeDNS, FakeDnsProcessed};
use crate::option::UDP_SESSION_TIMEOUT;
use crate::{net::create_outbound_udp_socket, option::UDP_RECV_CH_SIZE};

struct UdpPkg {
    payload: Vec<u8>,
}

type SessionMap = Arc<Mutex<HashMap<SocketAddr, Sender<UdpPkg>>>>;

pub struct UdpHandle {
    sessions: SessionMap,

    fake_dns: Arc<FakeDNS>,
}

impl UdpHandle {
    pub fn new(fake_dns: Arc<FakeDNS>) -> Self {
        Self {
            sessions: Default::default(),
            fake_dns,
        }
    }

    pub async fn handle_udp_socket(&self, udp_socket: Pin<Box<UdpSocket>>) -> io::Result<()> {
        let (udp_tx, mut udp_rx) = udp_socket.split();

        let sessions = &self.sessions;
        let udp_tx = Arc::new(udp_tx);
        let mut dns_proxy = DnsProxy::new(udp_tx.clone());

        // recv from local tunnel packet then send them to remote.
        let forward_to_outgoing =
            |mut rx: Receiver<UdpPkg>, d_addr: SocketAddr, proxy_udp_cl: Arc<TokioUdpSocket>| async move {
                loop {
                    let pkg = match rx.recv().await {
                        Some(pkg) => pkg,
                        None => {
                            debug!("udp recv channel closed");
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
            };

        // recv from outgoing udp socket and send to local tunnel.
        let forward_to_incoming =
            |sessions: SessionMap,
             s_addr: SocketAddr,
             udp_tx_cl: Arc<SendHalf>,
             proxy_udp: Arc<TokioUdpSocket>| async move {
                let mut buf = vec![0u8; 1500];
                let s_addr = s_addr;
                loop {
                    let timeout = time::timeout(
                        Duration::from_secs(*UDP_SESSION_TIMEOUT),
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
                            debug!("s_addr: {} removed, sessions: {}", s_addr, sesses.len());
                            sesses.remove(&s_addr);
                            return;
                        }
                    }
                }
            };
        while let Some((data, s_addr, d_addr)) = udp_rx.next().await {
            if d_addr.port() == 53 {
                match self.fake_dns.process_dns_packet(&data).await {
                    // use upstream to resolve domain.
                    Ok(FakeDnsProcessed::Upstream(msg)) => {
                        if let Err(e) = dns_proxy.send(s_addr, d_addr, msg).await {
                            error!("dns proxy send error {}", e);
                        }
                        continue;
                    }
                    // use fake ip as response.
                    Ok(FakeDnsProcessed::Response(resp)) => {
                        udp_tx.send_to(&resp, &d_addr, &s_addr)?;
                        continue;
                    }
                    Err(e) => {
                        error!("error process dns: {e}");
                        continue;
                    }
                }
            }
            let mut sesses = sessions.lock().await;
            let s_len = sesses.len();
            let entry = sesses.entry(s_addr);
            match entry {
                Entry::Occupied(mut entry) => {
                    if let Err(e) = entry.get_mut().send(UdpPkg { payload: data }).await {
                        error!("send channel is closed, error: {}", e);
                        entry.remove();
                    }
                }
                Entry::Vacant(vacant) => {
                    debug!(
                        "add seession s_addr: {s_addr} d_addr: {d_addr}, session: {}",
                        s_len
                    );
                    let (tx, rx) = channel::<UdpPkg>(*UDP_RECV_CH_SIZE);
                    tx.send(UdpPkg { payload: data }).await.unwrap();
                    let proxy_udp = create_outbound_udp_socket(&d_addr).unwrap();
                    let proxy_udp = Arc::new(proxy_udp);
                    let proxy_udp_cl = proxy_udp.clone();
                    vacant.insert(tx);
                    // recv from local channel and send to remote address.
                    tokio::spawn(forward_to_outgoing(rx, d_addr, proxy_udp_cl));
                    let udp_tx_cl = udp_tx.clone();
                    let sessions = sessions.clone();
                    tokio::spawn(forward_to_incoming(sessions, s_addr, udp_tx_cl, proxy_udp));
                }
            };
        }
        Ok(())
    }
}
