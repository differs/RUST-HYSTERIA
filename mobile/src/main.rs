mod android_bridge;
mod dns_proxy;
mod local_socks;
mod vpn_tun2socks;

use std::{
    fs::File,
    io::{BufRead, BufReader},
    net::{IpAddr, Ipv6Addr, SocketAddr, ToSocketAddrs},
    path::Path,
    sync::{
        Arc, Mutex, OnceLock,
        mpsc::{self, Receiver, Sender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use crate::dns_proxy::{DnsFailureNotifier, DnsProxy, DotUpstream};
use dioxus::prelude::*;
use futures_timer::Delay;
use hysteria_core::{
    Client, ClientConfig as CoreClientConfig, ClientTlsConfig, DEFAULT_KEEP_ALIVE_PERIOD,
    DEFAULT_MAX_IDLE_TIMEOUT, ObfsConfig, QuicTransportConfig, TransportSnapshot,
    run_client_health_check,
};
use hysteria_extras::speedtest::{Client as SpeedtestClient, SPEEDTEST_ADDR};
use crate::android_bridge::CaCatalog;
use local_socks::{FatalConnectionNotifier, LocalSocksConfig, serve_socks5};
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use url::Url;
use vpn_tun2socks::{Tun2SocksConfig, Tun2SocksHandle, Tun2SocksUdpMode};

#[cfg(target_os = "android")]
use std::io::Cursor;

#[cfg(target_os = "android")]
use jni::{
    Env, EnvUnowned,
    objects::{JClass, JObject, JString},
    sys::jboolean,
};

const LARGE_STREAM_WINDOW: u64 = 268_435_456;
const LARGE_CONN_WINDOW: u64 = 536_870_912;
const DEFAULT_TEST_DURATION: Duration = Duration::from_secs(8);
const MAX_LOG_LINES: usize = 200;
const LOCAL_SOCKS_HOST: &str = "127.0.0.1";
const LOCAL_SOCKS_PORT: u16 = 1080;
const VPN_TUN_NAME: &str = "hy0";
const VPN_TUN_MTU: u16 = 1500;
const VPN_TUN_IPV4_ADDR: &str = "10.8.0.2";
const VPN_TUN_CONNECT_TIMEOUT_MS: u32 = 15_000;
const VPN_DNS_SERVER_IP: &str = "1.1.1.1";
const VPN_DNS_DOT_PORT: u16 = 853;
const VPN_DNS_PROXY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(15);
const HEALTH_PROBE_INTERVAL: Duration = Duration::from_secs(15);
const HEALTH_PROBE_FAILURE_THRESHOLD: u32 = 2;
static RUNTIME_CONTROLLER: OnceLock<RuntimeController> = OnceLock::new();
static ANDROID_RUNTIME_INIT: OnceLock<()> = OnceLock::new();

fn main() {
    ensure_android_runtime_initialized();
    dioxus::launch(App);
}

fn ensure_android_runtime_initialized() {
    ANDROID_RUNTIME_INIT.get_or_init(|| {
        android_bridge::install_socket_protector();
    });
}

fn managed_vpn_tun_config(local_udp_enabled: bool) -> Tun2SocksConfig {
    // Managed Android DNS now uses a real public resolver IP so Android does not
    // depend on a synthetic mapdns endpoint. The local SOCKS/DNS path still
    // intercepts that destination and resolves over tunneled DoT.
    Tun2SocksConfig {
        socks_host: LOCAL_SOCKS_HOST.to_string(),
        socks_port: LOCAL_SOCKS_PORT,
        udp_mode: if local_udp_enabled {
            Tun2SocksUdpMode::Udp
        } else {
            Tun2SocksUdpMode::Tcp
        },
        tunnel_name: VPN_TUN_NAME.to_string(),
        mtu: VPN_TUN_MTU,
        ipv4_addr: VPN_TUN_IPV4_ADDR.to_string(),
        // The current managed Hysteria nodes do not guarantee IPv6 egress.
        // Advertising an IPv6 TUN makes Android apps prefer AAAA destinations
        // that the remote side cannot dial, which surfaces as ERR_CONNECTION_RESET.
        ipv6_addr: None,
        connect_timeout_ms: Some(VPN_TUN_CONNECT_TIMEOUT_MS),
    }
}

#[cfg(test)]
fn managed_vpn_dns_servers() -> [&'static str; 1] {
    [VPN_DNS_SERVER_IP]
}

fn managed_vpn_dot_upstreams() -> Vec<DotUpstream> {
    vec![
        DotUpstream {
            address: format!("1.1.1.1:{VPN_DNS_DOT_PORT}"),
            server_name: "cloudflare-dns.com".to_string(),
        },
        DotUpstream {
            address: format!("8.8.8.8:{VPN_DNS_DOT_PORT}"),
            server_name: "dns.google".to_string(),
        },
    ]
}

fn local_socks_udp_disabled(form: &FormState) -> bool {
    !form.local_udp_enabled
}

fn should_run_health_probe(phase: &str) -> bool {
    phase == "Connected"
}

fn should_preserve_tun2socks_on_connection_loss(desired_vpn_active: bool) -> bool {
    desired_vpn_active
}

fn should_restart_tun2socks_after_reconnect(
    desired_vpn_active: bool,
    tun2socks_running: bool,
) -> bool {
    desired_vpn_active && !tun2socks_running
}

fn build_local_socks_config(
    listen: String,
    dns_proxy: Option<Arc<DnsProxy>>,
    form: &FormState,
) -> LocalSocksConfig {
    LocalSocksConfig {
        listen,
        username: String::new(),
        password: String::new(),
        disable_udp: local_socks_udp_disabled(form),
        dns_proxy,
    }
}

fn build_managed_vpn_dns_proxy(
    client: Client,
    failure_notifier: Option<DnsFailureNotifier>,
) -> Result<Arc<DnsProxy>> {
    let root_certificates = load_system_root_certificates()?;
    Ok(Arc::new(DnsProxy::new(
        client,
        VPN_DNS_SERVER_IP,
        root_certificates,
        managed_vpn_dot_upstreams(),
        VPN_DNS_PROXY_TIMEOUT,
        failure_notifier,
    )?))
}

#[derive(Clone, Debug, PartialEq)]
struct FormState {
    import_uri: String,
    server: String,
    auth: String,
    obfs_password: String,
    sni: String,
    ca_path: String,
    pin_sha256: String,
    bandwidth_up: String,
    bandwidth_down: String,
    quic_init_stream_receive_window: String,
    quic_max_stream_receive_window: String,
    quic_init_connection_receive_window: String,
    quic_max_connection_receive_window: String,
    quic_max_idle_timeout: String,
    quic_keep_alive_period: String,
    local_udp_enabled: bool,
    quic_disable_path_mtu_discovery: bool,
    insecure_tls: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LaunchAutomation {
    auto_connect: bool,
    auto_request_vpn: bool,
    auto_start_vpn: bool,
}

impl Default for FormState {
    fn default() -> Self {
        Self {
            import_uri: String::new(),
            server: String::new(),
            auth: String::new(),
            obfs_password: String::new(),
            sni: String::new(),
            ca_path: String::new(),
            pin_sha256: String::new(),
            bandwidth_up: String::new(),
            bandwidth_down: String::new(),
            quic_init_stream_receive_window: String::new(),
            quic_max_stream_receive_window: String::new(),
            quic_init_connection_receive_window: String::new(),
            quic_max_connection_receive_window: String::new(),
            quic_max_idle_timeout: String::new(),
            quic_keep_alive_period: String::new(),
            local_udp_enabled: true,
            quic_disable_path_mtu_discovery: false,
            insecure_tls: false,
        }
    }
}

fn initial_form_state() -> FormState {
    let mut form = android_bridge::query_saved_profile()
        .ok()
        .flatten()
        .unwrap_or_default();
    match android_bridge::query_launch_config() {
        Ok(launch) => {
            eprintln!(
                "initial_form_state launch config server={:?} auth_present={} obfs_present={} insecure_tls={:?}",
                launch.server,
                launch
                    .auth
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty()),
                launch
                    .obfs_password
                    .as_ref()
                    .is_some_and(|value| !value.trim().is_empty()),
                launch.insecure_tls,
            );
            if let Some(server) = launch.server.filter(|value| !value.trim().is_empty()) {
                form.server = server;
            }
            if let Some(auth) = launch.auth.filter(|value| !value.trim().is_empty()) {
                form.auth = auth;
            }
            if let Some(obfs_password) = launch
                .obfs_password
                .filter(|value| !value.trim().is_empty())
            {
                form.obfs_password = obfs_password;
            }
            if let Some(sni) = launch.sni.filter(|value| !value.trim().is_empty()) {
                form.sni = sni;
            }
            if let Some(ca_path) = launch.ca_path.filter(|value| !value.trim().is_empty()) {
                form.ca_path = ca_path;
            }
            if let Some(pin_sha256) = launch.pin_sha256.filter(|value| !value.trim().is_empty()) {
                form.pin_sha256 = pin_sha256;
            }
            if let Some(bandwidth_up) = launch.bandwidth_up.filter(|value| !value.trim().is_empty())
            {
                form.bandwidth_up = bandwidth_up;
            }
            if let Some(bandwidth_down) = launch
                .bandwidth_down
                .filter(|value| !value.trim().is_empty())
            {
                form.bandwidth_down = bandwidth_down;
            }
            if let Some(value) = launch
                .quic_init_stream_receive_window
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_init_stream_receive_window = value;
            }
            if let Some(value) = launch
                .quic_max_stream_receive_window
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_max_stream_receive_window = value;
            }
            if let Some(value) = launch
                .quic_init_connection_receive_window
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_init_connection_receive_window = value;
            }
            if let Some(value) = launch
                .quic_max_connection_receive_window
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_max_connection_receive_window = value;
            }
            if let Some(value) = launch
                .quic_max_idle_timeout
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_max_idle_timeout = value;
            }
            if let Some(value) = launch
                .quic_keep_alive_period
                .filter(|value| !value.trim().is_empty())
            {
                form.quic_keep_alive_period = value;
            }
            if let Some(value) = launch.local_udp_enabled {
                form.local_udp_enabled = value;
            }
            if let Some(value) = launch.quic_disable_path_mtu_discovery {
                form.quic_disable_path_mtu_discovery = value;
            }
            if let Some(insecure_tls) = launch.insecure_tls {
                form.insecure_tls = insecure_tls;
            }
        }
        Err(err) => {
            eprintln!("initial_form_state launch config query failed: {err:#}");
        }
    }
    form
}

fn initial_launch_automation() -> LaunchAutomation {
    match android_bridge::query_launch_config() {
        Ok(launch) => LaunchAutomation {
            auto_connect: launch.auto_connect.unwrap_or(false),
            auto_request_vpn: launch.auto_request_vpn.unwrap_or(false),
            auto_start_vpn: launch.auto_start_vpn.unwrap_or(false),
        },
        Err(err) => {
            eprintln!("initial_launch_automation query failed: {err:#}");
            LaunchAutomation::default()
        }
    }
}

fn describe_prefill(form: &FormState) -> String {
    format!(
        "launch prefill: server={} auth={} obfs={} sni={} ca={} pin={} bandwidth.up={} bandwidth.down={} quic.stream={} quic.conn={} idle={} keepAlive={} udpLocal={} pmtudOff={} insecure_tls={}",
        if form.server.trim().is_empty() {
            "<empty>"
        } else {
            form.server.trim()
        },
        if form.auth.trim().is_empty() {
            "<empty>"
        } else {
            "<set>"
        },
        if form.obfs_password.trim().is_empty() {
            "<empty>"
        } else {
            "<set>"
        },
        if form.sni.trim().is_empty() {
            "<empty>"
        } else {
            form.sni.trim()
        },
        if form.ca_path.trim().is_empty() {
            "<empty>"
        } else {
            form.ca_path.trim()
        },
        if form.pin_sha256.trim().is_empty() {
            "<empty>"
        } else {
            "<set>"
        },
        if form.bandwidth_up.trim().is_empty() {
            "<empty>"
        } else {
            form.bandwidth_up.trim()
        },
        if form.bandwidth_down.trim().is_empty() {
            "<empty>"
        } else {
            form.bandwidth_down.trim()
        },
        if form.quic_init_stream_receive_window.trim().is_empty()
            && form.quic_max_stream_receive_window.trim().is_empty()
        {
            "<empty>".to_string()
        } else {
            format!(
                "{}/{}",
                if form.quic_init_stream_receive_window.trim().is_empty() {
                    "-"
                } else {
                    form.quic_init_stream_receive_window.trim()
                },
                if form.quic_max_stream_receive_window.trim().is_empty() {
                    "-"
                } else {
                    form.quic_max_stream_receive_window.trim()
                }
            )
        },
        if form.quic_init_connection_receive_window.trim().is_empty()
            && form.quic_max_connection_receive_window.trim().is_empty()
        {
            "<empty>".to_string()
        } else {
            format!(
                "{}/{}",
                if form.quic_init_connection_receive_window.trim().is_empty() {
                    "-"
                } else {
                    form.quic_init_connection_receive_window.trim()
                },
                if form.quic_max_connection_receive_window.trim().is_empty() {
                    "-"
                } else {
                    form.quic_max_connection_receive_window.trim()
                }
            )
        },
        if form.quic_max_idle_timeout.trim().is_empty() {
            "<empty>"
        } else {
            form.quic_max_idle_timeout.trim()
        },
        if form.quic_keep_alive_period.trim().is_empty() {
            "<empty>"
        } else {
            form.quic_keep_alive_period.trim()
        },
        form.local_udp_enabled,
        form.quic_disable_path_mtu_discovery,
        form.insecure_tls,
    )
}

fn describe_launch_automation(automation: LaunchAutomation) -> String {
    format!(
        "launch automation: auto_connect={} auto_request_vpn={} auto_start_vpn={}",
        automation.auto_connect, automation.auto_request_vpn, automation.auto_start_vpn,
    )
}

fn has_meaningful_form_prefill(form: &FormState) -> bool {
    !form.server.trim().is_empty()
        || !form.auth.trim().is_empty()
        || !form.obfs_password.trim().is_empty()
        || !form.sni.trim().is_empty()
        || !form.ca_path.trim().is_empty()
        || !form.pin_sha256.trim().is_empty()
        || !form.bandwidth_up.trim().is_empty()
        || !form.bandwidth_down.trim().is_empty()
        || !form.quic_init_stream_receive_window.trim().is_empty()
        || !form.quic_max_stream_receive_window.trim().is_empty()
        || !form.quic_init_connection_receive_window.trim().is_empty()
        || !form.quic_max_connection_receive_window.trim().is_empty()
        || !form.quic_max_idle_timeout.trim().is_empty()
        || !form.quic_keep_alive_period.trim().is_empty()
        || !form.local_udp_enabled
        || form.quic_disable_path_mtu_discovery
        || form.insecure_tls
}

fn has_launch_automation(automation: LaunchAutomation) -> bool {
    automation.auto_connect || automation.auto_request_vpn || automation.auto_start_vpn
}

fn has_required_connection_fields(form: &FormState) -> bool {
    normalize_form(form.clone()).is_ok()
}

fn config_value_or_empty(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "<empty>".to_string()
    } else {
        trimmed.to_string()
    }
}

fn config_presence(value: &str) -> &'static str {
    if value.trim().is_empty() {
        "<empty>"
    } else {
        "<set>"
    }
}

fn current_trust_label(
    form: &FormState,
    ca_catalog: &CaCatalog,
    imported_cert_name: &str,
) -> String {
    let mut parts = Vec::new();

    if !form.ca_path.trim().is_empty() {
        let explicit_label = if !imported_cert_name.trim().is_empty() {
            imported_cert_name.trim().to_string()
        } else if let Some(file) = ca_catalog
            .files
            .iter()
            .find(|file| file.path == form.ca_path)
        {
            file.name.clone()
        } else {
            Path::new(form.ca_path.trim())
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or(form.ca_path.trim())
                .to_string()
        };
        parts.push(explicit_label);
    } else if form.insecure_tls {
        parts.push("TLS insecure".to_string());
    } else {
        parts.push("System trust store".to_string());
    }

    if !form.pin_sha256.trim().is_empty() {
        parts.push("pinSHA256".to_string());
    }

    parts.join(" + ")
}

type TransportSample = TransportSnapshot;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct DashboardMetrics {
    latency: Option<Duration>,
    tx_total_bytes: u64,
    rx_total_bytes: u64,
    tx_rate_bytes: u64,
    rx_rate_bytes: u64,
}

impl DashboardMetrics {
}

fn derive_dashboard_metrics(
    previous: Option<TransportSample>,
    current: TransportSample,
    interval: Duration,
) -> DashboardMetrics {
    let seconds = interval.as_secs_f64();
    let (tx_rate_bytes, rx_rate_bytes) = if seconds <= f64::EPSILON {
        (0, 0)
    } else if let Some(previous) = previous {
        (
            ((current.tx_bytes.saturating_sub(previous.tx_bytes)) as f64 / seconds).round() as u64,
            ((current.rx_bytes.saturating_sub(previous.rx_bytes)) as f64 / seconds).round() as u64,
        )
    } else {
        (0, 0)
    };

    DashboardMetrics {
        latency: Some(current.rtt),
        tx_total_bytes: current.tx_bytes,
        rx_total_bytes: current.rx_bytes,
        tx_rate_bytes,
        rx_rate_bytes,
    }
}

fn apply_dashboard_metrics(status: &mut UiStatus, metrics: &DashboardMetrics) {
    status.latency = metrics.latency;
    status.tx_total_bytes = metrics.tx_total_bytes;
    status.rx_total_bytes = metrics.rx_total_bytes;
    status.tx_rate_bytes = metrics.tx_rate_bytes;
    status.rx_rate_bytes = metrics.rx_rate_bytes;
}

fn apply_settings_import(current: &FormState, input: &str) -> Result<(FormState, Option<String>)> {
    let trimmed = input.trim();
    let (mut imported, warning) = parse_imported_client_document(trimmed)?;
    if imported.ca_path.trim().is_empty() && !current.ca_path.trim().is_empty() {
        imported.ca_path = current.ca_path.clone();
    }
    if trimmed.starts_with("hy2://") || trimmed.starts_with("hysteria2://") {
        imported.import_uri = trimmed.to_string();
    }
    Ok((imported, warning))
}

fn show_udp_toggle_in_primary_controls() -> bool {
    true
}

