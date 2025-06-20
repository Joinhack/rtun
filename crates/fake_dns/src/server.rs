use std::net::IpAddr;
use std::time::Duration;
use std::{
    io::{self, Result as IOResult},
    sync::Arc,
};

use byteorder::{BigEndian, ByteOrder};
use bytes::{BufMut, BytesMut};
use futures::FutureExt;
use hickory_resolver::proto::op::Message;
use log::error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};

use crate::client_cache::{DnsClientCache, DnsKey};
use crate::options::ConnectOptions;

struct TcpServer {
    tcp_lisnter: TcpListener,
    dns_client_cache: Arc<DnsClientCache>,
    connect_opts: ConnectOptions,
    remote_addr: (IpAddr, u16),
}

impl TcpServer {
    pub async fn new(
        bind_addr: (IpAddr, u16),
        remote_addr: (IpAddr, u16),
        dns_client_cache: Arc<DnsClientCache>,
        connect_opts: ConnectOptions,
    ) -> IOResult<Self> {
        let tcp_lisnter = TcpListener::bind(bind_addr).await?;
        Ok(Self {
            tcp_lisnter,
            dns_client_cache,
            connect_opts,
            remote_addr,
        })
    }
    pub async fn run(self) -> IOResult<()> {
        loop {
            let (mut tcp_stream, _peer_addr) = match self.tcp_lisnter.accept().await {
                Ok(a) => a,
                Err(e) => {
                    error!("dns tcp listener accept error ! {}", e);
                    continue;
                }
            };
            let client_cache = self.dns_client_cache.clone();
            let connect_opts = self.connect_opts.clone();
            let key = DnsKey::TcpRemote(self.remote_addr.clone());
            tokio::spawn(async move {
                let connect_opts = connect_opts;
                loop {
                    let mut len_buf = [0u8; 2];
                    if let Err(e) = tcp_stream.read_exact(&mut len_buf).await {
                        error!("dns tcp read error: {}", e);
                        return;
                    }
                    let len = BigEndian::read_u16(&len_buf) as usize;
                    let mut buf = BytesMut::with_capacity(len);
                    unsafe {
                        buf.advance_mut(len);
                    }
                    if let Err(e) = tcp_stream.read_exact(&mut buf).await {
                        error!("dns tcp read error: {}", e);
                        return;
                    }
                    let message = match Message::from_vec(&buf) {
                        Ok(m) => m,
                        Err(e) => {
                            error!("dns tcp decode error: {e}");
                            return;
                        }
                    };
                    let resp_msg = match client_cache
                        .lookup_timeout(key.clone(), &connect_opts, message)
                        .await
                    {
                        Ok(m) => m,
                        Err(e) => {
                            error!("dns message lookup failed with error: {e}");
                            return;
                        }
                    };
                    let mut resp_vec = match resp_msg.to_vec() {
                        Ok(v) => v,
                        Err(e) => {
                            error!("dns message encode error: {e}");
                            return;
                        }
                    };
                    let resp_len = resp_vec.len();
                    resp_vec.resize(resp_len + 2, 0);
                    resp_vec.copy_within(0..resp_len, 2);
                    BigEndian::write_u16(&mut resp_vec[..2], resp_len as _);
                    if let Err(e) = tcp_stream.write_all(&resp_vec).await {
                        error!("dns tcp stream write error: {e}");
                        return;
                    }
                }
            });
        }
    }
}

struct UdpServer {
    udp_socket: Arc<UdpSocket>,
    dns_client_cache: Arc<DnsClientCache>,
    connect_opts: ConnectOptions,
    remote_addr: (IpAddr, u16),
}

impl UdpServer {
    pub async fn new(
        bind_addr: (IpAddr, u16),
        remote_addr: (IpAddr, u16),
        dns_client_cache: Arc<DnsClientCache>,
        connect_opts: ConnectOptions,
    ) -> IOResult<Self> {
        Ok(Self {
            remote_addr,
            udp_socket: Arc::new(UdpSocket::bind(bind_addr).await?),
            dns_client_cache,
            connect_opts: connect_opts,
        })
    }

