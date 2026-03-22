use std::{
    any::Any,
    fmt,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use bytes::Bytes;
use h3::client;
use http::Request;
use quinn::{
    ClientConfig as QuinnClientConfig, Endpoint, VarInt, crypto::rustls::QuicClientConfig,
};
use rustls::{
    ClientConfig as RustlsClientConfig, DigitallySignedStruct, RootCertStore, SignatureScheme,
    client::WebPkiServerVerifier,
    client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier},
    pki_types::{CertificateDer, ServerName, UnixTime},
};
use sha2::{Digest, Sha256};

use crate::{
    CoreError, CoreResult,
    limit::BandwidthLimiter,
    protocol::{
        AuthRequest, URL_HOST, URL_PATH, auth_request_to_headers, auth_response_from_headers,
    },
    quic::{QuicTransportConfig, build_transport_config},
    runtime_io,
    socket::{ObfsConfig, make_client_endpoint},
    stream::TcpProxyStream,
    udp::{ClientUdpManager, UdpSession},
};

const ALPN_H3: &[u8] = b"h3";
const CLOSE_ERR_CODE_OK: u32 = 0x100;
const CLOSE_ERR_CODE_PROTOCOL_ERROR: u32 = 0x101;

#[derive(Debug, Clone, Default)]
pub struct ClientTlsConfig {
    pub insecure: bool,
    pub root_certificates: Vec<CertificateDer<'static>>,
    pub pinned_certificate_sha256: Option<[u8; 32]>,
}

#[derive(Debug, Clone)]
pub struct ClientConfig {
    pub server_addr: SocketAddr,
    pub server_name: String,
    pub auth: String,
    pub bandwidth_max_tx: u64,
    pub bandwidth_max_rx: u64,
    pub obfs: Option<ObfsConfig>,
    pub tls: ClientTlsConfig,
    pub quic: QuicTransportConfig,
}

impl ClientConfig {
    pub fn new(server_addr: SocketAddr, server_name: impl Into<String>) -> Self {
        Self {
            server_addr,
            server_name: server_name.into(),
            auth: String::new(),
            bandwidth_max_tx: 0,
            bandwidth_max_rx: 0,
            obfs: None,
            tls: ClientTlsConfig::default(),
            quic: QuicTransportConfig::client_default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandshakeInfo {
    pub udp_enabled: bool,
    pub tx: u64,
}

pub struct Client {
    endpoint: Endpoint,
    connection: quinn::Connection,
    _send_request: client::SendRequest<h3_quinn::OpenStreams, Bytes>,
    udp_manager: Option<Arc<ClientUdpManager>>,
    tx_limiter: Option<Arc<BandwidthLimiter>>,
}

impl Clone for Client {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            connection: self.connection.clone(),
            _send_request: self._send_request.clone(),
            udp_manager: self.udp_manager.clone(),
            tx_limiter: self.tx_limiter.clone(),
        }
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Client")
            .field("remote_addr", &self.connection.remote_address())
            .finish_non_exhaustive()
    }
}

impl Client {
    pub async fn connect(config: ClientConfig) -> CoreResult<(Self, HandshakeInfo)> {
        if config.server_name.is_empty() {
            return Err(CoreError::Config("server_name must not be empty".into()));
        }

        let bind_addr = match config.server_addr {
            SocketAddr::V4(_) => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0),
            SocketAddr::V6(_) => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0),
        };
        let mut endpoint: Endpoint = make_client_endpoint(bind_addr, config.obfs.as_ref())?;
        let tx_target = Arc::new(AtomicU64::new(0));
        endpoint.set_default_client_config(build_client_config(&config, tx_target.clone())?);

        let connection = endpoint
            .connect(config.server_addr, &config.server_name)?
            .await?;
        verify_pinned_certificate(&connection, config.tls.pinned_certificate_sha256.as_ref())?;

        let h3_conn = h3_quinn::Connection::new(connection.clone());
        let (_driver, mut send_request) = client::new(h3_conn).await?;

        let uri = format!("https://{URL_HOST}{URL_PATH}");
        let mut request = Request::post(uri).body(())?;
        auth_request_to_headers(
            request.headers_mut(),
            &AuthRequest {
                auth: config.auth.clone(),
                rx: config.bandwidth_max_rx,
            },
        )?;

        let mut request_stream = send_request.send_request(request).await?;
        request_stream.finish().await?;
        let response = request_stream.recv_response().await?;
        if response.status().as_u16() != crate::protocol::STATUS_AUTH_OK {
            connection.close(
                VarInt::from_u32(CLOSE_ERR_CODE_PROTOCOL_ERROR),
                b"auth failed",
            );
            return Err(CoreError::Authentication(response.status().as_u16()));
        }

