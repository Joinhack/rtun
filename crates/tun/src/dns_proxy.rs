use anyhow::{Result, anyhow};

use futures::{FutureExt, task::AtomicWaker};
use log::{error, trace};
use netstack_lwip::udp::SendHalf;
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU16, Ordering},
    },
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{
        UdpSocket,
        tcp::{OwnedReadHalf, OwnedWriteHalf},
    },
    sync::{
        Mutex, OwnedMutexGuard,
        mpsc::{self, Receiver, UnboundedReceiver, UnboundedSender, error::SendError},
    },
    time::{sleep, timeout},
};
use trust_dns_proto::op::Message;

use crate::{
    net::{create_outbound_tcp_socket, create_outbound_udp_socket},
    option::{
        DNS_PROXY_LISTEN_PORT, DNS_SESSION_TIMEOUT, DNS_UPSTREAM_PROTO, DnsUpstreamProto,
        MAX_UDP_UPSTREAM, OUTBOUND_CONNECT_TIMEOUT,
    },
};

struct UdpPeer {
    src: SocketAddr,
    dest: SocketAddr,
}

struct DnsSession {
    peer: UdpPeer,
    old_id: u16,
    id: u16,
    time: Instant,
}

type LockType = Pin<Box<dyn Future<Output = OwnedMutexGuard<HashMap<u16, DnsSession>>> + Send>>;

enum SendState {
    Begin,
    Send(Vec<u8>),
    SessionsLock(LockType),
}

enum UpstreamSend {
    Udp(Arc<UdpSocket>),
    Tcp(OwnedWriteHalf),
}

enum UpstreamRecv {
    Udp(Arc<UdpSocket>),
    Tcp(OwnedReadHalf),
}

fn frame_dns_tcp(payload: Vec<u8>) -> Result<Vec<u8>> {
    let len = payload.len();
    if len > u16::MAX as usize {
        return Err(anyhow!("dns message too large for tcp framing: {len}"));
    }
    let mut framed = vec![(len >> 8) as u8, (len & 0xff) as u8];
    framed.extend(payload);
    Ok(framed)
}

/// The Send Future receive the dns message from tunnel thought `rx`,
/// after reset the message id as session id, send the packet to distination address
struct SendFut {
    state: SendState,
    session: Option<DnsSession>,
    id: Arc<AtomicU16>,
    // udp_socket: Arc<UdpSocket>,
    upstream_send: UpstreamSend,
    sendn: usize,
    rx: UnboundedReceiver<(UdpPeer, Message)>,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
}

impl SendFut {
    fn new(
        id: Arc<AtomicU16>,
        upstream_send: UpstreamSend,
        rx: UnboundedReceiver<(UdpPeer, Message)>,
        sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
    ) -> Self {
        Self {
            id,
            sendn: 0,
            state: SendState::Begin,
            upstream_send,
            sessions,
            rx,
            session: None,
        }
    }
}

impl Future for SendFut {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let SendFut {
            id,
            upstream_send,
            sendn,
            rx,
            sessions,
            session,
            state,
        } = &mut *self;
        loop {
            match state {
                SendState::Begin => {
                    // read from channel util the channel closed.
                    let d = match ready!(rx.poll_recv(cx)) {
                        Some(v) => v,
                        None => {
                            return Poll::Ready(Err(anyhow!("channel is shutdwon.")));
                        }
                    };
                    // unmarshal the data if fail continue to process next packet.
                    let mut messgae = d.1;
                    let id = id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let old_id = messgae.id();
                    messgae.set_id(id);
                    *session = Some(DnsSession {
                        peer: d.0,
                        time: Instant::now(),
                        old_id,
                        id,
                    });
                    let payload = match messgae.to_vec() {
                        Ok(m) => m,
                        Err(e) => {
                            error!("message to dns packet error, {}", e);
                            continue;
                        }
                    };
                    let v = match upstream_send {
                        UpstreamSend::Tcp(_) => match frame_dns_tcp(payload) {
                            Ok(m) => m,
                            Err(e) => {
                                error!("{e}");
                                continue;
                            }
                        },
                        UpstreamSend::Udp(_) => payload,
                    };
                    *state = SendState::Send(v);
                }
                // Sends the DNS message after replacing its message ID with the maintained session ID.
                SendState::Send(msg) => {
                    let sess = session.as_ref().unwrap();
                    let rs = match upstream_send {
                        UpstreamSend::Udp(udp_socket) => {
                            udp_socket.poll_send_to(cx, &msg[*sendn..msg.len()], sess.peer.dest)
                        }
                        UpstreamSend::Tcp(owned_write_half) => {
                            Pin::new(owned_write_half).poll_write(cx, &msg[*sendn..msg.len()])
                        }
                    };
                    match rs {
                        Poll::Ready(Ok(n)) => {
                            *sendn += n;
                            if *sendn < msg.len() {
                                return Poll::Pending;
                            } else {
                                let guard = Box::pin(sessions.clone().lock_owned());
                                *state = SendState::SessionsLock(guard);
                            }
                        }
                        Poll::Ready(Err(e)) => {
                            if e.kind() == io::ErrorKind::WouldBlock {
                                return Poll::Pending;
                            }
                            return Poll::Ready(Err(anyhow!("recv error, {}", e)));
                        }
                        Poll::Pending => return Poll::Pending,
                    }
                }
                SendState::SessionsLock(guard) => {
                    let mut sessions = ready!(guard.poll_unpin(cx));
                    let session = session.take().unwrap();
                    *sendn = 0;
                    trace!("dns request id {} sessions {}", session.id, sessions.len());
                    sessions.insert(session.id, session);
                    *state = SendState::Begin;
                }
            }
        }
    }
}

