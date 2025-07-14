use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::time::Duration;

use crate::net::{copy_bidirectional_with_timeout, create_outbound_tcp_socket};
use crate::option::{
    DOWNLINK_COPY_TIMEOUT, LINK_BUFFER_SIZE, OUTBOUND_CONNECT_TIMEOUT, UPLINK_COPY_TIMEOUT,
};
use log::{debug, error, info};
use netstack_lwip::TcpStream;
use tokio::time;

pub struct TcpHandle {}
impl TcpHandle {
    pub fn new() -> Self {
        TcpHandle {}
    }
    /// hanlde tcp packet.
    pub async fn handle_tcp_stream(
        &self,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        mut tcp_stream: Pin<Box<TcpStream>>,
    ) -> io::Result<()> {
        tokio::spawn(async move {
            info!("start tcp: {src_addr} <-> {dst_addr}");
            let r_socket = match create_outbound_tcp_socket(&dst_addr) {
                Ok(s) => s,
                Err(e) => {
                    error!("create tcp socket error: {}", e);
                    return;
                }
            };
            let connect_timeout = time::timeout(
                Duration::from_secs(*OUTBOUND_CONNECT_TIMEOUT),
                r_socket.connect(dst_addr),
            );

            // let stream =
            // socks5::Sock5TcpStream::connect(dst_addr, "127.0.0.1:1086".parse().unwrap());
            // let connect_timeout =
            // time::timeout(Duration::from_secs(*OUTBOUND_CONNECT_TIMEOUT), stream);
            let mut proxy_conn = match connect_timeout.await {
                Ok(Ok(c)) => c,
                Ok(Err(e)) => {
                    error!("connect remote {} error: {}", dst_addr, e);
                    return;
                }
                Err(_) => {
                    error!("connect remote {} timeout", dst_addr);
                    return;
                }
            };

            debug!("created TCP connection for {} <-> {}", src_addr, dst_addr);
            if let Err(e) = copy_bidirectional_with_timeout(
                &mut tcp_stream,
                &mut proxy_conn,
                *LINK_BUFFER_SIZE * 1024,
                Duration::from_secs(*DOWNLINK_COPY_TIMEOUT),
                Duration::from_secs(*UPLINK_COPY_TIMEOUT),
            )
            .await
            {
                error!("connect copy error: {}", e);
            }
        });

        Ok(())
    }
}
