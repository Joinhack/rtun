use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::task::{Context, Poll};

use futures::Stream;
use futures::task::AtomicWaker;

pub struct CancelHandle(Arc<AtomicBool>, Arc<AtomicWaker>);

impl CancelHandle {
    #[inline]
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
        self.1.wake();
    }
}

impl Drop for CancelHandle {
    fn drop(&mut self) {
        self.cancel();
    }
}

pin_project_lite::pin_project! {
    pub struct Cancelable<T> {
        #[pin]
        inner: T,
        cancelled: Arc<AtomicBool>,
        waker: Arc<AtomicWaker>,
    }
}

pub enum CancelableResult<T> {
    Result(T),
    Cancelled,
}

impl<T> Cancelable<T> {
    #[inline]
    pub fn new(t: T) -> (Self, CancelHandle) {
        let waker = Arc::new(AtomicWaker::new());
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = CancelHandle(cancelled.clone(), waker.clone());
        let cancelable = Cancelable {
            inner: t,
            waker,
            cancelled: cancelled,
        };
        (cancelable, handle)
    }
}

impl<T: Future> std::future::Future for Cancelable<T>
where
    T: Future + Send,
{
    type Output = CancelableResult<T::Output>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let cancel_flag = self.cancelled.load(Ordering::Acquire);
        if cancel_flag {
            return Poll::Ready(CancelableResult::Cancelled);
        }
        match self.as_mut().project().inner.poll(cx) {
            Poll::Ready(res) => return Poll::Ready(CancelableResult::Result(res)),
            Poll::Pending => {
                self.waker.register(cx.waker());
                Poll::Pending
            }
        }
    }
}

impl<T: Stream> Stream for Cancelable<T>
where
    T: Stream,
{
    type Item = CancelableResult<T::Item>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let cancel_flag = self.cancelled.load(Ordering::Acquire);
        if cancel_flag {
            return Poll::Ready(Some(CancelableResult::Cancelled));
        }

        match self.as_mut().project().inner.poll_next(cx) {
            Poll::Ready(Some(r)) => Poll::Ready(Some(CancelableResult::Result(r))),
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => {
                self.waker.register(cx.waker());
                Poll::Pending
            }
        }
    }
}