enum RecvState {
    Begin,
    TcpHeader { got: usize },
    TcpBody { len: usize, got: usize },
    SessionsLock(LockType),
}

/// The Receive Future is receive from udp socket `udp_socket`, find the session by dns message id
/// the send the dns message packet back to tunnel thought `tun_udp_sender`
struct RecvFut {
    state: RecvState,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
    running: Arc<AtomicBool>,
    // udp_socket: Arc<UdpSocket>,
    upstream_recv: UpstreamRecv,
    tun_udp_sender: Arc<SendHalf>,
    msg: Option<Message>,
    waker: Arc<AtomicWaker>,
    buf: Vec<u8>,
}

impl RecvFut {
    fn new(
        upstream_recv: UpstreamRecv,
        tun_udp_sender: Arc<SendHalf>,
        sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
        running: Arc<AtomicBool>,
        waker: Arc<AtomicWaker>,
    ) -> Self {
        Self {
            state: RecvState::Begin,
            sessions,
            upstream_recv,
            tun_udp_sender,
            msg: None,
            buf: vec![0u8; 1500],
            running,
            waker,
        }
    }
}

impl Future for RecvFut {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let RecvFut {
            sessions,
            upstream_recv,
            running,
            state,
            tun_udp_sender,
            buf,
            msg,
            waker,
        } = &mut *self;
        loop {
            if !running.load(Ordering::Acquire) {
                error!("recv future canceled.");
                return Poll::Ready(Err(anyhow!("recv future canceled.")));
            }
            waker.register(cx.waker());
            match state {
                RecvState::Begin => match upstream_recv {
                    UpstreamRecv::Udp(udp_socket) => {
                        let mut read_buf = ReadBuf::new(buf);
                        match ready!(udp_socket.poll_recv(cx, &mut read_buf)) {
                            Ok(_) => (),
                            Err(e) => {
                                if e.kind() == io::ErrorKind::WouldBlock {
                                    return Poll::Pending;
                                }
                                return Poll::Ready(Err(anyhow!("recv error, {e}")));
                            }
                        }
                        let n = read_buf.filled().len();
                        let m = match Message::from_vec(&buf[0..n]) {
                            Ok(m) => m,
                            Err(err) => {
                                error!("packet to message error, {}", err);
                                continue;
                            }
                        };
                        *msg = Some(m);
                        let guard = Box::pin(sessions.clone().lock_owned());
                        *state = RecvState::SessionsLock(guard);
                    }
                    UpstreamRecv::Tcp(_) => {
                        *state = RecvState::TcpHeader { got: 0 };
                    }
                },
                RecvState::TcpHeader { got } => {
                    let UpstreamRecv::Tcp(read) = upstream_recv else {
                        unreachable!("tcp recv state without tcp upstream");
                    };
                    let mut read_buf = ReadBuf::new(&mut buf[*got..2]);
                    match ready!(Pin::new(read).poll_read(cx, &mut read_buf)) {
                        Ok(()) if read_buf.filled().is_empty() => {
                            return Poll::Ready(Err(anyhow!("tcp upstream closed")));
                        }
                        Ok(()) => (),
                        Err(e) => {
                            if e.kind() == io::ErrorKind::WouldBlock {
                                return Poll::Pending;
                            }
                            return Poll::Ready(Err(anyhow!("recv error, {e}")));
                        }
                    }
                    *got += read_buf.filled().len();
                    if *got < 2 {
                        return Poll::Pending;
                    }
                    let len = u16::from_be_bytes([buf[0], buf[1]]) as usize;
                    if len == 0 {
                        error!("invalid dns tcp length 0");
                        *state = RecvState::Begin;
                        continue;
                    }
                    if buf.len() < len {
                        buf.resize(len, 0);
                    }
                    *state = RecvState::TcpBody { len, got: 0 };
                }
                RecvState::TcpBody { len, got } => {
                    let UpstreamRecv::Tcp(read) = upstream_recv else {
                        unreachable!("tcp recv state without tcp upstream");
                    };
                    let mut read_buf = ReadBuf::new(&mut buf[*got..*len]);
                    match ready!(Pin::new(read).poll_read(cx, &mut read_buf)) {
                        Ok(()) if read_buf.filled().is_empty() => {
                            return Poll::Ready(Err(anyhow!("tcp upstream closed")));
                        }
                        Ok(()) => (),
                        Err(e) => {
                            if e.kind() == io::ErrorKind::WouldBlock {
                                return Poll::Pending;
                            }
                            return Poll::Ready(Err(anyhow!("recv error, {e}")));
                        }
                    }
                    *got += read_buf.filled().len();
                    if *got < *len {
                        return Poll::Pending;
                    }
                    let m = match Message::from_vec(&buf[0..*len]) {
                        Ok(m) => m,
                        Err(err) => {
                            error!("packet to message error, {}", err);
                            *state = RecvState::Begin;
                            continue;
                        }
                    };
                    *msg = Some(m);
                    let guard = Box::pin(sessions.clone().lock_owned());
                    *state = RecvState::SessionsLock(guard);
                }
                RecvState::SessionsLock(lock) => match lock.poll_unpin(cx) {
                    Poll::Ready(mut sessions) => {
                        let mut msg = msg.take().unwrap();
                        let msg_id = msg.id();
                        if let Some(session) = sessions.remove(&msg_id) {
                            msg.set_id(session.old_id);
                            match msg.to_vec() {
                                Ok(data) => {
                                    tun_udp_sender
                                        .send_to(&data, &session.peer.dest, &session.peer.src)
                                        .unwrap();
                                }
                                Err(e) => {
                                    error!("message to bytes error {e}");
                                }
                            };
                            trace!("dns response id {} sessions {}", msg_id, sessions.len());
                        }
                        *state = RecvState::Begin;
                    }
                    Poll::Pending => return Poll::Pending,
                },
            }
        }
    }
}

