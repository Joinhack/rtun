use std::{
    collections::HashMap,
    io, mem,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Waker},
};

use futures::{FutureExt, Stream};
use log::{debug, error, trace};
use smoltcp::{
    iface::{Config as InterfaceConfig, Interface, PollResult, SocketHandle, SocketSet},
    phy::{Checksum, DeviceCapabilities, Medium},
    socket::tcp::{
        CongestionControl, Socket as TcpSocket, SocketBuffer as TcpSocketBuffer, State as TcpState,
    },
    storage::RingBuffer,
    time::{Duration as SmolDuration, Instant as SmolInstant},
    wire::{HardwareAddress, IpAddress, IpCidr, IpProtocol, Ipv4Address, Ipv6Address, TcpPacket},
};
use spin::Mutex as SpinMutex;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    sync::{
        Notify,
        mpsc::{self, Receiver, Sender, UnboundedSender},
    },
    time::timeout,
};

use crate::{
    option::{LISNTER_CHANNEL, TCP_KEEPALIVE},
    stack::{StackPacket, ip::IpPacket},
};

use super::virt_device::VirtTunDevice;

// NOTE: Default buffer could contain 5 AEAD packets
const DEFAULT_TCP_SEND_BUFFER_SIZE: u32 = (0x3FFFu32 * 5).next_power_of_two();
const DEFAULT_TCP_RECV_BUFFER_SIZE: u32 = (0x3FFFu32 * 5).next_power_of_two();

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum TcpSocketState {
    Normal,
    Close,
    Closing,
    Closed,
}

struct TcpSocketControl {
    send_buffer: RingBuffer<'static, u8>,
    send_waker: Option<Waker>,
    recv_buffer: RingBuffer<'static, u8>,
    recv_waker: Option<Waker>,
    recv_state: TcpSocketState,
    send_state: TcpSocketState,
}

struct TcpSocketManager {
    device: VirtTunDevice,
    iface: Interface,
    manager_notify: Arc<Notify>,
    lisnter_tx: Sender<ListenerType>,
    sockets: HashMap<SocketHandle, SharedTcpConnectionControl>,
    socket_creation_rx: mpsc::UnboundedReceiver<TcpSocketCreation>,
}

type SharedTcpConnectionControl = Arc<SpinMutex<TcpSocketControl>>;

struct TcpSocketCreation {
    control: SharedTcpConnectionControl,
    socket: TcpSocket<'static>,
    src_addr: SocketAddr,
    dst_addr: SocketAddr,
}

pub struct TcpStream {
    control: SharedTcpConnectionControl,
    manager_notify: Arc<Notify>,
}

impl Drop for TcpStream {
    fn drop(&mut self) {
        let mut control = self.control.lock();

        if matches!(control.recv_state, TcpSocketState::Normal) {
            control.recv_state = TcpSocketState::Close;
        }

        if matches!(control.send_state, TcpSocketState::Normal) {
            control.send_state = TcpSocketState::Close;
        }

        self.manager_notify.notify_waiters();
    }
}

impl TcpStream {
    fn create(
        socket: TcpSocket<'static>,
        socket_creation_tx: &mpsc::UnboundedSender<TcpSocketCreation>,
        src_addr: SocketAddr,
        dst_addr: SocketAddr,
    ) {
        let send_buffer_size = DEFAULT_TCP_SEND_BUFFER_SIZE;
        let recv_buffer_size = DEFAULT_TCP_RECV_BUFFER_SIZE;

        let control = Arc::new(SpinMutex::new(TcpSocketControl {
            send_buffer: RingBuffer::new(vec![0u8; send_buffer_size as usize]),
            send_waker: None,
            recv_buffer: RingBuffer::new(vec![0u8; recv_buffer_size as usize]),
            recv_waker: None,
            recv_state: TcpSocketState::Normal,
            send_state: TcpSocketState::Normal,
        }));
        let _ = socket_creation_tx.send(TcpSocketCreation {
            control: control.clone(),
            socket,
            src_addr,
            dst_addr,
        });
    }
}

