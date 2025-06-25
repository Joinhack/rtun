use log::{debug, error, trace};
use smoltcp::iface::{Config as InterfaceConfig, Interface, PollResult, SocketHandle, SocketSet};
use smoltcp::phy::{Checksum, DeviceCapabilities, Medium};
use smoltcp::storage::RingBuffer;
use smoltcp::time::{Duration as SDuration, Instant as SInstant};
use spin::mutex::SpinMutex;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::thread::{self, Thread};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf, copy_bidirectional};
use tokio::sync::mpsc::{self, UnboundedSender};

use smoltcp::socket::tcp::{
    CongestionControl, Socket as TcpSocket, SocketBuffer as TcpSocketBuffer, State as TcpState,
};
use smoltcp::time::Duration as SDurantion;
use smoltcp::wire::{HardwareAddress, IpAddress, IpCidr, Ipv4Address, Ipv6Address, TcpPacket};
use std::io;

use crate::net::set_ip_bound_if;
use crate::virtual_device::{TokenBuffer, VirtTunDevice};

const DEFAULT_TCP_SEND_BUFFER_SIZE: u32 = (0x3FFFu32 * 5).next_power_of_two();
const DEFAULT_TCP_RECV_BUFFER_SIZE: u32 = (0x3FFFu32 * 5).next_power_of_two();

/// Options for connecting to TCP remote server
#[derive(Debug, Clone, Default)]
pub struct TcpSocketOpts {
    /// TCP socket's `SO_SNDBUF`
    pub send_buffer_size: Option<u32>,

    /// TCP socket's `SO_RCVBUF`
    pub recv_buffer_size: Option<u32>,

    /// `TCP_NODELAY`
    pub nodelay: bool,

    /// `TCP_FASTOPEN`, enables TFO
    pub fastopen: bool,

    /// `SO_KEEPALIVE` and sets `TCP_KEEPIDLE`, `TCP_KEEPINTVL` and `TCP_KEEPCNT` respectively,
    /// enables keep-alive messages on connection-oriented sockets
    pub keepalive: Option<Duration>,

    /// Enable Multipath-TCP (mptcp)
    /// https://en.wikipedia.org/wiki/Multipath_TCP
    ///
    /// Currently only supported on
    /// - macOS (iOS, watchOS, ...) with Client Support only.
    /// - Linux (>5.19)
    pub mptcp: bool,
}

/// Inbound connection options
#[derive(Clone, Debug, Default)]
pub struct AcceptOpts {
    /// TCP options
    pub tcp: TcpSocketOpts,

    /// Enable IPV6_V6ONLY option for socket
    pub ipv6_only: bool,
}

struct TcpTunManager {
    thread: Thread,
}