fn show_expert_transport_toggles(show_advanced_fields: bool) -> bool {
    show_advanced_fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::android_bridge::{CaCatalog, CaFile};

    fn sample_form() -> FormState {
        FormState {
            import_uri: String::new(),
            server: String::new(),
            auth: String::new(),
            obfs_password: String::new(),
            sni: String::new(),
            ca_path: String::new(),
            pin_sha256: String::new(),
            bandwidth_up: String::new(),
            bandwidth_down: String::new(),
            quic_init_stream_receive_window: String::new(),
            quic_max_stream_receive_window: String::new(),
            quic_init_connection_receive_window: String::new(),
            quic_max_connection_receive_window: String::new(),
            quic_max_idle_timeout: String::new(),
            quic_keep_alive_period: String::new(),
            local_udp_enabled: true,
            quic_disable_path_mtu_discovery: false,
            insecure_tls: false,
        }
    }

    #[test]
    fn current_trust_label_prefers_catalog_name_for_explicit_ca_path() {
        let mut form = sample_form();
        form.ca_path = "/tmp/certs/custom.pem".to_string();

        let catalog = CaCatalog {
            directory: "/tmp/certs".to_string(),
            files: vec![CaFile {
                name: "custom.pem".to_string(),
                path: form.ca_path.clone(),
            }],
        };

        assert_eq!(current_trust_label(&form, &catalog, ""), "custom.pem");
    }

    #[test]
    fn current_trust_label_uses_system_trust_store_when_no_overrides_are_set() {
        let form = sample_form();
        let catalog = CaCatalog::default();

        assert_eq!(
            current_trust_label(&form, &catalog, ""),
            "System trust store"
        );
    }

    #[test]
    fn dashboard_derives_live_rates_and_totals_from_transport_samples() {
        let metrics = derive_dashboard_metrics(
            None,
            TransportSample {
                rtt: Duration::from_millis(84),
                tx_bytes: 2_048,
                rx_bytes: 4_096,
            },
            Duration::from_secs(1),
        );
        assert_eq!(format_latency(metrics.latency), "84 ms");
        assert_eq!(format_bytes_per_second(metrics.tx_rate_bytes), "0 B/s");
        assert_eq!(format_bytes_per_second(metrics.rx_rate_bytes), "0 B/s");
        assert_eq!(format_total_bytes(metrics.tx_total_bytes), "2.0 KB");
        assert_eq!(format_total_bytes(metrics.rx_total_bytes), "4.0 KB");

        let metrics = derive_dashboard_metrics(
            Some(TransportSample {
                rtt: Duration::from_millis(84),
                tx_bytes: 2_048,
                rx_bytes: 4_096,
            }),
            TransportSample {
                rtt: Duration::from_millis(96),
                tx_bytes: 5_120,
                rx_bytes: 10_240,
            },
            Duration::from_secs(2),
        );
        assert_eq!(format_latency(metrics.latency), "96 ms");
        assert_eq!(format_bytes_per_second(metrics.tx_rate_bytes), "1.5 KB/s");
        assert_eq!(format_bytes_per_second(metrics.rx_rate_bytes), "3.0 KB/s");
        assert_eq!(format_total_bytes(metrics.tx_total_bytes), "5.0 KB");
        assert_eq!(format_total_bytes(metrics.rx_total_bytes), "10.0 KB");
    }

    #[test]
    fn settings_import_accepts_hysteria2_uri_and_populates_draft() {
        let current = sample_form();
        let (imported, warning) = apply_settings_import(
            &current,
            "hysteria2://f36992567ace0abe30188df0cab4b0e9@hi.wedevs.org:12443/?obfs=salamander&obfs-password=ca784b9deaec1b7e9aad52ff8113f875&sni=hi.wedevs.org",
        )
        .expect("settings import should succeed");

        assert!(warning.is_none());
        assert_eq!(imported.server, "hi.wedevs.org:12443");
        assert_eq!(imported.auth, "f36992567ace0abe30188df0cab4b0e9");
        assert_eq!(imported.obfs_password, "ca784b9deaec1b7e9aad52ff8113f875");
        assert_eq!(imported.sni, "hi.wedevs.org");
        assert_eq!(
            imported.import_uri,
            "hysteria2://f36992567ace0abe30188df0cab4b0e9@hi.wedevs.org:12443/?obfs=salamander&obfs-password=ca784b9deaec1b7e9aad52ff8113f875&sni=hi.wedevs.org"
        );
    }

    #[test]
    fn settings_import_preserves_existing_ca_path_when_share_uri_has_no_ca() {
        let mut current = sample_form();
        current.ca_path = "/data/user/0/io.hysteria.mobile/files/certs/custom.pem".to_string();

        let (imported, warning) = apply_settings_import(
            &current,
            "hysteria2://token@example.com:443/?sni=example.com",
        )
        .expect("settings import should succeed");

        assert!(warning.is_none());
        assert_eq!(imported.ca_path, current.ca_path);
    }

    #[test]
    fn system_root_loader_prefers_platform_loader_when_available() {
        let expected = vec![CertificateDer::from(vec![1_u8, 2, 3, 4])];

        let certs = load_system_root_certificates_with(
            Some(|| Ok(expected.clone())),
            || -> Result<Vec<CertificateDer<'static>>> {
                panic!("native loader should not run when platform certificates are available")
            },
        )
        .expect("platform certificates should satisfy system root loading");

        assert_eq!(certs, expected);
    }

    #[test]
    fn managed_vpn_tunnel_defaults_to_ipv4_only() {
        let config = managed_vpn_tun_config(true);

        assert_eq!(config.ipv6_addr, None);
    }

    #[test]
    fn managed_vpn_tunnel_uses_real_ip_dns_without_mapdns() {
        let yaml = managed_vpn_tun_config(true).render();

        assert!(!yaml.contains("\nmapdns:\n"));
    }

    #[test]
    fn managed_vpn_tunnel_raises_connect_timeout_for_store_flows() {
        let yaml = managed_vpn_tun_config(true).render();

        assert!(yaml.contains("\n  connect-timeout: 15000\n"));
    }

    #[test]
    fn managed_vpn_tunnel_enables_udp_relay() {
        let yaml = managed_vpn_tun_config(true).render();

        assert!(yaml.contains("\n  udp: 'udp'\n"));
    }

    #[test]
    fn managed_vpn_tunnel_can_disable_udp_relay() {
        let yaml = managed_vpn_tun_config(false).render();

        assert!(yaml.contains("\n  udp: 'tcp'\n"));
    }

    #[test]
    fn mobile_local_socks_keeps_udp_associate_enabled() {
        let config = build_local_socks_config("127.0.0.1:1080".to_string(), None, &sample_form());

        assert!(!config.disable_udp);
        assert!(config.dns_proxy.is_none());
    }

    #[test]
    fn mobile_local_socks_can_disable_udp_associate() {
        let mut form = sample_form();
        form.local_udp_enabled = false;

        let config = build_local_socks_config("127.0.0.1:1080".to_string(), None, &form);

        assert!(config.disable_udp);
        assert!(config.dns_proxy.is_none());
    }

    #[test]
    fn udp_toggle_stays_visible_even_when_expert_mode_is_off() {
        assert!(show_udp_toggle_in_primary_controls());
        assert!(!show_expert_transport_toggles(false));
        assert!(show_expert_transport_toggles(true));
    }

    #[test]
    fn note_dns_failure_increments_only_dns_failure_counter() {
        let mut metrics = UiMetrics::default();
        metrics.error_count = 2;
        metrics.import_count = 1;

        note_dns_failure(&mut metrics);
        note_dns_failure(&mut metrics);

        assert_eq!(metrics.dns_failure_count, 2);
        assert_eq!(metrics.error_count, 2);
        assert_eq!(metrics.import_count, 1);
    }

    #[test]
    fn summarize_protocol_reports_udp_ready_when_server_and_local_udp_are_enabled() {
        let status = UiStatus {
            remote: "1.2.3.4:443".to_string(),
            server_udp_supported: true,
            local_udp_enabled: true,
            udp_enabled: true,
            ..UiStatus::default()
        };

        assert_eq!(summarize_protocol(&status), "QUIC active / UDP relay ready");
    }

    #[test]
    fn summarize_protocol_reports_server_udp_unavailable_when_server_disables_udp() {
        let status = UiStatus {
            remote: "1.2.3.4:443".to_string(),
            server_udp_supported: false,
            local_udp_enabled: true,
            udp_enabled: false,
            ..UiStatus::default()
        };

        assert_eq!(
            summarize_protocol(&status),
            "QUIC active / Server UDP unavailable"
        );
    }

    #[test]
    fn summarize_protocol_reports_local_udp_disabled_when_client_blocks_udp() {
        let status = UiStatus {
            remote: "1.2.3.4:443".to_string(),
            server_udp_supported: true,
            local_udp_enabled: false,
            udp_enabled: false,
            ..UiStatus::default()
        };

        assert_eq!(
            summarize_protocol(&status),
            "QUIC active / UDP disabled locally"
        );
    }

    #[test]
    fn managed_vpn_advertises_real_dns_server() {
        assert_eq!(managed_vpn_dns_servers(), ["1.1.1.1"]);
    }

    #[test]
    fn managed_vpn_dns_proxy_matches_public_dns_server_ip() {
        let dns_server = managed_vpn_dns_servers()[0];

        assert_eq!(
            crate::dns_proxy::match_dns_proxy_destination(
                dns_server,
                &format!("{dns_server}:53")
            ),
            Some(crate::dns_proxy::DnsProxyTransport::Plain)
        );
        assert_eq!(
            crate::dns_proxy::match_dns_proxy_destination(
                dns_server,
                &format!("{dns_server}:853")
            ),
            Some(crate::dns_proxy::DnsProxyTransport::Dot)
        );
    }

    #[test]
    fn managed_vpn_uses_tls_dns_upstreams() {
        let upstreams = managed_vpn_dot_upstreams();

        assert_eq!(upstreams.len(), 2);
        assert_eq!(upstreams[0].address, "1.1.1.1:853");
        assert_eq!(upstreams[0].server_name, "cloudflare-dns.com");
        assert_eq!(upstreams[1].address, "8.8.8.8:853");
        assert_eq!(upstreams[1].server_name, "dns.google");
    }

    #[test]
    fn disabled_secondary_buttons_keep_readable_text_and_border() {
        let style = button_style_with_state(button_surface_style(true), true, false);

        assert!(style.contains("color: rgba(243,246,251,0.72);"));
        assert!(style.contains("border-color: rgba(255,255,255,0.12);"));
        assert!(!style.contains("opacity: 0.55;"));
    }

    #[test]
    fn status_line_layout_keeps_value_right_aligned_with_wrapping_room() {
        let stylesheet = ui_stylesheet();

        assert!(stylesheet.contains(".status-line-value{flex:1;min-width:0;text-align:right;overflow-wrap:anywhere;word-break:break-word}"));
        assert!(stylesheet.contains(".status-line-label{flex:0 0 112px;min-width:112px}"));
    }

    #[test]
    fn app_shell_uses_multicolor_gradient_background() {
        let style = app_shell_style(&UiPrefs::default());

        assert!(style.contains("--bg-accent-a:"));
        assert!(style.contains("--bg-accent-b:"));
        assert!(style.contains("--bg-accent-c:"));
        assert!(style.contains("radial-gradient(circle at top left, var(--bg-accent-a) 0%, transparent 42%)"));
    }

    #[test]
    fn health_probe_runs_only_for_connected_sessions() {
        assert!(!should_run_health_probe("Disconnected"));
        assert!(!should_run_health_probe("Connecting"));
        assert!(!should_run_health_probe("Reconnecting"));
        assert!(should_run_health_probe("Connected"));
    }

    #[test]
    fn managed_reconnect_preserves_running_tun2socks_runtime() {
        assert!(should_preserve_tun2socks_on_connection_loss(true));
        assert!(!should_restart_tun2socks_after_reconnect(true, true));
    }

    #[test]
    fn reconnect_starts_tun2socks_when_managed_vpn_shell_is_missing() {
        assert!(should_restart_tun2socks_after_reconnect(true, false));
        assert!(!should_restart_tun2socks_after_reconnect(false, false));
    }
}

fn button_style_with_state(base: impl AsRef<str>, secondary: bool, enabled: bool) -> String {
    let base = base.as_ref();
    if enabled {
        base.to_string()
    } else if secondary {
        format!(
            "{base}background: rgba(255,255,255,0.05); border-color: rgba(255,255,255,0.12); color: rgba(243,246,251,0.72); box-shadow: none; cursor: default;"
        )
    } else {
        format!(
            "{base}background: rgba(138,180,248,0.38); border-color: rgba(168,199,250,0.18); color: rgba(11,18,32,0.78); box-shadow: none; cursor: default;"
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UiStatus {
    phase: String,
    remote: String,
    detail: String,
    server_udp_supported: bool,
    local_udp_enabled: bool,
    udp_enabled: bool,
    negotiated_tx: u64,
    latency: Option<Duration>,
    tx_total_bytes: u64,
    rx_total_bytes: u64,
    tx_rate_bytes: u64,
    rx_rate_bytes: u64,
    local_socks: String,
    vpn_available: bool,
    vpn_permission_granted: bool,
    vpn_active: bool,
}

impl Default for UiStatus {
    fn default() -> Self {
        Self {
            phase: "Disconnected".to_string(),
            remote: String::new(),
            detail:
                "Import a Linux-compatible config or fill server/auth, then connect. Explicit CA is optional."
                    .to_string(),
            server_udp_supported: false,
            local_udp_enabled: true,
            udp_enabled: false,
            negotiated_tx: 0,
            latency: None,
            tx_total_bytes: 0,
            rx_total_bytes: 0,
            tx_rate_bytes: 0,
            rx_rate_bytes: 0,
            local_socks: format!("{LOCAL_SOCKS_HOST}:{LOCAL_SOCKS_PORT}"),
            vpn_available: false,
            vpn_permission_granted: false,
            vpn_active: false,
        }
    }
}

#[cfg_attr(not(target_os = "android"), allow(dead_code))]
#[derive(Clone, Debug)]
enum AppCommand {
    Connect(FormState),
    StartManagedVpn(FormState),
    ManagedConnect(FormState),
    Disconnect,
    Speedtest(SpeedDirection),
    RequestVpnPermission,
    StopVpnShell,
    ServiceStopped,
    ConnectionClosed { generation: u64, reason: String },
    Reconnect { generation: u64, attempt: u32 },
    TransportPulse { generation: u64, metrics: DashboardMetrics },
}

#[derive(Clone, Debug)]
enum AppEvent {
    Status(UiStatus),
    Transport(DashboardMetrics),
    Log(String),
    DnsFailure(String),
}

#[derive(Clone, Copy, Debug)]
enum SpeedDirection {
    Download,
    Upload,
}

impl SpeedDirection {
    fn label(self) -> &'static str {
        match self {
            Self::Download => "download",
            Self::Upload => "upload",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AppTab {
    #[default]
    Home,
    Nodes,
    Stats,
    Settings,
}

impl AppTab {
    fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Nodes => "Nodes",
            Self::Stats => "Stats",
            Self::Settings => "Settings",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum NodeFilter {
    #[default]
    All,
    Active,
    Saved,
    Imported,
}

impl NodeFilter {
    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Active => "Active",
            Self::Saved => "Saved",
            Self::Imported => "Imported",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AccentTone {
    #[default]
    Neutral,
    Accent,
    Positive,
    Warning,
    Danger,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum NodeCardKind {
    #[default]
    ActiveDraft,
    SavedProfile,
    ImportedShare,
    LiveSession,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct UiPrefs {
    compact_layout: bool,
    motion_enabled: bool,
    show_advanced_fields: bool,
}

impl UiPrefs {
    fn content_gap(&self) -> usize {
        if self.compact_layout { 14 } else { 18 }
    }

    fn section_gap(&self) -> usize {
        if self.compact_layout { 18 } else { 24 }
    }

    fn card_padding(&self) -> usize {
        if self.compact_layout { 16 } else { 20 }
    }
}

impl Default for UiPrefs {
    fn default() -> Self {
        Self {
            compact_layout: false,
            motion_enabled: true,
            show_advanced_fields: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct LogEntry {
    message: String,
    recorded_at: Instant,
}

impl LogEntry {
    fn new(message: String) -> Self {
        Self {
            message,
            recorded_at: Instant::now(),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct UiMetrics {
    connected_since: Option<Instant>,
    last_connected_at: Option<Instant>,
    successful_connections: u32,
    reconnect_count: u32,
    error_count: u32,
    dns_failure_count: u32,
    import_count: u32,
    latest_download: Option<String>,
    latest_upload: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedClientConfig {
    #[serde(default)]
    server: String,
    #[serde(default)]
    auth: String,
    #[serde(default)]
    obfs: ImportedObfsConfig,
    #[serde(default)]
    tls: ImportedTlsConfig,
    #[serde(default)]
    quic: ImportedQuicConfig,
    #[serde(default)]
    bandwidth: ImportedBandwidthConfig,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedObfsConfig {
    #[serde(default)]
    r#type: String,
    #[serde(default)]
    salamander: ImportedSalamanderConfig,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedSalamanderConfig {
    #[serde(default)]
    password: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedTlsConfig {
    #[serde(default)]
    sni: String,
    #[serde(default)]
    insecure: bool,
    #[serde(default, rename = "pinSHA256")]
    pin_sha256: String,
    #[serde(default)]
    ca: String,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedQuicConfig {
    #[serde(default, rename = "initStreamReceiveWindow")]
    init_stream_receive_window: u64,
    #[serde(default, rename = "maxStreamReceiveWindow")]
    max_stream_receive_window: u64,
    #[serde(default, rename = "initConnReceiveWindow")]
    init_connection_receive_window: u64,
    #[serde(default, rename = "maxConnReceiveWindow")]
    max_connection_receive_window: u64,
    #[serde(default, with = "humantime_serde")]
    max_idle_timeout: Duration,
    #[serde(default, with = "humantime_serde")]
    keep_alive_period: Duration,
    #[serde(default, rename = "disablePathMTUDiscovery")]
    disable_path_mtu_discovery: bool,
}

#[derive(Clone, Debug, Deserialize, Default)]
struct ImportedBandwidthConfig {
    #[serde(default)]
    up: String,
    #[serde(default)]
    down: String,
}

#[derive(Clone, Debug, PartialEq)]
struct NodeCardData {
    kind: NodeCardKind,
    title: String,
    subtitle: String,
    meta: String,
    tags: Vec<String>,
    selected: bool,
    tone: AccentTone,
    form: Option<FormState>,
    action_label: &'static str,
}

#[derive(Clone)]
struct RuntimeController {
    tx: Sender<AppCommand>,
    rx: Arc<Mutex<Receiver<AppEvent>>>,
}

impl RuntimeController {
    fn spawn() -> Self {
        let (tx, rx_cmd) = mpsc::channel();
        let (tx_evt, rx_evt) = mpsc::channel();
        let tx_cmd = tx.clone();

        thread::spawn(move || controller_thread(rx_cmd, tx_cmd, tx_evt));

        Self {
            tx,
            rx: Arc::new(Mutex::new(rx_evt)),
        }
    }

    fn shared() -> Self {
        RUNTIME_CONTROLLER.get_or_init(Self::spawn).clone()
    }

    fn send(&self, command: AppCommand) {
        let _ = self.tx.send(command);
    }

    fn drain_events(&self) -> Vec<AppEvent> {
        let Ok(receiver) = self.rx.lock() else {
            return Vec::new();
        };
        receiver.try_iter().collect()
    }
}

fn controller_thread(
    rx_cmd: Receiver<AppCommand>,
    tx_cmd: Sender<AppCommand>,
    tx_evt: Sender<AppEvent>,
) {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            let _ = tx_evt.send(AppEvent::Status(UiStatus {
                phase: "Error".to_string(),
                detail: format!("failed to start runtime: {err}"),
                ..UiStatus::default()
            }));
            return;
        }
    };

    let mut current_client: Option<Client> = None;
    let mut connected_status = with_vpn_state(UiStatus::default());
    let mut local_socks_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut tun2socks_task: Option<Tun2SocksHandle> = None;
    let mut close_watch_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut transport_watch_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut health_probe_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut current_generation = 0_u64;
    let mut reconnect_form: Option<FormState> = None;
    let mut desired_vpn_active = false;

    while let Ok(command) = rx_cmd.recv() {
        match command {
            AppCommand::Connect(form) => {
                current_generation = current_generation.wrapping_add(1);
                desired_vpn_active = false;
                reconnect_form = None;
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    true,
                    true,
                    true,
                );

                let _ = tx_evt.send(AppEvent::Status(with_vpn_state(UiStatus {
                    phase: "Connecting".to_string(),
                    detail: format!("Connecting to {}...", form.server),
                    ..UiStatus::default()
                })));
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "connecting to {}",
                    form.server.trim()
                )));

                let reconnect_snapshot = form.clone();
                match runtime.block_on(connect_client(form)) {
                    Ok((client, status)) => {
                        let status = activate_connection(
                            runtime.handle().clone(),
                            tx_cmd.clone(),
                            tx_evt.clone(),
                            current_generation,
                            reconnect_snapshot.clone(),
                            client,
                            status,
	                            &mut current_client,
	                            &mut local_socks_task,
	                            &mut close_watch_task,
	                            &mut transport_watch_task,
	                            &mut health_probe_task,
	                            &mut reconnect_form,
	                        );
                        connected_status = status;
                    }
                    Err(err) => {
                        let _ = tx_evt.send(AppEvent::Status(with_vpn_state(UiStatus {
                            phase: "Error".to_string(),
                            detail: err.to_string(),
                            ..UiStatus::default()
                        })));
                        let _ = tx_evt.send(AppEvent::Log(format!("connect failed: {err:#}")));
                    }
                }
            }
            AppCommand::StartManagedVpn(form) => {
                current_generation = current_generation.wrapping_add(1);
                desired_vpn_active = true;
                reconnect_form = Some(form.clone());
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    true,
                    false,
                    true,
                );

                let _ = tx_evt.send(AppEvent::Status(with_vpn_state(UiStatus {
                    phase: "Starting VPN".to_string(),
                    detail: format!(
                        "Requesting Android system VPN and managed runtime for {}...",
                        form.server
                    ),
                    ..UiStatus::default()
                })));
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "requesting Android system VPN start for {}",
                    form.server.trim()
                )));

                match android_bridge::start_managed_vpn(&form, LOCAL_SOCKS_HOST, LOCAL_SOCKS_PORT) {
                    Ok(_) => {
                        connected_status = with_vpn_state(UiStatus {
                            phase: "Starting VPN".to_string(),
                            detail:
                                "Android system VPN requested. The service will own connect/reconnect."
                                    .to_string(),
                            local_socks: format!("{LOCAL_SOCKS_HOST}:{LOCAL_SOCKS_PORT}"),
                            ..UiStatus::default()
                        });
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                    }
                    Err(err) => {
                        desired_vpn_active = false;
                        reconnect_form = None;
                        connected_status = with_vpn_state(UiStatus {
                            phase: "Error".to_string(),
                            detail: err.to_string(),
                            ..UiStatus::default()
                        });
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                        let _ = tx_evt.send(AppEvent::Log(format!(
                            "managed Android VPN start failed: {err:#}"
                        )));
                    }
                }
            }
            AppCommand::ManagedConnect(form) => {
                current_generation = current_generation.wrapping_add(1);
                desired_vpn_active = true;
                reconnect_form = Some(form.clone());
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    true,
                    false,
                    true,
                );

                let _ = tx_evt.send(AppEvent::Status(with_vpn_state(UiStatus {
                    phase: "Connecting".to_string(),
                    detail: format!(
                        "Android VpnService is starting managed runtime for {}...",
                        form.server
                    ),
                    ..UiStatus::default()
                })));
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "Android VpnService is starting managed runtime for {}",
                    form.server.trim()
                )));

                let reconnect_snapshot = form.clone();
                match runtime.block_on(connect_client(form)) {
                    Ok((client, status)) => {
                        let local_udp_enabled = reconnect_snapshot.local_udp_enabled;
                        let mut status = activate_connection(
                            runtime.handle().clone(),
                            tx_cmd.clone(),
                            tx_evt.clone(),
                            current_generation,
                            reconnect_snapshot,
                            client,
                            status,
	                            &mut current_client,
	                            &mut local_socks_task,
	                            &mut close_watch_task,
	                            &mut transport_watch_task,
	                            &mut health_probe_task,
	                            &mut reconnect_form,
                        );
                        stop_vpn_runtime(&tx_evt, &mut tun2socks_task, false);
                        match start_vpn_runtime(&tx_evt, false, local_udp_enabled) {
                            Ok(handle) => {
                                tun2socks_task = Some(handle);
                                status.detail = "Android VpnService now owns the managed runtime: TUN -> tun2socks -> local SOCKS -> hysteria-core".to_string();
                            }
                            Err(err) => {
                                let _ = tx_evt.send(AppEvent::Log(format!(
                                    "managed runtime connected but failed to attach Android VPN shell: {err:#}"
                                )));
                                status.detail = format!(
                                    "Managed runtime connected, but Android VPN shell attach failed: {err}"
                                );
                            }
                        }
                        connected_status = with_vpn_state(status);
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                    }
                    Err(err) => {
                        connected_status = with_vpn_state(UiStatus {
                            phase: "Error".to_string(),
                            detail: err.to_string(),
                            ..UiStatus::default()
                        });
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                        let _ = tx_evt.send(AppEvent::Log(format!(
                            "managed runtime connect failed: {err:#}"
                        )));
                    }
                }
            }
            AppCommand::Disconnect => {
                current_generation = current_generation.wrapping_add(1);
                desired_vpn_active = false;
                reconnect_form = None;
                if current_client.is_none() {
                    let _ = tx_evt.send(AppEvent::Log(
                        "disconnect requested with no active connection".to_string(),
                    ));
                }
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    true,
                    true,
                    true,
                );

                connected_status = with_vpn_state(UiStatus::default());
                let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
            }
            AppCommand::Speedtest(direction) => {
                let Some(client) = current_client.clone() else {
                    let _ = tx_evt.send(AppEvent::Log(
                        "speedtest requested with no active connection".to_string(),
                    ));
                    continue;
                };

                let _ = tx_evt.send(AppEvent::Status(UiStatus {
                    phase: "Speedtest".to_string(),
                    detail: format!("Running {} speedtest...", direction.label()),
                    ..connected_status.clone()
                }));
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "running {} speedtest",
                    direction.label()
                )));

                match runtime.block_on(run_speedtest(&client, direction)) {
                    Ok(summary) => {
                        let _ = tx_evt.send(AppEvent::Log(summary));
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                    }
                    Err(err) => {
                        let _ = tx_evt.send(AppEvent::Log(format!(
                            "{} speedtest failed: {err:#}",
                            direction.label()
                        )));
                        let _ = tx_evt.send(AppEvent::Status(UiStatus {
                            phase: "Error".to_string(),
                            detail: err.to_string(),
                            ..connected_status.clone()
                        }));
                    }
                }
            }
            AppCommand::RequestVpnPermission => match android_bridge::request_permission() {
                Ok(_) => {
                    let _ = tx_evt.send(AppEvent::Log(
                        "requested Android VPN permission".to_string(),
                    ));
                    connected_status = with_vpn_state(connected_status);
                    let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                }
                Err(err) => {
                    let _ = tx_evt.send(AppEvent::Log(format!(
                        "request VPN permission failed: {err:#}"
                    )));
                }
            },
            AppCommand::StopVpnShell => {
                desired_vpn_active = false;
                stop_vpn_runtime(&tx_evt, &mut tun2socks_task, true);
                connected_status.detail = "Stopping Android system VPN...".to_string();
                connected_status = with_vpn_state(connected_status);
                let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
            }
            AppCommand::ServiceStopped => {
                current_generation = current_generation.wrapping_add(1);
                desired_vpn_active = false;
                reconnect_form = None;
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    true,
                    false,
                    true,
                );
                connected_status = with_vpn_state(UiStatus::default());
                let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                let _ = tx_evt.send(AppEvent::Log(
                    "Android VpnService stopped; managed runtime shut down".to_string(),
                ));
            }
            AppCommand::ConnectionClosed { generation, reason } => {
                if generation != current_generation || current_client.is_none() {
                    continue;
                }

                let restart_vpn = desired_vpn_active;
                let preserve_tun2socks =
                    should_preserve_tun2socks_on_connection_loss(restart_vpn);
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "connection closed unexpectedly: {reason}"
                )));
                stop_active_session(
                    &runtime,
                    &tx_evt,
                    &mut current_client,
                    &mut local_socks_task,
                    &mut tun2socks_task,
                    &mut close_watch_task,
                    &mut transport_watch_task,
                    &mut health_probe_task,
                    true,
                    !preserve_tun2socks,
                    false,
                    false,
                );
                connected_status = with_vpn_state(UiStatus {
                    phase: "Reconnecting".to_string(),
                    remote: connected_status.remote.clone(),
                    detail: if restart_vpn {
                        format!(
                            "Connection lost: {reason}. Reconnecting soon while keeping Android VPN shell alive..."
                        )
                    } else {
                        format!("Connection lost: {reason}. Reconnecting soon...")
                    },
                    server_udp_supported: connected_status.server_udp_supported,
                    local_udp_enabled: connected_status.local_udp_enabled,
                    udp_enabled: connected_status.udp_enabled,
                    negotiated_tx: connected_status.negotiated_tx,
                    latency: connected_status.latency,
                    tx_total_bytes: connected_status.tx_total_bytes,
                    rx_total_bytes: connected_status.rx_total_bytes,
                    tx_rate_bytes: 0,
                    rx_rate_bytes: 0,
                    local_socks: connected_status.local_socks.clone(),
                    ..UiStatus::default()
                });
                let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                schedule_reconnect(runtime.handle().clone(), tx_cmd.clone(), generation, 0);
            }
            AppCommand::Reconnect {
                generation,
                attempt,
            } => {
                if generation != current_generation {
                    continue;
                }
                let Some(form) = reconnect_form.clone() else {
                    continue;
                };

                let _ = tx_evt.send(AppEvent::Status(with_vpn_state(UiStatus {
                    phase: "Reconnecting".to_string(),
                    remote: connected_status.remote.clone(),
                    detail: format!(
                        "Reconnect attempt {} to {}...",
                        attempt + 1,
                        form.server.trim()
                    ),
                    server_udp_supported: connected_status.server_udp_supported,
                    local_udp_enabled: connected_status.local_udp_enabled,
                    udp_enabled: connected_status.udp_enabled,
                    negotiated_tx: connected_status.negotiated_tx,
                    latency: connected_status.latency,
                    tx_total_bytes: connected_status.tx_total_bytes,
                    rx_total_bytes: connected_status.rx_total_bytes,
                    tx_rate_bytes: 0,
                    rx_rate_bytes: 0,
                    local_socks: connected_status.local_socks.clone(),
                    ..UiStatus::default()
                })));
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "reconnect attempt {} -> {}",
                    attempt + 1,
                    form.server.trim()
                )));

                match runtime.block_on(connect_client(form.clone())) {
                    Ok((client, status)) => {
                        let local_udp_enabled = form.local_udp_enabled;
                        let status = activate_connection(
                            runtime.handle().clone(),
                            tx_cmd.clone(),
                            tx_evt.clone(),
                            current_generation,
                            form,
                            client,
                            status,
                            &mut current_client,
                            &mut local_socks_task,
                            &mut close_watch_task,
                            &mut transport_watch_task,
                            &mut health_probe_task,
                            &mut reconnect_form,
                        );
                        connected_status = status;
                        let _ =
                            tx_evt.send(AppEvent::Log("automatic reconnect succeeded".to_string()));

                        if should_restart_tun2socks_after_reconnect(
                            desired_vpn_active,
                            tun2socks_task.is_some(),
                        ) {
                            match start_vpn_runtime(&tx_evt, false, local_udp_enabled) {
                                Ok(handle) => {
                                    tun2socks_task = Some(handle);
                                    connected_status.detail = "Automatic reconnect restored Android system VPN: TUN -> tun2socks -> local SOCKS -> hysteria-core".to_string();
                                }
                                Err(err) => {
                                    let _ = tx_evt.send(AppEvent::Log(format!(
                                        "automatic reconnect restored client but failed to restart Android VPN: {err:#}"
                                    )));
                                    connected_status.detail = format!(
                                        "Reconnected, but Android system VPN restart failed: {err}"
                                    );
                                }
                            }
                        } else if desired_vpn_active {
                            let _ = tx_evt.send(AppEvent::Log(
                                "automatic reconnect preserved Android VPN shell and local DNS path"
                                    .to_string(),
                            ));
                            connected_status.detail =
                                "Automatic reconnect preserved Android system VPN shell and local DNS path"
                                    .to_string();
                            connected_status = with_vpn_state(connected_status);
                            let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                        }
                    }
                    Err(err) => {
                        let delay = reconnect_delay(attempt + 1);
                        let _ = tx_evt.send(AppEvent::Log(format!(
                            "reconnect attempt {} failed: {err:#}; retrying in {}s",
                            attempt + 1,
                            delay.as_secs()
                        )));
                        connected_status = with_vpn_state(UiStatus {
                            phase: "Reconnecting".to_string(),
                            remote: connected_status.remote.clone(),
                            detail: format!(
                                "Reconnect attempt {} failed: {err}. Retrying in {}s.",
                                attempt + 1,
                                delay.as_secs()
                            ),
                            server_udp_supported: connected_status.server_udp_supported,
                            local_udp_enabled: connected_status.local_udp_enabled,
                            udp_enabled: connected_status.udp_enabled,
                            negotiated_tx: connected_status.negotiated_tx,
                            latency: connected_status.latency,
                            tx_total_bytes: connected_status.tx_total_bytes,
                            rx_total_bytes: connected_status.rx_total_bytes,
                            tx_rate_bytes: 0,
                            rx_rate_bytes: 0,
                            local_socks: connected_status.local_socks.clone(),
                            ..UiStatus::default()
                        });
                        let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
                        schedule_reconnect(
                            runtime.handle().clone(),
                            tx_cmd.clone(),
                            generation,
                            attempt + 1,
                        );
                    }
                }
            }
            AppCommand::TransportPulse {
                generation,
                metrics,
            } => {
                if generation != current_generation || current_client.is_none() {
                    continue;
                }
                apply_dashboard_metrics(&mut connected_status, &metrics);
                let _ = tx_evt.send(AppEvent::Transport(metrics));
            }
        }
    }
}

