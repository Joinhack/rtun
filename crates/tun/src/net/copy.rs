use std::{
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use tokio::io::{self, AsyncRead, AsyncWrite, ReadBuf};

struct CopyBuf {
    read_done: bool,
    need_flush: bool,
    pos: usize,
    cap: usize,
    amt: u64,
    buf: Box<[u8]>,
}

impl CopyBuf {
    fn new(size: usize) -> Self {
        Self {
            read_done: false,
            need_flush: false,
            pos: 0,
            cap: 0,
            amt: 0,
            buf: vec![0; size].into_boxed_slice(),
        }
    }

    fn poll_copy<R, W>(
        &mut self,
        cx: &mut Context<'_>,
        mut reader: Pin<&mut R>,
        mut writer: Pin<&mut W>,
    ) -> Poll<io::Result<u64>>
    where
        R: AsyncRead + Unpin + ?Sized,
        W: AsyncWrite + Unpin + ?Sized,
    {
        loop {
            if self.pos == self.cap && !self.read_done {
                let me = &mut *self;
                let mut rbuf = ReadBuf::new(&mut me.buf);
                match reader.as_mut().poll_read(cx, &mut rbuf) {
                    Poll::Ready(Ok(_)) => (),
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        if self.need_flush {
                            ready!(writer.as_mut().poll_flush(cx))?;
                            self.need_flush = false;
                        }
                        return Poll::Pending;
                    }
                };
                // get the filled length.
                let n = rbuf.filled().len();
                if n == 0 {
                    // read eof.
                    self.read_done = true;
                } else {
                    // reset the pos
                    self.pos = 0;

                    self.cap = n;
                }
            }
            while self.pos < self.cap {
                let n = ready!(
                    writer
                        .as_mut()
                        .poll_write(cx, &self.buf[self.pos..self.cap])
                )?;
                if n == 0 {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "write error to writer.",
                    )));
                } else {
                    self.pos += n;
                    self.amt += n as u64;
                    self.need_flush = true;
                }
            }
            // If pos larger than cap, this loop will never stop.
            // In particular, user's wrong poll_write implementation returning
            // incorrect written length may lead to thread blocking.
            debug_assert!(
                self.pos <= self.cap,
                "writer returned length larger than input slice"
            );
            // If we've written all the data and we've seen EOF, flush out the
            // data and finish the transfer.
            if self.pos == self.cap && self.read_done {
                ready!(writer.as_mut().poll_flush(cx))?;
                return Poll::Ready(Ok(self.amt));
            }
        }
    }
}

enum TranferState {
    Running(CopyBuf),
    Shutdown(u64),
    Done,
}

struct CopyBidirectional<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_to_b_amt: u64,
    b_to_a_amt: u64,
    a_to_b_state: TranferState,
    b_to_a_state: TranferState,
    a_to_b_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    b_to_a_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    a_to_b_timeout: Duration,
    b_to_a_timeout: Duration,
}

impl<'a, A, B> Future for CopyBidirectional<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    type Output = io::Result<(u64, u64)>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let CopyBidirectional {
            a,
            b,
            a_to_b_amt,
            b_to_a_amt,
            a_to_b_state,
            b_to_a_state,
            a_to_b_sleep,
            b_to_a_sleep,
            a_to_b_timeout,
            b_to_a_timeout,
        } = &mut *self;
        let mut a = Pin::new(a);
        let mut b = Pin::new(b);
        loop {
            match a_to_b_state {
                TranferState::Running(buf) => match buf.poll_copy(cx, a.as_mut(), b.as_mut()) {
                    Poll::Ready(Ok(n)) => {
                        *a_to_b_state = TranferState::Shutdown(n);
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        if let Some(sleep) = a_to_b_sleep {
                            match sleep.as_mut().poll(cx) {
                                Poll::Ready(_) => *a_to_b_state = TranferState::Shutdown(buf.amt),
                                Poll::Pending => (),
                            }
                        }
                    }
                },
                // if the a_to_b is shutdown, it mean that peer a read eof, so close b.
                TranferState::Shutdown(n) => match b.as_mut().poll_shutdown(cx) {
                    Poll::Ready(_) => {
                        *a_to_b_amt += *n;
                        *a_to_b_state = TranferState::Done;
                        //The connection of a is closed. So when reading the pending data from b, it should start to sleep until timeout..
                        b_to_a_sleep.replace(Box::pin(tokio::time::sleep(*a_to_b_timeout)));
                        continue;
                    }
                    Poll::Pending => (),
                },
                TranferState::Done => (),
            };
            match b_to_a_state {
                TranferState::Running(buf) => match buf.poll_copy(cx, b.as_mut(), a.as_mut()) {
                    Poll::Ready(Ok(n)) => {
                        *b_to_a_state = TranferState::Shutdown(n);
                        continue;
                    }
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => {
                        if let Some(sleep) = b_to_a_sleep {
                            match sleep.as_mut().poll(cx) {
                                Poll::Ready(_) => *b_to_a_state = TranferState::Shutdown(buf.amt),
                                Poll::Pending => (),
                            }
                        }
                    }
                },
                // if the b_to_a is shutdown, it mean that peer b read eof, so close a.
                TranferState::Shutdown(n) => match b.as_mut().poll_shutdown(cx) {
                    Poll::Ready(_) => {
                        *b_to_a_amt += *n;
                        *b_to_a_state = TranferState::Done;
                        //The connection of b is closed. So when reading the pending data from a, it should start to sleep until timeout.
                        a_to_b_sleep.replace(Box::pin(tokio::time::sleep(*b_to_a_timeout)));
                        continue;
                    }
                    Poll::Pending => (),
                },
                TranferState::Done => (),
            };
            match (b_to_a_state, a_to_b_state) {
                (TranferState::Done, TranferState::Done) => {
                    return Poll::Ready(Ok((*a_to_b_amt, *b_to_a_amt)));
                }
                _ => return Poll::Pending,
            }
        }
    }
}

pub async fn copy_bidirectional_with_timeout<A, B>(
    a: &mut A,
    b: &mut B,
    size: usize,
    a_to_b_timeout: Duration,
    b_to_a_timeout: Duration,
) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let copy = CopyBidirectional {
        a,
        b,
        a_to_b_state: TranferState::Running(CopyBuf::new(size)),
        b_to_a_state: TranferState::Running(CopyBuf::new(size)),
        a_to_b_amt: 0,
        b_to_a_amt: 0,
        a_to_b_timeout,
        b_to_a_timeout,
        a_to_b_sleep: None,
        b_to_a_sleep: None,
    };
    copy.await
}

pub trait CopyTrait: AsyncRead + AsyncWrite + Unpin + Send {}