impl TcpTunManager {
    #[inline(always)]
    pub fn notify(&self) {
        self.thread.unpark();
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum SocketState {
    Normal,
    Close,
    Closing,
    Closed,
}

pub struct TcpIConnectInner {
    recv_buf: RingBuffer<'static, u8>,
    send_buf: RingBuffer<'static, u8>,
    send_waker: Option<Waker>,
    recv_waker: Option<Waker>,
    recv_state: SocketState,
    send_state: SocketState,
}

type SharedTcpConnect = Arc<SpinMutex<TcpIConnectInner>>;

struct CreateTcpConnect {
    tcp_connect_inner: SharedTcpConnect,
    sock: TcpSocket<'static>,
    created_tx: oneshot::Sender<()>,
}

#[derive(Clone)]
pub struct TcpConnect {
    inner: SharedTcpConnect,
    tun_manager: Arc<TcpTunManager>,
}

impl Drop for TcpConnect {
    fn drop(&mut self) {
        let mut inner = self.inner.lock();
        if matches!(inner.recv_state, SocketState::Normal) {
            inner.recv_state = SocketState::Close;
        }
        if matches!(inner.send_state, SocketState::Normal) {
            inner.send_state = SocketState::Close;
        }
        drop(inner);
        self.tun_manager.notify();
    }
}

impl AsyncRead for TcpConnect {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut connect_inner = self.inner.lock();
        if connect_inner.recv_buf.is_empty() {
            if let SocketState::Closed = connect_inner.recv_state {
                return Poll::Ready(Ok(()));
            }
            if let Some(waker) = connect_inner.recv_waker.replace(cx.waker().clone()) {
                // check it's the same task with current task.
                if waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
            return Poll::Pending;
        }
        let recv_buf = unsafe { std::mem::transmute::<_, &mut [u8]>(buf.unfilled_mut()) };
        // copy data to recv_buf
        let n = connect_inner.recv_buf.dequeue_slice(recv_buf);
        buf.advance(n);
        if n > 0 {
            self.tun_manager.notify();
        }
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for TcpConnect {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let mut connect_inner = self.inner.lock();
        if !matches!(connect_inner.send_state, SocketState::Normal) {
            return Err(io::ErrorKind::BrokenPipe.into()).into();
        }

        if connect_inner.send_buf.is_full() {
            if let Some(waker) = connect_inner.send_waker.replace(cx.waker().clone()) {
                if waker.will_wake(cx.waker()) {
                    waker.wake();
                }
            }
            return Poll::Pending;
        }
        // write data from buf
        let n = connect_inner.send_buf.enqueue_slice(buf);
        if n > 0 {
            self.tun_manager.notify();
        }
        Poll::Ready(Ok(n))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        let mut connect_inner = self.inner.lock();
        if matches!(connect_inner.send_state, SocketState::Close) {
            return Poll::Ready(Ok(()));
        }
        if matches!(connect_inner.send_state, SocketState::Normal) {
            connect_inner.send_state = SocketState::Close;
        }

        if let Some(waker) = connect_inner.send_waker.replace(cx.waker().clone()) {
            if waker.will_wake(cx.waker()) {
                waker.wake();
            }
        }
        self.tun_manager.notify();
        Poll::Pending
    }
}

impl TcpConnect {
    fn new(
        sock: TcpSocket<'static>,
        tun_manager: Arc<TcpTunManager>,
        create_tx: mpsc::UnboundedSender<CreateTcpConnect>,
    ) -> impl Future<Output = Self> {
        let send_buffer_size = DEFAULT_TCP_SEND_BUFFER_SIZE as _;
        let recv_buffer_size = DEFAULT_TCP_RECV_BUFFER_SIZE as _;
        let inner = Arc::new(SpinMutex::new(TcpIConnectInner {
            recv_buf: RingBuffer::new(vec![0u8; recv_buffer_size]),
            send_buf: RingBuffer::new(vec![0u8; send_buffer_size]),
            send_waker: None,
            recv_waker: None,
            recv_state: SocketState::Normal,
            send_state: SocketState::Normal,
        }));
        let (tx, rx) = oneshot::channel();
        let _ = create_tx.send(CreateTcpConnect {
            tcp_connect_inner: inner.clone(),
            created_tx: tx,
            sock,
        });
        async move {
            let _ = rx.await;
            Self {
                inner: inner,
                tun_manager,
            }
        }
    }
}

struct TcpSocketManager {
    device: VirtTunDevice,
    iface: Interface,
    socks_connects: HashMap<SocketHandle, SharedTcpConnect>,
    tcp_sock_create_rx: mpsc::UnboundedReceiver<CreateTcpConnect>,
}

pub struct TcpTun {
    manager: Arc<TcpTunManager>,
    tcp_sock_create_tx: UnboundedSender<CreateTcpConnect>,
    iface_tx: mpsc::UnboundedSender<TokenBuffer>,
    iface_rx: mpsc::UnboundedReceiver<TokenBuffer>,
    iface_tx_avail: Arc<AtomicBool>,
    manager_running: Arc<AtomicBool>,
}

impl TcpTun {
    pub fn chutdown(&self) {
        self.manager_running.store(false, Ordering::Release);
    }

    fn inner_run(
        mut socks_manager: TcpSocketManager,
        running: Arc<AtomicBool>,
        mut sockets_set: SocketSet<'_>,
    ) {
        let TcpSocketManager {
            ref mut device,
            ref mut iface,
            ref mut socks_connects,
            ref mut tcp_sock_create_rx,
        } = socks_manager;
        while running.load(Ordering::Acquire) {
            let previous_instant = SInstant::now();
            while let Ok(sock) = tcp_sock_create_rx.try_recv() {
                let handle = sockets_set.add(sock.sock);
                sock.created_tx.send(()).unwrap();
                socks_connects.insert(handle, sock.tcp_connect_inner);
            }
            if let PollResult::SocketStateChanged =
                iface.poll(previous_instant, device, &mut sockets_set)
            {
                trace!(
                    "VirtDevice::poll cost: {}",
                    SInstant::now() - previous_instant
                );
            }

            let mut remove_socks = vec![];

            for (handle, conn_inner) in socks_connects.iter() {
                let handle = *handle;
                let sock: &mut TcpSocket = sockets_set.get_mut(handle);
                let mut conn_inner = conn_inner.lock();

                // if socket closed, process the connect inner.
                if sock.state() == TcpState::Closed {
                    remove_socks.push(handle);
                    conn_inner.recv_state = SocketState::Closed;
                    conn_inner.send_state = SocketState::Closed;
                    if let Some(waker) = conn_inner.send_waker.take() {
                        waker.wake();
                    }
                    if let Some(waker) = conn_inner.recv_waker.take() {
                        waker.wake();
                    }
                    trace!("closed tcp connection");
                    continue;
                }

                //SHUT_WR which poll shutdown.
                if matches!(conn_inner.send_state, SocketState::Close)
                    && conn_inner.send_buf.len() == 0
                    && sock.send_queue() == 0
                {
                    trace!("closing TCP Write Half, {:?}", sock.state());
                    sock.close();
                    conn_inner.send_state = SocketState::Closing;
                }

                // Check if readable, read all data from socket and put it to the connect recv buffer.
                let mut wake_receiver = false;
                while sock.can_recv() && !conn_inner.recv_buf.is_full() {
                    let res = sock.recv(|buf| {
                        let n = conn_inner.recv_buf.enqueue_slice(buf);
                        (n, ())
                    });
                    match res {
                        Ok(_) => wake_receiver = true,
                        Err(e) => {
                            error!("socket recv error: {:?}, {:?}", e, sock.state());
                            sock.abort();
                            if matches!(conn_inner.recv_state, SocketState::Normal) {
                                conn_inner.recv_state = SocketState::Closed
                            }
                            wake_receiver = true;
                            break;
                        }
                    }
                }
                // If socket is not in ESTABLISH, FIN-WAIT-1, FIN-WAIT-2,
                // the local client have closed our receiver.
                if matches!(conn_inner.recv_state, SocketState::Normal)
                    && !sock.may_recv()
                    && !matches!(
                        sock.state(),
                        TcpState::Listen
                            | TcpState::SynReceived
                            | TcpState::Established
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                {
                    trace!("closed TCP Read Half, {:?}", sock.state());

                    // Let TcpConnection::poll_read returns EOF.
                    conn_inner.recv_state = SocketState::Closed;
                    wake_receiver = true;
                }
                if wake_receiver && conn_inner.recv_waker.is_some() {
                    if let Some(waker) = conn_inner.recv_waker.take() {
                        waker.wake();
                    }
                }

                // Check if writable
                // put the connect write buffer data to socket
                let mut wake_sender = false;
                while sock.can_send() && !conn_inner.send_buf.is_empty() {
                    let result = sock.send(|buffer| {
                        let n = conn_inner.send_buf.dequeue_slice(buffer);
                        (n, ())
                    });

                    match result {
                        Ok(_) => {
                            wake_sender = true;
                        }
                        Err(err) => {
                            error!("socket send error: {:?}, {:?}", err, sock.state());

                            // Don't know why. Abort the connection.
                            sock.abort();

                            if matches!(conn_inner.send_state, SocketState::Normal) {
                                conn_inner.send_state = SocketState::Closed;
                            }
                            wake_sender = true;

                            // The socket will be recycled in the next poll.
                            break;
                        }
                    }
                }
                if wake_sender && conn_inner.send_waker.is_some() {
                    if let Some(waker) = conn_inner.send_waker.take() {
                        waker.wake();
                    }
                }
            }

            for handle in remove_socks {
                sockets_set.remove(handle);
                socks_connects.remove(&handle);
            }

            if !device.recv_available() {
                let next_duration = iface
                    .poll_delay(previous_instant, &sockets_set)
                    .unwrap_or(SDuration::from_millis(5));
                if next_duration != SDuration::ZERO {
                    thread::park_timeout(next_duration.into());
                }
            }
        }
    }

    pub fn new(mtu: u32) -> Self {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = mtu as usize;
        capabilities.checksum.ipv4 = Checksum::Tx;
        capabilities.checksum.tcp = Checksum::Tx;
        capabilities.checksum.udp = Checksum::Tx;
        capabilities.checksum.icmpv4 = Checksum::Tx;
        capabilities.checksum.icmpv6 = Checksum::Tx;
        let mut iface_config = InterfaceConfig::new(HardwareAddress::Ip);
        let (mut device, iface_tx, iface_rx, iface_tx_avail) = VirtTunDevice::new(capabilities);
        iface_config.random_seed = rand::random();
        let mut iface = Interface::new(iface_config, &mut device, SInstant::now());
        iface.update_ip_addrs(|ip_addrs| {
            ip_addrs
                .push(IpCidr::new(IpAddress::v4(0, 0, 0, 1), 0))
                .expect("iface IPv4");
            ip_addrs
                .push(IpCidr::new(IpAddress::v6(0, 0, 0, 0, 0, 0, 0, 1), 0))
                .expect("iface IPv6");
        });
        iface
            .routes_mut()
            .add_default_ipv4_route(Ipv4Address::new(0, 0, 0, 1))
            .expect("IPv4 default route");
        iface
            .routes_mut()
            .add_default_ipv6_route(Ipv6Address::new(0, 0, 0, 0, 0, 0, 0, 1))
            .expect("IPv6 default route");
        iface.set_any_ip(true);

        let (tcp_sock_create_tx, tcp_sock_create_rx) =
            mpsc::unbounded_channel::<CreateTcpConnect>();

        let sockets_set = SocketSet::new(vec![]);
        let socks_manager = TcpSocketManager {
            device,
            iface,
            socks_connects: HashMap::new(),
            tcp_sock_create_rx,
        };
        let manager_running = Arc::new(AtomicBool::new(true));
        let running = manager_running.clone();
        let thread_fn = move || {
            Self::inner_run(socks_manager, running, sockets_set);
        };
        let manager_thread = thread::Builder::new()
            .name("TcpTunThread".into())
            .spawn(thread_fn)
            .unwrap()
            .thread()
            .clone();
        Self {
            manager: Arc::new(TcpTunManager {
                thread: manager_thread,
            }),
            iface_tx,
            iface_rx,
            iface_tx_avail,
            tcp_sock_create_tx,
            manager_running,
        }
    }

    pub async fn recv_packet(&mut self) -> TokenBuffer {
        self.iface_rx
            .recv()
            .await
            .expect("channel closed unexpectedly")
    }

    /// handle tcp raw frame after tcp connection created.
    pub fn send_tcp_frame(&mut self, frame: TokenBuffer) {
        self.iface_tx
            .send(frame)
            .expect("interface send channel close.");
        self.iface_tx_avail.store(true, Ordering::Release);
        // the backend thread will call poll of tcp stack
        self.manager.notify();
    }

    /// hanlde tcp packet.
    pub fn intercept_tcp_syn(
        &mut self,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
        packet: &TcpPacket<&[u8]>,
    ) -> io::Result<()> {
        // TCP syn packet, three-way handleshake start.
        if packet.syn() && !packet.ack() {
            let send_buffer_size = DEFAULT_TCP_SEND_BUFFER_SIZE as _;
            let recv_buffer_size = DEFAULT_TCP_RECV_BUFFER_SIZE as _;

            let mut sock = TcpSocket::new(
                TcpSocketBuffer::new(vec![0u8; recv_buffer_size]),
                TcpSocketBuffer::new(vec![0u8; send_buffer_size]),
            );
            sock.set_timeout(Some(SDurantion::from_secs(7200)));
            sock.set_congestion_control(CongestionControl::Cubic);
            if let Err(e) = sock.listen(dst_addr) {
                error!("handle socket listen error: {}", e);
                return Err(io::Error::other(format!("listen error: {:?}", e)));
            }
            let tcp_conn_fut =
                TcpConnect::new(sock, self.manager.clone(), self.tcp_sock_create_tx.clone());
            tokio::spawn(async move {
                let mut tcp_conn = tcp_conn_fut.await;
                let r_socket = tokio::net::TcpSocket::new_v4().unwrap();
                r_socket.bind("10.4.126.67:0".parse().unwrap()).unwrap();

                let mut proxy_conn = match r_socket.connect(dst_addr).await {
                    Ok(c) => c,
                    Err(e) => {
                        error!("connect remote error: {}", e);
                        return;
                    }
                };
                set_ip_bound_if(&proxy_conn, &dst_addr, "en0").unwrap();
                if let Err(e) = copy_bidirectional(&mut tcp_conn, &mut proxy_conn).await {
                    error!("connect copy error: {}", e);
                }
            });
            debug!("created TCP connection for {} <-> {}", src_addr, dst_addr);
        }
        Ok(())
    }
}