fn activate_connection(
    runtime_handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    tx_evt: Sender<AppEvent>,
    generation: u64,
    reconnect_snapshot: FormState,
    client: Client,
    status: UiStatus,
    current_client: &mut Option<Client>,
    local_socks_task: &mut Option<tokio::task::JoinHandle<()>>,
    close_watch_task: &mut Option<tokio::task::JoinHandle<()>>,
    transport_watch_task: &mut Option<tokio::task::JoinHandle<()>>,
    health_probe_task: &mut Option<tokio::task::JoinHandle<()>>,
    reconnect_form: &mut Option<FormState>,
) -> UiStatus {
    let local_udp_enabled = reconnect_snapshot.local_udp_enabled;
    let socks_listen = format!("{LOCAL_SOCKS_HOST}:{LOCAL_SOCKS_PORT}");
    let close_watch_handle = runtime_handle.clone();
    let transport_watch_handle = runtime_handle.clone();
    let transport_watch_tx = tx_cmd.clone();
    *local_socks_task = Some(spawn_local_socks(
        runtime_handle.clone(),
        transport_watch_tx.clone(),
        tx_evt.clone(),
        client.clone(),
        socks_listen.clone(),
        generation,
        local_udp_enabled,
    ));
    *close_watch_task = Some(spawn_connection_close_watcher(
        close_watch_handle,
        tx_cmd,
        client.clone(),
        generation,
    ));
    *transport_watch_task = Some(spawn_transport_metrics_watcher(
        transport_watch_handle,
        transport_watch_tx.clone(),
        client.clone(),
        generation,
    ));
    *health_probe_task = if should_run_health_probe(&status.phase) {
        Some(spawn_health_probe_watcher(
            runtime_handle.clone(),
            transport_watch_tx,
            client.clone(),
            generation,
        ))
    } else {
        None
    };

    let mut status = with_vpn_state(status);
    status.local_socks = socks_listen.clone();
    apply_dashboard_metrics(
        &mut status,
        &derive_dashboard_metrics(None, client.transport_snapshot(), Duration::from_secs(1)),
    );
    *reconnect_form = Some(reconnect_snapshot);
    *current_client = Some(client);

    let _ = tx_evt.send(AppEvent::Status(status.clone()));
    let _ = tx_evt.send(AppEvent::Log(format!(
        "connected: remote={} udp_enabled={} negotiated_tx={}B/s",
        status.remote, status.udp_enabled, status.negotiated_tx,
    )));
    let _ = tx_evt.send(AppEvent::Log(format!(
        "local SOCKS runtime started on {}",
        socks_listen
    )));
    status
}

fn stop_active_session(
    runtime: &tokio::runtime::Runtime,
    tx_evt: &Sender<AppEvent>,
    current_client: &mut Option<Client>,
    local_socks_task: &mut Option<tokio::task::JoinHandle<()>>,
    tun2socks_task: &mut Option<Tun2SocksHandle>,
    close_watch_task: &mut Option<tokio::task::JoinHandle<()>>,
    transport_watch_task: &mut Option<tokio::task::JoinHandle<()>>,
    health_probe_task: &mut Option<tokio::task::JoinHandle<()>>,
    stop_local_socks_runtime: bool,
    stop_tun2socks_runtime: bool,
    stop_service: bool,
    gracefully_close_client: bool,
) {
    if let Some(task) = close_watch_task.take() {
        task.abort();
    }
    if let Some(task) = transport_watch_task.take() {
        task.abort();
    }
    if let Some(task) = health_probe_task.take() {
        task.abort();
    }

    if stop_tun2socks_runtime {
        stop_vpn_runtime(tx_evt, tun2socks_task, stop_service);
    }

    if stop_local_socks_runtime
        && let Some(task) = local_socks_task.take()
    {
        task.abort();
    }

    if let Some(client) = current_client.take()
        && gracefully_close_client
    {
        match runtime.block_on(client.close()) {
            Ok(_) => {
                let _ = tx_evt.send(AppEvent::Log("connection closed".to_string()));
            }
            Err(err) => {
                let _ = tx_evt.send(AppEvent::Log(format!("disconnect error: {err:#}")));
            }
        }
    }
}

fn spawn_connection_close_watcher(
    handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    client: Client,
    generation: u64,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let reason = client.wait_closed().await.to_string();
        let _ = tx_cmd.send(AppCommand::ConnectionClosed { generation, reason });
    })
}

fn spawn_transport_metrics_watcher(
    handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    client: Client,
    generation: u64,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let mut previous = None;
        loop {
            let current = client.transport_snapshot();
            let metrics =
                derive_dashboard_metrics(previous, current, Duration::from_secs(1));
            if tx_cmd
                .send(AppCommand::TransportPulse { generation, metrics })
                .is_err()
            {
                break;
            }
            previous = Some(current);
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
}

fn spawn_health_probe_watcher(
    handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    client: Client,
    generation: u64,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let mut failures = 0_u32;
        loop {
            tokio::time::sleep(HEALTH_PROBE_INTERVAL).await;
            match run_client_health_check(&client).await {
                Ok(_) => failures = 0,
                Err(err) => {
                    failures = failures.saturating_add(1);
                    if failures >= HEALTH_PROBE_FAILURE_THRESHOLD {
                        let _ = tx_cmd.send(AppCommand::ConnectionClosed {
                            generation,
                            reason: format!("health probe failed: {err}"),
                        });
                        break;
                    }
                }
            }
        }
    })
}

fn reconnect_delay(attempt: u32) -> Duration {
    let seconds = 1_u64
        .checked_shl(attempt.min(4))
        .unwrap_or(MAX_RECONNECT_BACKOFF.as_secs());
    Duration::from_secs(seconds.min(MAX_RECONNECT_BACKOFF.as_secs()))
}

fn schedule_reconnect(
    handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    generation: u64,
    attempt: u32,
) {
    let delay = reconnect_delay(attempt);
    handle.spawn(async move {
        tokio::time::sleep(delay).await;
        let _ = tx_cmd.send(AppCommand::Reconnect {
            generation,
            attempt,
        });
    });
}

fn start_vpn_runtime(
    tx_evt: &Sender<AppEvent>,
    request_service_start: bool,
    local_udp_enabled: bool,
) -> Result<Tun2SocksHandle> {
    if request_service_start {
        android_bridge::start_vpn_service(LOCAL_SOCKS_HOST, i32::from(LOCAL_SOCKS_PORT))
            .context("failed to request Android VPN service start")?;
        let _ = tx_evt.send(AppEvent::Log(format!(
            "requested Android VPN service start -> {}:{}",
            LOCAL_SOCKS_HOST, LOCAL_SOCKS_PORT
        )));
    }

    for attempt in 0..20u64 {
        let state = android_bridge::query_state().unwrap_or_default();
        if state.active {
            let tun_fd = android_bridge::take_tun_fd().context("failed to fetch TUN fd")?;
            if tun_fd >= 0 {
                let handle = vpn_tun2socks::spawn(
                    managed_vpn_tun_config(local_udp_enabled),
                    tun_fd,
                )
                    .context("failed to spawn tun2socks runtime")?;
                let _ = tx_evt.send(AppEvent::Log(
                    "Android system VPN started: TUN -> tun2socks -> local SOCKS -> hysteria-core"
                        .to_string(),
                ));
                return Ok(handle);
            }
        } else if !request_service_start {
            bail!("Android VpnService is not active");
        }

        thread::sleep(Duration::from_millis(200 + attempt * 20));
    }

    bail!("Android VpnService did not become active in time")
}

fn stop_vpn_runtime(
    tx_evt: &Sender<AppEvent>,
    tun2socks_task: &mut Option<Tun2SocksHandle>,
    stop_service: bool,
) {
    if let Some(handle) = tun2socks_task.take() {
        match vpn_tun2socks::stop(handle) {
            Ok(_) => {
                let _ = tx_evt.send(AppEvent::Log("tun2socks runtime stopped".to_string()));
            }
            Err(err) => {
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "tun2socks runtime stop failed: {err:#}"
                )));
            }
        }
    }

    if stop_service && android_bridge::query_state().unwrap_or_default().available {
        match android_bridge::stop_vpn_service() {
            Ok(_) => {
                let _ = tx_evt.send(AppEvent::Log(
                    "requested Android VPN service stop".to_string(),
                ));
            }
            Err(err) => {
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "stop Android VPN service failed: {err:#}"
                )));
            }
        }
    }
}

fn spawn_local_socks(
    handle: tokio::runtime::Handle,
    tx_cmd: Sender<AppCommand>,
    tx_evt: Sender<AppEvent>,
    client: Client,
    listen: String,
    generation: u64,
    local_udp_enabled: bool,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let fatal_notifier: FatalConnectionNotifier = Arc::new({
            let tx_cmd = tx_cmd.clone();
            move |reason: String| {
                let _ = tx_cmd.send(AppCommand::ConnectionClosed { generation, reason });
            }
        });
        let dns_failure_notifier: DnsFailureNotifier = Arc::new({
            let tx_evt = tx_evt.clone();
            move |reason: String| {
                let _ = tx_evt.send(AppEvent::DnsFailure(reason));
            }
        });
        let dns_proxy = match build_managed_vpn_dns_proxy(
            client.clone(),
            Some(dns_failure_notifier),
        ) {
            Ok(proxy) => Some(proxy),
            Err(err) => {
                let _ = tx_evt.send(AppEvent::Log(format!(
                    "managed VPN DNS proxy init failed: {err:#}"
                )));
                None
            }
        };
        let config = build_local_socks_config(
            listen.clone(),
            dns_proxy,
            &FormState {
                local_udp_enabled,
                ..FormState::default()
            },
        );
        if let Err(err) = serve_socks5(config, client, Some(fatal_notifier)).await {
            let _ = tx_evt.send(AppEvent::Log(format!(
                "local SOCKS runtime stopped: {err:#}"
            )));
        }
    })
}

fn with_vpn_state(mut status: UiStatus) -> UiStatus {
    let vpn_state = android_bridge::query_state().unwrap_or_default();
    status.vpn_available = vpn_state.available;
    status.vpn_permission_granted = vpn_state.permission_granted;
    status.vpn_active = vpn_state.active;
    status
}