impl AsyncRead for TcpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let mut control = self.control.lock();

        // Read from buffer
        if control.recv_buffer.is_empty() {
            // If socket is already closed / half closed, just return EOF directly.
            if matches!(control.recv_state, TcpSocketState::Closed) {
                return Ok(()).into();
            }

            // Nothing could be read. Wait for notify.
            if let Some(old_waker) = control.recv_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let recv_buf =
            unsafe { mem::transmute::<&mut [mem::MaybeUninit<u8>], &mut [u8]>(buf.unfilled_mut()) };
        let n = control.recv_buffer.dequeue_slice(recv_buf);
        buf.advance(n);

        if n > 0 {
            self.manager_notify.notify_waiters();
        }
        Ok(()).into()
    }
}

impl AsyncWrite for TcpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let mut control = self.control.lock();

        // If state == Close | Closing | Closed, the TCP stream WR half is closed.
        if !matches!(control.send_state, TcpSocketState::Normal) {
            return Err(io::ErrorKind::BrokenPipe.into()).into();
        }

        // Write to buffer

        if control.send_buffer.is_full() {
            if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
                if !old_waker.will_wake(cx.waker()) {
                    old_waker.wake();
                }
            }

            return Poll::Pending;
        }

        let n = control.send_buffer.enqueue_slice(buf);

        if n > 0 {
            self.manager_notify.notify_waiters();
        }
        Ok(n).into()
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Ok(()).into()
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut control = self.control.lock();

        if matches!(control.send_state, TcpSocketState::Closed) {
            return Ok(()).into();
        }

        // SHUT_WR
        if matches!(control.send_state, TcpSocketState::Normal) {
            control.send_state = TcpSocketState::Close;
        }

        if let Some(old_waker) = control.send_waker.replace(cx.waker().clone()) {
            if !old_waker.will_wake(cx.waker()) {
                old_waker.wake();
            }
        }

        self.manager_notify.notify_waiters();
        Poll::Pending
    }
}

pub type ListenerType = (TcpStream, SocketAddr, SocketAddr);

pub struct TcpListener {
    lisnter_rx: Receiver<ListenerType>,
}

impl Stream for TcpListener {
    type Item = ListenerType;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let TcpListener { lisnter_rx } = &mut *self;
        lisnter_rx.poll_recv(cx)
    }
}

pub struct TcpStack {
    manager_notify: Arc<Notify>,
    manager_running: Arc<AtomicBool>,
    manager: TcpSocketManager,
}

struct TcpPacketHandle {
    tcp_rx: Receiver<StackPacket>,
    iface_tx: UnboundedSender<StackPacket>,
    manager_socket_creation_tx: mpsc::UnboundedSender<TcpSocketCreation>,
    iface_tx_avail: Arc<AtomicBool>,
    manager_notify: Arc<Notify>,
}

impl Drop for TcpStack {
    fn drop(&mut self) {
        self.manager_running.store(false, Ordering::Relaxed);
        self.manager_notify.notify_waiters();
    }
}

pub type TcpRunner = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

impl TcpPacketHandle {
    pub async fn handle_packet(&mut self) {
        // TCP first handshake packet, create a new Connection
        while let Some(frame) = self.tcp_rx.recv().await {
            let ip = match IpPacket::new_checked(&frame) {
                Ok(p) => p,
                Err(e) => {
                    trace!("ip packet marshall error, {e}");
                    continue;
                }
            };
            let src_addr = ip.src_addr();
            let dst_addr = ip.dst_addr();
            if !matches!(ip.protocol(), IpProtocol::Tcp) {
                trace!("invail protocol {}", ip.protocol());
                continue;
            }
            let tcp = TcpPacket::new_unchecked(ip.payload());
            if tcp.syn() && !tcp.ack() {
                let src_addr = SocketAddr::new(src_addr, tcp.src_port());
                let dst_addr = SocketAddr::new(dst_addr, tcp.dst_port());
                let mut socket = TcpSocket::new(
                    TcpSocketBuffer::new(vec![0u8; DEFAULT_TCP_SEND_BUFFER_SIZE as usize]),
                    TcpSocketBuffer::new(vec![0u8; DEFAULT_TCP_RECV_BUFFER_SIZE as usize]),
                );
                socket.set_keep_alive(Some(smoltcp::time::Duration::from_secs(*TCP_KEEPALIVE)));
                // FIXME: It should follow system's setting. 7200 is Linux's default.
                socket.set_timeout(Some(SmolDuration::from_secs(7200)));
                // NO ACK delay
                // socket.set_ack_delay(None);
                // Enable Cubic congestion control
                socket.set_congestion_control(CongestionControl::Cubic);

                if let Err(err) = socket.listen(dst_addr) {
                    error!("listen error: {:?}", err);
                    continue;
                }
                TcpStream::create(socket, &self.manager_socket_creation_tx, src_addr, dst_addr);
                debug!("created TCP connection for {} <-> {}", src_addr, dst_addr);
            }
            self.drive_interface_state(frame);
        }
    }
    pub fn drive_interface_state(&mut self, frame: StackPacket) {
        if self.iface_tx.send(frame).is_err() {
            panic!("interface send channel closed unexpectedly");
        }

        // Wake up and poll the interface.
        self.iface_tx_avail.store(true, Ordering::Release);
        self.manager_notify.notify_waiters();
    }
}