#[derive(Clone)]
struct UpstreamSender {
    inner: UnboundedSender<(UdpPeer, Message)>,
    running: Arc<AtomicBool>,
    waker: Arc<AtomicWaker>,
}

impl UpstreamSender {
    fn send(&mut self, msg: (UdpPeer, Message)) -> Result<(), SendError<(UdpPeer, Message)>> {
        self.inner.send(msg)
    }
}

impl Drop for UpstreamSender {
    fn drop(&mut self) {
        self.running.store(false, Ordering::Release);
        self.waker.wake();
    }
}

pub struct DnsProxy {
    upstream: Arc<Mutex<Vec<Option<UpstreamSender>>>>,
    id: Arc<AtomicU16>,
    session_clear: Option<Pin<Box<dyn Future<Output = ()> + Send + 'static>>>,
    idx: usize,
    max_upstream: u8,
    dns_listen_port: u16,
    tun_udp_sender: Arc<SendHalf>,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
}

impl DnsProxy {
    pub fn new(tun_udp_sender: Arc<SendHalf>, dns_clear_rx: Receiver<()>) -> Self {
        let sessions: Arc<Mutex<HashMap<u16, DnsSession>>> = Default::default();
        let sessions1 = sessions.clone();

        let session_clear = Some(
            async move {
                let sessions = sessions1;
                loop {
                    sleep(Duration::from_secs(10)).await;
                    let mut guard = sessions.lock().await;
                    let mut removed = Vec::new();
                    let now = Instant::now();
                    for (id, session) in guard.iter() {
                        if now.duration_since(session.time).as_secs() > *DNS_SESSION_TIMEOUT {
                            removed.push(*id);
                        }
                    }
                    for id in removed {
                        guard.remove(&id);
                    }
                }
            }
            .boxed(),
        );
        let max_upstream = *MAX_UDP_UPSTREAM;
        let upstream = Arc::new(Mutex::new(vec![None; max_upstream as usize]));
        let upstream1 = upstream.clone();
        tokio::spawn(async move {
            let mut dns_clear_rx = dns_clear_rx;
            loop {
                if let Some(_) = dns_clear_rx.recv().await {
                    let mut upstream = upstream1.lock().await;
                    // release listener.
                    for item in upstream.iter_mut() {
                        *item = None;
                    }
                } else {
                    return;
                }
            }
        });
        Self {
            upstream,
            id: Arc::new(AtomicU16::new(1)),
            idx: 0,
            session_clear,
            max_upstream,
            tun_udp_sender,
            sessions,
            dns_listen_port: *DNS_PROXY_LISTEN_PORT,
        }
    }