#[cfg(target_os = "android")]
fn jstring_to_string(env: &Env<'_>, value: &JString<'_>) -> Result<String> {
    value
        .try_to_string(env)
        .context("failed to read Java string")
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_dioxus_main_MainActivity_nativePrimeAndroidActivityBridge(
    mut env: EnvUnowned<'_>,
    activity: JObject<'_>,
) {
    let result = env
        .with_env(|env| -> Result<()> {
            android_bridge::cache_java_vm(env)?;
            android_bridge::cache_main_activity(env, &activity)?;
            ensure_android_runtime_initialized();
            Ok(())
        })
        .into_outcome();

    if let jni::Outcome::Err(err) = result {
        eprintln!("nativePrimeAndroidActivityBridge failed: {err:#}");
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_dioxus_main_HysteriaVpnService_nativePrimeAndroidServiceBridge(
    mut env: EnvUnowned<'_>,
    service: JObject<'_>,
) {
    let result = env
        .with_env(|env| -> Result<()> {
            android_bridge::cache_java_vm(env)?;
            android_bridge::cache_vpn_service(env, &service)?;
            ensure_android_runtime_initialized();
            Ok(())
        })
        .into_outcome();

    if let jni::Outcome::Err(err) = result {
        eprintln!("nativePrimeAndroidServiceBridge failed: {err:#}");
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_dioxus_main_HysteriaVpnService_nativeStartManagedRuntime(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
    server: JString<'_>,
    auth: JString<'_>,
    obfs_password: JString<'_>,
    sni: JString<'_>,
    ca_path: JString<'_>,
    pin_sha256: JString<'_>,
    bandwidth_up: JString<'_>,
    bandwidth_down: JString<'_>,
    quic_init_stream_receive_window: JString<'_>,
    quic_max_stream_receive_window: JString<'_>,
    quic_init_connection_receive_window: JString<'_>,
    quic_max_connection_receive_window: JString<'_>,
    quic_max_idle_timeout: JString<'_>,
    quic_keep_alive_period: JString<'_>,
    local_udp_enabled: jboolean,
    quic_disable_path_mtu_discovery: jboolean,
    insecure_tls: jboolean,
    _restore_vpn: jboolean,
) {
    let result = env
        .with_env(|env| -> Result<()> {
            let _ = android_bridge::cache_java_vm(env);
            ensure_android_runtime_initialized();
            let controller = RuntimeController::shared();
            controller.send(AppCommand::ManagedConnect(FormState {
                import_uri: String::new(),
                server: jstring_to_string(env, &server)?,
                auth: jstring_to_string(env, &auth)?,
                obfs_password: jstring_to_string(env, &obfs_password)?,
                sni: jstring_to_string(env, &sni)?,
                ca_path: jstring_to_string(env, &ca_path)?,
                pin_sha256: jstring_to_string(env, &pin_sha256)?,
                bandwidth_up: jstring_to_string(env, &bandwidth_up)?,
                bandwidth_down: jstring_to_string(env, &bandwidth_down)?,
                quic_init_stream_receive_window: jstring_to_string(
                    env,
                    &quic_init_stream_receive_window,
                )?,
                quic_max_stream_receive_window: jstring_to_string(
                    env,
                    &quic_max_stream_receive_window,
                )?,
                quic_init_connection_receive_window: jstring_to_string(
                    env,
                    &quic_init_connection_receive_window,
                )?,
                quic_max_connection_receive_window: jstring_to_string(
                    env,
                    &quic_max_connection_receive_window,
                )?,
                quic_max_idle_timeout: jstring_to_string(env, &quic_max_idle_timeout)?,
                quic_keep_alive_period: jstring_to_string(env, &quic_keep_alive_period)?,
                local_udp_enabled,
                quic_disable_path_mtu_discovery,
                insecure_tls,
            }));
            Ok(())
        })
        .into_outcome();

    if let jni::Outcome::Err(err) = result {
        eprintln!("nativeStartManagedRuntime failed: {err:#}");
    }
}

#[cfg(target_os = "android")]
#[unsafe(no_mangle)]
pub extern "system" fn Java_dev_dioxus_main_HysteriaVpnService_nativeStopManagedRuntime(
    mut env: EnvUnowned<'_>,
    _class: JClass<'_>,
) {
    let _ = env.with_env(|env| android_bridge::cache_java_vm(env));
    ensure_android_runtime_initialized();
    RuntimeController::shared().send(AppCommand::ServiceStopped);
}

#[component]
fn App() -> Element {
    use_context_provider(RuntimeController::shared);
    let controller = use_context::<RuntimeController>();

    let mut form = use_signal(initial_form_state);
    let mut saved_profile = use_signal(|| android_bridge::query_saved_profile().ok().flatten());
    let launch_prefill_applied = use_signal(|| has_meaningful_form_prefill(&initial_form_state()));
    let launch_automation = use_signal(initial_launch_automation);
    let launch_automation_started = use_signal(|| false);
    let status = use_signal(UiStatus::default);
    let metrics = use_signal(UiMetrics::default);
    let mut prefs = use_signal(UiPrefs::default);
    let mut active_tab = use_signal(|| AppTab::Home);
    let mut settings_return_tab = use_signal(|| AppTab::Home);
    let mut diagnostics_expanded = use_signal(|| false);
    let mut node_filter = use_signal(|| NodeFilter::All);
    let mut node_search = use_signal(String::new);
    let mut ca_catalog = use_signal(|| android_bridge::query_ca_catalog().unwrap_or_default());
    let mut live_tick = use_signal(|| 0_u64);
    let mut auto_connect_after_vpn_permission = use_signal(|| false);
    let mut imported_config_name = use_signal(String::new);
    let imported_cert_name = use_signal(String::new);
    let mut logs = use_signal(|| {
        let vpn_note = android_bridge::availability_message(
            &android_bridge::query_state().unwrap_or_default(),
        )
        .unwrap_or("Android VPN shell unavailable in this build");
        vec![
            LogEntry::new("Minimal Android flow ready.".to_string()),
            LogEntry::new(
                "Import a config, optionally import a CA certificate, then connect.".to_string(),
            ),
            LogEntry::new(vpn_note.to_string()),
        ]
    });

    let controller_for_events = controller.clone();
    let controller_for_launch_automation = controller.clone();
    let controller_for_permission = controller.clone();
    let _event_pump = use_future(move || {
        let controller = controller_for_events.clone();
        let mut status = status;
        let mut logs = logs;
        let mut metrics = metrics;
        async move {
            let mut last_vpn_state = android_bridge::query_state().unwrap_or_default();
            loop {
                for event in controller.drain_events() {
                    match event {
                        AppEvent::Status(next) => {
                            let previous = status();
                            record_status_transition(&mut metrics, &previous, &next);
                            status.set(next);
                        }
                        AppEvent::Transport(next) => {
                            status.with_mut(|current| apply_dashboard_metrics(current, &next));
                        }
                        AppEvent::Log(message) => {
                            record_log_metrics(&mut metrics, &message);
                            append_log(&mut logs, message);
                        }
                        AppEvent::DnsFailure(reason) => {
                            record_dns_failure(&mut metrics);
                            append_log(&mut logs, format!("DNS failure: {reason}"));
                        }
                    }
                }
                let vpn_state = android_bridge::query_state().unwrap_or_default();
                if vpn_state != last_vpn_state {
                    status.with_mut(|current| {
                        current.vpn_available = vpn_state.available;
                        current.vpn_permission_granted = vpn_state.permission_granted;
                        current.vpn_active = vpn_state.active;
                    });
                    last_vpn_state = vpn_state;
                }
                Delay::new(Duration::from_millis(150)).await;
            }
        }
    });

    let _live_tick = use_future(move || async move {
        loop {
            Delay::new(Duration::from_secs(1)).await;
            live_tick.with_mut(|tick| *tick = tick.wrapping_add(1));
        }
    });

    let _launch_prefill = use_future(move || {
        let mut form = form;
        let mut launch_prefill_applied = launch_prefill_applied;
        let mut logs = logs;
        async move {
            if launch_prefill_applied() {
                append_log(&mut logs, describe_prefill(&form()));
                return;
            }

            for _ in 0..20 {
                let launch_form = initial_form_state();
                if has_meaningful_form_prefill(&launch_form) {
                    append_log(
                        &mut logs,
                        format!("{} (applied)", describe_prefill(&launch_form)),
                    );
                    form.set(launch_form);
                    launch_prefill_applied.set(true);
                    return;
                }
                Delay::new(Duration::from_millis(150)).await;
            }

            append_log(
                &mut logs,
                format!("{} (none applied)", describe_prefill(&initial_form_state())),
            );
        }
    });

    let _ca_catalog_bootstrap = use_future(move || {
        let mut ca_catalog = ca_catalog;
        async move {
            for _ in 0..20 {
                if let Ok(catalog) = android_bridge::query_ca_catalog() {
                    ca_catalog.set(catalog);
                    return;
                }
                Delay::new(Duration::from_millis(150)).await;
            }
        }
    });

    let _import_poll = use_future(move || {
        let mut form = form;
        let mut logs = logs;
        let mut saved_profile = saved_profile;
        let mut ca_catalog = ca_catalog;
        let mut imported_config_name = imported_config_name;
        let mut imported_cert_name = imported_cert_name;
        async move {
            loop {
                match android_bridge::take_config_import() {
                    Ok(Some(imported)) => match parse_imported_client_document(&imported.content) {
                        Ok((mut imported_form, warning)) => {
                            let current = form();
                            if imported_form.ca_path.trim().is_empty()
                                && !current.ca_path.trim().is_empty()
                            {
                                imported_form.ca_path = current.ca_path.clone();
                            }
                            imported_config_name.set(imported.name.clone());
                            form.set(imported_form.clone());
                            saved_profile.set(Some(imported_form.clone()));
                            match android_bridge::save_profile(&imported_form) {
                                Ok(_) => append_log(
                                    &mut logs,
                                    format!(
                                        "config imported from {} -> {}",
                                        imported.name,
                                        config_value_or_empty(&imported_form.server)
                                    ),
                                ),
                                Err(err) => append_log(
                                    &mut logs,
                                    format!("imported config but failed to persist it: {err:#}"),
                                ),
                            }
                            if let Some(warning) = warning {
                                append_log(&mut logs, warning);
                            }
                        }
                        Err(err) => append_log(
                            &mut logs,
                            format!("failed to parse imported config: {err:#}"),
                        ),
                    },
                    Ok(None) => {}
                    Err(err) => {
                        append_log(&mut logs, format!("config import bridge failed: {err:#}"))
                    }
                }

                match android_bridge::take_ca_import() {
                    Ok(Some(imported)) => {
                        imported_cert_name.set(imported.name.clone());
                        form.with_mut(|current| current.ca_path = imported.path.clone());
                        let updated = form();
                        saved_profile.set(Some(updated.clone()));
                        match android_bridge::save_profile(&updated) {
                            Ok(_) => append_log(
                                &mut logs,
                                format!(
                                    "certificate imported: {} -> {}",
                                    imported.name, imported.path
                                ),
                            ),
                            Err(err) => append_log(
                                &mut logs,
                                format!("imported certificate but failed to persist it: {err:#}"),
                            ),
                        }
                        match android_bridge::query_ca_catalog() {
                            Ok(catalog) => ca_catalog.set(catalog),
                            Err(err) => append_log(
                                &mut logs,
                                format!("certificate imported but CA refresh failed: {err:#}"),
                            ),
                        }
                    }
                    Ok(None) => {}
                    Err(err) => append_log(
                        &mut logs,
                        format!("certificate import bridge failed: {err:#}"),
                    ),
                }

                Delay::new(Duration::from_millis(250)).await;
            }
        }
    });

    let _auto_connect_after_permission = use_future(move || {
        let controller = controller_for_permission.clone();
        let form = form;
        let mut pending = auto_connect_after_vpn_permission;
        async move {
            loop {
                if pending()
                    && android_bridge::query_state()
                        .unwrap_or_default()
                        .permission_granted
                {
                    pending.set(false);
                    let snapshot = form();
                    if has_required_connection_fields(&snapshot) {
                        controller.send(AppCommand::StartManagedVpn(snapshot));
                    }
                }
                Delay::new(Duration::from_millis(250)).await;
            }
        }
    });

    let _launch_automation = use_future(move || {
        let controller = controller_for_launch_automation.clone();
        let form = form;
        let status = status;
        let mut logs = logs;
        let automation = launch_automation;
        let mut launch_automation_started = launch_automation_started;
        async move {
            if launch_automation_started() {
                return;
            }

            let automation = automation();
            if !has_launch_automation(automation) {
                return;
            }

            launch_automation_started.set(true);
            append_log(&mut logs, describe_launch_automation(automation));

            for _ in 0..20 {
                if has_meaningful_form_prefill(&form()) {
                    break;
                }
                Delay::new(Duration::from_millis(150)).await;
            }

            if automation.auto_start_vpn {
                let snapshot = form();
                if !has_required_connection_fields(&snapshot) {
                    append_log(
                        &mut logs,
                        "launch automation skipped managed VPN start because required fields are empty"
                            .to_string(),
                    );
                    return;
                }
                append_log(
                    &mut logs,
                    "launch automation: starting managed Android system VPN".to_string(),
                );
                controller.send(AppCommand::StartManagedVpn(snapshot));
                return;
            }

            if automation.auto_connect || automation.auto_request_vpn {
                let snapshot = form();
                if !has_required_connection_fields(&snapshot) {
                    append_log(
                        &mut logs,
                        "launch automation skipped connect because required fields are empty"
                            .to_string(),
                    );
                    return;
                }
                append_log(&mut logs, "launch automation: sending Connect".to_string());
                controller.send(AppCommand::Connect(snapshot));
            }

            if automation.auto_request_vpn {
                for _ in 0..120 {
                    let current = status();
                    if current.phase == "Connected" {
                        break;
                    }
                    Delay::new(Duration::from_millis(250)).await;
                }

                if status().phase != "Connected" {
                    append_log(
                        &mut logs,
                        "launch automation: connect did not reach Connected in time".to_string(),
                    );
                    return;
                }

                append_log(
                    &mut logs,
                    "launch automation: requesting Android VPN permission".to_string(),
                );
                controller.send(AppCommand::RequestVpnPermission);
            }
        }
    });

    let status_snapshot = status();
    let log_items = logs();
    let form_snapshot = form();
    let saved_profile_snapshot = saved_profile();
    let metrics_snapshot = metrics();
    let prefs_snapshot = prefs();
    let active_tab_snapshot = active_tab();
    let diagnostics_expanded_snapshot = diagnostics_expanded();
    let node_filter_snapshot = node_filter();
    let node_search_snapshot = node_search();
    let ca_catalog_snapshot = ca_catalog();
    let imported_config_name_snapshot = imported_config_name();
    let imported_cert_name_snapshot = imported_cert_name();
    let _ = live_tick();
    let now = Instant::now();
    let can_connect = has_required_connection_fields(&form_snapshot);
    let launch_automation_snapshot = launch_automation();
    let (page_title, page_subtitle) = match active_tab_snapshot {
        AppTab::Home => (
            "Hysteria".to_string(),
            format!(
                "{} · {}",
                status_snapshot.phase,
                config_value_or_empty(&form_snapshot.server)
            ),
        ),
        AppTab::Nodes => (
            "Nodes".to_string(),
            "Profiles, imports, and the current working draft.".to_string(),
        ),
        AppTab::Stats => (
            "Statistics".to_string(),
            "Runtime health, tests, and recent diagnostics.".to_string(),
        ),
        AppTab::Settings => (
            "Settings".to_string(),
            "Import, configure, and inspect the mobile runtime.".to_string(),
        ),
    };
    let topbar_action_label: Option<&'static str> = Some(match active_tab_snapshot {
        AppTab::Settings => "Back",
        _ => "Settings",
    });
    let connection_tone = phase_tone(&status_snapshot.phase);
    let can_load_saved = saved_profile_snapshot.is_some();
    let latest_download = metrics_snapshot
        .latest_download
        .clone()
        .unwrap_or_else(|| "Run a download test".to_string());
    let latest_upload = metrics_snapshot
        .latest_upload
        .clone()
        .unwrap_or_else(|| "Run an upload test".to_string());
    let online_duration = metrics_snapshot
        .connected_since
        .map(|started| format_elapsed(started, now))
        .unwrap_or_else(|| "--".to_string());
    let last_connected = metrics_snapshot
        .last_connected_at
        .map(|connected_at| format_relative_time(connected_at, now))
        .unwrap_or_else(|| "No successful session yet".to_string());
    let node_cards = build_node_cards(
        &form_snapshot,
        saved_profile_snapshot.as_ref(),
        &status_snapshot,
    );
    let filtered_node_cards: Vec<NodeCardData> = node_cards
        .into_iter()
        .filter(|card| node_filter_matches(node_filter_snapshot, card.kind))
        .filter(|card| node_search_matches(&node_search_snapshot, card))
        .collect();
    let _home_recent_logs: Vec<LogEntry> = log_items.iter().take(4).cloned().collect();
    let stats_recent_logs: Vec<LogEntry> = log_items.iter().take(8).cloned().collect();
    let _vpn_action_label = vpn_action_label(&status_snapshot);
    let health_score = connection_health_score(&metrics_snapshot, &status_snapshot);
    let vpn_score = vpn_health_score(&status_snapshot);
    let transport_score = transport_health_score(&status_snapshot);
    let current_config_label = if !imported_config_name_snapshot.trim().is_empty() {
        imported_config_name_snapshot.clone()
    } else if saved_profile_snapshot.is_some() {
        "Saved profile".to_string()
    } else if has_meaningful_form_prefill(&form_snapshot) {
        "Current draft".to_string()
    } else {
        "No config imported".to_string()
    };
    let current_trust_label = current_trust_label(
        &form_snapshot,
        &ca_catalog_snapshot,
        &imported_cert_name_snapshot,
    );
    let current_node_label = if !imported_config_name_snapshot.trim().is_empty() {
        imported_config_name_snapshot.clone()
    } else if !form_snapshot.server.trim().is_empty() {
        summarize_endpoint(&form_snapshot.server)
    } else {
        "No node configured".to_string()
    };
    let upload_rate_label = format_bytes_per_second(status_snapshot.tx_rate_bytes);
    let download_rate_label = format_bytes_per_second(status_snapshot.rx_rate_bytes);
    let upload_total_label = format_total_bytes(status_snapshot.tx_total_bytes);
    let download_total_label = format_total_bytes(status_snapshot.rx_total_bytes);
    let latency_label = format_latency(status_snapshot.latency);
    let primary_action_label = if should_offer_disconnect(&status_snapshot.phase) {
        "Disconnect"
    } else if !status_snapshot.vpn_permission_granted {
        "Grant VPN Permission"
    } else {
        "Connect"
    };

    rsx! {
        div {
            style: app_shell_style(&prefs_snapshot),
            div { dangerous_inner_html: format!("<style>{}</style>", ui_stylesheet()) }
            div {
                style: "position: relative; z-index: 1; width: min(100%, 560px); margin: 0 auto; display: flex; flex-direction: column; gap: 18px;",
                TopBar {
                    title: page_title,
                    subtitle: page_subtitle,
                    action_label: topbar_action_label,
                    on_action: move |_| {
                        if active_tab() == AppTab::Settings {
                            active_tab.set(AppTab::Home);
                        } else {
                            active_tab.set(AppTab::Settings);
                        }
                    },
                }

                match active_tab_snapshot {
                    AppTab::Home => rsx! {
                        div {
                            style: page_stack_style(&prefs_snapshot),
                            section {
                                style: status_card_style(connection_tone, &prefs_snapshot),
                                div {
                                    class: "row between gap-16 start wrap",
                                    div {
                                        class: "col gap-6",
                                        h1 {
                                            class: "m-0 text-2xl fw-600",
                                            style: "line-height: 1.15;",
                                            "{status_snapshot.phase}"
                                        }
                                        p {
                                            class: "m-0 text-base c-secondary",
                                            "{status_snapshot.detail}"
                                        }
                                    }
                                    div {
                                        class: "row wrap gap-8 end-x",
                                        StatusPill { label: format!("VPN {}", vpn_badge_label(&status_snapshot)), tone: if status_snapshot.vpn_active { AccentTone::Positive } else { AccentTone::Neutral } }
                                        StatusPill { label: summarize_protocol(&status_snapshot), tone: udp_status_tone(&status_snapshot) }
                                    }
                                }

                                div {
                                    style: "display: flex; justify-content: center; padding: 8px 0 2px;",
                                    button {
                                        style: button_style_with_state(
                                            "width: 196px; height: 196px; border: 1px solid rgba(255,255,255,0.08); border-radius: 999px; background: radial-gradient(circle at top, rgba(82,136,255,0.22), rgba(255,255,255,0.03)); color: #f3f7ff; font-size: 28px; font-weight: 700; letter-spacing: -0.03em; box-shadow: 0 18px 54px rgba(0,0,0,0.28);",
                                            false,
                                            can_connect || should_offer_disconnect(&status_snapshot.phase),
                                        ),
                                        disabled: !can_connect && !should_offer_disconnect(&status_snapshot.phase),
                                        onclick: {
                                            let controller = controller.clone();
                                            let snapshot = form();
                                            move |_| {
                                                if should_offer_disconnect(&status_snapshot.phase) {
                                                    auto_connect_after_vpn_permission.set(false);
                                                    controller.send(AppCommand::Disconnect);
                                                } else if !has_required_connection_fields(&snapshot) {
                                                    append_log(&mut logs, "import a valid config before connecting".to_string());
                                                    active_tab.set(AppTab::Settings);
                                                } else if !status_snapshot.vpn_permission_granted {
                                                    auto_connect_after_vpn_permission.set(true);
                                                    controller.send(AppCommand::RequestVpnPermission);
                                                } else {
                                                    controller.send(AppCommand::StartManagedVpn(snapshot.clone()));
                                                }
                                            }
                                        },
                                        "{primary_action_label}"
                                    }
                                }

                                div {
                                    class: "grid-2 gap-12",
                                    MetricCard {
                                        eyebrow: "Upload",
                                        value: upload_rate_label.clone(),
                                        detail: "Real-time upstream traffic through the active tunnel.".to_string(),
                                        tone: AccentTone::Accent,
                                    }
                                    MetricCard {
                                        eyebrow: "Download",
                                        value: download_rate_label.clone(),
                                        detail: "Real-time downstream traffic through the active tunnel.".to_string(),
                                        tone: AccentTone::Positive,
                                    }
                                    MetricCard {
                                        eyebrow: "Uploaded",
                                        value: upload_total_label.clone(),
                                        detail: "Cumulative bytes sent over the active QUIC connection.".to_string(),
                                        tone: AccentTone::Neutral,
                                    }
                                    MetricCard {
                                        eyebrow: "Downloaded",
                                        value: download_total_label.clone(),
                                        detail: "Cumulative bytes received over the active QUIC connection.".to_string(),
                                        tone: AccentTone::Neutral,
                                    }
                                }

                                div {
                                    class: "info-box col",
                                    StatusLine { label: "Node", value: current_node_label }
                                    StatusLine { label: "Config", value: current_config_label }
                                    StatusLine { label: "Endpoint", value: config_value_or_empty(&form_snapshot.server) }
                                    StatusLine { label: "Remote", value: display_or_dash(&status_snapshot.remote) }
                                    StatusLine { label: "Latency", value: latency_label.clone() }
                                    StatusLine { label: "Session", value: online_duration.clone() }
                                    StatusLine { label: "Trust", value: current_trust_label }
                                }
                                if !can_connect && !should_offer_disconnect(&status_snapshot.phase) {
                                    p {
                                        style: "margin: 14px 0 0; color: #fca5a5; font-size: 13px; line-height: 1.5;",
                                        "还没有可用配置。点右上角进入 Settings，粘贴 hysteria2:// 链接或手动填写服务器和认证信息。"
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Connection Summary",
                                    subtitle: "Home keeps the main path minimal: connect, watch the link, and jump into settings when you need to change anything.".to_string(),
                                }
                                StatusLine { label: "VPN", value: vpn_badge_label(&status_snapshot).to_string() }
                                StatusLine { label: "Transport", value: summarize_protocol(&status_snapshot) }
                                StatusLine { label: "Last connected", value: last_connected.clone() }
                                StatusLine { label: "Local SOCKS", value: status_snapshot.local_socks.clone() }
                                p {
                                    style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                    "Settings now owns direct import, manual editing, saved profiles, Android VPN controls, and diagnostics."
                                }
                            }
                        }
                    },
                    AppTab::Nodes => rsx! {
                        div {
                            style: page_stack_style(&prefs_snapshot),
                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Node Library",
                                    subtitle: "Manage the working draft, saved profile, and imported share data.".to_string(),
                                }
                                input {
                                    style: input_style(false, &prefs_snapshot),
                                    value: node_search_snapshot.clone(),
                                    placeholder: "Search by endpoint, SNI, or source",
                                    oninput: move |evt| node_search.set(evt.value()),
                                }
                                div {
                                    style: "display: flex; gap: 10px; flex-wrap: wrap; margin-top: 14px;",
                                    for filter in [NodeFilter::All, NodeFilter::Active, NodeFilter::Saved, NodeFilter::Imported] {
                                        button {
                                            style: filter_chip_style(node_filter_snapshot == filter),
                                            onclick: move |_| node_filter.set(filter),
                                            "{filter.label()}"
                                        }
                                    }
                                }
                                div {
                                    style: "display: flex; flex-direction: column; gap: 12px; margin-top: 16px;",
                                    if filtered_node_cards.is_empty() {
                                        EmptyState {
                                            title: "No matching nodes".to_string(),
                                            detail: "Try a different filter, or import a share URI to add another endpoint source.".to_string(),
                                        }
                                    } else {
                                        for card in filtered_node_cards.iter() {
                                            NodeItemCard {
                                                title: card.title.clone(),
                                                subtitle: card.subtitle.clone(),
                                                meta: card.meta.clone(),
                                                tags: card.tags.clone(),
                                                selected: card.selected,
                                                tone: card.tone,
                                                action_label: card.action_label,
                                                disabled: card.form.is_none(),
                                                onclick: {
                                                    let target_form = card.form.clone();
                                                    move |_| {
                                                        if let Some(target_form) = target_form.clone() {
                                                            form.set(target_form);
                                                        }
                                                    }
                                                },
                                            }
                                        }
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Import Share URI",
                                    subtitle: "Paste a hy2:// or hysteria2:// share link, then apply it into the draft.".to_string(),
                                }
                                textarea {
                                    style: input_style(true, &prefs_snapshot),
                                    value: form_snapshot.import_uri.clone(),
                                    placeholder: "Paste a hy2:// or hysteria2:// share URI here",
                                    oninput: move |evt| form.write().import_uri = evt.value(),
                                }
                                div {
                                    style: "display: flex; gap: 12px; margin-top: 14px; flex-wrap: wrap;",
                                    PrimaryButton {
                                        label: "Import URI",
                                        disabled: form_snapshot.import_uri.trim().is_empty(),
                                        secondary: false,
                                        onclick: move |_| {
                                            let imported = {
                                                let current = form();
                                                import_share_uri(&current.import_uri)
                                            };
                                            match imported {
                                                Ok(imported) => {
                                                    let import_uri = form().import_uri;
                                                    let mut next = imported;
                                                    next.import_uri = import_uri;
                                                    form.set(next);
                                                    append_log(&mut logs, "share URI imported into the draft".to_string());
                                                }
                                                Err(err) => append_log(&mut logs, format!("share URI import failed: {err:#}")),
                                            }
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Show Stats",
                                        disabled: false,
                                        secondary: true,
                                        onclick: move |_| {
                                            settings_return_tab.set(AppTab::Stats);
                                            active_tab.set(AppTab::Stats);
                                        },
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Active Draft",
                                    subtitle: "Keep Android aligned with the Linux client: server, auth, optional obfs/SNI, then connect. If CA path is empty, the app falls back to the system trust store.".to_string(),
                                }
                                FieldRow { label: "Server", placeholder: "Host:port or hy2:// URI", value: form_snapshot.server.clone(), oninput: move |value| form.write().server = value }
                                FieldRow { label: "Auth", placeholder: "Password or auth token", value: form_snapshot.auth.clone(), oninput: move |value| form.write().auth = value }
                                FieldRow { label: "Salamander", placeholder: "Optional obfs password", value: form_snapshot.obfs_password.clone(), oninput: move |value| form.write().obfs_password = value }
                                FieldRow { label: "TLS SNI", placeholder: "Optional SNI override", value: form_snapshot.sni.clone(), oninput: move |value| form.write().sni = value }
                                if prefs_snapshot.show_advanced_fields {
                                    div {
                                        style: "display: flex; flex-direction: column;",
                                        p {
                                            style: "margin: 12px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                            "Expert controls are only for explicit trust overrides, TLS debugging, bandwidth caps, and QUIC tuning."
                                        }
                                        FieldRow { label: "CA path", placeholder: "Optional PEM path inside app storage", value: form_snapshot.ca_path.clone(), oninput: move |value| form.write().ca_path = value }
                                        CaSelector {
                                            directory: ca_catalog_snapshot.directory.clone(),
                                            files: ca_catalog_snapshot.files.clone(),
                                            selected_path: form_snapshot.ca_path.clone(),
                                            onselect: move |path: String| {
                                                form.write().ca_path = path.clone();
                                                if path.trim().is_empty() {
                                                    append_log(&mut logs, "cleared CA path selection".to_string());
                                                } else {
                                                    append_log(&mut logs, format!("selected CA path: {path}"));
                                                }
                                            },
                                            onrefresh: move |_| {
                                                match android_bridge::query_ca_catalog() {
                                                    Ok(catalog) => {
                                                        let count = catalog.files.len();
                                                        let directory = catalog.directory.clone();
                                                        ca_catalog.set(catalog);
                                                        if count == 0 {
                                                            append_log(
                                                                &mut logs,
                                                                format!(
                                                                    "no CA files found in {}",
                                                                    config_value_or_empty(&directory)
                                                                ),
                                                            );
                                                        } else {
                                                            append_log(
                                                                &mut logs,
                                                                format!(
                                                                    "refreshed CA catalog: {count} file(s) in {directory}"
                                                                ),
                                                            );
                                                        }
                                                    }
                                                    Err(err) => append_log(
                                                        &mut logs,
                                                        format!("refresh CA catalog failed: {err:#}"),
                                                    ),
                                                }
                                            }
                                        }
                                        FieldRow { label: "pinSHA256", placeholder: "Optional certificate pin", value: form_snapshot.pin_sha256.clone(), oninput: move |value| form.write().pin_sha256 = value }
                                        FieldRow { label: "Bandwidth up", placeholder: "Optional, e.g. 100 Mbps", value: form_snapshot.bandwidth_up.clone(), oninput: move |value| form.write().bandwidth_up = value }
                                        FieldRow { label: "Bandwidth down", placeholder: "Optional, e.g. 500 Mbps", value: form_snapshot.bandwidth_down.clone(), oninput: move |value| form.write().bandwidth_down = value }
                                        FieldRow { label: "QUIC init stream window", placeholder: "Optional bytes, default 268435456", value: form_snapshot.quic_init_stream_receive_window.clone(), oninput: move |value| form.write().quic_init_stream_receive_window = value }
                                        FieldRow { label: "QUIC max stream window", placeholder: "Optional bytes, default 268435456", value: form_snapshot.quic_max_stream_receive_window.clone(), oninput: move |value| form.write().quic_max_stream_receive_window = value }
                                        FieldRow { label: "QUIC init conn window", placeholder: "Optional bytes, default 536870912", value: form_snapshot.quic_init_connection_receive_window.clone(), oninput: move |value| form.write().quic_init_connection_receive_window = value }
                                        FieldRow { label: "QUIC max conn window", placeholder: "Optional bytes, default 536870912", value: form_snapshot.quic_max_connection_receive_window.clone(), oninput: move |value| form.write().quic_max_connection_receive_window = value }
                                        FieldRow { label: "QUIC idle timeout", placeholder: "Optional duration, default 30s", value: form_snapshot.quic_max_idle_timeout.clone(), oninput: move |value| form.write().quic_max_idle_timeout = value }
                                        FieldRow { label: "QUIC keep alive", placeholder: "Optional duration, default 10s", value: form_snapshot.quic_keep_alive_period.clone(), oninput: move |value| form.write().quic_keep_alive_period = value }
                                    }
                                }
                                div {
                                    style: "display: flex; gap: 12px; align-items: center; margin-top: 14px; flex-wrap: wrap;",
                                    button {
                                        style: filter_chip_style(prefs_snapshot.show_advanced_fields),
                                        onclick: move |_| prefs.with_mut(|current| current.show_advanced_fields = !current.show_advanced_fields),
                                        if prefs_snapshot.show_advanced_fields { "Expert mode: On" } else { "Expert mode: Off" }
                                    }
                                    if show_udp_toggle_in_primary_controls() {
                                        button {
                                            style: filter_chip_style(form_snapshot.local_udp_enabled),
                                            onclick: move |_| {
                                                let next = !form().local_udp_enabled;
                                                form.write().local_udp_enabled = next;
                                            },
                                            if form_snapshot.local_udp_enabled { "UDP relay: ON" } else { "UDP relay: OFF" }
                                        }
                                    }
                                    if show_expert_transport_toggles(prefs_snapshot.show_advanced_fields) {
                                        button {
                                            style: filter_chip_style(form_snapshot.insecure_tls),
                                            onclick: move |_| {
                                                let next = !form().insecure_tls;
                                                form.write().insecure_tls = next;
                                            },
                                            if form_snapshot.insecure_tls { "TLS insecure: ON" } else { "TLS insecure: OFF" }
                                        }
                                        button {
                                            style: filter_chip_style(form_snapshot.quic_disable_path_mtu_discovery),
                                            onclick: move |_| {
                                                let next = !form().quic_disable_path_mtu_discovery;
                                                form.write().quic_disable_path_mtu_discovery = next;
                                            },
                                            if form_snapshot.quic_disable_path_mtu_discovery {
                                                "PMTUD disabled"
                                            } else {
                                                "PMTUD enabled"
                                            }
                                        }
                                    }
                                }
                                if !can_connect {
                                    p {
                                        style: "margin: 14px 0 0; color: #fca5a5; font-size: 13px;",
                                        "Server and Auth must be set before the Android connection flow can start."
                                    }
                                } else if !prefs_snapshot.show_advanced_fields {
                                    p {
                                        style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                        "Normal mobile flow is ready. UDP relay stays visible here; leave Expert mode off unless you need explicit CA files, TLS insecure, pinning, bandwidth caps, or QUIC tuning."
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Profile Storage",
                                    subtitle: "Persist the draft locally. Connect and disconnect stay on Home so the main Android path only has one place to start.".to_string(),
                                }
                                div {
                                    style: "display: flex; gap: 12px; flex-wrap: wrap;",
                                    PrimaryButton {
                                        label: "Save Profile",
                                        disabled: !can_connect,
                                        secondary: true,
                                        onclick: {
                                            let snapshot = form();
                                            move |_| match android_bridge::save_profile(&snapshot) {
                                                Ok(_) => {
                                                    saved_profile.set(Some(snapshot.clone()));
                                                    append_log(&mut logs, "saved current profile".to_string());
                                                }
                                                Err(err) => append_log(&mut logs, format!("save profile failed: {err:#}")),
                                            }
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Load Saved",
                                        disabled: !can_load_saved,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::query_saved_profile() {
                                            Ok(Some(saved)) => {
                                                form.set(saved.clone());
                                                saved_profile.set(Some(saved));
                                                append_log(&mut logs, "loaded saved profile into the draft".to_string());
                                            }
                                            Ok(None) => {
                                                saved_profile.set(None);
                                                append_log(&mut logs, "no saved profile found".to_string());
                                            }
                                            Err(err) => append_log(&mut logs, format!("load saved profile failed: {err:#}")),
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Clear Saved",
                                        disabled: !can_load_saved,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::clear_saved_profile() {
                                            Ok(_) => {
                                                saved_profile.set(None);
                                                append_log(&mut logs, "cleared saved profile".to_string());
                                            }
                                            Err(err) => append_log(&mut logs, format!("clear saved profile failed: {err:#}")),
                                        },
                                    }
                                }
                            }
                        }
                    },
                    AppTab::Stats => rsx! {
                        div {
                            style: page_stack_style(&prefs_snapshot),
                            div {
                                style: "display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 12px;",
                                MetricCard {
                                    eyebrow: "Successful connects",
                                    value: metrics_snapshot.successful_connections.to_string(),
                                    detail: "Cumulative successful sessions in this app run.",
                                    tone: AccentTone::Positive,
                                }
                                MetricCard {
                                    eyebrow: "Reconnects",
                                    value: metrics_snapshot.reconnect_count.to_string(),
                                    detail: "Automatic recovery attempts after closure.",
                                    tone: AccentTone::Warning,
                                }
                                MetricCard {
                                    eyebrow: "Errors",
                                    value: metrics_snapshot.error_count.to_string(),
                                    detail: "Transitions that ended in the Error phase.",
                                    tone: AccentTone::Danger,
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Runtime Overview",
                                    subtitle: "Operational counters and the last observed transport state.".to_string(),
                                }
                                div {
                                    class: "grid-2 gap-12",
                                    MetricCard {
                                        eyebrow: "Online time",
                                        value: online_duration,
                                        detail: "Only counts while the link remains active.",
                                        tone: AccentTone::Accent,
                                    }
                                    MetricCard {
                                        eyebrow: "Current TX cap",
                                        value: format_negotiated_rate(status_snapshot.negotiated_tx),
                                        detail: "Negotiated transmit allowance from the remote.",
                                        tone: AccentTone::Neutral,
                                    }
                                    MetricCard {
                                        eyebrow: "Last download",
                                        value: latest_download,
                                        detail: "Most recent built-in download probe.",
                                        tone: AccentTone::Positive,
                                    }
                                    MetricCard {
                                        eyebrow: "Last upload",
                                        value: latest_upload,
                                        detail: "Most recent built-in upload probe.",
                                        tone: AccentTone::Accent,
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Connection Health",
                                    subtitle: "Lightweight stability indicators derived from the live session.".to_string(),
                                }
                                MetricCard {
                                    eyebrow: "DNS failures",
                                    value: metrics_snapshot.dns_failure_count.to_string(),
                                    detail: "Requests that ended in local DNS proxy failure or SERVFAIL.",
                                    tone: if metrics_snapshot.dns_failure_count == 0 {
                                        AccentTone::Positive
                                    } else {
                                        AccentTone::Warning
                                    },
                                }
                                ProgressMetric {
                                    label: "Stability",
                                    value: health_score,
                                    detail: format!("{} successful connects, {} reconnects, {} errors, {} DNS failures.", metrics_snapshot.successful_connections, metrics_snapshot.reconnect_count, metrics_snapshot.error_count, metrics_snapshot.dns_failure_count),
                                    tone: AccentTone::Positive,
                                }
                                ProgressMetric {
                                    label: "VPN readiness",
                                    value: vpn_score,
                                    detail: vpn_badge_label(&status_snapshot).to_string(),
                                    tone: if status_snapshot.vpn_active { AccentTone::Positive } else { AccentTone::Warning },
                                }
                                ProgressMetric {
                                    label: "Transport health",
                                    value: transport_score,
                                    detail: summarize_protocol(&status_snapshot),
                                    tone: udp_status_tone(&status_snapshot),
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Session Details",
                                    subtitle: "The latest runtime values exposed by hysteria-core and Android.".to_string(),
                                }
                                StatusLine { label: "Phase", value: status_snapshot.phase.clone() }
                                StatusLine { label: "Remote", value: display_or_dash(&status_snapshot.remote) }
                                StatusLine { label: "UDP status", value: udp_status_label(&status_snapshot).to_string() }
                                StatusLine { label: "VPN available", value: bool_word(status_snapshot.vpn_available).to_string() }
                                StatusLine { label: "VPN permission", value: bool_word(status_snapshot.vpn_permission_granted).to_string() }
                                StatusLine { label: "VPN active", value: bool_word(status_snapshot.vpn_active).to_string() }
                                StatusLine { label: "Local SOCKS", value: status_snapshot.local_socks.clone() }
                                StatusLine { label: "Detail", value: status_snapshot.detail.clone() }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Recent Timeline",
                                    subtitle: "Newest diagnostic events first.".to_string(),
                                }
                                div {
                                    style: "display: flex; flex-direction: column; gap: 10px;",
                                    if stats_recent_logs.is_empty() {
                                        EmptyState {
                                            title: "No diagnostics yet".to_string(),
                                            detail: "The event timeline will populate once the runtime starts producing logs.".to_string(),
                                        }
                                    } else {
                                        for entry in stats_recent_logs.iter() {
                                            ActivityRow { message: entry.message.clone(), age: format_relative_time(entry.recorded_at, now) }
                                        }
                                    }
                                }
                                div {
                                    style: "display: flex; gap: 12px; margin-top: 14px; flex-wrap: wrap;",
                                    PrimaryButton {
                                        label: "Download Test",
                                        disabled: status_snapshot.phase != "Connected",
                                        secondary: false,
                                        onclick: {
                                            let controller = controller.clone();
                                            move |_| controller.send(AppCommand::Speedtest(SpeedDirection::Download))
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Upload Test",
                                        disabled: status_snapshot.phase != "Connected",
                                        secondary: true,
                                        onclick: {
                                            let controller = controller.clone();
                                            move |_| controller.send(AppCommand::Speedtest(SpeedDirection::Upload))
                                        },
                                    }
                                }
                            }
                        }
                    },
                    AppTab::Settings => rsx! {
                        div {
                            style: page_stack_style(&prefs_snapshot),
                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Direct Import",
                                    subtitle: "Paste a hysteria2:// link or a compatible YAML client config. This updates the active draft below.".to_string(),
                                }
                                textarea {
                                    style: input_style(true, &prefs_snapshot),
                                    value: form_snapshot.import_uri.clone(),
                                    placeholder: "Paste hysteria2://... or YAML here",
                                    oninput: move |evt| form.write().import_uri = evt.value(),
                                }
                                div {
                                    style: "display: flex; gap: 12px; margin-top: 14px; flex-wrap: wrap;",
                                    PrimaryButton {
                                        label: "Import Paste",
                                        disabled: form_snapshot.import_uri.trim().is_empty(),
                                        secondary: false,
                                        onclick: move |_| {
                                            let current = form();
                                            let raw_input = current.import_uri.clone();
                                            match apply_settings_import(&current, &raw_input) {
                                                Ok((imported, warning)) => {
                                                    let label = if raw_input.trim().starts_with("hy2://")
                                                        || raw_input.trim().starts_with("hysteria2://")
                                                    {
                                                        format!("Direct · {}", summarize_endpoint(&imported.server))
                                                    } else {
                                                        "Pasted config".to_string()
                                                    };
                                                    imported_config_name.set(label);
                                                    form.set(imported.clone());
                                                    append_log(
                                                        &mut logs,
                                                        format!(
                                                            "settings import applied -> {}",
                                                            config_value_or_empty(&imported.server)
                                                        ),
                                                    );
                                                    if let Some(warning) = warning {
                                                        append_log(&mut logs, warning);
                                                    }
                                                }
                                                Err(err) => append_log(
                                                    &mut logs,
                                                    format!("settings import failed: {err:#}"),
                                                ),
                                            }
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Import File",
                                        disabled: false,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::request_config_import() {
                                            Ok(_) => append_log(&mut logs, "opened Android config picker".to_string()),
                                            Err(err) => append_log(&mut logs, format!("open config picker failed: {err:#}")),
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Import Cert",
                                        disabled: false,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::request_ca_import() {
                                            Ok(_) => append_log(&mut logs, "opened Android certificate picker".to_string()),
                                            Err(err) => append_log(&mut logs, format!("open certificate picker failed: {err:#}")),
                                        },
                                    }
                                }
                                p {
                                    style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                    "示例: hysteria2://token@hi.wedevs.org:12443/?obfs=salamander&obfs-password=...&sni=hi.wedevs.org"
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Connection Draft",
                                    subtitle: "Everything needed to connect now lives here. Home only shows the live session.".to_string(),
                                }
                                FieldRow { label: "Server", placeholder: "Host:port or hy2:// URI", value: form_snapshot.server.clone(), oninput: move |value| form.write().server = value }
                                FieldRow { label: "Auth", placeholder: "Password or auth token", value: form_snapshot.auth.clone(), oninput: move |value| form.write().auth = value }
                                FieldRow { label: "Salamander", placeholder: "Optional obfs password", value: form_snapshot.obfs_password.clone(), oninput: move |value| form.write().obfs_password = value }
                                FieldRow { label: "TLS SNI", placeholder: "Optional SNI override", value: form_snapshot.sni.clone(), oninput: move |value| form.write().sni = value }
                                div {
                                    style: "display: flex; gap: 12px; align-items: center; margin-top: 14px; flex-wrap: wrap;",
                                    button {
                                        style: filter_chip_style(prefs_snapshot.show_advanced_fields),
                                        onclick: move |_| prefs.with_mut(|current| current.show_advanced_fields = !current.show_advanced_fields),
                                        if prefs_snapshot.show_advanced_fields { "Expert mode: On" } else { "Expert mode: Off" }
                                    }
                                    if show_udp_toggle_in_primary_controls() {
                                        button {
                                            style: filter_chip_style(form_snapshot.local_udp_enabled),
                                            onclick: move |_| {
                                                let next = !form().local_udp_enabled;
                                                form.write().local_udp_enabled = next;
                                            },
                                            if form_snapshot.local_udp_enabled { "UDP relay: ON" } else { "UDP relay: OFF" }
                                        }
                                    }
                                    if show_expert_transport_toggles(prefs_snapshot.show_advanced_fields) {
                                        button {
                                            style: filter_chip_style(form_snapshot.insecure_tls),
                                            onclick: move |_| {
                                                let next = !form().insecure_tls;
                                                form.write().insecure_tls = next;
                                            },
                                            if form_snapshot.insecure_tls { "TLS insecure: ON" } else { "TLS insecure: OFF" }
                                        }
                                        button {
                                            style: filter_chip_style(form_snapshot.quic_disable_path_mtu_discovery),
                                            onclick: move |_| {
                                                let next = !form().quic_disable_path_mtu_discovery;
                                                form.write().quic_disable_path_mtu_discovery = next;
                                            },
                                            if form_snapshot.quic_disable_path_mtu_discovery {
                                                "PMTUD disabled"
                                            } else {
                                                "PMTUD enabled"
                                            }
                                        }
                                    }
                                }
                                if prefs_snapshot.show_advanced_fields {
                                    FieldRow { label: "CA path", placeholder: "Optional PEM path inside app storage", value: form_snapshot.ca_path.clone(), oninput: move |value| form.write().ca_path = value }
                                    CaSelector {
                                        directory: ca_catalog_snapshot.directory.clone(),
                                        files: ca_catalog_snapshot.files.clone(),
                                        selected_path: form_snapshot.ca_path.clone(),
                                        onselect: move |path: String| {
                                            form.write().ca_path = path.clone();
                                            if path.trim().is_empty() {
                                                append_log(&mut logs, "cleared CA path selection".to_string());
                                            } else {
                                                append_log(&mut logs, format!("selected CA path: {path}"));
                                            }
                                        },
                                        onrefresh: move |_| {
                                            match android_bridge::query_ca_catalog() {
                                                Ok(catalog) => {
                                                    let count = catalog.files.len();
                                                    let directory = catalog.directory.clone();
                                                    ca_catalog.set(catalog);
                                                    append_log(
                                                        &mut logs,
                                                        format!("refreshed CA catalog: {count} file(s) in {directory}"),
                                                    );
                                                }
                                                Err(err) => append_log(
                                                    &mut logs,
                                                    format!("refresh CA catalog failed: {err:#}"),
                                                ),
                                            }
                                        }
                                    }
                                    FieldRow { label: "pinSHA256", placeholder: "Optional certificate pin", value: form_snapshot.pin_sha256.clone(), oninput: move |value| form.write().pin_sha256 = value }
                                    FieldRow { label: "Bandwidth up", placeholder: "Optional, e.g. 100 Mbps", value: form_snapshot.bandwidth_up.clone(), oninput: move |value| form.write().bandwidth_up = value }
                                    FieldRow { label: "Bandwidth down", placeholder: "Optional, e.g. 500 Mbps", value: form_snapshot.bandwidth_down.clone(), oninput: move |value| form.write().bandwidth_down = value }
                                    FieldRow { label: "QUIC init stream window", placeholder: "Optional bytes, default 268435456", value: form_snapshot.quic_init_stream_receive_window.clone(), oninput: move |value| form.write().quic_init_stream_receive_window = value }
                                    FieldRow { label: "QUIC max stream window", placeholder: "Optional bytes, default 268435456", value: form_snapshot.quic_max_stream_receive_window.clone(), oninput: move |value| form.write().quic_max_stream_receive_window = value }
                                    FieldRow { label: "QUIC init conn window", placeholder: "Optional bytes, default 536870912", value: form_snapshot.quic_init_connection_receive_window.clone(), oninput: move |value| form.write().quic_init_connection_receive_window = value }
                                    FieldRow { label: "QUIC max conn window", placeholder: "Optional bytes, default 536870912", value: form_snapshot.quic_max_connection_receive_window.clone(), oninput: move |value| form.write().quic_max_connection_receive_window = value }
                                    FieldRow { label: "QUIC idle timeout", placeholder: "Optional duration, default 30s", value: form_snapshot.quic_max_idle_timeout.clone(), oninput: move |value| form.write().quic_max_idle_timeout = value }
                                    FieldRow { label: "QUIC keep alive", placeholder: "Optional duration, default 10s", value: form_snapshot.quic_keep_alive_period.clone(), oninput: move |value| form.write().quic_keep_alive_period = value }
                                } else {
                                    p {
                                        style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                        "高级模式里可以管理显式 CA、TLS insecure、证书 pin、带宽和 QUIC 参数。"
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Profile Storage",
                                    subtitle: "Persist the current draft locally, then return to Home to connect.".to_string(),
                                }
                                div {
                                    style: "display: flex; gap: 12px; flex-wrap: wrap;",
                                    PrimaryButton {
                                        label: "Save Profile",
                                        disabled: !can_connect,
                                        secondary: true,
                                        onclick: {
                                            let snapshot = form();
                                            move |_| match android_bridge::save_profile(&snapshot) {
                                                Ok(_) => {
                                                    saved_profile.set(Some(snapshot.clone()));
                                                    append_log(&mut logs, "saved current profile".to_string());
                                                }
                                                Err(err) => append_log(&mut logs, format!("save profile failed: {err:#}")),
                                            }
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Load Saved",
                                        disabled: !can_load_saved,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::query_saved_profile() {
                                            Ok(Some(saved)) => {
                                                form.set(saved.clone());
                                                saved_profile.set(Some(saved));
                                                append_log(&mut logs, "loaded saved profile into the draft".to_string());
                                            }
                                            Ok(None) => {
                                                saved_profile.set(None);
                                                append_log(&mut logs, "no saved profile found".to_string());
                                            }
                                            Err(err) => append_log(&mut logs, format!("load saved profile failed: {err:#}")),
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Clear Saved",
                                        disabled: !can_load_saved,
                                        secondary: true,
                                        onclick: move |_| match android_bridge::clear_saved_profile() {
                                            Ok(_) => {
                                                saved_profile.set(None);
                                                append_log(&mut logs, "cleared saved profile".to_string());
                                            }
                                            Err(err) => append_log(&mut logs, format!("clear saved profile failed: {err:#}")),
                                        },
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Android VPN",
                                    subtitle: "Permission and service controls for the system tunnel.".to_string(),
                                }
                                SettingRow {
                                    label: "Permission",
                                    detail: (if status_snapshot.vpn_permission_granted {
                                        "Android has already granted VPN permission."
                                    } else {
                                        "Permission is required before the app can own a system tunnel."
                                    })
                                    .to_string(),
                                    control: rsx! {
                                        button {
                                            style: filter_chip_style(status_snapshot.vpn_permission_granted),
                                            onclick: {
                                                let controller = controller.clone();
                                                move |_| controller.send(AppCommand::RequestVpnPermission)
                                            },
                                            if status_snapshot.vpn_permission_granted { "Granted" } else { "Request" }
                                        }
                                    },
                                }
                                SettingRow {
                                    label: "VPN service",
                                    detail: vpn_badge_label(&status_snapshot).to_string(),
                                    control: rsx! {
                                        button {
                                            style: filter_chip_style(status_snapshot.vpn_active),
                                            onclick: {
                                                let controller = controller.clone();
                                                let snapshot = form();
                                                move |_| {
                                                    if status_snapshot.vpn_active {
                                                        controller.send(AppCommand::StopVpnShell);
                                                    } else {
                                                        controller.send(AppCommand::StartManagedVpn(snapshot.clone()));
                                                    }
                                                }
                                            },
                                            if status_snapshot.vpn_active { "Stop" } else { "Start" }
                                        }
                                    },
                                }
                                SettingRow {
                                    label: "Launch automation",
                                    detail: describe_launch_automation(launch_automation_snapshot),
                                    control: rsx! {
                                        StatusPill {
                                            label: "Read only".to_string(),
                                            tone: AccentTone::Neutral,
                                        }
                                    },
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Diagnostics",
                                    subtitle: "Collapsed by default. Expand for session details, logs, and built-in probes.".to_string(),
                                }
                                div {
                                    style: "display: flex; justify-content: space-between; gap: 12px; align-items: center; flex-wrap: wrap;",
                                    StatusPill {
                                        label: if diagnostics_expanded_snapshot { "Expanded".to_string() } else { "Collapsed".to_string() },
                                        tone: if diagnostics_expanded_snapshot { AccentTone::Accent } else { AccentTone::Neutral },
                                    }
                                    button {
                                        style: filter_chip_style(diagnostics_expanded_snapshot),
                                        onclick: move |_| diagnostics_expanded.set(!diagnostics_expanded()),
                                        if diagnostics_expanded_snapshot { "Hide Diagnostics" } else { "Show Diagnostics" }
                                    }
                                }
                                if diagnostics_expanded_snapshot {
                                    div {
                                        style: "display: flex; flex-direction: column; gap: 0; margin-top: 14px; border-radius: 18px; background: rgba(255,255,255,0.03); border: 1px solid rgba(255,255,255,0.06); padding: 4px 16px;",
                                        StatusLine { label: "Phase", value: status_snapshot.phase.clone() }
                                        StatusLine { label: "Remote", value: display_or_dash(&status_snapshot.remote) }
                                        StatusLine { label: "Latency", value: latency_label.clone() }
                                        StatusLine { label: "Upload total", value: upload_total_label.clone() }
                                        StatusLine { label: "Download total", value: download_total_label.clone() }
                                        StatusLine { label: "Local SOCKS", value: status_snapshot.local_socks.clone() }
                                        StatusLine { label: "Detail", value: status_snapshot.detail.clone() }
                                    }
                                    div {
                                        style: "display: flex; gap: 12px; margin-top: 14px; flex-wrap: wrap;",
                                        PrimaryButton {
                                            label: "Download Test",
                                            disabled: status_snapshot.phase != "Connected",
                                            secondary: false,
                                            onclick: {
                                                let controller = controller.clone();
                                                move |_| controller.send(AppCommand::Speedtest(SpeedDirection::Download))
                                            },
                                        }
                                        PrimaryButton {
                                            label: "Upload Test",
                                            disabled: status_snapshot.phase != "Connected",
                                            secondary: true,
                                            onclick: {
                                                let controller = controller.clone();
                                                move |_| controller.send(AppCommand::Speedtest(SpeedDirection::Upload))
                                            },
                                        }
                                    }
                                    div {
                                        style: "display: flex; flex-direction: column; gap: 10px; margin-top: 14px;",
                                        if stats_recent_logs.is_empty() {
                                            EmptyState {
                                                title: "No diagnostics yet".to_string(),
                                                detail: "Runtime events will appear here after import, connect, reconnect, and speedtest actions.".to_string(),
                                            }
                                        } else {
                                            for entry in stats_recent_logs.iter() {
                                                ActivityRow { message: entry.message.clone(), age: format_relative_time(entry.recorded_at, now) }
                                            }
                                        }
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "About",
                                    subtitle: "Build metadata and the current mobile runtime footprint.".to_string(),
                                }
                                StatusLine { label: "App version", value: env!("CARGO_PKG_VERSION").to_string() }
                                StatusLine { label: "UI runtime", value: "Dioxus 0.7 mobile".to_string() }
                                StatusLine { label: "Core transport", value: "hysteria-core over QUIC".to_string() }
                                StatusLine { label: "Current endpoint", value: config_value_or_empty(&form_snapshot.server) }
                                StatusLine { label: "Import count", value: metrics_snapshot.import_count.to_string() }
                            }
                        }
                    },
                }
            }

            if false {
                BottomNav {
                    active: active_tab_snapshot,
                    onselect: move |tab| {
                        settings_return_tab.set(tab);
                        active_tab.set(tab);
                    },
                }
            }
        }
    }
}

#[component]
fn TopBar(
    title: String,
    subtitle: String,
    action_label: Option<&'static str>,
    on_action: EventHandler<()>,
) -> Element {
    rsx! {
        section {
            style: "position: sticky; top: 0; z-index: 3; padding: 10px 0 12px; background: linear-gradient(180deg, rgba(16,19,26,0.96) 0%, rgba(16,19,26,0.88) 70%, rgba(16,19,26,0) 100%); backdrop-filter: blur(12px);",
            div {
                class: "row between gap-12 center",
                div {
                    class: "col gap-4 min-w-0",
                    span {
                        class: "text-22 fw-600 tracking-title",
                        "{title}"
                    }
                    p {
                        class: "m-0 text-base c-secondary",
                        "{subtitle}"
                    }
                }
                if let Some(action_label) = action_label {
                    button {
                        style: topbar_icon_button_style(),
                        onclick: move |_| on_action.call(()),
                        "{action_label}"
                    }
                }
            }
        }
    }
}

#[component]
fn BottomNav(active: AppTab, onselect: EventHandler<AppTab>) -> Element {
    rsx! {
        nav {
            style: bottom_nav_style(),
            for tab in [AppTab::Home, AppTab::Nodes, AppTab::Stats] {
                button {
                    style: bottom_nav_item_style(active == tab),
                    onclick: move |_| onselect.call(tab),
                    "{tab.label()}"
                }
            }
        }
    }
}

#[component]
fn SectionHeader(title: &'static str, subtitle: String) -> Element {
    rsx! {
        div {
            class: "section-header",
            h2 {
                class: "m-0 text-xl fw-600 tracking-tight",
                "{title}"
            }
            p {
                class: "m-0 text-sm c-secondary",
                "{subtitle}"
            }
        }
    }
}

#[component]
fn FieldRow(
    label: &'static str,
    placeholder: &'static str,
    value: String,
    oninput: EventHandler<String>,
) -> Element {
    rsx! {
        label {
            class: "field-label",
            span { class: "text-sm c-secondary", "{label}" }
            input {
                style: input_style(false, &UiPrefs::default()),
                value: value.clone(),
                placeholder: "{placeholder}",
                oninput: move |evt| oninput.call(evt.value()),
            }
        }
    }
}

#[component]
fn CaSelector(
    directory: String,
    files: Vec<android_bridge::CaFile>,
    selected_path: String,
    onselect: EventHandler<String>,
    onrefresh: EventHandler<()>,
) -> Element {
    let has_directory = !directory.trim().is_empty();
    let has_selected_path = !selected_path.trim().is_empty();
    rsx! {
        div {
            class: "col gap-10 mt-12",
            div {
                class: "row between gap-12 start wrap",
                div {
                    class: "col gap-6",
                    span { class: "text-sm c-secondary", "Installed CAs" }
                    p {
                        class: "m-0 text-xs c-muted",
                        if has_directory {
                            "ADB directory: {directory}"
                        } else {
                            "ADB directory will appear when the Android bridge is ready."
                        }
                    }
                }
                div {
                    class: "row wrap gap-8",
                    button {
                        style: filter_chip_style(false),
                        onclick: move |_| onrefresh.call(()),
                        "Refresh"
                    }
                    if has_selected_path {
                        button {
                            style: filter_chip_style(false),
                            onclick: move |_| onselect.call(String::new()),
                            "Clear Selection"
                        }
                    }
                }
            }
            if files.is_empty() {
                p {
                    class: "m-0 text-xs c-muted",
                    "No CA files found. Push a .crt or .pem file into the directory above, then refresh."
                }
            } else {
                div {
                    class: "col gap-10",
                    for file in files {
                        button {
                            style: selectable_item_style(selected_path == file.path),
                            onclick: {
                                let file_path = file.path.clone();
                                move |_| onselect.call(file_path.clone())
                            },
                            div {
                                class: "col start gap-4 w-full",
                                strong {
                                    class: "text-base c-primary fw-600",
                                    if selected_path == file.path {
                                        "Using {file.name}"
                                    } else {
                                        "Use {file.name}"
                                    }
                                }
                                span {
                                    class: "text-xs c-secondary text-left",
                                    "{file.path}"
                                }
                            }
                        }
                    }
                }
            }
            if has_selected_path {
                p {
                    class: "m-0 text-xs c-muted",
                    "Selected path: {selected_path}"
                }
            }
        }
    }
}

#[component]
fn StatusLine(label: &'static str, value: String) -> Element {
    rsx! {
        div {
            class: "status-line",
            strong {
                class: "status-line-label c-primary text-base fw-500",
                "{label}"
            }
            span {
                class: "status-line-value c-secondary text-base",
                "{value}"
            }
        }
    }
}

#[component]
fn StatusPill(label: String, tone: AccentTone) -> Element {
    rsx! {
        span {
            style: pill_style(tone),
            "{label}"
        }
    }
}

#[component]
fn MetricCard(eyebrow: &'static str, value: String, detail: String, tone: AccentTone) -> Element {
    rsx! {
        div {
            style: metric_card_style(tone),
            span {
                style: "font-size: 12px; color: #a8b3c7; text-transform: uppercase; letter-spacing: 0.1em;",
                "{eyebrow}"
            }
            strong {
                style: "font-size: 24px; line-height: 1.1; letter-spacing: -0.03em; color: #f3f7ff; margin-top: 6px;",
                "{value}"
            }
            p {
                style: "margin: 8px 0 0; color: #a8b3c7; font-size: 13px; line-height: 1.5;",
                "{detail}"
            }
        }
    }
}

#[component]
fn PrimaryButton(
    label: &'static str,
    disabled: bool,
    secondary: bool,
    onclick: EventHandler<()>,
) -> Element {
    rsx! {
        button {
            style: button_style_with_state(button_surface_style(secondary), secondary, !disabled),
            disabled: disabled,
            onclick: move |_| onclick.call(()),
            "{label}"
        }
    }
}

#[component]
fn NodeItemCard(
    title: String,
    subtitle: String,
    meta: String,
    tags: Vec<String>,
    selected: bool,
    tone: AccentTone,
    action_label: &'static str,
    disabled: bool,
    onclick: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            style: node_card_style(selected, tone),
            div {
                class: "row between gap-12 start",
                div {
                    class: "col gap-6",
                    strong {
                        class: "text-lg c-primary fw-600",
                        "{title}"
                    }
                    span {
                        class: "text-sm c-secondary",
                        "{subtitle}"
                    }
                }
                StatusPill {
                    label: if selected { "Selected".to_string() } else { "Available".to_string() },
                    tone: if selected { AccentTone::Accent } else { tone },
                }
            }
            p {
                class: "m-0 text-sm c-muted mt-12",
                "{meta}"
            }
            div {
                class: "tag-row mt-14",
                for tag in tags {
                    StatusPill { label: tag, tone: tone }
                }
            }
            button {
                style: button_style_with_state(button_surface_style(true), true, !disabled),
                disabled: disabled,
                onclick: move |_| onclick.call(()),
                "{action_label}"
            }
        }
    }
}

#[component]
fn ProgressMetric(label: &'static str, value: u8, detail: String, tone: AccentTone) -> Element {
    rsx! {
        div {
            class: "col gap-8 mt-14",
            div {
                class: "row between gap-12",
                strong { class: "text-base c-primary", "{label}" }
                span { class: "text-sm c-secondary", "{value}%" }
            }
            div {
                style: progress_track_style(),
                div { style: progress_fill_style(value, tone) }
            }
            p {
                class: "m-0 text-sm c-muted",
                "{detail}"
            }
        }
    }
}

#[component]
fn ActivityRow(message: String, age: String) -> Element {
    rsx! {
        div {
            class: "activity-row",
            span { class: "text-base c-primary", "{message}" }
            span { class: "text-xs c-muted nowrap", "{age}" }
        }
    }
}

#[component]
fn EmptyState(title: String, detail: String) -> Element {
    rsx! {
        div {
            class: "empty-state",
            strong { class: "text-lg c-primary", "{title}" }
            p { class: "m-0 text-sm c-secondary", "{detail}" }
        }
    }
}

#[component]
fn SettingRow(label: &'static str, detail: String, control: Element) -> Element {
    rsx! {
        div {
            class: "setting-row",
            div {
                class: "col gap-6",
                strong { class: "text-base c-primary fw-500", "{label}" }
                p { class: "m-0 text-sm c-secondary", "{detail}" }
            }
            {control}
        }
    }
}

fn append_log(logs: &mut Signal<Vec<LogEntry>>, message: String) {
    let mut lines = logs.write();
    lines.insert(0, LogEntry::new(message));
    if lines.len() > MAX_LOG_LINES {
        lines.truncate(MAX_LOG_LINES);
    }
}

fn build_mobile_quic_transport(form: &FormState) -> Result<QuicTransportConfig> {
    Ok(QuicTransportConfig {
        stream_receive_window: resolve_mobile_quic_window(
            parse_optional_u64_field(
                "quic.initStreamReceiveWindow",
                &form.quic_init_stream_receive_window,
            )?,
            parse_optional_u64_field(
                "quic.maxStreamReceiveWindow",
                &form.quic_max_stream_receive_window,
            )?,
            LARGE_STREAM_WINDOW,
            "quic.initStreamReceiveWindow",
            "quic.maxStreamReceiveWindow",
        )?,
        receive_window: resolve_mobile_quic_window(
            parse_optional_u64_field(
                "quic.initConnReceiveWindow",
                &form.quic_init_connection_receive_window,
            )?,
            parse_optional_u64_field(
                "quic.maxConnReceiveWindow",
                &form.quic_max_connection_receive_window,
            )?,
            LARGE_CONN_WINDOW,
            "quic.initConnReceiveWindow",
            "quic.maxConnReceiveWindow",
        )?,
        max_idle_timeout: resolve_mobile_quic_idle_timeout(&form.quic_max_idle_timeout)?,
        keep_alive_interval: Some(resolve_mobile_keep_alive_period(
            &form.quic_keep_alive_period,
        )?),
        max_concurrent_bidi_streams: None,
        disable_path_mtu_discovery: form.quic_disable_path_mtu_discovery,
    })
}

fn parse_optional_u64_field(field: &str, input: &str) -> Result<u64> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Ok(0);
    }
    trimmed.parse().with_context(|| format!("invalid {field}"))
}

fn resolve_mobile_quic_window(
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

fn resolve_mobile_quic_idle_timeout(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(DEFAULT_MAX_IDLE_TIMEOUT)
    } else {
        let value = humantime::parse_duration(trimmed)
            .with_context(|| "invalid quic.maxIdleTimeout".to_string())?;
        if !(Duration::from_secs(4)..=Duration::from_secs(120)).contains(&value) {
            bail!("quic.maxIdleTimeout must be between 4s and 120s");
        }
        Ok(value)
    }
}

fn resolve_mobile_keep_alive_period(input: &str) -> Result<Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        Ok(DEFAULT_KEEP_ALIVE_PERIOD)
    } else {
        let value = humantime::parse_duration(trimmed)
            .with_context(|| "invalid quic.keepAlivePeriod".to_string())?;
        if !(Duration::from_secs(2)..=Duration::from_secs(60)).contains(&value) {
            bail!("quic.keepAlivePeriod must be between 2s and 60s");
        }
        Ok(value)
    }
}

fn parse_bandwidth_field(field: &str, input: &str) -> Result<u64> {
    let value = parse_bandwidth(input).with_context(|| format!("invalid {field}"))?;
    Ok(value)
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

async fn connect_client(form: FormState) -> Result<(Client, UiStatus)> {
    let normalized = normalize_form(form)?;
    let server_addr = resolve_socket_addr(&normalized.server)?;
    let server_name = infer_server_name(&normalized.server, &normalized.sni)?;
    let root_certificates = load_root_certificates(&normalized.ca_path)?;
    let pinned = parse_optional_pinned_sha256(&normalized.pin_sha256)?;

    let mut config = CoreClientConfig::new(server_addr, server_name);
    config.auth = normalized.auth.clone();
    config.bandwidth_max_tx = parse_bandwidth_field("bandwidth.up", &normalized.bandwidth_up)?;
    config.bandwidth_max_rx = parse_bandwidth_field("bandwidth.down", &normalized.bandwidth_down)?;
    config.obfs = build_obfs_config(&normalized.obfs_password)?;
    config.tls = ClientTlsConfig {
        insecure: normalized.insecure_tls,
        root_certificates,
        pinned_certificate_sha256: pinned,
    };
    config.quic = build_mobile_quic_transport(&normalized)?;

    let (client, info) = Client::connect(config)
        .await
        .context("failed to connect hysteria client")?;
    let status = UiStatus {
        phase: "Connected".to_string(),
        remote: client.remote_addr().to_string(),
        detail: "Client connected. Local SOCKS is ready and Android system VPN can be started."
            .to_string(),
        server_udp_supported: info.udp_enabled,
        local_udp_enabled: !local_socks_udp_disabled(&normalized),
        udp_enabled: info.udp_enabled && !local_socks_udp_disabled(&normalized),
        negotiated_tx: info.tx,
        ..UiStatus::default()
    };
    Ok((client, status))
}

async fn run_speedtest(client: &Client, direction: SpeedDirection) -> Result<String> {
    let stream = client
        .tcp(SPEEDTEST_ADDR)
        .await
        .with_context(|| "failed to connect speedtest stream")?;
    let mut speedtest = SpeedtestClient::new(stream);
    let summary = match direction {
        SpeedDirection::Download => speedtest.download(0, DEFAULT_TEST_DURATION, |_| {}).await?,
        SpeedDirection::Upload => speedtest.upload(0, DEFAULT_TEST_DURATION, |_| {}).await?,
    };
    Ok(format!(
        "{} speedtest complete: bytes={} elapsed={} average={}",
        direction.label(),
        summary.bytes,
        humantime::format_duration(summary.elapsed),
        format_speed(summary.bytes, summary.elapsed)
    ))
}

fn format_speed(bytes: u64, duration: Duration) -> String {
    if duration.is_zero() {
        return "0.00 bps".to_string();
    }

    let mut speed = bytes as f64 / duration.as_secs_f64() * 8.0;
    let units = ["bps", "Kbps", "Mbps", "Gbps"];
    let mut unit_index = 0usize;
    while speed > 1000.0 && unit_index < units.len() - 1 {
        speed /= 1000.0;
        unit_index += 1;
    }
    format!("{speed:.2} {}", units[unit_index])
}

fn normalize_form(mut form: FormState) -> Result<FormState> {
    if form.server.trim().starts_with("hy2://") || form.server.trim().starts_with("hysteria2://") {
        let imported = import_share_uri(&form.server)?;
        if form.auth.trim().is_empty() {
            form.auth = imported.auth;
        }
        if form.obfs_password.trim().is_empty() {
            form.obfs_password = imported.obfs_password;
        }
        if form.sni.trim().is_empty() {
            form.sni = imported.sni;
        }
        if form.pin_sha256.trim().is_empty() {
            form.pin_sha256 = imported.pin_sha256;
        }
        if form.server.trim().starts_with("hy2://")
            || form.server.trim().starts_with("hysteria2://")
        {
            form.server = imported.server;
        }
        form.insecure_tls = form.insecure_tls || imported.insecure_tls;
    }

    if form.server.trim().is_empty() {
        bail!("server must not be empty");
    }
    if form.auth.trim().is_empty() {
        bail!("auth must not be empty");
    }
    Ok(form)
}

fn import_share_uri(input: &str) -> Result<FormState> {
    let uri = Url::parse(input.trim()).context("failed to parse share URI")?;
    match uri.scheme() {
        "hy2" | "hysteria2" => {}
        other => bail!("unsupported URI scheme {other}"),
    }

    let host = uri
        .host_str()
        .ok_or_else(|| anyhow!("share URI is missing a host"))?;
    let server = match uri.port() {
        Some(port) if host.contains(':') => format!("[{host}]:{port}"),
        Some(port) => format!("{host}:{port}"),
        None if host.contains(':') => format!("[{host}]:443"),
        None => format!("{host}:443"),
    };
    let auth = match (uri.username(), uri.password()) {
        ("", None) => String::new(),
        (user, Some(password)) => format!(
            "{}:{}",
            decode_userinfo_component(user)?,
            decode_userinfo_component(password)?
        ),
        (user, None) => decode_userinfo_component(user)?,
    };

    let query = uri
        .query_pairs()
        .collect::<std::collections::HashMap<_, _>>();
    let obfs_password = query
        .get("obfs-password")
        .map(|value| value.to_string())
        .unwrap_or_default();
    let obfs_type = query
        .get("obfs")
        .map(|value| value.to_ascii_lowercase())
        .unwrap_or_default();
    if !obfs_type.is_empty() && obfs_type != "plain" && obfs_type != "salamander" {
        bail!("unsupported obfs type {obfs_type}");
    }

    Ok(FormState {
        import_uri: input.trim().to_string(),
        server,
        auth,
        obfs_password: if obfs_type == "salamander" {
            obfs_password
        } else {
            String::new()
        },
        sni: query
            .get("sni")
            .map(|value| value.to_string())
            .unwrap_or_default(),
        ca_path: String::new(),
        pin_sha256: query
            .get("pinSHA256")
            .map(|value| value.to_string())
            .unwrap_or_default(),
        bandwidth_up: String::new(),
        bandwidth_down: String::new(),
        quic_init_stream_receive_window: String::new(),
        quic_max_stream_receive_window: String::new(),
        quic_init_connection_receive_window: String::new(),
        quic_max_connection_receive_window: String::new(),
        quic_max_idle_timeout: String::new(),
        quic_keep_alive_period: String::new(),
        local_udp_enabled: true,
        quic_disable_path_mtu_discovery: false,
        insecure_tls: query
            .get("insecure")
            .and_then(|value| parse_bool_like(value))
            .unwrap_or(false),
    })
}

fn parse_imported_client_document(input: &str) -> Result<(FormState, Option<String>)> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        bail!("imported config is empty");
    }

    if trimmed.starts_with("hy2://") || trimmed.starts_with("hysteria2://") {
        return Ok((import_share_uri(trimmed)?, None));
    }

    let config: ImportedClientConfig =
        serde_yaml::from_str(trimmed).context("failed to parse imported YAML config")?;

    let mut ca_warning = None;
    let ca_path = match config.tls.ca.trim() {
        "" => String::new(),
        path if Path::new(path).exists() => path.to_string(),
        path => {
            ca_warning = Some(format!(
                "imported tls.ca `{path}` is not available on Android; import the certificate separately"
            ));
            String::new()
        }
    };

    Ok((
        FormState {
            import_uri: String::new(),
            server: config.server.trim().to_string(),
            auth: config.auth.trim().to_string(),
            obfs_password: if config.obfs.r#type.eq_ignore_ascii_case("salamander") {
                config.obfs.salamander.password
            } else {
                String::new()
            },
            sni: config.tls.sni,
            ca_path,
            pin_sha256: config.tls.pin_sha256,
            bandwidth_up: config.bandwidth.up,
            bandwidth_down: config.bandwidth.down,
            quic_init_stream_receive_window: optional_u64_text(
                config.quic.init_stream_receive_window,
            ),
            quic_max_stream_receive_window: optional_u64_text(
                config.quic.max_stream_receive_window,
            ),
            quic_init_connection_receive_window: optional_u64_text(
                config.quic.init_connection_receive_window,
            ),
            quic_max_connection_receive_window: optional_u64_text(
                config.quic.max_connection_receive_window,
            ),
            quic_max_idle_timeout: optional_duration_text(config.quic.max_idle_timeout),
            quic_keep_alive_period: optional_duration_text(config.quic.keep_alive_period),
            local_udp_enabled: true,
            quic_disable_path_mtu_discovery: config.quic.disable_path_mtu_discovery,
            insecure_tls: config.tls.insecure,
        },
        ca_warning,
    ))
}

fn optional_u64_text(value: u64) -> String {
    if value == 0 {
        String::new()
    } else {
        value.to_string()
    }
}

fn optional_duration_text(value: Duration) -> String {
    if value.is_zero() {
        String::new()
    } else {
        humantime::format_duration(value).to_string()
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
                    bail!("invalid percent-encoding in URI auth");
                }
                let high = hex_nibble(bytes[index + 1])?;
                let low = hex_nibble(bytes[index + 2])?;
                decoded.push((high << 4) | low);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }

    String::from_utf8(decoded).context("URI auth is not valid UTF-8")
}

fn build_obfs_config(password: &str) -> Result<Option<ObfsConfig>> {
    let password = password.trim();
    if password.is_empty() {
        Ok(None)
    } else {
        Ok(Some(ObfsConfig::Salamander {
            password: password.to_string(),
        }))
    }
}

fn infer_server_name(server: &str, sni: &str) -> Result<String> {
    if !sni.trim().is_empty() {
        return Ok(sni.trim().to_string());
    }

    if let Ok(addr) = server.parse::<SocketAddr>() {
        return Ok(addr.ip().to_string());
    }

    if server.starts_with('[') {
        if let Some(end) = server.find(']') {
            return Ok(server[1..end].to_string());
        }
    }

    if let Some((host, port)) = server.rsplit_once(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(host.to_string());
        }
    }

    bail!("failed to infer server name; set TLS SNI explicitly")
}

fn resolve_socket_addr(input: &str) -> Result<SocketAddr> {
    let normalized = normalize_server_addr(input)?;
    normalized
        .to_socket_addrs()
        .with_context(|| format!("failed to resolve {normalized}"))?
        .next()
        .ok_or_else(|| anyhow!("no socket addresses resolved for {normalized}"))
}

fn normalize_server_addr(input: &str) -> Result<String> {
    let input = input.trim();
    if input.is_empty() {
        bail!("server must not be empty");
    }
    if input.parse::<SocketAddr>().is_ok() {
        return Ok(input.to_string());
    }
    if input.starts_with('[') {
        return if input.contains("]:") {
            Ok(input.to_string())
        } else {
            Ok(format!("{input}:443"))
        };
    }
    if input.parse::<IpAddr>().is_ok() {
        return match input.parse::<IpAddr>()? {
            IpAddr::V4(_) => Ok(format!("{input}:443")),
            IpAddr::V6(addr) => Ok(format!("[{addr}]:443")),
        };
    }

    if let Some((_, port)) = input.rsplit_once(':') {
        if port.parse::<u16>().is_ok() {
            return Ok(input.to_string());
        }
    }
    if input.contains(':') && input.parse::<Ipv6Addr>().is_ok() {
        return Ok(format!("[{input}]:443"));
    }
    Ok(format!("{input}:443"))
}

fn load_root_certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    load_root_certificates_with(path, load_system_root_certificates)
}

fn load_root_certificates_with<F>(
    path: &str,
    system_loader: F,
) -> Result<Vec<CertificateDer<'static>>>
where
    F: FnOnce() -> Result<Vec<CertificateDer<'static>>>,
{
    if path.trim().is_empty() {
        return system_loader();
    }

    let file = File::open(Path::new(path))
        .with_context(|| format!("failed to open certificate file {path}"))?;
    let mut reader = BufReader::new(file);
    parse_pem_certificates(&mut reader, path)
}

fn parse_pem_certificates<R>(
    reader: &mut R,
    source: &str,
) -> Result<Vec<CertificateDer<'static>>>
where
    R: BufRead,
{
    let certs = rustls_pemfile::certs(reader)
        .with_context(|| format!("failed to parse PEM certificates from {source}"))?;
    if certs.is_empty() {
        bail!("no certificates found in {source}");
    }
    Ok(certs.into_iter().map(CertificateDer::from).collect())
}

fn load_system_root_certificates_with<F, G>(
    platform_loader: Option<F>,
    native_loader: G,
) -> Result<Vec<CertificateDer<'static>>>
where
    F: FnOnce() -> Result<Vec<CertificateDer<'static>>>,
    G: FnOnce() -> Result<Vec<CertificateDer<'static>>>,
{
    if let Some(loader) = platform_loader {
        return loader().context("failed to load platform system root certificates");
    }

    native_loader()
}

#[cfg(target_os = "android")]
fn load_android_system_root_certificates() -> Result<Vec<CertificateDer<'static>>> {
    let bundle = android_bridge::system_ca_pem_bundle()
        .context("failed to read Android system CA store")?;
    let mut reader = Cursor::new(bundle.into_bytes());
    parse_pem_certificates(&mut reader, "Android system trust store")
}

fn load_native_system_root_certificates() -> Result<Vec<CertificateDer<'static>>> {
    let result = rustls_native_certs::load_native_certs();
    if !result.certs.is_empty() {
        return Ok(result.certs);
    }

    if result.errors.is_empty() {
        bail!(
            "no system root certificates found; set an explicit CA path or enable TLS insecure for testing"
        );
    }

    let details = result
        .errors
        .into_iter()
        .map(|err| err.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    bail!("failed to load system root certificates: {details}");
}

fn load_system_root_certificates() -> Result<Vec<CertificateDer<'static>>> {
    #[cfg(target_os = "android")]
    {
        return load_system_root_certificates_with(
            Some(load_android_system_root_certificates as fn() -> Result<Vec<CertificateDer<'static>>>),
            load_native_system_root_certificates,
        );
    }

    #[cfg(not(target_os = "android"))]
    {
        load_system_root_certificates_with(
            None::<fn() -> Result<Vec<CertificateDer<'static>>>>,
            load_native_system_root_certificates,
        )
    }
}

fn parse_optional_pinned_sha256(input: &str) -> Result<Option<[u8; 32]>> {
    let normalized = normalize_cert_hash(input);
    if normalized.is_empty() {
        return Ok(None);
    }
    if normalized.len() != 64 {
        bail!("pinSHA256 must be exactly 64 hex characters");
    }

    let mut output = [0_u8; 32];
    for (index, chunk) in normalized.as_bytes().chunks_exact(2).enumerate() {
        let high = hex_nibble(chunk[0])?;
        let low = hex_nibble(chunk[1])?;
        output[index] = (high << 4) | low;
    }
    Ok(Some(output))
}

fn normalize_cert_hash(hash: &str) -> String {
    hash.trim().to_ascii_lowercase().replace([':', '-'], "")
}

fn parse_bool_like(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" => Some(true),
        "0" | "false" | "f" | "no" | "n" => Some(false),
        _ => None,
    }
}

fn hex_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => bail!("invalid hex digit"),
    }
}

