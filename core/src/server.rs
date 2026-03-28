use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use bytes::Bytes;
use h3::server;
use http::{Method, Response, StatusCode};
use quinn::{Endpoint, VarInt, crypto::rustls::QuicServerConfig};
use rustls::{
    ServerConfig as RustlsServerConfig,
    pki_types::{CertificateDer, PrivateKeyDer},
};
use tokio::net::TcpStream;

use crate::{
    CoreError, CoreResult,
    health::HEALTH_CHECK_DEST,
    limit::{BandwidthLimiter, negotiated_limit},
    protocol::{
        AuthResponse, STATUS_AUTH_OK, URL_HOST, URL_PATH, auth_request_from_headers,
        auth_response_to_headers,
    },
    quic::{QuicTransportConfig, build_transport_config},
    relay::copy_bidirectional_with_limit,
    runtime_io,
    socket::{ObfsConfig, make_server_endpoint},
    stream::TcpProxyStream,
    udp::{DEFAULT_UDP_IDLE_TIMEOUT, run_server_udp},
};
use hysteria_extras::speedtest::{SPEEDTEST_DEST, spawn_server_conn};

const ALPN_H3: &[u8] = b"h3";
const CLOSE_ERR_CODE_OK: u32 = 0x100;

pub trait Authenticator: Send + Sync + 'static {
    fn authenticate(&self, remote_addr: SocketAddr, auth: &str, client_tx: u64) -> Option<String>;
}

#[derive(Debug, Clone)]
pub struct PasswordAuthenticator {
    password: String,
    auth_id: String,
}

impl PasswordAuthenticator {
    pub fn new(password: impl Into<String>) -> Self {
        let password = password.into();
        Self {
            auth_id: password.clone(),
            password,
        }
    }

    pub fn with_auth_id(password: impl Into<String>, auth_id: impl Into<String>) -> Self {
        Self {
            password: password.into(),
            auth_id: auth_id.into(),
        }
    }
}

impl Authenticator for PasswordAuthenticator {
    fn authenticate(
        &self,
        _remote_addr: SocketAddr,
        auth: &str,
        _client_tx: u64,
    ) -> Option<String> {
        (auth == self.password).then(|| self.auth_id.clone())
    }
}

impl<F> Authenticator for F
where
    F: Fn(SocketAddr, &str, u64) -> Option<String> + Send + Sync + 'static,
{
    fn authenticate(&self, remote_addr: SocketAddr, auth: &str, client_tx: u64) -> Option<String> {
        self(remote_addr, auth, client_tx)
    }
}

pub struct ServerConfig {
    pub bind_addr: SocketAddr,
    pub certificates: Vec<CertificateDer<'static>>,
    pub private_key: PrivateKeyDer<'static>,
    pub authenticator: Arc<dyn Authenticator>,
    pub obfs: Option<ObfsConfig>,
    pub speed_test: bool,
    pub disable_udp: bool,
    pub udp_idle_timeout: Duration,
    pub bandwidth_max_tx: u64,
    pub bandwidth_max_rx: u64,
    pub ignore_client_bandwidth: bool,
    pub quic: QuicTransportConfig,
}

struct ServerState {
    base_quinn_config: quinn::ServerConfig,
    quic: QuicTransportConfig,
    authenticator: Arc<dyn Authenticator>,
    disable_udp: bool,
    speed_test: bool,
    udp_idle_timeout: Duration,
    bandwidth_max_tx: u64,
    bandwidth_max_rx: u64,
    ignore_client_bandwidth: bool,
}

pub struct Server {
    endpoint: Endpoint,
    state: Arc<ServerState>,
}

impl Server {
    pub async fn bind(config: ServerConfig) -> CoreResult<Self> {
        if config.certificates.is_empty() {
            return Err(CoreError::Config(
                "at least one certificate must be provided".into(),
            ));
        }

        let mut crypto = RustlsServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_no_client_auth()
        .with_single_cert(config.certificates, config.private_key)
        .map_err(|err| CoreError::Tls(err.to_string()))?;
        crypto.max_early_data_size = u32::MAX;
        crypto.alpn_protocols = vec![ALPN_H3.to_vec()];

        let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(
            QuicServerConfig::try_from(crypto).map_err(|err| CoreError::Tls(err.to_string()))?,
        ));
        server_config.transport_config(Arc::new(build_transport_config(
            &config.quic,
            Arc::new(AtomicU64::new(0)),
        )?));
        let endpoint = make_server_endpoint(
            config.bind_addr,
            server_config.clone(),
            config.obfs.as_ref(),
        )?;