    async fn start_upstream(
        slot: &mut Option<UpstreamSender>,
        dns_listen_port: &mut u16,
        id: Arc<AtomicU16>,
        sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
        tun_udp_sender: Arc<SendHalf>,
        src: SocketAddr,
        dest: SocketAddr,
        message: Message,
        proto: DnsUpstreamProto,
    ) -> Result<()> {
        let (tx, rx) = mpsc::unbounded_channel();
        let (upstream_send, upstream_recv) = match proto {
            DnsUpstreamProto::Udp => {
                let listener_port = *dns_listen_port;
                let socket_addr: SocketAddr = format!("0.0.0.0:{listener_port}")
                    .parse()
                    .map_err(|e| anyhow!("{e}"))?;
                let udp_socket =
                    Arc::new(create_outbound_udp_socket(&dest, Some(socket_addr)).unwrap());
                *dns_listen_port += 1;
                (
                    UpstreamSend::Udp(udp_socket.clone()),
                    UpstreamRecv::Udp(udp_socket),
                )
            }
            DnsUpstreamProto::Tcp => {
                let tcp_sock = create_outbound_tcp_socket(&dest)
                    .map_err(|e| anyhow!("create tcp socket error, {e}"))?;
                let stream = timeout(
                    Duration::from_secs(*OUTBOUND_CONNECT_TIMEOUT),
                    tcp_sock.connect(dest),
                )
                .await
                .map_err(|_| anyhow!("tcp connect timeout"))?
                .map_err(|e| anyhow!("tcp connect error, {e}"))?;
                let (read, write) = stream.into_split();
                (UpstreamSend::Tcp(write), UpstreamRecv::Tcp(read))
            }
        };

        tx.send((UdpPeer { src, dest }, message))?;
        let recv_fut_running = Arc::new(AtomicBool::new(true));
        let waker = Arc::new(AtomicWaker::new());
        slot.replace(UpstreamSender {
            inner: tx,
            running: recv_fut_running.clone(),
            waker: waker.clone(),
        });
        tokio::spawn(SendFut::new(id, upstream_send, rx, sessions.clone()));
        tokio::spawn(RecvFut::new(
            upstream_recv,
            tun_udp_sender,
            sessions,
            recv_fut_running,
            waker,
        ));
        Ok(())
    }

    pub async fn send(&mut self, src: SocketAddr, dest: SocketAddr, msg: Message) -> Result<()> {
        let idx = self.idx % self.max_upstream as usize;
        self.idx += 1;
        if let Some(sessions_clear) = self.session_clear.take() {
            tokio::spawn(sessions_clear);
        }

        let id = self.id.clone();
        let sessions = self.sessions.clone();
        let tun_udp_sender = self.tun_udp_sender.clone();
        let mut dns_listen_port = self.dns_listen_port;

        let mut upstream = self.upstream.lock().await;
        let proto = *DNS_UPSTREAM_PROTO;
        match upstream.get_mut(idx) {
            Some(s) => {
                if s.is_none() {
                    Self::start_upstream(
                        s,
                        &mut dns_listen_port,
                        id,
                        sessions,
                        tun_udp_sender,
                        src,
                        dest,
                        msg,
                        proto,
                    )
                    .await?;
                } else if let Some(Err(e)) =
                    s.as_mut().map(|s| s.send((UdpPeer { src, dest }, msg)))
                {
                    Self::start_upstream(
                        s,
                        &mut dns_listen_port,
                        id,
                        sessions,
                        tun_udp_sender,
                        src,
                        dest,
                        e.0.1,
                        proto,
                    )
                    .await?;
                }
            }
            None => unreachable!("can't reach there."),
        };
        self.dns_listen_port = dns_listen_port;
        Ok(())
    }
}
