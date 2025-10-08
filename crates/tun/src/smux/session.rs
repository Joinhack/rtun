use bytes::{BufMut, Bytes};
use futures::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, StreamExt,
    channel::mpsc::{Receiver, Sender},
    io::{ReadHalf, WriteHalf},
    lock::Mutex,
};
use log::error;
use std::{
    collections::HashMap,
    io,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};
use tokio::net::UdpSocket;

use crate::smux::{
    CMD_FIN, CMD_NOP, CMD_PSH, CMD_SYN, CMD_UDP, FrameHdr, HEAD_SIZE, UPD_HDR_SIZE, stream::Stream,
};

struct UdpStream {
    socket: UdpSocket,
}

impl AsyncRead for UdpStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let mut buf = tokio::io::ReadBuf::new(buf);
        match self.socket.poll_recv(cx, &mut buf) {
            Poll::Ready(_) => {
                let n = buf.filled().len();
                Poll::Ready(Ok(n))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for UdpStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut &self.socket).poll_send(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// A request to send data on a specific stream.
pub struct SmuxRequest {
    pub stream_id: u32,

    pub cmd: u8,

    pub data: Bytes,
}

pub struct Session<C> {
    conn: Option<C>,

    sessions: Arc<Mutex<HashMap<u32, Stream>>>,

    max_frame_size: usize,

    version: u8,

    next_stream_id: u32,

    write_chan_recv: Option<Receiver<SmuxRequest>>,

    write_chan_sender: Sender<SmuxRequest>,
}

impl<C> Session<C> {
    pub fn new(conn: C, max_frame_size: usize, version: u8) -> Self {
        let (tx, rx) = futures::channel::mpsc::channel(1024);
        Self {
            conn: Some(conn),
            sessions: Default::default(),
            max_frame_size,
            version,
            next_stream_id: 1,
            write_chan_recv: Some(rx),
            write_chan_sender: tx,
        }
    }

    pub fn open_stream(&mut self) -> Option<Stream> {
        let stream_id = self.next_stream_id;
        self.next_stream_id += 2;
        let stream = Stream::new(
            self.write_chan_sender.clone(),
            self.max_frame_size,
            stream_id,
        );
        let req = SmuxRequest {
            stream_id,
            cmd: CMD_SYN,
            data: Bytes::new(),
        };
        if let Err(e) = self.write_chan_sender.try_send(req) {
            error!("open stream send syn error, {e}");
            return None;
        }
        Some(stream)
    }
}

impl Session<UdpStream> {
    /// Start the session's read and write loops.
    pub fn start(&mut self) {
        if let Some(conn) = self.conn.take() {
            let (reader, writer) = conn.split();
            let recv_loop_fut = self.recv_loop(reader);
            let send_loop_fut = self.send_loop(writer);
            tokio::spawn(recv_loop_fut);
            tokio::spawn(send_loop_fut);
        }
    }
}

impl<C: AsyncRead + Unpin> Session<C> {
    /// Start the receive loop for the session.
    fn recv_loop(&mut self, mut reader: ReadHalf<C>) -> impl Future<Output = ()> + use<C> {
        let sessions = self.sessions.clone();
        let version = self.version;
        let max_frame_size = self.max_frame_size;
        let mut buf = vec![0u8; max_frame_size];
        let write_chan_sender = self.write_chan_sender.clone();
        return async move {
            loop {
                match reader.read_exact(&mut buf[..HEAD_SIZE]).await {
                    Ok(n) => n,
                    Err(e) => {
                        error!("session connect read error, {e}");
                        return;
                    }
                };
                let mut frame_hdr = FrameHdr::new();
                if let Err(e) = frame_hdr.unmarshal(&mut buf) {
                    error!("unmarshal error, {e}");
                    return;
                };
                if frame_hdr.version != version {
                    error!("the version not match.");
                    return;
                }

                match frame_hdr.cmd {
                    CMD_NOP => (),
                    CMD_SYN => {
                        let mut guard = sessions.lock().await;
                        if !guard.contains_key(&frame_hdr.sid) {
                            let stream = Stream::new(
                                write_chan_sender.clone(),
                                max_frame_size,
                                frame_hdr.sid,
                            );
                            guard.insert(frame_hdr.sid, stream);
                        }
                    }
                    CMD_PSH => {
                        match reader
                            .read_exact(&mut buf[HEAD_SIZE..HEAD_SIZE + frame_hdr.len as usize])
                            .await
                        {
                            Ok(_) => (),
                            Err(e) => {
                                error!("read data error, {e}");
                                return;
                            }
                        };
                        let mut guard = sessions.lock().await;
                        if let Some(stream) = guard.get_mut(&frame_hdr.sid) {
                            let src_slice = &buf[..HEAD_SIZE + frame_hdr.len as usize];
                            stream.push_data(Bytes::copy_from_slice(src_slice));
                        }
                    }
                    CMD_UDP => {
                        match reader.read_exact(&mut buf[..UPD_HDR_SIZE]).await {
                            Ok(_) => (),
                            Err(e) => {
                                error!("read update error, {e}");
                                return;
                            }
                        };
                    }
                    CMD_FIN => {
                        let mut guard = sessions.lock().await;
                        if let Some(stream) = guard.get_mut(&frame_hdr.sid) {
                            stream.fin();
                        }
                    }
                    _ => {
                        error!("invaild version.");
                        return;
                    }
                };
            }
        };
    }
}

impl<C: AsyncWrite + Unpin> Session<C> {
    /// Start the send loop for the session.
    fn send_loop(&mut self, mut writer: WriteHalf<C>) -> impl Future<Output = ()> + use<C> {
        let mut write_chan_recv = self.write_chan_recv.take().unwrap();
        let version = self.version;
        return async move {
            let mut buf = vec![0u8; HEAD_SIZE + 1 << 16];
            while let Some(req) = write_chan_recv.next().await {
                let frame_hdr = FrameHdr {
                    version: version,
                    cmd: req.cmd,
                    sid: req.stream_id,
                    len: req.data.len() as u16,
                };
                frame_hdr.marshal(&mut buf);
                if req.data.len() > 0 {
                    buf.put(req.data);
                }
                if let Err(e) = writer.write_all(&buf).await {
                    error!("session write error, {e}");
                    return;
                }
            }
        };
    }
}