        let auth_response = auth_response_from_headers(response.headers());
        let tx = if auth_response.rx_auto {
            0
        } else if auth_response.rx == 0 || auth_response.rx > config.bandwidth_max_tx {
            config.bandwidth_max_tx
        } else {
            auth_response.rx
        };
        tx_target.store(tx, Ordering::Relaxed);
        let tx_limiter = BandwidthLimiter::optional(tx);
        let udp_manager = auth_response
            .udp_enabled
            .then(|| ClientUdpManager::new(connection.clone(), tx_limiter.clone()));

        Ok((
            Self {
                endpoint,
                connection,
                _send_request: send_request,
                udp_manager,
                tx_limiter,
            },
            HandshakeInfo {
                udp_enabled: auth_response.udp_enabled,
                tx,
            },
        ))
    }

    pub async fn tcp(&self, addr: &str) -> CoreResult<TcpProxyStream> {
        let (mut send, mut recv) = self.connection.open_bi().await?;
        runtime_io::write_tcp_request(&mut send, addr).await?;
        let (ok, message) = runtime_io::read_tcp_response(&mut recv).await?;
        if !ok {
            return Err(CoreError::Dial(message));
        }
        Ok(TcpProxyStream::new(send, recv, self.tx_limiter.clone()))
    }

    pub fn udp(&self) -> CoreResult<UdpSession> {
        let manager = self
            .udp_manager
            .as_ref()
            .ok_or_else(|| CoreError::Dial("UDP not enabled".into()))?;
        manager.new_udp()
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.connection.remote_address()
    }

    pub async fn wait_closed(&self) -> CoreError {
        CoreError::Closed(self.connection.closed().await.to_string())
    }

    pub async fn close(&self) -> CoreResult<()> {
        self.connection
            .close(VarInt::from_u32(CLOSE_ERR_CODE_OK), b"");
        self.endpoint.wait_idle().await;
        Ok(())
    }
}

fn build_client_config(
    config: &ClientConfig,
    bandwidth_target: Arc<AtomicU64>,
) -> CoreResult<QuinnClientConfig> {
    let crypto = RustlsClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_protocol_versions(&[&rustls::version::TLS13])?;

    let mut crypto = if config.tls.insecure {
        crypto
            .dangerous()
            .with_custom_certificate_verifier(SkipServerVerification::new())
            .with_no_client_auth()
    } else {
        let mut roots = RootCertStore::empty();
        for cert in &config.tls.root_certificates {
            roots
                .add(cert.clone())
                .map_err(|err| CoreError::Tls(err.to_string()))?;
        }
        let verifier = WebPkiServerVerifier::builder(Arc::new(roots))
            .build()
            .map_err(|err| CoreError::Tls(err.to_string()))?;
        crypto
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth()
    };

    crypto.enable_early_data = true;
    crypto.alpn_protocols = vec![ALPN_H3.to_vec()];

    let mut client_config = QuinnClientConfig::new(Arc::new(
        QuicClientConfig::try_from(crypto).map_err(|err| CoreError::Tls(err.to_string()))?,
    ));
    client_config.transport_config(Arc::new(build_transport_config(
        &config.quic,
        bandwidth_target,
    )?));
    Ok(client_config)
}

#[derive(Debug)]
struct SkipServerVerification(Arc<rustls::crypto::CryptoProvider>);

impl SkipServerVerification {
    fn new() -> Arc<Self> {
        Arc::new(Self(Arc::new(rustls::crypto::ring::default_provider())))
    }
}

fn verify_pinned_certificate(
    connection: &quinn::Connection,
    expected_hash: Option<&[u8; 32]>,
) -> CoreResult<()> {
    let Some(expected_hash) = expected_hash else {
        return Ok(());
    };

    let identity = connection
        .peer_identity()
        .ok_or_else(|| CoreError::Tls("peer did not present a certificate".into()))?;
    let certs = downcast_certificates(identity)?;
    let certificate = certs
        .first()
        .ok_or_else(|| CoreError::Tls("peer did not present an end-entity certificate".into()))?;
    let actual_hash: [u8; 32] = Sha256::digest(certificate.as_ref()).into();

    if &actual_hash == expected_hash {
        Ok(())
    } else {
        connection.close(
            VarInt::from_u32(CLOSE_ERR_CODE_PROTOCOL_ERROR),
            b"tls pin mismatch",
        );
        Err(CoreError::Tls(
            "no certificate matches the pinned SHA-256 hash".into(),
        ))
    }
}

fn downcast_certificates(identity: Box<dyn Any>) -> CoreResult<Vec<CertificateDer<'static>>> {
    identity
        .downcast::<Vec<CertificateDer<'static>>>()
        .map(|certs| *certs)
        .map_err(|_| CoreError::Tls("unexpected peer identity type".into()))
}

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}
