use std::net::SocketAddr;

#[derive(Debug, Clone, Default)]
pub struct TcpOptions {
    // tcp option SO_SNDBUF
    pub send_buf_size: Option<u32>,

    // tcp option SO_RECVBUF
    pub recv_buf_size: Option<u32>,

    pub nodelay: bool,

    /// `TCP_FASTOPEN`, enables TFO
    pub fastopen: bool,
}

/// Options for UDP server
#[derive(Debug, Clone, Default)]
pub struct UdpSocketOpts {
    /// Maximum Transmission Unit (MTU) for UDP socket `recv`
    ///
    /// NOTE: MTU includes IP header, UDP header, UDP payload
    pub mtu: Option<usize>,

    /// Outbound UDP socket allows IP fragmentation
    pub allow_fragmentation: bool,
}

#[derive(Debug, Clone)]
pub struct ConnectOptions {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    pub fwmark: option<u32>,

    // Outbound socket binds to this IP address, bind local ip address for choose the network interfaces.
    pub bind_local_addr: Option<SocketAddr>,

    // Outbound socket binds to interface
    pub bind_interface: Option<String>,

    pub tcp_opts: Option<TcpOptions>,

    pub udp_opts: Option<UdpSocketOpts>,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            #[cfg(any(target_os = "linux", target_os = "android"))]
            fwmark: None,
            bind_local_addr: Some("0.0.0.0:0".parse().unwrap()),
            bind_interface: None,
            tcp_opts: None,
            udp_opts: None,
        }
    }
}
