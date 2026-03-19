use std::{
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use crate::limit::{BandwidthLimiter, LimitDecision};

#[derive(Debug)]
pub struct TcpProxyStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
    limiter_sleep: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl TcpProxyStream {
    pub(crate) fn new(
        send: quinn::SendStream,
        recv: quinn::RecvStream,
        tx_limiter: Option<Arc<BandwidthLimiter>>,
    ) -> Self {
        Self {
            send,
            recv,
            tx_limiter,
            limiter_sleep: None,
        }
    }
}

impl AsyncRead for TcpProxyStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        AsyncRead::poll_read(Pin::new(&mut self.recv), cx, buf)
    }
}

impl AsyncWrite for TcpProxyStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if let Some(limiter) = self.tx_limiter.clone() {
            if let Some(delay) = self.limiter_sleep.as_mut() {
                match delay.as_mut().poll(cx) {
                    Poll::Ready(()) => self.limiter_sleep = None,
                    Poll::Pending => return Poll::Pending,
                }
            }

            let granted = loop {
                match limiter.take_stream_budget(buf.len()) {
                    LimitDecision::Ready(granted) => break granted,
                    LimitDecision::Wait(duration) => {
                        let mut delay = Box::pin(tokio::time::sleep(duration));
                        match delay.as_mut().poll(cx) {
                            Poll::Ready(()) => continue,
                            Poll::Pending => {
                                self.limiter_sleep = Some(delay);
                                return Poll::Pending;
                            }
                        }
                    }
                }
            };

            AsyncWrite::poll_write(Pin::new(&mut self.send), cx, &buf[..granted])
        } else {
            AsyncWrite::poll_write(Pin::new(&mut self.send), cx, buf)
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_flush(Pin::new(&mut self.send), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.send), cx)
    }
}
