#![forbid(unsafe_code)]

#[cfg(target_os = "android")]
pub mod android;
pub mod client;
pub mod errors;
pub mod frag;
pub mod health;
mod limit;
pub mod protocol;
mod quic;
mod relay;
mod runtime_io;
pub mod server;
mod socket;
mod stream;
mod udp;
pub mod varint;

pub use client::{Client, ClientConfig, ClientTlsConfig, HandshakeInfo, TransportSnapshot};
pub use errors::{CoreError, CoreResult, ProtocolError};
pub use health::{HEALTH_CHECK_DEST, run_client_health_check};
pub use quic::{
    DEFAULT_CONNECTION_RECEIVE_WINDOW, DEFAULT_KEEP_ALIVE_PERIOD, DEFAULT_MAX_IDLE_TIMEOUT,
    DEFAULT_MAX_INCOMING_STREAMS, DEFAULT_STREAM_RECEIVE_WINDOW, QuicTransportConfig,
};
pub use server::{Authenticator, PasswordAuthenticator, Server, ServerConfig};
pub use socket::ObfsConfig;
pub use stream::TcpProxyStream;
pub use udp::{DEFAULT_CLIENT_UDP_MESSAGE_CHANNEL_SIZE, UdpSession, UdpSessionConfig};