        Ok(Self {
            endpoint,
            state: Arc::new(ServerState {
                base_quinn_config: server_config,
                quic: config.quic,
                authenticator: config.authenticator,
                disable_udp: config.disable_udp,
                speed_test: config.speed_test,
                udp_idle_timeout: if config.udp_idle_timeout.is_zero() {
                    DEFAULT_UDP_IDLE_TIMEOUT
                } else {
                    config.udp_idle_timeout
                },
                bandwidth_max_tx: config.bandwidth_max_tx,
                bandwidth_max_rx: config.bandwidth_max_rx,
                ignore_client_bandwidth: config.ignore_client_bandwidth,
            }),
        })
    }

    pub fn local_addr(&self) -> CoreResult<SocketAddr> {
        self.endpoint.local_addr().map_err(Into::into)
    }

    pub async fn serve(&self) -> CoreResult<()> {
        while let Some(incoming) = self.endpoint.accept().await {
            let state = self.state.clone();
            tokio::spawn(async move {
                let tx_target = Arc::new(AtomicU64::new(0));
                let mut server_config = state.base_quinn_config.clone();
                let transport = build_transport_config(&state.quic, tx_target.clone());
                let _ = match transport {
                    Ok(transport) => {
                        server_config.transport_config(Arc::new(transport));
                        match incoming.accept_with(Arc::new(server_config)) {
                            Ok(connecting) => handle_connection(connecting, state, tx_target).await,
                            Err(err) => Err(err.into()),
                        }
                    }
                    Err(err) => Err(err),
                };
            });
        }
        Ok(())
    }

    pub fn close(&self) {
        self.endpoint
            .close(VarInt::from_u32(CLOSE_ERR_CODE_OK), b"");
    }
}

async fn handle_connection(
    connecting: quinn::Connecting,
    state: Arc<ServerState>,
    tx_target: Arc<AtomicU64>,
) -> CoreResult<()> {
    let connection = connecting.await?;
    let mut h3_connection: server::Connection<_, Bytes> =
        server::Connection::new(h3_quinn::Connection::new(connection.clone())).await?;

    let Some(resolver) = h3_connection.accept().await? else {
        return Ok(());
    };
    let (request, mut request_stream) = resolver.resolve_request().await?;

    let valid_auth_request = request.method() == Method::POST
        && request.uri().path() == URL_PATH
        && request.uri().host() == Some(URL_HOST);

    if !valid_auth_request {
        let response = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
        request_stream.send_response(response).await?;
        request_stream.finish().await?;
        return keep_h3_alive_until_closed(connection, h3_connection).await;
    }

    let auth_request = auth_request_from_headers(request.headers());
    let Some(_auth_id) = state.authenticator.authenticate(
        connection.remote_address(),
        &auth_request.auth,
        auth_request.rx,
    ) else {
        let response = Response::builder().status(StatusCode::NOT_FOUND).body(())?;
        request_stream.send_response(response).await?;
        request_stream.finish().await?;
        return keep_h3_alive_until_closed(connection, h3_connection).await;
    };

    let actual_tx = if state.ignore_client_bandwidth {
        0
    } else {
        negotiated_limit(state.bandwidth_max_tx, auth_request.rx)
    };
    tx_target.store(actual_tx, Ordering::Relaxed);
    let mut response = Response::builder().status(STATUS_AUTH_OK).body(())?;
    auth_response_to_headers(
        response.headers_mut(),
        &AuthResponse {
            udp_enabled: !state.disable_udp,
            rx: state.bandwidth_max_rx,
            rx_auto: state.ignore_client_bandwidth,
        },
    )?;
    request_stream.send_response(response).await?;
    request_stream.finish().await?;

    let hold_connection = connection.clone();
    tokio::spawn(async move {
        let _h3_connection = h3_connection;
        let _ = hold_connection.closed().await;
    });
    handle_authenticated_connection(connection, state, actual_tx).await
}

async fn keep_h3_alive_until_closed(
    connection: quinn::Connection,
    h3_connection: server::Connection<h3_quinn::Connection, Bytes>,
) -> CoreResult<()> {
    let _h3_connection = h3_connection;
    let _ = connection.closed().await;
    Ok(())
}

