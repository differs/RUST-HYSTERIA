#![allow(dead_code)]

use std::{
    fmt, fs,
    net::{SocketAddr, ToSocketAddrs},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use hysteria_core as core;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs1KeyDer, PrivatePkcs8KeyDer, PrivateSec1KeyDer,
};
use serde::Deserialize;

#[derive(Debug, Clone)]
pub struct LoadedConfig<T> {
    pub path: PathBuf,
    pub value: T,
}

#[derive(Debug, Clone)]
pub struct RunnableClientConfig {
    pub core: core::ClientConfig,
    pub socks5: Option<Socks5Config>,
    pub http: Option<HttpConfig>,
    pub tcp_forwarding: Vec<TcpForwardingEntry>,
    pub udp_forwarding: Vec<UdpForwardingEntry>,
}

pub struct RunnableServerConfig {
    pub core: core::ServerConfig,
}

pub fn load_client_config(config_path: Option<&Path>) -> Result<LoadedConfig<ClientConfig>> {
    load_yaml(config_path)
}

pub fn load_server_config(config_path: Option<&Path>) -> Result<LoadedConfig<ServerConfig>> {
    load_yaml(config_path)
}

fn load_yaml<T>(config_path: Option<&Path>) -> Result<LoadedConfig<T>>
where
    T: for<'de> Deserialize<'de>,
{
    let path = resolve_config_path(config_path)?;
    let raw = fs::read_to_string(&path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let value = serde_yaml::from_str::<T>(&raw)
        .with_context(|| format!("failed to parse YAML config {}", path.display()))?;
    Ok(LoadedConfig { path, value })
}

pub fn resolve_config_path(config_path: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = config_path {
        return Ok(path.to_path_buf());
    }

    let mut candidates = vec![PathBuf::from("config.yaml"), PathBuf::from("config.yml")];

    if let Some(home) = dirs::home_dir() {
        candidates.push(home.join(".hysteria/config.yaml"));
        candidates.push(home.join(".hysteria/config.yml"));
    }

    candidates.push(PathBuf::from("/etc/hysteria/config.yaml"));
    candidates.push(PathBuf::from("/etc/hysteria/config.yml"));

    candidates.into_iter().find(|path| path.exists()).ok_or_else(|| {
        anyhow::anyhow!(
            "no config file found; checked ./config.yaml, ./config.yml, $HOME/.hysteria/config.yaml, $HOME/.hysteria/config.yml, /etc/hysteria/config.yaml, /etc/hysteria/config.yml"
        )
    })
}

pub fn build_runnable_client_config(config: &ClientConfig) -> Result<RunnableClientConfig> {
    ensure_client_runtime_mode_supported(config)?;
    let core_config = build_client_core_config(config)?;

    Ok(RunnableClientConfig {
        core: core_config,
        socks5: config.socks5.clone(),
        http: config.http.clone(),
        tcp_forwarding: config.tcp_forwarding.clone(),
        udp_forwarding: config.udp_forwarding.clone(),
    })
}

pub fn build_client_core_config(config: &ClientConfig) -> Result<core::ClientConfig> {
    let config = normalize_client_config(config)?;
    ensure_client_core_supported(&config)?;

    let mut core_config = core::ClientConfig::new(
        resolve_socket_addr(&config.server, None).context("invalid server address")?,
        infer_server_name(&config)?,
    );
    core_config.auth = config.auth.clone();
    core_config.bandwidth_max_tx =
        parse_bandwidth(&config.bandwidth.up).context("invalid bandwidth.up")?;
    core_config.bandwidth_max_rx =
        parse_bandwidth(&config.bandwidth.down).context("invalid bandwidth.down")?;
    core_config.obfs = build_obfs_config(&config.obfs).context("invalid obfs config")?;
    core_config.quic = build_client_quic_transport(&config.quic).context("invalid quic config")?;
    core_config.tls = core::ClientTlsConfig {
        insecure: config.tls.insecure,
        root_certificates: load_optional_certificates(&config.tls.ca)
            .context("failed to load tls.ca")?,
        pinned_certificate_sha256: parse_optional_pinned_sha256(&config.tls.pin_sha256)
            .context("invalid tls.pinSHA256")?,
    };

    if !core_config.tls.insecure && core_config.tls.root_certificates.is_empty() {
        bail!("tls.ca is required for the Rust client unless tls.insecure=true");
    }

    Ok(core_config)
}

pub fn normalize_client_config(config: &ClientConfig) -> Result<ClientConfig> {
    let mut normalized = config.clone();
    let server = normalized.server.trim();
    if !(server.starts_with("hy2://") || server.starts_with("hysteria2://")) {
        return Ok(normalized);
    }

    let uri = url::Url::parse(server).context("failed to parse client server URI")?;
    match uri.scheme() {
        "hy2" | "hysteria2" => {}
        _ => return Ok(normalized),
    }

    let host = uri
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("client URI is missing a host"))?;
    normalized.server = match uri.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    };
    if !uri.username().is_empty() || uri.password().is_some() {
        let username = decode_userinfo_component(uri.username())
            .context("failed to decode auth username from client URI")?;
        let password = uri
            .password()
            .map(|password| {
                decode_userinfo_component(password)
                    .context("failed to decode auth password from client URI")
            })
            .transpose()?;
        normalized.auth = match password {
            Some(password) => format!("{username}:{password}"),
            None => username,
        };
    }

    let query = uri
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    if let Some(obfs_type) = query.get("obfs") {
        normalized.obfs.r#type = obfs_type.to_string();
        if obfs_type.eq_ignore_ascii_case("salamander") {
            normalized.obfs.salamander.password = query
                .get("obfs-password")
                .map(ToString::to_string)
                .unwrap_or_default();
        }
    }
    if let Some(sni) = query.get("sni") {
        normalized.tls.sni = sni.to_string();
    }
    if let Some(insecure) = query
        .get("insecure")
        .and_then(|value| parse_bool_like(value))
    {
        normalized.tls.insecure = insecure;
    }
    if let Some(pin_sha256) = query.get("pinSHA256") {
        normalized.tls.pin_sha256 = pin_sha256.to_string();
    }

    Ok(normalized)
}