fn record_status_transition(metrics: &mut Signal<UiMetrics>, previous: &UiStatus, next: &UiStatus) {
    let now = Instant::now();
    let was_active = is_active_phase(&previous.phase);
    let is_active = is_active_phase(&next.phase);

    metrics.with_mut(|current| {
        if next.phase == "Connected"
            && previous.phase != "Connected"
            && previous.phase != "Speedtest"
        {
            current.successful_connections = current.successful_connections.saturating_add(1);
            current.connected_since = Some(now);
            current.last_connected_at = Some(now);
        } else if !was_active && is_active && current.connected_since.is_none() {
            current.connected_since = Some(now);
        }

        if previous.phase != "Reconnecting" && next.phase == "Reconnecting" {
            current.reconnect_count = current.reconnect_count.saturating_add(1);
            current.connected_since = None;
        }

        if previous.phase != "Error" && next.phase == "Error" {
            current.error_count = current.error_count.saturating_add(1);
        }

        if !is_active {
            current.connected_since = None;
        }
    });
}

fn record_log_metrics(metrics: &mut Signal<UiMetrics>, message: &str) {
    metrics.with_mut(|current| {
        if message.contains("share URI imported") {
            current.import_count = current.import_count.saturating_add(1);
        }
        if let Some(average) = parse_speedtest_average(message, "download") {
            current.latest_download = Some(average);
        }
        if let Some(average) = parse_speedtest_average(message, "upload") {
            current.latest_upload = Some(average);
        }
    });
}