async fn handle_authenticated_connection(
    connection: quinn::Connection,
    state: Arc<ServerState>,
    actual_tx: u64,
) -> CoreResult<()> {
    let tx_limiter = BandwidthLimiter::optional(actual_tx);
    if state.disable_udp {
        return handle_authenticated_streams(connection, state.speed_test, tx_limiter).await;
    }

    let mut tcp_task = tokio::spawn(handle_authenticated_streams(
        connection.clone(),
        state.speed_test,
        tx_limiter.clone(),
    ));
    let mut udp_task = tokio::spawn(run_server_udp(
        connection.clone(),
        state.udp_idle_timeout,
        tx_limiter,
    ));

    let result = tokio::select! {
        tcp_result = &mut tcp_task => tcp_result.map_err(|err| CoreError::Transport(err.to_string()))?,
        udp_result = &mut udp_task => udp_result.map_err(|err| CoreError::Transport(err.to_string()))?,
    };

    tcp_task.abort();
    udp_task.abort();
    result
}

async fn handle_authenticated_streams(
    connection: quinn::Connection,
    speed_test_enabled: bool,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
) -> CoreResult<()> {
    loop {
        let (send, recv) = match connection.accept_bi().await {
            Ok(stream) => stream,
            Err(quinn::ConnectionError::ApplicationClosed { .. })
            | Err(quinn::ConnectionError::LocallyClosed) => return Ok(()),
            Err(err) => return Err(err.into()),
        };

        let stream_limiter = tx_limiter.clone();
        tokio::spawn(async move {
            let _ = handle_tcp_stream(send, recv, speed_test_enabled, stream_limiter).await;
        });
    }
}

async fn handle_tcp_stream(
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    speed_test_enabled: bool,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
) -> CoreResult<()> {
    let req_addr = runtime_io::read_framed_tcp_request(&mut recv).await?;
    if is_healthcheck_request(&req_addr) {
        runtime_io::write_tcp_response(&mut send, true, "Connected").await?;
        let payload = recv
            .read_to_end(64)
            .await
            .map_err(|err| CoreError::Transport(err.to_string()))?;
        if payload != b"ping" {
            let _ = send.write_all(b"bad-healthcheck").await;
            send.finish()
                .map_err(|finish_err| CoreError::Transport(finish_err.to_string()))?;
            return Err(CoreError::Dial("invalid healthcheck payload".into()));
        }
        send.write_all(b"pong")
            .await
            .map_err(|err| CoreError::Transport(err.to_string()))?;
        send.finish()
            .map_err(|finish_err| CoreError::Transport(finish_err.to_string()))?;
        return Ok(());
    }
    if is_speedtest_request(&req_addr) {
        if !speed_test_enabled {
            let _ = runtime_io::write_tcp_response(&mut send, false, "speed test disabled").await;
            send.finish()
                .map_err(|finish_err| CoreError::Transport(finish_err.to_string()))?;
            return Err(CoreError::Dial("speed test disabled".into()));
        }

        runtime_io::write_tcp_response(&mut send, true, "Connected").await?;
        let proxy_stream = TcpProxyStream::new(send, recv, None);
        let remote = spawn_server_conn();
        let _ = copy_bidirectional_with_limit(proxy_stream, remote, None, tx_limiter).await;
        return Ok(());
    }

    match TcpStream::connect(&req_addr).await {
        Ok(remote) => {
            runtime_io::write_tcp_response(&mut send, true, "Connected").await?;
            let proxy_stream = TcpProxyStream::new(send, recv, None);
            let _ = copy_bidirectional_with_limit(proxy_stream, remote, None, tx_limiter).await;
            Ok(())
        }
        Err(err) => {
            let _ = runtime_io::write_tcp_response(&mut send, false, &err.to_string()).await;
            send.finish()
                .map_err(|finish_err| CoreError::Transport(finish_err.to_string()))?;
            Err(CoreError::Dial(err.to_string()))
        }
    }
}

fn is_healthcheck_request(addr: &str) -> bool {
    matches!(addr.trim(), HEALTH_CHECK_DEST)
}

fn is_speedtest_request(addr: &str) -> bool {
    matches!(addr.trim(), SPEEDTEST_DEST | "@SpeedTest:0")
        || addr
            .split_once(':')
            .map(|(host, _)| host == SPEEDTEST_DEST)
            .unwrap_or(false)
}