pub fn build_runnable_server_config(config: &ServerConfig) -> Result<RunnableServerConfig> {
    ensure_server_mode_supported(config)?;

    let tls = config
        .tls
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tls must be configured for the Rust server"))?;

    let bandwidth_max_tx = parse_bandwidth(&config.bandwidth.up).context("invalid bandwidth.up")?;
    let bandwidth_max_rx =
        parse_bandwidth(&config.bandwidth.down).context("invalid bandwidth.down")?;
    validate_min_bandwidth("bandwidth.up", bandwidth_max_tx)?;
    validate_min_bandwidth("bandwidth.down", bandwidth_max_rx)?;

    Ok(RunnableServerConfig {
        core: core::ServerConfig {
            bind_addr: resolve_socket_addr(
                if config.listen.is_empty() {
                    ":443"
                } else {
                    &config.listen
                },
                Some("0.0.0.0"),
            )
            .context("invalid listen address")?,
            certificates: load_certificates(Path::new(&tls.cert))
                .context("failed to load tls.cert")?,
            private_key: load_private_key(Path::new(&tls.key)).context("failed to load tls.key")?,
            authenticator: Arc::new(build_password_authenticator(&config.auth)?),
            obfs: build_obfs_config(&config.obfs).context("invalid obfs config")?,
            speed_test: config.speed_test,
            disable_udp: config.disable_udp,
            udp_idle_timeout: config.udp_idle_timeout,
            bandwidth_max_tx,
            bandwidth_max_rx,
            ignore_client_bandwidth: config.ignore_client_bandwidth,
            quic: build_server_quic_transport(&config.quic).context("invalid quic config")?,
        },
    })
}

fn ensure_client_core_supported(config: &ClientConfig) -> Result<()> {
    if config.server.trim().is_empty() {
        bail!("server must not be empty");
    }
    if !matches!(
        config.transport.r#type.trim().to_ascii_lowercase().as_str(),
        "" | "udp"
    ) {
        bail!("transport.type is not supported yet by the Rust client CLI");
    }
    if !matches!(
        config.obfs.r#type.trim().to_ascii_lowercase().as_str(),
        "" | "plain" | "salamander"
    ) {
        bail!("obfs.type is not supported yet by the Rust client CLI");
    }
    if !config.tls.client_certificate.trim().is_empty() || !config.tls.client_key.trim().is_empty()
    {
        bail!("mutual TLS client certificates are not supported yet by the Rust client CLI");
    }
    if config.fast_open {
        bail!("fastOpen is not supported yet by the Rust client CLI");
    }
    if config.lazy {
        bail!("lazy mode is not supported yet by the Rust client CLI");
    }
    if config.tcp_tproxy.is_some()
        || config.udp_tproxy.is_some()
        || config.tcp_redirect.is_some()
        || config.tun.is_some()
    {
        bail!("tproxy/redirect/tun are not supported yet by the Rust client CLI");
    }

    if let Some(socks5) = &config.socks5 {
        if socks5.listen.trim().is_empty() {
            bail!("socks5.listen must not be empty");
        }
    }
    if let Some(http) = &config.http {
        if http.listen.trim().is_empty() {
            bail!("http.listen must not be empty");
        }
    }

    Ok(())
}

fn ensure_client_runtime_mode_supported(config: &ClientConfig) -> Result<()> {
    ensure_client_core_supported(config)?;

    if config.socks5.is_none()
        && config.http.is_none()
        && config.tcp_forwarding.is_empty()
        && config.udp_forwarding.is_empty()
    {
        bail!("no client mode specified; configure socks5, http, tcpForwarding, or udpForwarding");
    }
    for (index, entry) in config.tcp_forwarding.iter().enumerate() {
        if entry.listen.trim().is_empty() {
            bail!("tcpForwarding[{index}].listen must not be empty");
        }
        if entry.remote.trim().is_empty() {
            bail!("tcpForwarding[{index}].remote must not be empty");
        }
    }
    for (index, entry) in config.udp_forwarding.iter().enumerate() {
        if entry.listen.trim().is_empty() {
            bail!("udpForwarding[{index}].listen must not be empty");
        }
        if entry.remote.trim().is_empty() {
            bail!("udpForwarding[{index}].remote must not be empty");
        }
    }
    Ok(())
}

