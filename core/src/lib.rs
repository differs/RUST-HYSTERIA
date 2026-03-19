#![forbid(unsafe_code)]

pub mod client;
pub mod errors;
pub mod frag;
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

pub use client::{Client, ClientConfig, ClientTlsConfig, HandshakeInfo};
pub use errors::{CoreError, CoreResult, ProtocolError};
pub use quic::{
    DEFAULT_CONNECTION_RECEIVE_WINDOW, DEFAULT_KEEP_ALIVE_PERIOD, DEFAULT_MAX_IDLE_TIMEOUT,
    DEFAULT_MAX_INCOMING_STREAMS, DEFAULT_STREAM_RECEIVE_WINDOW, QuicTransportConfig,
};
pub use server::{Authenticator, PasswordAuthenticator, Server, ServerConfig};
pub use socket::ObfsConfig;
pub use stream::TcpProxyStream;
pub use udp::UdpSession;
