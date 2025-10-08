use bytes::Bytes;
use futures::channel::mpsc::Sender;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};
use std::{collections::VecDeque, io, pin::Pin, sync::Mutex};
use tokio::io::{AsyncRead, AsyncWrite};

use crate::smux::session::SmuxRequest;

pub struct Stream {
    session_send_ch: Sender<SmuxRequest>,
    frame_size: usize,
    buffer: Mutex<VecDeque<Bytes>>,
    closed: AtomicBool,
    id: u32,
}

impl Stream {
    pub fn new(session_send_ch: Sender<SmuxRequest>, frame_size: usize, id: u32) -> Self {
        Self {
            id,
            frame_size,
            session_send_ch,
            closed: AtomicBool::new(false),
            buffer: Default::default(),
        }
    }

    /// Push data into the stream's buffer, when data is received.
    pub fn push_data(&mut self, data: Bytes) {
        let mut buf = self.buffer.lock().unwrap();
        buf.push_back(data);
    }

    pub fn fin(&mut self) {
        self.closed.store(true, Ordering::Release);
    }

    fn try_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut buffers = self.buffer.lock().unwrap();
        if buffers.len() == 0 {
            return Ok(0);
        }
        // Determine how many bytes we can read, which is
        // the smaller of the buffer size and the first chunk size
        let len = if buf.len() < buffers[0].len() {
            buf.len()
        } else {
            buffers[0].len()
        };
        buf[..len].copy_from_slice(&buffers[0][..len]);
        // Remove the bytes we've read from the front buffer
        if len == buffers[0].len() {
            buffers.pop_front();
        } else {
            let _ = buffers[0].split_to(len);
        }
        Ok(len)
    }

    fn try_send(&mut self, buf: &[u8]) -> io::Result<usize> {
        let len = if buf.len() > self.frame_size {
            self.frame_size
        } else {
            buf.len()
        };
        let req = SmuxRequest {
            stream_id: self.id,
            cmd: crate::smux::CMD_PSH,
            data: Bytes::copy_from_slice(&buf[..len]),
        };
        match self.session_send_ch.try_send(req) {
            Ok(_) => Ok(len),
            Err(e) => Err(io::Error::other(format!("{e}"))),
        }
    }
}

impl AsyncRead for Stream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // If the stream is closed, return Ready with Ok
        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Ok(()));
        }
        // Safety: This is safe because we are not moving the buffer, just
        // getting a mutable slice to its unfilled part.
        let mut unfilled = unsafe {
            let unfilled = buf.unfilled_mut();
            std::slice::from_raw_parts_mut(unfilled.as_mut_ptr() as *mut u8, unfilled.len())
        };

        let rs = match self.try_read(&mut unfilled) {
            Ok(n) => {
                // If no data was read, indicate that we're not ready
                if n == 0 {
                    return Poll::Pending;
                }
                buf.advance(n);
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        };
        rs
    }
}

impl AsyncWrite for Stream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        // If the stream is closed, return Ready with an error
        if self.closed.load(Ordering::Acquire) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "stream is closed",
            )));
        }
        let rs = match self.try_send(buf) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) => Poll::Ready(Err(e)),
        };
        rs
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // For simplicity, we assume flush is always successful
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Mark the stream as closed
        self.closed.store(true, Ordering::Release);
        Poll::Ready(Ok(()))
    }
}
