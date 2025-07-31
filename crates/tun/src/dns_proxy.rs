use anyhow::{Result, anyhow};

use futures::FutureExt;
use log::{error, trace};
use netstack_lwip::udp::SendHalf;
use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, atomic::AtomicU16},
    task::{Context, Poll, ready},
    time::{Duration, Instant},
};
use tokio::{
    io::ReadBuf,
    net::UdpSocket,
    sync::{
        Mutex, OwnedMutexGuard,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
    },
    time::sleep,
};
use trust_dns_proto::op::Message;

use crate::{net::create_outbound_udp_socket, option::DNS_SESSION_TIMEOUT};

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

struct SendFut {
    state: SendState,
    session: Option<DnsSession>,
    id: Arc<AtomicU16>,
    udp_socket: Arc<UdpSocket>,
    sendn: usize,
    rx: UnboundedReceiver<(UdpPeer, Message)>,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
}

impl SendFut {
    fn new(
        id: Arc<AtomicU16>,
        udp_socket: Arc<UdpSocket>,
        rx: UnboundedReceiver<(UdpPeer, Message)>,
        sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
    ) -> Self {
        Self {
            id,
            sendn: 0,
            state: SendState::Begin,
            udp_socket,
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
            udp_socket,
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
                        None => return Poll::Ready(Err(anyhow!("channel is shutdwon."))),
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
                    let v = match messgae.to_vec() {
                        Ok(m) => m,
                        Err(e) => {
                            error!("message to dns packet error, {}", e);
                            continue;
                        }
                    };
                    *state = SendState::Send(v);
                }
                // Sends the DNS message after replacing its message ID with the maintained session ID.
                //
                SendState::Send(msg) => {
                    let sess = session.as_ref().unwrap();
                    match udp_socket.poll_send_to(cx, &msg[*sendn..msg.len()], sess.peer.dest) {
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
    SessionsLock(LockType),
}

struct RecvFut {
    state: RecvState,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
    udp_socket: Arc<UdpSocket>,
    tun_udp_sender: Arc<SendHalf>,
    msg: Option<Message>,
    buf: Vec<u8>,
}

impl RecvFut {
    fn new(
        udp_socket: Arc<UdpSocket>,
        tun_udp_sender: Arc<SendHalf>,
        sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
    ) -> Self {
        Self {
            state: RecvState::Begin,
            sessions,
            udp_socket,
            tun_udp_sender,
            msg: None,
            buf: vec![0u8; 1500],
        }
    }
}

impl Future for RecvFut {
    type Output = Result<()>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let RecvFut {
            sessions,
            udp_socket,
            state,
            tun_udp_sender,
            buf,
            msg,
        } = &mut *self;
        loop {
            match state {
                RecvState::Begin => {
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
                    let n = read_buf.capacity() - read_buf.remaining();
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

pub struct DnsProxy {
    upstream: Vec<UnboundedSender<(UdpPeer, Message)>>,
    id: Arc<AtomicU16>,
    session_clear: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    idx: usize,
    max_upstream: u8,
    tun_udp_sender: Arc<SendHalf>,
    sessions: Arc<Mutex<HashMap<u16, DnsSession>>>,
}

impl DnsProxy {
    pub fn new(tun_udp_sender: Arc<SendHalf>) -> Self {
        let sessions: Arc<Mutex<HashMap<u16, DnsSession>>> = Default::default();
        let sessions_clone = sessions.clone();
        let session_clear = Some(
            async move {
                let sessions = sessions_clone;
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
        Self {
            upstream: Vec::new(),
            id: Arc::new(AtomicU16::new(1)),
            idx: 0,
            session_clear,
            max_upstream: 8,
            tun_udp_sender,
            sessions,
        }
    }

    pub async fn send(&mut self, src: SocketAddr, dest: SocketAddr, msg: Message) -> Result<()> {
        let idx = self.idx % self.max_upstream as usize;
        self.idx += 1;
        if let Some(sessions_clear) = self.session_clear.take() {
            tokio::spawn(sessions_clear);
        }

        match self.upstream.get(idx) {
            Some(s) => s.send((UdpPeer { src, dest }, msg))?,
            None => {
                let (tx, rx) = mpsc::unbounded_channel();
                let udp_socket = Arc::new(create_outbound_udp_socket(&dest).unwrap());
                tx.send((UdpPeer { src, dest }, msg))?;
                self.upstream.push(tx);
                tokio::spawn(SendFut::new(
                    self.id.clone(),
                    udp_socket.clone(),
                    rx,
                    self.sessions.clone(),
                ));
                tokio::spawn(RecvFut::new(
                    udp_socket,
                    self.tun_udp_sender.clone(),
                    self.sessions.clone(),
                ));
            }
        };
        Ok(())
    }
}