impl TcpStack {
    pub fn new(
        mtu: u32,
        tcp_rx: Receiver<StackPacket>,
        stack_tx: Sender<StackPacket>,
    ) -> (TcpRunner, TcpListener) {
        let mut capabilities = DeviceCapabilities::default();
        capabilities.medium = Medium::Ip;
        capabilities.max_transmission_unit = mtu as usize;
        capabilities.checksum.ipv4 = Checksum::Tx;
        capabilities.checksum.tcp = Checksum::Tx;
        capabilities.checksum.udp = Checksum::Tx;
        capabilities.checksum.icmpv4 = Checksum::Tx;
        capabilities.checksum.icmpv6 = Checksum::Tx;

        let (mut device, iface_tx, iface_tx_avail) = VirtTunDevice::new(capabilities, stack_tx);

        let mut iface_config = InterfaceConfig::new(HardwareAddress::Ip);
        iface_config.random_seed = rand::random();
        let mut iface = Interface::new(iface_config, &mut device, SmolInstant::now());
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

        let (manager_socket_creation_tx, manager_socket_creation_rx) = mpsc::unbounded_channel();
        let (lisnter_tx, lisnter_rx) = mpsc::channel(*LISNTER_CHANNEL);
        let manager_notify = Arc::new(Notify::new());
        let manager = TcpSocketManager {
            device,
            iface,
            lisnter_tx,
            sockets: HashMap::new(),
            manager_notify: manager_notify.clone(),
            socket_creation_rx: manager_socket_creation_rx,
        };

        let manager_running = Arc::new(AtomicBool::new(true));
        let listener = TcpListener { lisnter_rx };
        let mut stack = Self {
            manager_notify: manager_notify.clone(),
            manager,
            manager_running,
        };
        let mut handle = TcpPacketHandle {
            tcp_rx,
            iface_tx,
            manager_socket_creation_tx,
            iface_tx_avail,
            manager_notify,
        };
        let runner = async move {
            tokio::select! {
                _ = handle.handle_packet() => (),
                _ = stack.run() => (),
            }
        }
        .boxed();
        (runner, listener)
    }