fn ensure_server_mode_supported(config: &ServerConfig) -> Result<()> {
    if !matches!(
        config.obfs.r#type.trim().to_ascii_lowercase().as_str(),
        "" | "plain" | "salamander"
    ) {
        bail!("obfs.type is not supported yet by the Rust server CLI");
    }
    if config.acme.is_some() {
        bail!("acme is not supported yet by the Rust server CLI");
    }
    if !config.auth.userpass.is_empty()
        || !config.auth.http.url.trim().is_empty()
        || !config.auth.command.trim().is_empty()
    {
        bail!("only password authentication is supported by the Rust server CLI");
    }
    if !config.resolver.r#type.trim().is_empty()
        || config.sniff.enable
        || !config.acl.file.trim().is_empty()
        || !config.acl.inline.is_empty()
        || !config.acl.geoip.trim().is_empty()
        || !config.acl.geosite.trim().is_empty()
        || !config.outbounds.is_empty()
        || !config.traffic_stats.listen.trim().is_empty()
        || !config.masquerade.r#type.trim().is_empty()
        || !config.masquerade.listen_http.trim().is_empty()
        || !config.masquerade.listen_https.trim().is_empty()
    {
        bail!(
            "resolver/sniff/acl/outbounds/trafficStats/masquerade are not supported yet by the Rust server CLI"
        );
    }

    let tls = config
        .tls
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("tls must be configured"))?;
    if tls.cert.trim().is_empty() || tls.key.trim().is_empty() {
        bail!("tls.cert and tls.key must not be empty");
    }
    if !tls.client_ca.trim().is_empty() {
        bail!("tls.clientCA is not supported yet by the Rust server CLI");
    }
    if !tls.sni_guard.trim().is_empty() {
        bail!("tls.sniGuard is not supported yet by the Rust server CLI");
    }

    if !matches!(
        config.auth.r#type.trim().to_ascii_lowercase().as_str(),
        "" | "password"
    ) {
        bail!("auth.type must be empty or password");
    }
    if config.auth.password.trim().is_empty() {
        bail!("auth.password must not be empty");
    }
    if !config.udp_idle_timeout.is_zero()
        && (config.udp_idle_timeout < Duration::from_secs(2)
            || config.udp_idle_timeout > Duration::from_secs(600))
    {
        bail!("udpIdleTimeout must be between 2s and 600s");
    }

    Ok(())
}

fn build_password_authenticator(auth: &ServerAuthConfig) -> Result<core::PasswordAuthenticator> {
    if auth.password.trim().is_empty() {
        bail!("auth.password must not be empty");
    }
    Ok(core::PasswordAuthenticator::new(auth.password.clone()))
}

fn build_client_quic_transport(config: &ClientQuicConfig) -> Result<core::QuicTransportConfig> {
    Ok(core::QuicTransportConfig {
        stream_receive_window: resolve_quic_window(
            config.init_stream_receive_window,
            config.max_stream_receive_window,
            core::DEFAULT_STREAM_RECEIVE_WINDOW,
            "quic.initStreamReceiveWindow",
            "quic.maxStreamReceiveWindow",
        )?,
        receive_window: resolve_quic_window(
            config.init_connection_receive_window,
            config.max_connection_receive_window,
            core::DEFAULT_CONNECTION_RECEIVE_WINDOW,
            "quic.initConnReceiveWindow",
            "quic.maxConnReceiveWindow",
        )?,
        max_idle_timeout: resolve_quic_idle_timeout(config.max_idle_timeout)?,
        keep_alive_interval: Some(resolve_keep_alive_period(config.keep_alive_period)?),
        max_concurrent_bidi_streams: None,
        disable_path_mtu_discovery: config.disable_path_mtu_discovery,
    })
}

fn build_server_quic_transport(config: &ServerQuicConfig) -> Result<core::QuicTransportConfig> {
    let max_incoming_streams = if config.max_incoming_streams == 0 {
        core::DEFAULT_MAX_INCOMING_STREAMS
    } else if config.max_incoming_streams < 8 {
        bail!("quic.maxIncomingStreams must be at least 8");
    } else {
        config.max_incoming_streams as u64
    };

    Ok(core::QuicTransportConfig {
        stream_receive_window: resolve_quic_window(
            config.init_stream_receive_window,
            config.max_stream_receive_window,
            core::DEFAULT_STREAM_RECEIVE_WINDOW,
            "quic.initStreamReceiveWindow",
            "quic.maxStreamReceiveWindow",
        )?,
        receive_window: resolve_quic_window(
            config.init_connection_receive_window,
            config.max_connection_receive_window,
            core::DEFAULT_CONNECTION_RECEIVE_WINDOW,
            "quic.initConnReceiveWindow",
            "quic.maxConnReceiveWindow",
        )?,
        max_idle_timeout: resolve_quic_idle_timeout(config.max_idle_timeout)?,
        keep_alive_interval: None,
        max_concurrent_bidi_streams: Some(max_incoming_streams),
        disable_path_mtu_discovery: config.disable_path_mtu_discovery,
    })
}