fn note_dns_failure(metrics: &mut UiMetrics) {
    metrics.dns_failure_count = metrics.dns_failure_count.saturating_add(1);
}

fn record_dns_failure(metrics: &mut Signal<UiMetrics>) {
    metrics.with_mut(|current| {
        note_dns_failure(current);
    });
}

fn parse_speedtest_average(message: &str, direction: &str) -> Option<String> {
    let prefix = format!("{direction} speedtest complete:");
    if !message.starts_with(&prefix) {
        return None;
    }

    message
        .split("average=")
        .nth(1)
        .map(|value| value.trim().to_string())
}

fn build_node_cards(
    form: &FormState,
    saved_profile: Option<&FormState>,
    status: &UiStatus,
) -> Vec<NodeCardData> {
    let mut cards = vec![NodeCardData {
        kind: NodeCardKind::ActiveDraft,
        title: profile_title(form),
        subtitle: format!(
            "{} / auth {}",
            config_value_or_empty(&form.server),
            config_presence(&form.auth)
        ),
        meta: if form.sni.trim().is_empty() {
            "Working draft currently shown in the editor.".to_string()
        } else {
            format!("Working draft with TLS SNI {}", form.sni.trim())
        },
        tags: vec![
            "Draft".to_string(),
            if form.insecure_tls {
                "TLS insecure".to_string()
            } else {
                "TLS verified".to_string()
            },
            if form.obfs_password.trim().is_empty() {
                "Plain".to_string()
            } else {
                "Salamander".to_string()
            },
        ],
        selected: true,
        tone: phase_tone(&status.phase),
        form: Some(form.clone()),
        action_label: "Use draft",
    }];

    if let Some(saved) = saved_profile {
        cards.push(NodeCardData {
            kind: NodeCardKind::SavedProfile,
            title: profile_title(saved),
            subtitle: format!(
                "{} / auth {}",
                config_value_or_empty(&saved.server),
                config_presence(&saved.auth)
            ),
            meta: "Stored locally in Android SharedPreferences.".to_string(),
            tags: vec![
                "Saved".to_string(),
                if saved.insecure_tls {
                    "TLS insecure".to_string()
                } else {
                    "TLS verified".to_string()
                },
            ],
            selected: saved == form,
            tone: AccentTone::Positive,
            form: Some(saved.clone()),
            action_label: "Load saved",
        });
    }

    if !form.import_uri.trim().is_empty() {
        cards.push(NodeCardData {
            kind: NodeCardKind::ImportedShare,
            title: "Imported share".to_string(),
            subtitle: trim_middle(form.import_uri.trim(), 42),
            meta: "Share URI parsed and applied into the active draft.".to_string(),
            tags: vec![
                "Imported".to_string(),
                if form.pin_sha256.trim().is_empty() {
                    "No pin".to_string()
                } else {
                    "Pinned".to_string()
                },
            ],
            selected: false,
            tone: AccentTone::Warning,
            form: Some(form.clone()),
            action_label: "Apply import",
        });
    }

    if !status.remote.trim().is_empty() {
        cards.push(NodeCardData {
            kind: NodeCardKind::LiveSession,
            title: "Live session".to_string(),
            subtitle: status.remote.clone(),
            meta: status.detail.clone(),
            tags: vec![
                "Connected".to_string(),
                udp_status_short_tag(status).to_string(),
            ],
            selected: status.phase == "Connected" || status.phase == "Speedtest",
            tone: AccentTone::Accent,
            form: None,
            action_label: "Live only",
        });
    }

    cards
}

