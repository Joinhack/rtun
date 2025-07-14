use std::{io, net::SocketAddr, pin::Pin};

use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
};

use crate::socks5::{
    HandShakeRequest, HandShakeResponse, Reply, SOCKS5_AUTH_METHOD_NONE, SOCKS5_CMD_TCP_CONNECT,
    Socks5TcpRequest, Socks5TcpResponse,
};

pub struct Sock5TcpStream {
    stream: TcpStream,
}

impl Sock5TcpStream {
    pub async fn connect(addr: SocketAddr, proxy: SocketAddr) -> io::Result<Self> {
        let mut stream = TcpStream::connect(proxy).await?;
        let req = HandShakeRequest::new(vec![SOCKS5_AUTH_METHOD_NONE]);
        req.write_to(&mut stream).await?;
        let resp = HandShakeResponse::read_from(&mut stream).await?;
        if resp.choose_method != SOCKS5_AUTH_METHOD_NONE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "choose error socks5 method",
            ));
        }
        let tcp_req = Socks5TcpRequest::new(SOCKS5_CMD_TCP_CONNECT, addr);
        tcp_req.write_to(&mut stream).await?;
        let tcp_resp = Socks5TcpResponse::read_from(&mut stream).await?;
        match tcp_resp.reply {
            Reply::Succeeded => (),
            r @ _ => {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    format!("reply error: {}", r.to_string()),
                ));
            }
        };
        Ok(Self { stream })
    }
}

impl AsyncRead for Sock5TcpStream {
    #[inline]
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        let Sock5TcpStream { stream } = &mut *self;
        let stream = Pin::new(stream);
        stream.poll_read(cx, buf)
    }
}

impl AsyncWrite for Sock5TcpStream {
    #[inline]
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<Result<usize, io::Error>> {
        let Self { stream } = &mut *self;
        let stream = Pin::new(stream);
        stream.poll_write(cx, buf)
    }

    #[inline]
    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        let Self { stream } = &mut *self;
        let stream = Pin::new(stream);
        stream.poll_flush(cx)
    }

    #[inline]
    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), io::Error>> {
        let Self { stream } = &mut *self;
        let stream = Pin::new(stream);
        stream.poll_shutdown(cx)
    }
}