fn resolve_quic_window(
    init_window: u64,
    max_window: u64,
    default_window: u64,
    init_field: &str,
    max_field: &str,
) -> Result<u64> {
    let value = match (init_window, max_window) {
        (0, 0) => default_window,
        (0, max_window) => max_window,
        (init_window, 0) => init_window,
        (init_window, max_window) => init_window.max(max_window),
    };
    if value < 16 * 1024 {
        bail!("{init_field} and {max_field} must be at least 16384");
    }
    Ok(value)
}

fn resolve_quic_idle_timeout(value: Duration) -> Result<Duration> {
    if value.is_zero() {
        Ok(core::DEFAULT_MAX_IDLE_TIMEOUT)
    } else if !(Duration::from_secs(4)..=Duration::from_secs(120)).contains(&value) {
        bail!("quic.maxIdleTimeout must be between 4s and 120s");
    } else {
        Ok(value)
    }
}

fn resolve_keep_alive_period(value: Duration) -> Result<Duration> {
    if value.is_zero() {
        Ok(core::DEFAULT_KEEP_ALIVE_PERIOD)
    } else if !(Duration::from_secs(2)..=Duration::from_secs(60)).contains(&value) {
        bail!("quic.keepAlivePeriod must be between 2s and 60s");
    } else {
        Ok(value)
    }
}

fn parse_optional_pinned_sha256(input: &str) -> Result<Option<[u8; 32]>> {
    let normalized = normalize_cert_hash(input);
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.len() != 64 {
        bail!("pinned SHA-256 hash must be exactly 64 hex characters");
    }

    let mut output = [0_u8; 32];
    for (index, chunk) in normalized.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        output[index] = (high << 4) | low;
    }
    Ok(Some(output))
}

fn hex_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("invalid hex digit in pinned SHA-256 hash"),
    }
}

pub fn normalize_cert_hash(hash: &str) -> String {
    hash.trim().to_ascii_lowercase().replace([':', '-'], "")
}

fn parse_bool_like(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" => Some(true),
        "0" | "false" | "f" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn decode_userinfo_component(input: &str) -> Result<String> {
    if !input.as_bytes().contains(&b'%') {
        return Ok(input.to_string());
    }

    let bytes = input.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                if index + 2 >= bytes.len() {
                    bail!("invalid percent-encoding in client URI auth");
                }
                let high = percent_nibble(bytes[index + 1])?;
                let low = percent_nibble(bytes[index + 2])?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(decoded).context("client URI auth is not valid UTF-8")
}

fn percent_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("invalid percent-encoding in client URI auth"),
    }
}

fn build_obfs_config<T>(config: &T) -> Result<Option<core::ObfsConfig>>
where
    T: ObfsSection,
{
    match config.obfs_type().trim().to_ascii_lowercase().as_str() {
        "" | "plain" => Ok(None),
        "salamander" => {
            let password = config.salamander_password().trim();
            if password.is_empty() {
                bail!("obfs.salamander.password must not be empty");
            }
            Ok(Some(core::ObfsConfig::Salamander {
                password: password.to_string(),
            }))
        }
        other => bail!("unsupported obfs.type {other}"),
    }
}

trait ObfsSection {
    fn obfs_type(&self) -> &str;
    fn salamander_password(&self) -> &str;
}

impl ObfsSection for ClientObfsConfig {
    fn obfs_type(&self) -> &str {
        &self.r#type
    }

    fn salamander_password(&self) -> &str {
        &self.salamander.password
    }
}

impl ObfsSection for ServerObfsConfig {
    fn obfs_type(&self) -> &str {
        &self.r#type
    }

    fn salamander_password(&self) -> &str {
        &self.salamander.password
    }
}

fn infer_server_name(config: &ClientConfig) -> Result<String> {
    if !config.tls.sni.trim().is_empty() {
        return Ok(config.tls.sni.clone());
    }

    let server = config.server.trim();
    if let Ok(addr) = server.parse::<SocketAddr>() {
        return Ok(addr.ip().to_string());
    }
    if let Some((host, _)) = server.rsplit_once(':') {
        return Ok(host.trim_matches(['[', ']']).to_string());
    }
    bail!("failed to infer server name from server address; set tls.sni explicitly");
}

fn resolve_socket_addr(input: &str, default_host: Option<&str>) -> Result<SocketAddr> {
    let input = input.trim();
    if input.is_empty() {
        bail!("address must not be empty");
    }

    let normalized = if input.starts_with(':') {
        format!("{}{}", default_host.unwrap_or("127.0.0.1"), input)
    } else {
        input.to_string()
    };

    normalized
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {normalized}"))?
        .next()
        .ok_or_else(|| anyhow::anyhow!("no socket addresses resolved for {normalized}"))
}

fn load_optional_certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    if path.trim().is_empty() {
        return Ok(Vec::new());
    }
    load_certificates(Path::new(path))
}

fn load_certificates(path: &Path) -> Result<Vec<CertificateDer<'static>>> {
    let file = fs::File::open(path)
        .with_context(|| format!("failed to open certificate file {}", path.display()))?;
    let mut reader = std::io::BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .with_context(|| format!("failed to parse PEM certificates from {}", path.display()))?;
    if certs.is_empty() {
        bail!("no certificates found in {}", path.display());
    }
    Ok(certs.into_iter().map(CertificateDer::from).collect())
}