fn node_filter_matches(filter: NodeFilter, kind: NodeCardKind) -> bool {
    match filter {
        NodeFilter::All => true,
        NodeFilter::Active => matches!(kind, NodeCardKind::ActiveDraft | NodeCardKind::LiveSession),
        NodeFilter::Saved => kind == NodeCardKind::SavedProfile,
        NodeFilter::Imported => kind == NodeCardKind::ImportedShare,
    }
}

fn node_search_matches(search: &str, card: &NodeCardData) -> bool {
    let search = search.trim().to_ascii_lowercase();
    if search.is_empty() {
        return true;
    }

    let mut haystack =
        format!("{} {} {}", card.title, card.subtitle, card.meta).to_ascii_lowercase();
    for tag in &card.tags {
        haystack.push(' ');
        haystack.push_str(&tag.to_ascii_lowercase());
    }
    haystack.contains(&search)
}

fn phase_tone(phase: &str) -> AccentTone {
    match phase {
        "Connected" => AccentTone::Positive,
        "Speedtest" => AccentTone::Accent,
        "Connecting" | "Reconnecting" | "Starting VPN" => AccentTone::Warning,
        "Error" => AccentTone::Danger,
        _ => AccentTone::Neutral,
    }
}

fn should_offer_disconnect(phase: &str) -> bool {
    matches!(
        phase,
        "Connected" | "Connecting" | "Reconnecting" | "Speedtest" | "Starting VPN"
    )
}

