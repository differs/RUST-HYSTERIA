use std::io;

use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("invalid address length")]
    InvalidAddressLength,
    #[error("invalid message length")]
    InvalidMessageLength,
    #[error("invalid padding length")]
    InvalidPaddingLength,
    #[error("varint value exceeds 62 bits")]
    VarintOverflow,
    #[error("unexpected eof")]
    UnexpectedEof,
    #[error("invalid http header value")]
    InvalidHeaderValue,
    #[error("io error: {0}")]
    Io(String),
}

impl From<io::Error> for ProtocolError {
    fn from(value: io::Error) -> Self {
        if value.kind() == io::ErrorKind::UnexpectedEof {
            Self::UnexpectedEof
        } else {
            Self::Io(value.to_string())
        }
    }
}

#[derive(Debug, Error)]
pub enum CoreError {
    #[error(transparent)]
    Protocol(#[from] ProtocolError),
    #[error("configuration error: {0}")]
    Config(String),
    #[error("closed: {0}")]
    Closed(String),
    #[error("connect error: {0}")]
    Connect(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("authentication failed with status {0}")]
    Authentication(u16),
    #[error("dial error: {0}")]
    Dial(String),
    #[error("unexpected frame type {0:#x}")]
    UnexpectedFrameType(u64),
    #[error("tls error: {0}")]
    Tls(String),
}

pub type CoreResult<T> = std::result::Result<T, CoreError>;

impl From<io::Error> for CoreError {
    fn from(value: io::Error) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<http::Error> for CoreError {
    fn from(value: http::Error) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<quinn::ConnectError> for CoreError {
    fn from(value: quinn::ConnectError) -> Self {
        Self::Connect(value.to_string())
    }
}

impl From<quinn::ConnectionError> for CoreError {
    fn from(value: quinn::ConnectionError) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<quinn::WriteError> for CoreError {
    fn from(value: quinn::WriteError) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<quinn::ReadError> for CoreError {
    fn from(value: quinn::ReadError) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<quinn::ClosedStream> for CoreError {
    fn from(value: quinn::ClosedStream) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<quinn::SendDatagramError> for CoreError {
    fn from(value: quinn::SendDatagramError) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<rustls::Error> for CoreError {
    fn from(value: rustls::Error) -> Self {
        Self::Tls(value.to_string())
    }
}

impl From<h3::error::ConnectionError> for CoreError {
    fn from(value: h3::error::ConnectionError) -> Self {
        Self::Transport(value.to_string())
    }
}

impl From<h3::error::StreamError> for CoreError {
    fn from(value: h3::error::StreamError) -> Self {
        Self::Transport(value.to_string())
    }
}
