use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use crate::cancelable::{CancelHandle, Cancelable, CancelableResult};
use crate::fakedns::FakeDNS;
use crate::net::{CopyTrait, copy_bidirectional_with_timeout, create_outbound_tcp_socket};
use crate::option::{LINK_BUFFER_SIZE, LINK_COPY_TIMEOUT, OUTBOUND_CONNECT_TIMEOUT, SOCKS5_ADDR};
use crate::socks5::{self, Socks5Addr};
use futures::FutureExt;
use log::{debug, error, info};
use netstack_lwip::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::time;

impl CopyTrait for tokio::net::TcpStream {}

impl Into<Box<dyn CopyTrait>> for tokio::net::TcpStream {
    fn into(self) -> Box<dyn CopyTrait> {
        Box::new(self)
    }
}

type ConnectsType = Arc<Mutex<HashMap<(SocketAddr, SocketAddr), CancelHandle>>>;

type ConnectFut = Pin<Box<dyn Future<Output = io::Result<Box<dyn CopyTrait>>> + Send>>;

pub struct TcpHandle {
    fake_dns: Arc<FakeDNS>,
    clear_task: Option<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    connects: ConnectsType,
}

impl TcpHandle {
    pub fn new(fake_dns: Arc<FakeDNS>, mut notify: Receiver<()>) -> Self {
        let connects: ConnectsType = Default::default();
        let connects_cl = connects.clone();
        let clear_task = async move {
            loop {
                match notify.recv().await {
                    Ok(_) => {
                        // the ip chaned, clean all connections.
                        info!("clear all tcp connections");
                        let mut guard = connects_cl.lock().await;
                        guard.clear();
                    }
                    Err(RecvError::Lagged(_)) => {
                        debug!("recv Lagged message.")
                    }
                    Err(RecvError::Closed) => {
                        debug!("notify channel closed.");
                        return;
                    }
                };
            }
        }
        .boxed();
        TcpHandle {
            fake_dns,
            clear_task: Some(clear_task),
            connects: connects,
        }
    }

    /// Handle a TCP stream, connecting to the destination address.
    /// This function spawns a new task to handle the TCP connection.
    /// It uses a fake DNS to resolve domain names if necessary,
    /// and it supports SOCKS5 proxying if the destination address is a domain.
    /// The function will copy data bidirectionally
    /// between the TCP stream and the connected socket,
    /// with timeouts for both uplink and downlink.
    pub async fn handle_tcp_stream(
        &mut self,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        mut tcp_stream: Pin<Box<TcpStream>>,
    ) -> io::Result<()> {
        let fake_dns = self.fake_dns.clone();
        let connects = self.connects.clone();
        if let Some(fut) = self.clear_task.take() {
            tokio::spawn(fut);
        }
        tokio::spawn(async move {
            info!("start tcp: {src_addr} <-> {dst_addr}");
            let r_socket = match create_outbound_tcp_socket(&dst_addr) {
                Ok(s) => s,
                Err(e) => {
                    error!("create tcp socket error: {}", e);
                    return;
                }
            };

            let mut connect_fut: Option<ConnectFut> = None;
            if let SocketAddr::V4(addr) = dst_addr {
                // connect the sock5 address
                if let Some(s) = fake_dns.query_domain(addr.ip().to_bits()).await {
                    connect_fut = Some(Box::pin(async move {
                        socks5::Sock5TcpStream::connect(
                            Socks5Addr::Domain(s, addr.port()),
                            *SOCKS5_ADDR,
                        )
                        .await
                        .map(|s| s.into())
                    }));
                }
            }
            if connect_fut.is_none() {
                connect_fut = Some(Box::pin(async move {
                    r_socket.connect(dst_addr).await.map(|s| s.into())
                }));
            }
            let connect_timeout = time::timeout(
                Duration::from_secs(*OUTBOUND_CONNECT_TIMEOUT),
                connect_fut.unwrap(),
            );
            let mut proxy_conn = match connect_timeout.await {
                Ok(Err(e)) => {
                    error!("connect remote error {}", e);
                    return;
                }
                Ok(Ok(c)) => c,
                Err(_) => {
                    error!("connect remote {} timeout", dst_addr);
                    return;
                }
            };

            let connects_key = (src_addr, dst_addr);

            let copy_fut = copy_bidirectional_with_timeout(
                &mut proxy_conn,
                &mut tcp_stream,
                *LINK_BUFFER_SIZE * 1024,
                Duration::from_secs(*LINK_COPY_TIMEOUT),
            );
            let (copy_fut, handle) = Cancelable::new(copy_fut);
            //connects_guard must drop  if insert success.
            let count = {
                let mut connects_guard = connects.lock().await;
                connects_guard.insert(connects_key, handle);
                connects_guard.len()
            };
            info!("created TCP connection for {src_addr} <-> {dst_addr} count:{count}");
            match copy_fut.await {
                CancelableResult::Result(Ok(_)) => (),
                CancelableResult::Result(Err(e)) => {
                    error!("TCP copy {src_addr} <-> {dst_addr} error, {e}");
                }
                CancelableResult::Cancelled => {
                    error!("Tcp copy {src_addr} <-> {dst_addr} cancelled.");
                }
            };
            let count = {
                let mut connects_guard = connects.lock().await;
                connects_guard.remove(&connects_key);
                connects_guard.len()
            };
            info!("Tcp disconnect  {src_addr} <-> {dst_addr} count:{count}");
        });

        Ok(())
    }
}