    pub async fn run(&mut self) {
        let TcpSocketManager {
            ref mut device,
            ref mut iface,
            ref mut sockets,
            ref mut manager_notify,
            ref mut socket_creation_rx,
            ref mut lisnter_tx,
            ..
        } = self.manager;

        let mut socket_set = SocketSet::new(vec![]);

        let manager_running = self.manager_running.clone();
        while manager_running.load(Ordering::Relaxed) {
            while let Ok(TcpSocketCreation {
                control,
                socket,
                src_addr,
                dst_addr,
            }) = socket_creation_rx.try_recv()
            {
                let handle = socket_set.add(socket);
                sockets.insert(handle, control.clone());
                let _ = lisnter_tx.send((
                    TcpStream {
                        control: control,
                        manager_notify: manager_notify.clone(),
                    },
                    src_addr,
                    dst_addr,
                ));
            }

            let before_poll = SmolInstant::now();
            if let PollResult::SocketStateChanged = iface.poll(before_poll, device, &mut socket_set)
            {
                trace!(
                    "VirtDevice::poll costed {}",
                    SmolInstant::now() - before_poll
                );
            }

            // Check all the sockets' status
            let mut sockets_to_remove = Vec::new();

            for (socket_handle, control) in sockets.iter() {
                let socket_handle = *socket_handle;
                let socket = socket_set.get_mut::<TcpSocket>(socket_handle);
                let mut control = control.lock();

                // Remove the socket only when it is in the closed state.
                if socket.state() == TcpState::Closed {
                    sockets_to_remove.push(socket_handle);

                    control.send_state = TcpSocketState::Closed;
                    control.recv_state = TcpSocketState::Closed;

                    if let Some(waker) = control.send_waker.take() {
                        waker.wake();
                    }
                    if let Some(waker) = control.recv_waker.take() {
                        waker.wake();
                    }

                    trace!("closed TCP connection");
                    continue;
                }

                // SHUT_WR
                if matches!(control.send_state, TcpSocketState::Close)
                    && socket.send_queue() == 0
                    && control.send_buffer.is_empty()
                {
                    trace!("closing TCP Write Half, {:?}", socket.state());

                    // Close the socket. Set to FIN state
                    socket.close();
                    control.send_state = TcpSocketState::Closing;

                    // We can still process the pending buffer.
                }

                // Check if readable
                let mut wake_receiver = false;
                while socket.can_recv() && !control.recv_buffer.is_full() {
                    let result = socket.recv(|buffer| {
                        let n = control.recv_buffer.enqueue_slice(buffer);
                        (n, ())
                    });

                    match result {
                        Ok(..) => {
                            wake_receiver = true;
                        }
                        Err(err) => {
                            error!("socket recv error: {:?}, {:?}", err, socket.state());

                            // Don't know why. Abort the connection.
                            socket.abort();

                            if matches!(control.recv_state, TcpSocketState::Normal) {
                                control.recv_state = TcpSocketState::Closed;
                            }
                            wake_receiver = true;

                            // The socket will be recycled in the next poll.
                            break;
                        }
                    }
                }

                // If socket is not in ESTABLISH, FIN-WAIT-1, FIN-WAIT-2,
                // the local client have closed our receiver.
                if matches!(control.recv_state, TcpSocketState::Normal)
                    && !socket.may_recv()
                    && !matches!(
                        socket.state(),
                        TcpState::Listen
                            | TcpState::SynReceived
                            | TcpState::Established
                            | TcpState::FinWait1
                            | TcpState::FinWait2
                    )
                {
                    trace!("closed TCP Read Half, {:?}", socket.state());

                    // Let TcpConnection::poll_read returns EOF.
                    control.recv_state = TcpSocketState::Closed;
                    wake_receiver = true;
                }

                if wake_receiver && control.recv_waker.is_some() {
                    if let Some(waker) = control.recv_waker.take() {
                        waker.wake();
                    }
                }

                // Check if writable
                let mut wake_sender = false;
                while socket.can_send() && !control.send_buffer.is_empty() {
                    let result = socket.send(|buffer| {
                        let n = control.send_buffer.dequeue_slice(buffer);
                        (n, ())
                    });

                    match result {
                        Ok(..) => {
                            wake_sender = true;
                        }
                        Err(err) => {
                            error!("socket send error: {:?}, {:?}", err, socket.state());

                            // Don't know why. Abort the connection.
                            socket.abort();

                            if matches!(control.send_state, TcpSocketState::Normal) {
                                control.send_state = TcpSocketState::Closed;
                            }
                            wake_sender = true;

                            // The socket will be recycled in the next poll.
                            break;
                        }
                    }
                }

                if wake_sender && control.send_waker.is_some() {
                    if let Some(waker) = control.send_waker.take() {
                        waker.wake();
                    }
                }
            }

            for socket_handle in sockets_to_remove {
                sockets.remove(&socket_handle);
                socket_set.remove(socket_handle);
            }

            if !device.recv_available() {
                let next_duration = iface
                    .poll_delay(before_poll, &socket_set)
                    .unwrap_or(SmolDuration::from_millis(5));
                if next_duration != SmolDuration::ZERO {
                    let _ = timeout(next_duration.into(), manager_notify.notified()).await;
                }
            }
        }

        trace!("VirtDevice::poll thread exited");
    }
}
