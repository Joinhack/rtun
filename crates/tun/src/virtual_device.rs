use std::{
    ops::{Deref, DerefMut},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use bytes::BytesMut;
use smoltcp::phy::{self, DeviceCapabilities};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender, error::TryRecvError};

pub struct TokenBuffer(BytesMut);

impl TokenBuffer {
    pub fn with_capacity(capacity: usize) -> Self {
        Self(BytesMut::with_capacity(capacity))
    }
}

impl Deref for TokenBuffer {
    type Target = BytesMut;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for TokenBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

pub struct VirtRxToken(TokenBuffer);

impl phy::RxToken for VirtRxToken {
    fn consume<R, F>(self, f: F) -> R
    where
        F: FnOnce(&[u8]) -> R,
    {
        f(&self.0)
    }
}

pub struct VirtTxToken<'a>(&'a mut VirtTunDevice);

impl<'a> phy::TxToken for VirtTxToken<'a> {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut tx_buffer = TokenBuffer::with_capacity(len);
        unsafe {
            tx_buffer.set_len(len);
        }
        let result = f(&mut tx_buffer);
        self.0
            .out_buf
            .send(tx_buffer)
            .expect("virtual device send error.");
        result
    }
}

pub struct VirtTunDevice {
    in_buf: UnboundedReceiver<TokenBuffer>,
    out_buf: UnboundedSender<TokenBuffer>,
    capabilities: DeviceCapabilities,
    in_buf_avail: Arc<AtomicBool>,
}

impl VirtTunDevice {
    pub fn new(
        capabilities: DeviceCapabilities,
    ) -> (
        Self,
        UnboundedSender<TokenBuffer>,
        UnboundedReceiver<TokenBuffer>,
        Arc<AtomicBool>,
    ) {
        let (out_buf, rx) = mpsc::unbounded_channel();
        let (tx, in_buf) = mpsc::unbounded_channel();
        let in_buf_avail = Arc::new(AtomicBool::new(false));
        (
            Self {
                capabilities,
                in_buf,
                out_buf,
                in_buf_avail: in_buf_avail.clone(),
            },
            tx,
            rx,
            in_buf_avail,
        )
    }

    #[inline(always)]
    pub fn recv_available(&self) -> bool {
        self.in_buf_avail.load(Ordering::Acquire)
    }
}

impl phy::Device for VirtTunDevice {
    type RxToken<'a>
        = VirtRxToken
    where
        Self: 'a;

    type TxToken<'a> = VirtTxToken<'a>;

    fn receive(
        &mut self,
        _timestamp: smoltcp::time::Instant,
    ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        match self.in_buf.try_recv() {
            Ok(buf) => {
                let rx = VirtRxToken(buf);
                let tx = VirtTxToken(self);

                Some((rx, tx))
            }
            Err(TryRecvError::Empty) => None,
            Err(_) => panic!("channel is closed"),
        }
    }

    fn transmit(&mut self, _timestamp: smoltcp::time::Instant) -> Option<Self::TxToken<'_>> {
        Some(VirtTxToken(self))
    }

    fn capabilities(&self) -> phy::DeviceCapabilities {
        self.capabilities.clone()
    }
}
