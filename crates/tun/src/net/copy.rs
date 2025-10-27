use std::{
    fmt::Display,
    future::poll_fn,
    pin::Pin,
    task::{Context, Poll, ready},
    time::Duration,
};

use futures::FutureExt;
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

enum TransferState {
    Running(CopyBuf),
    Shuttingdow(u64),
    Done(u64),
}

impl Display for TransferState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransferState::Running(_) => f.write_str("Running"),
            TransferState::Shuttingdow(_) => f.write_str("Shuttingdow"),
            TransferState::Done(_) => f.write_str("Done"),
        }
    }
}

#[inline]
fn transfer_one_direction<R, W>(
    cx: &mut Context<'_>,
    reader: &mut R,
    writer: &mut W,
    state: &mut TransferState,
) -> Poll<io::Result<u64>>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut reader = Pin::new(reader);
    let mut writer = Pin::new(writer);
    loop {
        match state {
            TransferState::Running(cop_buf) => {
                let count = ready!(cop_buf.poll_copy(cx, reader.as_mut(), writer.as_mut()))?;
                *state = TransferState::Shuttingdow(count);
            }
            TransferState::Shuttingdow(count) => {
                ready!(writer.as_mut().poll_shutdown(cx))?;
                *state = TransferState::Done(*count);
            }
            TransferState::Done(count) => return Poll::Ready(Ok(*count)),
        }
    }
}

struct CopyOneDirection<'a, R: ?Sized, W: ?Sized> {
    reader: Pin<&'a mut R>,
    writer: Pin<&'a mut W>,
    state: &'a mut TransferState,
    amt: &'a mut u64,
    sleep: &'a mut Option<Pin<Box<tokio::time::Sleep>>>,
    sleep_time: &'a Duration,
}

impl<'a, R, W> CopyOneDirection<'a, R, W>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    #[inline]
    fn copy(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<u64>> {
        let Self {
            amt,
            reader,
            writer,
            state,
            sleep,
            sleep_time,
        } = &mut *self;
        loop {
            match state {
                TransferState::Running(copy_buf) => {
                    let copy_rs = copy_buf.poll_copy(cx, reader.as_mut(), writer.as_mut())?;
                    match copy_rs {
                        Poll::Ready(n) => **state = TransferState::Shuttingdow(n),
                        Poll::Pending => {
                            if **amt == copy_buf.amt {
                                if let Some(sleep) = sleep {
                                    ready!(sleep.poll_unpin(cx));
                                    **state = TransferState::Shuttingdow(**amt);
                                    continue;
                                } else {
                                    **sleep = Some(Box::pin(tokio::time::sleep(**sleep_time)));
                                }
                            } else {
                                **amt = copy_buf.amt;
                                self.sleep.take();
                            }
                            return Poll::Pending;
                        }
                    }
                }
                TransferState::Shuttingdow(n) => {
                    ready!(writer.as_mut().poll_shutdown(cx))?;
                    **state = TransferState::Done(*n);
                }
                TransferState::Done(n) => return Poll::Ready(Ok(*n)),
            }
        }
    }
}

struct CopyBidirectional<'a, A: ?Sized, B: ?Sized> {
    a: &'a mut A,
    b: &'a mut B,
    a_to_b_amt: u64,
    b_to_a_amt: u64,
    a_to_b_state: TransferState,
    b_to_a_state: TransferState,
    a_to_b_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    b_to_a_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
    a_to_b_timeout: Duration,
    b_to_a_timeout: Duration,
}

impl<'a, A, B> CopyBidirectional<'a, A, B>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    fn copy(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<(u64, u64)>> {
        let Self {
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
        let mut a_to_b = CopyOneDirection {
            reader: a.as_mut(),
            writer: b.as_mut(),
            state: a_to_b_state,
            sleep: a_to_b_sleep,
            amt: a_to_b_amt,
            sleep_time: a_to_b_timeout,
        };
        let a_to_b = a_to_b.copy(cx)?;

        let mut b_to_a = CopyOneDirection {
            reader: b.as_mut(),
            writer: a.as_mut(),
            state: b_to_a_state,
            amt: b_to_a_amt,
            sleep: b_to_a_sleep,
            sleep_time: b_to_a_timeout,
        };
        let b_to_a = b_to_a.copy(cx)?;
        let a_to_b = ready!(a_to_b);
        let b_to_a = ready!(b_to_a);
        return Poll::Ready(Ok((a_to_b, b_to_a)));
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
    let mut copy_bidirectional = CopyBidirectional {
        a,
        b,
        a_to_b_state: TransferState::Running(CopyBuf::new(size)),
        b_to_a_state: TransferState::Running(CopyBuf::new(size)),
        a_to_b_amt: 0,
        b_to_a_amt: 0,
        a_to_b_timeout,
        b_to_a_timeout,
        a_to_b_sleep: None,
        b_to_a_sleep: None,
    };
    poll_fn(|cx| copy_bidirectional.copy(cx)).await
}

#[allow(dead_code)]
pub async fn copy_bidirectional<A, B>(a: &mut A, b: &mut B) -> io::Result<(u64, u64)>
where
    A: AsyncRead + AsyncWrite + Unpin + ?Sized,
    B: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    let mut a_to_b = TransferState::Running(CopyBuf::new(1024));
    let mut b_to_a = TransferState::Running(CopyBuf::new(1024));
    poll_fn(|cx| -> Poll<io::Result<(u64, u64)>> {
        let a_to_b = transfer_one_direction(cx, a, b, &mut a_to_b)?;
        let b_to_a = transfer_one_direction(cx, b, a, &mut b_to_a)?;
        let b_to_a = ready!(b_to_a);
        let a_to_b = ready!(a_to_b);
        Poll::Ready(Ok((a_to_b, b_to_a)))
    })
    .await
}

pub trait CopyTrait: AsyncRead + AsyncWrite + Unpin + Send {}