    pub async fn run(self) -> IOResult<()> {
        let mut buf = [0u8; 512];
        loop {
            let (n, peer) = match self.udp_socket.recv_from(&mut buf).await {
                Ok(n) => n,
                Err(e) => {
                    error!("udp server recv_from failed with error: {}", e);
                    continue;
                }
            };
            let message = match Message::from_vec(&buf[..n]) {
                Ok(m) => m,
                Err(e) => {
                    error!("udp decode dns failed with error: {}", e);
                    continue;
                }
            };
            let dnskey = DnsKey::UdpRemote(self.remote_addr.clone());
            let opts = self.connect_opts.clone();
            let cache = self.dns_client_cache.clone();
            let socket = self.udp_socket.clone();
            tokio::spawn(async move {
                let opts = opts;
                let resp_msg = match cache.lookup_timeout(dnskey, &opts, message).await {
                    Ok(m) => m,
                    Err(e) => {
                        error!("dns message lookup failed with error: {e}");
                        return;
                    }
                };
                let m_vec = match resp_msg.to_vec() {
                    Ok(m) => m,
                    Err(e) => {
                        error!("dns message encode failed with error: {e}");
                        return;
                    }
                };
                if let Err(e) = socket.send_to(&m_vec, peer).await {
                    error!("dns message sendto failed with error: {e}");
                }
            });
        }
    }
}

pub struct DnsServerBuilder {
    udp_remote: Option<(IpAddr, u16)>,
    tcp_remote: Option<(IpAddr, u16)>,
    connect_opts: ConnectOptions,
    timeout: Duration,
    retry_times: u8,
    svr_bind_addr: (IpAddr, u16),
}

impl DnsServerBuilder {
    pub fn new(svr_bind_addr: (IpAddr, u16)) -> Self {
        Self {
            udp_remote: None,
            tcp_remote: None,
            timeout: Duration::from_secs(3),
            retry_times: 3,
            connect_opts: ConnectOptions::default(),
            svr_bind_addr,
        }
    }

    pub fn bind_addr(&mut self, addr: (IpAddr, u16)) -> &mut Self {
        self.svr_bind_addr = addr;
        self
    }

    pub fn retry_times(&mut self, retry_times: u8) -> &mut Self {
        self.retry_times = retry_times;
        self
    }

    pub fn timeout(&mut self, timeout: Duration) -> &mut Self {
        self.timeout = timeout;
        self
    }

    pub fn connect_options(&mut self, connect_opts: ConnectOptions) -> &mut Self {
        self.connect_opts = connect_opts;
        self
    }

    pub fn udp_remote(&mut self, udp_remote: Option<(IpAddr, u16)>) -> &mut Self {
        self.udp_remote = udp_remote;
        self
    }

    pub fn tcp_remote(&mut self, tcp_remote: Option<(IpAddr, u16)>) -> &mut Self {
        self.tcp_remote = tcp_remote;
        self
    }

    pub async fn build(&mut self) -> IOResult<DnsServer> {
        if self.udp_remote.is_none() && self.tcp_remote.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::AddrNotAvailable,
                "remote address is needed.",
            ));
        }
        let dns_client_cache = Arc::new(DnsClientCache::new(self.timeout, self.retry_times));
        let udp_svr = match self.udp_remote {
            Some(r) => Some(
                UdpServer::new(
                    self.svr_bind_addr.clone(),
                    r,
                    dns_client_cache.clone(),
                    self.connect_opts.clone(),
                )
                .await?,
            ),
            None => None,
        };
        let tcp_svr = match self.udp_remote {
            Some(r) => Some(
                TcpServer::new(
                    self.svr_bind_addr.clone(),
                    r,
                    dns_client_cache.clone(),
                    self.connect_opts.clone(),
                )
                .await?,
            ),
            None => None,
        };
        Ok(DnsServer {
            udp_svr,
            tcp_svr,
            dns_client_cache,
        })
    }
}

pub struct DnsServer {
    udp_svr: Option<UdpServer>,
    tcp_svr: Option<TcpServer>,
    dns_client_cache: Arc<DnsClientCache>,
}

impl DnsServer {
    pub async fn run(self) -> IOResult<()> {
        let mut vec = Vec::new();
        if let Some(udp_svr) = self.udp_svr {
            vec.push(udp_svr.run().boxed());
        }
        if let Some(tcp_svr) = self.tcp_svr {
            vec.push(tcp_svr.run().boxed());
        }
        let (res, ..) = futures::future::select_all(vec).await;
        return res;
    }
}
