use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicU32;
use std::time::Duration;

use crate::fakedns::FakeDNS;
use crate::net::{CopyTrait, copy_bidirectional_with_timeout, create_outbound_tcp_socket};
use crate::option::{
    DOWNLINK_COPY_TIMEOUT, LINK_BUFFER_SIZE, OUTBOUND_CONNECT_TIMEOUT, SOCKS5_ADDR,
    UPLINK_COPY_TIMEOUT,
};
use crate::socks5::{self, Socks5Addr};
use log::{debug, error, info};
use netstack_lwip::TcpStream;
use tokio::time;

impl CopyTrait for tokio::net::TcpStream {}

impl Into<Box<dyn CopyTrait>> for tokio::net::TcpStream {
    fn into(self) -> Box<dyn CopyTrait> {
        Box::new(self)
    }
}

pub struct TcpHandle {
    fake_dns: Arc<FakeDNS>,
}

impl TcpHandle {
    pub fn new(fake_dns: Arc<FakeDNS>) -> Self {
        TcpHandle { fake_dns }
    }
    /// hanlde tcp packet.
    pub async fn handle_tcp_stream(
        &self,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        mut tcp_stream: Pin<Box<TcpStream>>,
    ) -> io::Result<()> {
        let fake_dns = self.fake_dns.clone();
        tokio::spawn(async move {
            info!("start tcp: {src_addr} <-> {dst_addr}");
            let r_socket = match create_outbound_tcp_socket(&dst_addr) {
                Ok(s) => s,
                Err(e) => {
                    error!("create tcp socket error: {}", e);
                    return;
                }
            };
            let mut connect_fut: Option<
                Pin<Box<dyn Future<Output = io::Result<Box<dyn CopyTrait>>> + Send>>,
            > = None;
            if let SocketAddr::V4(addr) = dst_addr {
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
            debug!("created TCP connection for {src_addr} <-> {dst_addr}");
            if let Err(e) = copy_bidirectional_with_timeout(
                &mut tcp_stream,
                &mut proxy_conn,
                *LINK_BUFFER_SIZE * 1024,
                Duration::from_secs(*DOWNLINK_COPY_TIMEOUT),
                Duration::from_secs(*UPLINK_COPY_TIMEOUT),
            )
            .await
            {
                error!(" Tcp copy error, {e}");
            }
            debug!(" Tcp disconnect  {src_addr} <-> {dst_addr}");
        });

        Ok(())
    }
}