fn load_private_key(path: &Path) -> Result<PrivateKeyDer<'static>> {
    let pkcs8 = {
        let file = fs::File::open(path)
            .with_context(|| format!("failed to open private key file {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        rustls_pemfile::pkcs8_private_keys(&mut reader).with_context(|| {
            format!(
                "failed to parse PKCS#8 private keys from {}",
                path.display()
            )
        })?
    };
    if let Some(key) = pkcs8.into_iter().next() {
        return Ok(PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)));
    }

    let rsa = {
        let file = fs::File::open(path)
            .with_context(|| format!("failed to open private key file {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        rustls_pemfile::rsa_private_keys(&mut reader).with_context(|| {
            format!(
                "failed to parse PKCS#1 private keys from {}",
                path.display()
            )
        })?
    };
    if let Some(key) = rsa.into_iter().next() {
        return Ok(PrivateKeyDer::Pkcs1(PrivatePkcs1KeyDer::from(key)));
    }

    let sec1 = {
        let file = fs::File::open(path)
            .with_context(|| format!("failed to open private key file {}", path.display()))?;
        let mut reader = std::io::BufReader::new(file);
        rustls_pemfile::ec_private_keys(&mut reader)
            .with_context(|| format!("failed to parse SEC1 private keys from {}", path.display()))?
    };
    if let Some(key) = sec1.into_iter().next() {
        return Ok(PrivateKeyDer::Sec1(PrivateSec1KeyDer::from(key)));
    }

    bail!("no supported private key found in {}", path.display())
}

fn parse_bandwidth(input: &str) -> Result<u64> {
    let normalized = input.trim().to_ascii_lowercase();
    if normalized.is_empty() {
        return Ok(0);
    }

    let mut split = 0usize;
    for (index, ch) in normalized.char_indices() {
        if !ch.is_ascii_digit() {
            split = index;
            break;
        }
    }
    if split == 0 {
        bail!("invalid bandwidth format");
    }

    let value: u64 = normalized[..split]
        .parse()
        .with_context(|| format!("invalid bandwidth value {}", &normalized[..split]))?;
    let unit = normalized[split..].trim();

    const BYTE: u64 = 1;
    const KILOBYTE: u64 = BYTE * 1000;
    const MEGABYTE: u64 = KILOBYTE * 1000;
    const GIGABYTE: u64 = MEGABYTE * 1000;
    const TERABYTE: u64 = GIGABYTE * 1000;

    let bytes_per_second = match unit {
        "b" | "bps" => value.saturating_mul(BYTE) / 8,
        "k" | "kb" | "kbps" => value.saturating_mul(KILOBYTE) / 8,
        "m" | "mb" | "mbps" => value.saturating_mul(MEGABYTE) / 8,
        "g" | "gb" | "gbps" => value.saturating_mul(GIGABYTE) / 8,
        "t" | "tb" | "tbps" => value.saturating_mul(TERABYTE) / 8,
        _ => bail!("unsupported bandwidth unit"),
    };
    Ok(bytes_per_second)
}

