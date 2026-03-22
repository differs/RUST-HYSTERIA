use std::{
    fmt, io,
    net::SocketAddr,
    sync::Arc,
    task::{Context, Poll},
};

#[cfg(target_os = "android")]
use std::os::fd::AsRawFd;

use hysteria_extras::obfs::{Obfuscator, SALAMANDER_SALT_LEN, SalamanderObfuscator};
use quinn::{
    AsyncUdpSocket, Endpoint, EndpointConfig, Runtime, ServerConfig as QuinnServerConfig,
    TokioRuntime, UdpPoller,
    udp::{RecvMeta, Transmit},
};
use socket2::SockRef;

use crate::{CoreError, CoreResult};

const DEFAULT_UDP_SOCKET_BUFFER_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObfsConfig {
    Salamander { password: String },
}

pub(crate) fn make_client_endpoint(
    bind_addr: SocketAddr,
    obfs: Option<&ObfsConfig>,
) -> CoreResult<Endpoint> {
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    configure_udp_socket(&socket);
    #[cfg(target_os = "android")]
    maybe_protect_android_vpn_socket(&socket);
    socket.set_nonblocking(true)?;

    let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);
    if let Some(obfs) = obfs {
        let inner = runtime.wrap_udp_socket(socket)?;
        let wrapped: Arc<dyn AsyncUdpSocket> =
            Arc::new(ObfsUdpSocket::new(inner, build_obfuscator(obfs)?));
        Ok(Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            None,
            wrapped,
            runtime,
        )?)
    } else {
        Ok(Endpoint::new(
            EndpointConfig::default(),
            None,
            socket,
            runtime,
        )?)
    }
}

pub(crate) fn make_server_endpoint(
    bind_addr: SocketAddr,
    server_config: QuinnServerConfig,
    obfs: Option<&ObfsConfig>,
) -> CoreResult<Endpoint> {
    make_endpoint(bind_addr, Some(server_config), obfs)
}

fn make_endpoint(
    bind_addr: SocketAddr,
    server_config: Option<QuinnServerConfig>,
    obfs: Option<&ObfsConfig>,
) -> CoreResult<Endpoint> {
    let socket = std::net::UdpSocket::bind(bind_addr)?;
    configure_udp_socket(&socket);
    socket.set_nonblocking(true)?;

    let runtime: Arc<dyn Runtime> = Arc::new(TokioRuntime);
    if let Some(obfs) = obfs {
        let inner = runtime.wrap_udp_socket(socket)?;
        let wrapped: Arc<dyn AsyncUdpSocket> =
            Arc::new(ObfsUdpSocket::new(inner, build_obfuscator(obfs)?));
        Ok(Endpoint::new_with_abstract_socket(
            EndpointConfig::default(),
            server_config,
            wrapped,
            runtime,
        )?)
    } else {
        Ok(Endpoint::new(
            EndpointConfig::default(),
            server_config,
            socket,
            runtime,
        )?)
    }
}

fn configure_udp_socket(socket: &std::net::UdpSocket) {
    let sock_ref = SockRef::from(socket);
    let _ = sock_ref.set_recv_buffer_size(DEFAULT_UDP_SOCKET_BUFFER_SIZE);
    let _ = sock_ref.set_send_buffer_size(DEFAULT_UDP_SOCKET_BUFFER_SIZE);
}

#[cfg(target_os = "android")]
fn maybe_protect_android_vpn_socket(socket: &std::net::UdpSocket) {
    crate::android::maybe_protect_socket(socket.as_raw_fd());
}

fn build_obfuscator(config: &ObfsConfig) -> CoreResult<Arc<SalamanderObfuscator>> {
    match config {
        ObfsConfig::Salamander { password } => SalamanderObfuscator::new(password.clone())
            .map(Arc::new)
            .map_err(|err| CoreError::Config(err.to_string())),
    }
}

struct ObfsUdpSocket {
    inner: Arc<dyn AsyncUdpSocket>,
    obfuscator: Arc<SalamanderObfuscator>,
}