fn is_active_phase(phase: &str) -> bool {
    matches!(phase, "Connected" | "Speedtest")
}

fn vpn_action_label(status: &UiStatus) -> &'static str {
    if !status.vpn_permission_granted {
        "Request Permission"
    } else if status.vpn_active {
        "Stop System VPN"
    } else {
        "Start System VPN"
    }
}

fn vpn_badge_label(status: &UiStatus) -> &'static str {
    if status.vpn_active {
        "Active"
    } else if status.vpn_permission_granted {
        "Ready"
    } else if status.vpn_available {
        "Needs permission"
    } else {
        "Unavailable"
    }
}

fn udp_status_label(status: &UiStatus) -> &'static str {
    if status.udp_enabled {
        "UDP relay ready"
    } else if !status.local_udp_enabled {
        "UDP disabled locally"
    } else if !status.server_udp_supported {
        "Server UDP unavailable"
    } else {
        "UDP unavailable"
    }
}

fn udp_status_short_tag(status: &UiStatus) -> &'static str {
    if status.udp_enabled {
        "UDP"
    } else if !status.local_udp_enabled {
        "UDP off"
    } else if !status.server_udp_supported {
        "Server TCP only"
    } else {
        "TCP only"
    }
}

fn udp_status_tone(status: &UiStatus) -> AccentTone {
    if status.udp_enabled {
        AccentTone::Accent
    } else if !status.local_udp_enabled || !status.server_udp_supported {
        AccentTone::Neutral
    } else {
        AccentTone::Warning
    }
}

fn summarize_protocol(status: &UiStatus) -> String {
    let udp = udp_status_label(status);
    if status.remote.trim().is_empty() {
        format!("QUIC client idle / {udp}")
    } else {
        format!("QUIC active / {udp}")
    }
}

fn profile_title(form: &FormState) -> String {
    let server = form.server.trim();
    if server.is_empty() {
        "Draft profile".to_string()
    } else {
        summarize_endpoint(server)
    }
}

fn summarize_endpoint(server: &str) -> String {
    let server = server.trim();
    if server.is_empty() {
        return "Draft profile".to_string();
    }

    if let Some(stripped) = server.strip_prefix('[')
        && let Some((host, _)) = stripped.split_once("]:")
    {
        return host.to_string();
    }

    if let Some((host, port)) = server.rsplit_once(':')
        && port.parse::<u16>().is_ok()
    {
        return host.to_string();
    }

    server.to_string()
}

fn display_or_dash(value: &str) -> String {
    if value.trim().is_empty() {
        "--".to_string()
    } else {
        value.trim().to_string()
    }
}

fn bool_word(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn format_negotiated_rate(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        "Adaptive".to_string()
    } else {
        format_bytes_per_second(bytes_per_second)
    }
}

fn format_bytes_per_second(bytes_per_second: u64) -> String {
    if bytes_per_second == 0 {
        return "0 B/s".to_string();
    }
    let mut value = bytes_per_second as f64;
    let units = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < units.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.1} {}", units[unit_index])
}

fn format_total_bytes(bytes: u64) -> String {
    let mut value = bytes as f64;
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < units.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{} {}", bytes, units[unit_index])
    } else {
        format!("{value:.1} {}", units[unit_index])
    }
}

fn format_latency(latency: Option<Duration>) -> String {
    match latency {
        Some(value) => format!("{} ms", value.as_millis()),
        None => "--".to_string(),
    }
}

fn format_elapsed(started: Instant, now: Instant) -> String {
    let seconds = now.duration_since(started).as_secs();
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let seconds = seconds % 60;
    if hours > 0 {
        format!("{hours:02}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

fn format_relative_time(instant: Instant, now: Instant) -> String {
    let elapsed = now.duration_since(instant).as_secs();
    match elapsed {
        0..=4 => "just now".to_string(),
        5..=59 => format!("{elapsed}s ago"),
        60..=3599 => format!("{}m ago", elapsed / 60),
        _ => format!("{}h ago", elapsed / 3600),
    }
}

fn connection_health_score(metrics: &UiMetrics, status: &UiStatus) -> u8 {
    let mut score: u32 = if metrics.successful_connections == 0 {
        28
    } else {
        88
    };
    score = score.saturating_sub((metrics.reconnect_count * 8).min(24));
    score = score.saturating_sub((metrics.error_count * 15).min(45));
    if matches!(status.phase.as_str(), "Connected" | "Speedtest") {
        score = (score + 8).min(100);
    }
    score as u8
}

fn vpn_health_score(status: &UiStatus) -> u8 {
    if status.vpn_active {
        100
    } else if status.vpn_permission_granted {
        76
    } else if status.vpn_available {
        42
    } else {
        0
    }
}

fn transport_health_score(status: &UiStatus) -> u8 {
    if status.remote.trim().is_empty() {
        24
    } else if status.udp_enabled {
        92
    } else {
        64
    }
}

fn trim_middle(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let keep = max.saturating_sub(3) / 2;
    let start: String = value.chars().take(keep).collect();
    let end: String = value
        .chars()
        .rev()
        .take(keep)
        .collect::<String>()
        .chars()
        .rev()
        .collect();
    format!("{start}...{end}")
}

fn ui_stylesheet() -> &'static str {
    concat!(
        ".col{display:flex;flex-direction:column}",
        ".row{display:flex;flex-direction:row}",
        ".wrap{flex-wrap:wrap}",
        ".center{align-items:center}",
        ".start{align-items:flex-start}",
        ".between{justify-content:space-between}",
        ".end-x{justify-content:flex-end}",
        ".w-full{width:100%;box-sizing:border-box}",
        ".min-w-0{min-width:0}",
        ".gap-4{gap:4px}",
        ".gap-6{gap:6px}",
        ".gap-8{gap:8px}",
        ".gap-10{gap:10px}",
        ".gap-12{gap:12px}",
        ".gap-14{gap:14px}",
        ".gap-16{gap:16px}",
        ".gap-18{gap:18px}",
        ".grid-2{display:grid;grid-template-columns:repeat(2,minmax(0,1fr))}",
        ".text-xs{font-size:12px;line-height:1.5}",
        ".text-sm{font-size:13px;line-height:1.5}",
        ".text-base{font-size:14px;line-height:1.5}",
        ".text-lg{font-size:16px;line-height:1.4}",
        ".text-xl{font-size:20px;line-height:1.3}",
        ".text-2xl{font-size:24px;line-height:1.15}",
        ".text-22{font-size:22px;line-height:1.2}",
        ".c-primary{color:var(--text-primary)}",
        ".c-secondary{color:var(--text-secondary)}",
        ".c-muted{color:var(--text-muted)}",
        ".fw-500{font-weight:500}",
        ".fw-600{font-weight:600}",
        ".m-0{margin:0}",
        ".mt-6{margin-top:6px}",
        ".mt-8{margin-top:8px}",
        ".mt-12{margin-top:12px}",
        ".mt-14{margin-top:14px}",
        ".nowrap{white-space:nowrap}",
        ".text-right{text-align:right}",
        ".text-left{text-align:left}",
        ".tracking-tight{letter-spacing:-0.02em}",
        ".tracking-tighter{letter-spacing:-0.03em}",
        ".tracking-title{letter-spacing:-0.01em}",
        ".uppercase{text-transform:uppercase;letter-spacing:0.1em}",
        ".section-header{display:flex;flex-direction:column;gap:6px;margin-bottom:14px}",
        ".field-label{display:flex;flex-direction:column;gap:8px;margin-top:12px}",
        ".status-line{display:flex;justify-content:space-between;gap:16px;padding:10px 0;border-bottom:1px solid rgba(255,255,255,0.06);align-items:flex-start}",
        ".status-line-label{flex:0 0 112px;min-width:112px}",
        ".status-line-value{flex:1;min-width:0;text-align:right;overflow-wrap:anywhere;word-break:break-word}",
        ".setting-row{display:flex;justify-content:space-between;gap:16px;padding:14px 0;border-bottom:1px solid rgba(255,255,255,0.06);align-items:center}",
        ".activity-row{display:flex;justify-content:space-between;gap:14px;padding:12px 14px;border-radius:16px;background:rgba(255,255,255,0.03);border:1px solid rgba(255,255,255,0.05)}",
        ".empty-state{display:flex;flex-direction:column;gap:6px;padding:18px;border-radius:18px;background:rgba(255,255,255,0.03);border:1px dashed rgba(255,255,255,0.08)}",
        ".info-box{border-radius:18px;background:rgba(255,255,255,0.03);border:1px solid rgba(255,255,255,0.06);padding:4px 16px}",
        ".tag-row{display:flex;gap:8px;flex-wrap:wrap}",
    )
}

fn app_shell_style(prefs: &UiPrefs) -> String {
    format!(
        concat!(
            "--bg-main: #10131A;",
            "--surface: #171B22;",
            "--surface-container: #1D222B;",
            "--surface-container-high: #232A34;",
            "--bg-accent-a: rgba(255,111,145,0.24);",
            "--bg-accent-b: rgba(96,165,250,0.22);",
            "--bg-accent-c: rgba(251,191,36,0.18);",
            "--bg-accent-d: rgba(52,211,153,0.16);",
            "--text-primary: #F3F6FB;",
            "--text-secondary: #BCC5D3;",
            "--text-muted: #8893A5;",
            "--accent: #8AB4F8;",
            "--accent-strong: #A8C7FA;",
            "--success: #81C995;",
            "--warning: #FBCB65;",
            "--danger: #F28B82;",
            "--border: rgba(255,255,255,0.08);",
            "--motion: {}ms;",
            "font-family: system-ui, -apple-system, BlinkMacSystemFont, \"Segoe UI\", sans-serif;",
            "min-height: 100vh;",
            "padding: {}px 16px 112px;",
            "background:",
            "radial-gradient(circle at top left, var(--bg-accent-a) 0%, transparent 42%),",
            "radial-gradient(circle at top right, var(--bg-accent-b) 0%, transparent 36%),",
            "radial-gradient(circle at 18% 78%, var(--bg-accent-c) 0%, transparent 28%),",
            "radial-gradient(circle at 82% 72%, var(--bg-accent-d) 0%, transparent 30%),",
            "linear-gradient(180deg, #120F1E 0%, #0E1624 42%, #0A1118 100%);",
            "color: var(--text-primary);",
            "position: relative;",
            "overflow-x: hidden;"
        ),
        if prefs.motion_enabled { 220 } else { 0 },
        if prefs.compact_layout { 16 } else { 20 },
    )
}

fn page_stack_style(prefs: &UiPrefs) -> String {
    format!(
        "display: flex; flex-direction: column; gap: {}px;",
        prefs.section_gap()
    )
}

fn panel_style(prefs: &UiPrefs) -> String {
    format!(
        concat!(
            "display: flex; flex-direction: column; gap: {}px;",
            "padding: {}px;",
            "border-radius: 24px;",
            "background: linear-gradient(180deg, rgba(255,255,255,0.045), rgba(255,255,255,0.018)), var(--surface-container);",
            "border: 1px solid var(--border);",
            "box-shadow: 0 8px 24px rgba(0,0,0,0.16);"
        ),
        prefs.content_gap(),
        prefs.card_padding()
    )
}

fn status_card_style(tone: AccentTone, prefs: &UiPrefs) -> String {
    format!(
        concat!(
            "display: flex; flex-direction: column; gap: {}px;",
            "padding: {}px;",
            "border-radius: 24px;",
            "background: {};",
            "border: 1px solid var(--border);",
            "box-shadow: 0 10px 28px rgba(0,0,0,0.18);"
        ),
        prefs.content_gap(),
        prefs.card_padding(),
        tone_gradient(tone)
    )
}

fn input_style(multiline: bool, prefs: &UiPrefs) -> String {
    let extra = if multiline {
        "min-height: 110px; resize: vertical;"
    } else {
        ""
    };
    format!(
        concat!(
            "width: 100%; box-sizing: border-box;",
            "padding: {}px 14px;",
            "border-radius: 14px;",
            "border: 1px solid rgba(255,255,255,0.08);",
            "background: rgba(18,26,43,0.92);",
            "color: #F3F7FF;",
            "font-size: 14px;",
            "line-height: 1.5;",
            "outline: none;",
            "transition: border-color var(--motion), background var(--motion);",
            "{}"
        ),
        if prefs.compact_layout { 11 } else { 13 },
        extra
    )
}

fn topbar_icon_button_style() -> &'static str {
    "padding: 10px 14px; border-radius: 20px; border: 1px solid var(--border); background: var(--surface-container-high); color: var(--text-primary); font-size: 13px; font-weight: 500; transition: background var(--motion), border-color var(--motion);"
}

fn bottom_nav_style() -> &'static str {
    "position: fixed; left: 0; right: 0; bottom: 0; width: 100%; box-sizing: border-box; display: grid; grid-template-columns: repeat(3, minmax(0, 1fr)); gap: 8px; padding: 10px 12px calc(env(safe-area-inset-bottom, 0px) + 10px); background: rgba(23,27,34,0.96); border-top: 1px solid var(--border); backdrop-filter: blur(12px);"
}

fn bottom_nav_item_style(active: bool) -> String {
    if active {
        "padding: 12px 10px; border-radius: 18px; border: 1px solid transparent; background: rgba(138,180,248,0.18); color: var(--text-primary); font-size: 13px; font-weight: 600;".to_string()
    } else {
        "padding: 12px 10px; border-radius: 18px; border: 1px solid transparent; background: transparent; color: var(--text-secondary); font-size: 13px; font-weight: 500;".to_string()
    }
}

fn pill_style(tone: AccentTone) -> String {
    format!(
        "display: inline-flex; align-items: center; gap: 6px; padding: 8px 12px; border-radius: 999px; border: 1px solid {}; background: {}; color: #F3F7FF; font-size: 12px; line-height: 1; letter-spacing: 0.04em;",
        tone_border(tone),
        tone_fill(tone)
    )
}

fn metric_card_style(tone: AccentTone) -> String {
    format!(
        concat!(
            "display: flex; flex-direction: column; min-height: 136px;",
            "padding: 18px;",
            "border-radius: 20px;",
            "border: 1px solid {};",
            "background: linear-gradient(180deg, {}, var(--surface-container-high));",
            "box-shadow: inset 0 1px 0 rgba(255,255,255,0.02);"
        ),
        tone_border(tone),
        tone_fill(tone)
    )
}

fn button_surface_style(secondary: bool) -> &'static str {
    if secondary {
        "padding: 14px 16px; border-radius: 20px; border: 1px solid rgba(168,199,250,0.18); background: linear-gradient(180deg, rgba(255,255,255,0.05), rgba(255,255,255,0.02)); color: #F3F6FB; font-size: 14px; font-weight: 600; box-shadow: inset 0 1px 0 rgba(255,255,255,0.03); appearance: none; -webkit-appearance: none; transition: background var(--motion), border-color var(--motion), color var(--motion);"
    } else {
        "padding: 14px 16px; border-radius: 20px; border: 1px solid transparent; background: #8AB4F8; color: #0B1220; font-size: 14px; font-weight: 600; box-shadow: inset 0 1px 0 rgba(255,255,255,0.14); appearance: none; -webkit-appearance: none; transition: background var(--motion), border-color var(--motion), color var(--motion);"
    }
}

fn node_card_style(selected: bool, tone: AccentTone) -> String {
    let border = if selected {
        "rgba(138,180,248,0.42)"
    } else {
        tone_border(tone)
    };
    let background = if selected {
        "linear-gradient(180deg, rgba(138,180,248,0.14), var(--surface-container-high))"
    } else {
        "linear-gradient(180deg, rgba(255,255,255,0.02), var(--surface-container-high))"
    };
    format!(
        "display: flex; flex-direction: column; gap: 14px; padding: 18px; border-radius: 20px; border: 1px solid {border}; background: {background};"
    )
}

fn progress_track_style() -> &'static str {
    "width: 100%; height: 10px; border-radius: 999px; background: rgba(255,255,255,0.06); overflow: hidden;"
}

fn progress_fill_style(value: u8, tone: AccentTone) -> String {
    format!(
        "width: {}%; height: 100%; border-radius: 999px; background: linear-gradient(90deg, {}, {});",
        value.min(100),
        tone_solid(tone),
        tone_highlight(tone)
    )
}

fn filter_chip_style(active: bool) -> String {
    if active {
        "padding: 10px 12px; border-radius: 18px; border: 1px solid transparent; background: rgba(138,180,248,0.18); color: var(--text-primary); font-size: 13px; font-weight: 600;".to_string()
    } else {
        "padding: 10px 12px; border-radius: 18px; border: 1px solid var(--border); background: var(--surface-container-high); color: var(--text-secondary); font-size: 13px; font-weight: 500;".to_string()
    }
}

fn selectable_item_style(active: bool) -> String {
    if active {
        "display: flex; width: 100%; box-sizing: border-box; padding: 14px 16px; border-radius: 18px; border: 1px solid transparent; background: rgba(138,180,248,0.18); color: var(--text-primary); transition: background var(--motion), border-color var(--motion);".to_string()
    } else {
        "display: flex; width: 100%; box-sizing: border-box; padding: 14px 16px; border-radius: 18px; border: 1px solid var(--border); background: var(--surface-container-high); color: var(--text-primary); transition: background var(--motion), border-color var(--motion);".to_string()
    }
}

fn tone_gradient(tone: AccentTone) -> &'static str {
    match tone {
        AccentTone::Positive => {
            "linear-gradient(180deg, rgba(129,201,149,0.16) 0%, var(--surface-container-high) 100%)"
        }
        AccentTone::Accent => {
            "linear-gradient(180deg, rgba(138,180,248,0.20) 0%, var(--surface-container-high) 100%)"
        }
        AccentTone::Warning => {
            "linear-gradient(180deg, rgba(251,203,101,0.18) 0%, var(--surface-container-high) 100%)"
        }
        AccentTone::Danger => {
            "linear-gradient(180deg, rgba(242,139,130,0.18) 0%, var(--surface-container-high) 100%)"
        }
        AccentTone::Neutral => {
            "linear-gradient(180deg, var(--surface-container) 0%, var(--surface-container-high) 100%)"
        }
    }
}

fn tone_border(tone: AccentTone) -> &'static str {
    match tone {
        AccentTone::Positive => "rgba(129,201,149,0.28)",
        AccentTone::Accent => "rgba(138,180,248,0.28)",
        AccentTone::Warning => "rgba(251,203,101,0.28)",
        AccentTone::Danger => "rgba(242,139,130,0.28)",
        AccentTone::Neutral => "rgba(255,255,255,0.10)",
    }
}

fn tone_fill(tone: AccentTone) -> &'static str {
    match tone {
        AccentTone::Positive => "rgba(129,201,149,0.10)",
        AccentTone::Accent => "rgba(138,180,248,0.12)",
        AccentTone::Warning => "rgba(251,203,101,0.12)",
        AccentTone::Danger => "rgba(242,139,130,0.12)",
        AccentTone::Neutral => "rgba(255,255,255,0.04)",
    }
}

fn tone_solid(tone: AccentTone) -> &'static str {
    match tone {
        AccentTone::Positive => "#81C995",
        AccentTone::Accent => "#8AB4F8",
        AccentTone::Warning => "#FBCB65",
        AccentTone::Danger => "#F28B82",
        AccentTone::Neutral => "#8893A5",
    }
}

fn tone_highlight(tone: AccentTone) -> &'static str {
    match tone {
        AccentTone::Positive => "#A8DAB5",
        AccentTone::Accent => "#A8C7FA",
        AccentTone::Warning => "#FDD97D",
        AccentTone::Danger => "#F6AEA9",
        AccentTone::Neutral => "#BCC5D3",
    }
}