fn validate_min_bandwidth(field: &str, value: u64) -> Result<()> {
    if value != 0 && value < 65_536 {
        bail!("{field} must be at least 65536 bytes per second");
    }
    Ok(())
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientConfig {
    pub server: String,
    #[serde(default)]
    pub auth: String,
    #[serde(default)]
    pub transport: ClientTransportConfig,
    #[serde(default)]
    pub obfs: ClientObfsConfig,
    #[serde(default)]
    pub tls: ClientTlsConfig,
    #[serde(default)]
    pub quic: ClientQuicConfig,
    #[serde(default)]
    pub bandwidth: BandwidthConfig,
    #[serde(default, rename = "fastOpen")]
    pub fast_open: bool,
    #[serde(default)]
    pub lazy: bool,
    #[serde(default)]
    pub socks5: Option<Socks5Config>,
    #[serde(default)]
    pub http: Option<HttpConfig>,
    #[serde(default, rename = "tcpForwarding")]
    pub tcp_forwarding: Vec<TcpForwardingEntry>,
    #[serde(default, rename = "udpForwarding")]
    pub udp_forwarding: Vec<UdpForwardingEntry>,
    #[serde(default, rename = "tcpTProxy")]
    pub tcp_tproxy: Option<TcpTProxyConfig>,
    #[serde(default, rename = "udpTProxy")]
    pub udp_tproxy: Option<UdpTProxyConfig>,
    #[serde(default, rename = "tcpRedirect")]
    pub tcp_redirect: Option<TcpRedirectConfig>,
    #[serde(default)]
    pub tun: Option<TunConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientTransportConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub udp: ClientTransportUdpConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientTransportUdpConfig {
    #[serde(default, with = "humantime_serde")]
    pub hop_interval: Duration,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientObfsConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub salamander: SalamanderConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct SalamanderConfig {
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientTlsConfig {
    #[serde(default)]
    pub sni: String,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default, rename = "pinSHA256")]
    pub pin_sha256: String,
    #[serde(default)]
    pub ca: String,
    #[serde(default, rename = "clientCertificate")]
    pub client_certificate: String,
    #[serde(default, rename = "clientKey")]
    pub client_key: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientQuicConfig {
    #[serde(default, rename = "initStreamReceiveWindow")]
    pub init_stream_receive_window: u64,
    #[serde(default, rename = "maxStreamReceiveWindow")]
    pub max_stream_receive_window: u64,
    #[serde(default, rename = "initConnReceiveWindow")]
    pub init_connection_receive_window: u64,
    #[serde(default, rename = "maxConnReceiveWindow")]
    pub max_connection_receive_window: u64,
    #[serde(default, with = "humantime_serde")]
    pub max_idle_timeout: Duration,
    #[serde(default, with = "humantime_serde")]
    pub keep_alive_period: Duration,
    #[serde(default, rename = "disablePathMTUDiscovery")]
    pub disable_path_mtu_discovery: bool,
    #[serde(default)]
    pub sockopts: ClientQuicSockoptsConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ClientQuicSockoptsConfig {
    #[serde(default, rename = "bindInterface")]
    pub bind_interface: Option<String>,
    #[serde(default, rename = "fwmark")]
    pub firewall_mark: Option<u32>,
    #[serde(default, rename = "fdControlUnixSocket")]
    pub fd_control_unix_socket: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct BandwidthConfig {
    #[serde(default)]
    pub up: String,
    #[serde(default)]
    pub down: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct Socks5Config {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default, rename = "disableUDP")]
    pub disable_udp: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct HttpConfig {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub realm: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TcpForwardingEntry {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub remote: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UdpForwardingEntry {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub remote: String,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TcpTProxyConfig {
    #[serde(default)]
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct UdpTProxyConfig {
    #[serde(default)]
    pub listen: String,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TcpRedirectConfig {
    #[serde(default)]
    pub listen: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TunConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub mtu: u32,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub address: TunAddressConfig,
    #[serde(default)]
    pub route: Option<TunRouteConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TunAddressConfig {
    #[serde(default)]
    pub ipv4: String,
    #[serde(default)]
    pub ipv6: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct TunRouteConfig {
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub ipv4: Vec<String>,
    #[serde(default)]
    pub ipv6: Vec<String>,
    #[serde(default, rename = "ipv4Exclude")]
    pub ipv4_exclude: Vec<String>,
    #[serde(default, rename = "ipv6Exclude")]
    pub ipv6_exclude: Vec<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerConfig {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub obfs: ServerObfsConfig,
    #[serde(default)]
    pub tls: Option<ServerTlsConfig>,
    #[serde(default)]
    pub acme: Option<ServerAcmeConfig>,
    #[serde(default)]
    pub quic: ServerQuicConfig,
    #[serde(default)]
    pub bandwidth: BandwidthConfig,
    #[serde(default, rename = "ignoreClientBandwidth")]
    pub ignore_client_bandwidth: bool,
    #[serde(default, rename = "speedTest")]
    pub speed_test: bool,
    #[serde(default, rename = "disableUDP")]
    pub disable_udp: bool,
    #[serde(default, rename = "udpIdleTimeout", with = "humantime_serde")]
    pub udp_idle_timeout: Duration,
    #[serde(default)]
    pub auth: ServerAuthConfig,
    #[serde(default)]
    pub resolver: ServerResolverConfig,
    #[serde(default)]
    pub sniff: ServerSniffConfig,
    #[serde(default)]
    pub acl: ServerAclConfig,
    #[serde(default)]
    pub outbounds: Vec<ServerOutboundEntry>,
    #[serde(default, rename = "trafficStats")]
    pub traffic_stats: ServerTrafficStatsConfig,
    #[serde(default)]
    pub masquerade: ServerMasqueradeConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerObfsConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub salamander: SalamanderConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerTlsConfig {
    #[serde(default)]
    pub cert: String,
    #[serde(default)]
    pub key: String,
    #[serde(default, rename = "sniGuard")]
    pub sni_guard: String,
    #[serde(default, rename = "clientCA")]
    pub client_ca: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAcmeConfig {
    #[serde(default)]
    pub domains: Vec<String>,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub ca: String,
    #[serde(default, rename = "listenHost")]
    pub listen_host: String,
    #[serde(default)]
    pub dir: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub http: ServerAcmeHttpConfig,
    #[serde(default)]
    pub tls: ServerAcmeTlsConfig,
    #[serde(default)]
    pub dns: ServerAcmeDnsConfig,
    #[serde(default, rename = "disableHTTP")]
    pub disable_http: bool,
    #[serde(default, rename = "disableTLSALPN")]
    pub disable_tlsalpn: bool,
    #[serde(default, rename = "altHTTPPort")]
    pub alt_http_port: i32,
    #[serde(default, rename = "altTLSALPNPort")]
    pub alt_tlsalpn_port: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAcmeHttpConfig {
    #[serde(default, rename = "altPort")]
    pub alt_port: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAcmeTlsConfig {
    #[serde(default, rename = "altPort")]
    pub alt_port: i32,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAcmeDnsConfig {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub config: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerQuicConfig {
    #[serde(default, rename = "initStreamReceiveWindow")]
    pub init_stream_receive_window: u64,
    #[serde(default, rename = "maxStreamReceiveWindow")]
    pub max_stream_receive_window: u64,
    #[serde(default, rename = "initConnReceiveWindow")]
    pub init_connection_receive_window: u64,
    #[serde(default, rename = "maxConnReceiveWindow")]
    pub max_connection_receive_window: u64,
    #[serde(default, with = "humantime_serde")]
    pub max_idle_timeout: Duration,
    #[serde(default, rename = "maxIncomingStreams")]
    pub max_incoming_streams: i64,
    #[serde(default, rename = "disablePathMTUDiscovery")]
    pub disable_path_mtu_discovery: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAuthConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub password: String,
    #[serde(default)]
    pub userpass: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub http: ServerAuthHttpConfig,
    #[serde(default)]
    pub command: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAuthHttpConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResolverEndpointConfig {
    #[serde(default)]
    pub addr: String,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ResolverTlsEndpointConfig {
    #[serde(default)]
    pub addr: String,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default)]
    pub sni: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerResolverConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub tcp: ResolverEndpointConfig,
    #[serde(default)]
    pub udp: ResolverEndpointConfig,
    #[serde(default)]
    pub tls: ResolverTlsEndpointConfig,
    #[serde(default)]
    pub https: ResolverTlsEndpointConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerSniffConfig {
    #[serde(default)]
    pub enable: bool,
    #[serde(default, with = "humantime_serde")]
    pub timeout: Duration,
    #[serde(default, rename = "rewriteDomain")]
    pub rewrite_domain: bool,
    #[serde(default, rename = "tcpPorts")]
    pub tcp_ports: String,
    #[serde(default, rename = "udpPorts")]
    pub udp_ports: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerAclConfig {
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub inline: Vec<String>,
    #[serde(default)]
    pub geoip: String,
    #[serde(default)]
    pub geosite: String,
    #[serde(default, rename = "geoUpdateInterval", with = "humantime_serde")]
    pub geo_update_interval: Duration,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerOutboundEntry {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub direct: ServerOutboundDirectConfig,
    #[serde(default)]
    pub socks5: ServerOutboundSocks5Config,
    #[serde(default)]
    pub http: ServerOutboundHttpConfig,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerOutboundDirectConfig {
    #[serde(default)]
    pub mode: String,
    #[serde(default, rename = "bindIPv4")]
    pub bind_ipv4: String,
    #[serde(default, rename = "bindIPv6")]
    pub bind_ipv6: String,
    #[serde(default, rename = "bindDevice")]
    pub bind_device: String,
    #[serde(default, rename = "fastOpen")]
    pub fast_open: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerOutboundSocks5Config {
    #[serde(default)]
    pub addr: String,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub password: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerOutboundHttpConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerTrafficStatsConfig {
    #[serde(default)]
    pub listen: String,
    #[serde(default)]
    pub secret: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerMasqueradeConfig {
    #[serde(default)]
    pub r#type: String,
    #[serde(default)]
    pub file: ServerMasqueradeFileConfig,
    #[serde(default)]
    pub proxy: ServerMasqueradeProxyConfig,
    #[serde(default)]
    pub string: ServerMasqueradeStringConfig,
    #[serde(default, rename = "listenHTTP")]
    pub listen_http: String,
    #[serde(default, rename = "listenHTTPS")]
    pub listen_https: String,
    #[serde(default, rename = "forceHTTPS")]
    pub force_https: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerMasqueradeFileConfig {
    #[serde(default)]
    pub dir: String,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerMasqueradeProxyConfig {
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "rewriteHost")]
    pub rewrite_host: bool,
    #[serde(default)]
    pub insecure: bool,
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ServerMasqueradeStringConfig {
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub headers: std::collections::BTreeMap<String, String>,
    #[serde(default, rename = "statusCode")]
    pub status_code: i32,
}

impl<T: fmt::Debug> fmt::Display for LoadedConfig<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} => {:?}", self.path.display(), self.value)
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use super::*;

    #[test]
    fn explicit_path_wins() {
        let tempdir = tempfile::tempdir().unwrap();
        let config_path = tempdir.path().join("config.yaml");
        fs::write(&config_path, "server: example.com:443\nauth: test\n").unwrap();
        let loaded = load_client_config(Some(&config_path)).unwrap();
        assert_eq!(loaded.path, config_path);
        assert_eq!(loaded.value.server, "example.com:443");
    }

    #[test]
    fn resolve_config_path_finds_local_file() {
        let tempdir = tempfile::tempdir().unwrap();
        let previous_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(tempdir.path()).unwrap();
        let path = PathBuf::from("config.yaml");
        fs::write(&path, "listen: :443\n").unwrap();
        let resolved = resolve_config_path(None).unwrap();
        assert!(resolved.ends_with("config.yaml"));
        std::env::set_current_dir(previous_dir).unwrap();
    }

    #[test]
    fn parse_bandwidth_matches_go_semantics() {
        assert_eq!(parse_bandwidth("").unwrap(), 0);
        assert_eq!(parse_bandwidth("100 Mbps").unwrap(), 12_500_000);
        assert_eq!(parse_bandwidth("512 kbps").unwrap(), 64_000);
        assert_eq!(parse_bandwidth("1g").unwrap(), 125_000_000);
        assert!(parse_bandwidth("100").is_err());
    }

    #[test]
    fn build_runnable_client_config_rejects_missing_mode() {
        let config = ClientConfig {
            server: "127.0.0.1:443".into(),
            tls: ClientTlsConfig {
                insecure: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let err = build_runnable_client_config(&config)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no client mode specified"));
    }

    #[test]
    fn build_client_core_config_allows_missing_mode() {
        let config = ClientConfig {
            server: "127.0.0.1:443".into(),
            tls: ClientTlsConfig {
                insecure: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let runtime = build_client_core_config(&config).unwrap();
        assert_eq!(runtime.server_addr, "127.0.0.1:443".parse().unwrap());
        assert!(runtime.tls.insecure);
        assert_eq!(
            runtime.quic.stream_receive_window,
            core::DEFAULT_STREAM_RECEIVE_WINDOW
        );
        assert_eq!(
            runtime.quic.receive_window,
            core::DEFAULT_CONNECTION_RECEIVE_WINDOW
        );
        assert_eq!(
            runtime.quic.keep_alive_interval,
            Some(core::DEFAULT_KEEP_ALIVE_PERIOD)
        );
        assert_eq!(
            runtime.quic.max_idle_timeout,
            core::DEFAULT_MAX_IDLE_TIMEOUT
        );
    }

    #[test]
    fn build_client_quic_transport_prefers_larger_explicit_windows() {
        let quic = build_client_quic_transport(&ClientQuicConfig {
            init_stream_receive_window: 128 * 1024,
            max_stream_receive_window: 256 * 1024,
            init_connection_receive_window: 512 * 1024,
            max_connection_receive_window: 1024 * 1024,
            max_idle_timeout: Duration::from_secs(45),
            keep_alive_period: Duration::from_secs(15),
            disable_path_mtu_discovery: true,
            ..Default::default()
        })
        .unwrap();

        assert_eq!(quic.stream_receive_window, 256 * 1024);
        assert_eq!(quic.receive_window, 1024 * 1024);
        assert_eq!(quic.max_idle_timeout, Duration::from_secs(45));
        assert_eq!(quic.keep_alive_interval, Some(Duration::from_secs(15)));
        assert!(quic.disable_path_mtu_discovery);
    }

    #[test]
    fn build_server_quic_transport_uses_go_defaults() {
        let quic = build_server_quic_transport(&ServerQuicConfig::default()).unwrap();
        assert_eq!(
            quic.stream_receive_window,
            core::DEFAULT_STREAM_RECEIVE_WINDOW
        );
        assert_eq!(quic.receive_window, core::DEFAULT_CONNECTION_RECEIVE_WINDOW);
        assert_eq!(quic.max_idle_timeout, core::DEFAULT_MAX_IDLE_TIMEOUT);
        assert_eq!(
            quic.max_concurrent_bidi_streams,
            Some(core::DEFAULT_MAX_INCOMING_STREAMS)
        );
    }

    #[test]
    fn normalize_client_config_parses_hy2_uri() {
        let config = ClientConfig {
            server: "hy2://john:wick@demo.example.com:4443/?insecure=1&obfs=salamander&obfs-password=66ccff&pinSHA256=DEAD:BEEF&sni=crap.cc".into(),
            ..Default::default()
        };

        let normalized = normalize_client_config(&config).unwrap();
        assert_eq!(normalized.server, "demo.example.com:4443");
        assert_eq!(normalized.auth, "john:wick");
        assert_eq!(normalized.obfs.r#type, "salamander");
        assert_eq!(normalized.obfs.salamander.password, "66ccff");
        assert_eq!(normalized.tls.sni, "crap.cc");
        assert!(normalized.tls.insecure);
        assert_eq!(normalized.tls.pin_sha256, "DEAD:BEEF");
    }

    #[test]
    fn normalize_client_config_decodes_special_auth_characters_from_hy2_uri() {
        let config = ClientConfig {
            server: "hy2://john:doe%3Ap%40ss%2Fword%3F@example.com:443/".into(),
            ..Default::default()
        };

        let normalized = normalize_client_config(&config).unwrap();
        assert_eq!(normalized.server, "example.com:443");
        assert_eq!(normalized.auth, "john:doe:p@ss/word?");
    }

    #[test]
    fn normalize_client_config_preserves_plus_in_auth() {
        let config = ClientConfig {
            server: "hy2://john:pa+ss@example.com:443/".into(),
            ..Default::default()
        };

        let normalized = normalize_client_config(&config).unwrap();
        assert_eq!(normalized.auth, "john:pa+ss");
    }

    #[test]
    fn parse_optional_pinned_sha256_accepts_normalized_hex() {
        let hash = parse_optional_pinned_sha256("AA:BB-cc")
            .unwrap_err()
            .to_string();
        assert!(hash.contains("64 hex characters"));

        let parsed = parse_optional_pinned_sha256(
            "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        )
        .unwrap()
        .unwrap();
        assert_eq!(parsed[0], 0x01);
        assert_eq!(parsed[31], 0xef);
    }
}