impl ObfsUdpSocket {
    fn new(inner: Arc<dyn AsyncUdpSocket>, obfuscator: Arc<SalamanderObfuscator>) -> Self {
        Self { inner, obfuscator }
    }
}

impl fmt::Debug for ObfsUdpSocket {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ObfsUdpSocket").finish_non_exhaustive()
    }
}

impl AsyncUdpSocket for ObfsUdpSocket {
    fn create_io_poller(self: Arc<Self>) -> std::pin::Pin<Box<dyn UdpPoller>> {
        self.inner.clone().create_io_poller()
    }

    fn try_send(&self, transmit: &Transmit<'_>) -> io::Result<()> {
        let segment_size = transmit
            .segment_size
            .unwrap_or(transmit.contents.len().max(1));
        let segment_count = transmit.contents.len().div_ceil(segment_size);
        let mut encoded =
            Vec::with_capacity(transmit.contents.len() + segment_count * SALAMANDER_SALT_LEN);

        for chunk in transmit.contents.chunks(segment_size) {
            let mut out = vec![0_u8; chunk.len() + SALAMANDER_SALT_LEN];
            let written = self.obfuscator.obfuscate(chunk, &mut out);
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "failed to obfuscate QUIC datagram",
                ));
            }
            encoded.extend_from_slice(&out[..written]);
        }

        let obfuscated = Transmit {
            destination: transmit.destination,
            ecn: transmit.ecn,
            contents: &encoded,
            segment_size: transmit.segment_size.map(|size| size + SALAMANDER_SALT_LEN),
            src_ip: transmit.src_ip,
        };
        self.inner.try_send(&obfuscated)
    }

    fn poll_recv(
        &self,
        cx: &mut Context<'_>,
        bufs: &mut [std::io::IoSliceMut<'_>],
        meta: &mut [RecvMeta],
    ) -> Poll<io::Result<usize>> {
        match self.inner.poll_recv(cx, bufs, meta) {
            Poll::Ready(Ok(count)) => {
                for index in 0..count {
                    let buffer = &mut bufs[index];
                    let total_len = meta[index].len;
                    if total_len == 0 {
                        continue;
                    }

                    let original_stride = if meta[index].stride == 0 {
                        total_len
                    } else {
                        meta[index].stride
                    };
                    if original_stride <= SALAMANDER_SALT_LEN {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "invalid obfuscated QUIC datagram",
                        )));
                    }

                    let datagram_count = total_len.div_ceil(original_stride);
                    let decoded_stride = original_stride - SALAMANDER_SALT_LEN;
                    let mut decoded_total = 0usize;

                    for segment_index in 0..datagram_count {
                        let input_offset = segment_index * original_stride;
                        let input_len = total_len.saturating_sub(input_offset).min(original_stride);
                        let input = buffer[input_offset..input_offset + input_len].to_vec();

                        let output_offset = segment_index * decoded_stride;
                        let output_len = input_len - SALAMANDER_SALT_LEN;
                        let written = self.obfuscator.deobfuscate(
                            &input,
                            &mut buffer[output_offset..output_offset + output_len],
                        );
                        if written != output_len {
                            return Poll::Ready(Err(io::Error::new(
                                io::ErrorKind::InvalidData,
                                "failed to deobfuscate QUIC datagram",
                            )));
                        }
                        decoded_total += written;
                    }

                    meta[index].len = decoded_total;
                    meta[index].stride = if datagram_count > 1 {
                        decoded_stride
                    } else {
                        decoded_total
                    };
                }
                Poll::Ready(Ok(count))
            }
            other => other,
        }
    }

    fn local_addr(&self) -> io::Result<SocketAddr> {
        self.inner.local_addr()
    }

    fn max_transmit_segments(&self) -> usize {
        self.inner.max_transmit_segments()
    }

    fn max_receive_segments(&self) -> usize {
        self.inner.max_receive_segments()
    }

    fn may_fragment(&self) -> bool {
        self.inner.may_fragment()
    }
}
