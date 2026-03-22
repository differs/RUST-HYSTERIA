mod android_bridge;
mod local_socks;
mod vpn_tun2socks;

use std::{
    fs::File,
    io::BufReader,
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
use dioxus::prelude::*;
use futures_timer::Delay;
use hysteria_core::{
    Client, ClientConfig as CoreClientConfig, ClientTlsConfig, DEFAULT_KEEP_ALIVE_PERIOD,
    DEFAULT_MAX_IDLE_TIMEOUT, ObfsConfig, QuicTransportConfig,
};
use hysteria_extras::speedtest::{Client as SpeedtestClient, SPEEDTEST_ADDR};
use local_socks::{LocalSocksConfig, serve_socks5};
use rustls::pki_types::CertificateDer;
use serde::Deserialize;
use url::Url;
use vpn_tun2socks::{Tun2SocksConfig, Tun2SocksHandle};

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
const VPN_TUN_IPV6_ADDR: &str = "fd00::2";
const MAX_RECONNECT_BACKOFF: Duration = Duration::from_secs(15);
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
            quic_disable_path_mtu_discovery: false,
            insecure_tls: true,
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
        "launch prefill: server={} auth={} obfs={} sni={} ca={} pin={} bandwidth.up={} bandwidth.down={} quic.stream={} quic.conn={} idle={} keepAlive={} pmtudOff={} insecure_tls={}",
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
        || form.quic_disable_path_mtu_discovery
        || !form.insecure_tls
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

fn button_style_with_state(base: impl AsRef<str>, enabled: bool) -> String {
    let base = base.as_ref();
    if enabled {
        base.to_string()
    } else {
        format!("{base}opacity: 0.55; filter: saturate(0.7);")
    }
}

#[derive(Clone, Debug, PartialEq)]
struct UiStatus {
    phase: String,
    remote: String,
    detail: String,
    udp_enabled: bool,
    negotiated_tx: u64,
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
            detail: "Import a config, optionally import a certificate, then connect.".to_string(),
            udp_enabled: false,
            negotiated_tx: 0,
            local_socks: format!("{LOCAL_SOCKS_HOST}:{LOCAL_SOCKS_PORT}"),
            vpn_available: false,
            vpn_permission_granted: false,
            vpn_active: false,
        }
    }
}

#[derive(Clone, Debug)]
enum AppCommand {
    Connect(FormState),
    StartManagedVpn(FormState),
    ManagedConnect(FormState),
    Disconnect,
    Speedtest(SpeedDirection),
    RequestVpnPermission,
    StartVpnShell,
    StopVpnShell,
    ServiceStopped,
    ConnectionClosed { generation: u64, reason: String },
    Reconnect { generation: u64, attempt: u32 },
}

#[derive(Clone, Debug)]
enum AppEvent {
    Status(UiStatus),
    Log(String),
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
                            &mut reconnect_form,
                        );
                        stop_vpn_runtime(&tx_evt, &mut tun2socks_task, false);
                        match start_vpn_runtime(&tx_evt, false) {
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
            AppCommand::StartVpnShell => {
                if current_client.is_none() {
                    let _ = tx_evt.send(AppEvent::Log(
                        "connect the client first so the local SOCKS runtime exists".to_string(),
                    ));
                    continue;
                }

                if local_socks_task.is_none() {
                    let _ = tx_evt.send(AppEvent::Log(
                        "local SOCKS runtime is not running yet".to_string(),
                    ));
                    continue;
                }

                desired_vpn_active = true;
                stop_vpn_runtime(&tx_evt, &mut tun2socks_task, false);
                match start_vpn_runtime(&tx_evt, true) {
                    Ok(handle) => {
                        tun2socks_task = Some(handle);
                        connected_status.detail =
                            "Android system VPN started: TUN -> tun2socks -> local SOCKS -> hysteria-core"
                                .to_string();
                    }
                    Err(err) => {
                        let _ =
                            tx_evt.send(AppEvent::Log(format!("start system VPN failed: {err:#}")));
                        connected_status.detail = format!("start system VPN failed: {err}");
                    }
                }
                connected_status = with_vpn_state(connected_status);
                let _ = tx_evt.send(AppEvent::Status(connected_status.clone()));
            }
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
                    false,
                    false,
                );
                connected_status = with_vpn_state(UiStatus {
                    phase: "Reconnecting".to_string(),
                    remote: connected_status.remote.clone(),
                    detail: if restart_vpn {
                        format!(
                            "Connection lost: {reason}. Reconnecting soon and restoring Android VPN..."
                        )
                    } else {
                        format!("Connection lost: {reason}. Reconnecting soon...")
                    },
                    udp_enabled: connected_status.udp_enabled,
                    negotiated_tx: connected_status.negotiated_tx,
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
                    udp_enabled: connected_status.udp_enabled,
                    negotiated_tx: connected_status.negotiated_tx,
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
                            &mut reconnect_form,
                        );
                        connected_status = status;
                        let _ =
                            tx_evt.send(AppEvent::Log("automatic reconnect succeeded".to_string()));

                        if desired_vpn_active {
                            stop_vpn_runtime(&tx_evt, &mut tun2socks_task, false);
                            match start_vpn_runtime(&tx_evt, false) {
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
                            udp_enabled: connected_status.udp_enabled,
                            negotiated_tx: connected_status.negotiated_tx,
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
    reconnect_form: &mut Option<FormState>,
) -> UiStatus {
    let socks_listen = format!("{LOCAL_SOCKS_HOST}:{LOCAL_SOCKS_PORT}");
    *local_socks_task = Some(spawn_local_socks(
        runtime_handle.clone(),
        tx_evt.clone(),
        client.clone(),
        socks_listen.clone(),
    ));
    *close_watch_task = Some(spawn_connection_close_watcher(
        runtime_handle,
        tx_cmd,
        client.clone(),
        generation,
    ));

    let mut status = with_vpn_state(status);
    status.local_socks = socks_listen.clone();
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
    stop_service: bool,
    gracefully_close_client: bool,
) {
    if let Some(task) = close_watch_task.take() {
        task.abort();
    }

    stop_vpn_runtime(tx_evt, tun2socks_task, stop_service);

    if let Some(task) = local_socks_task.take() {
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
                    Tun2SocksConfig {
                        socks_host: LOCAL_SOCKS_HOST.to_string(),
                        socks_port: LOCAL_SOCKS_PORT,
                        tunnel_name: VPN_TUN_NAME.to_string(),
                        mtu: VPN_TUN_MTU,
                        ipv4_addr: VPN_TUN_IPV4_ADDR.to_string(),
                        ipv6_addr: Some(VPN_TUN_IPV6_ADDR.to_string()),
                    },
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
    tx_evt: Sender<AppEvent>,
    client: Client,
    listen: String,
) -> tokio::task::JoinHandle<()> {
    handle.spawn(async move {
        let config = LocalSocksConfig {
            listen: listen.clone(),
            username: String::new(),
            password: String::new(),
            disable_udp: false,
        };
        if let Err(err) = serve_socks5(config, client).await {
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
    let mut node_filter = use_signal(|| NodeFilter::All);
    let mut node_search = use_signal(String::new);
    let mut ca_catalog = use_signal(|| android_bridge::query_ca_catalog().unwrap_or_default());
    let mut live_tick = use_signal(|| 0_u64);
    let mut auto_connect_after_vpn_permission = use_signal(|| false);
    let imported_config_name = use_signal(String::new);
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
                        AppEvent::Log(message) => {
                            record_log_metrics(&mut metrics, &message);
                            append_log(&mut logs, message);
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
            "Connection, Android VPN, and appearance.".to_string(),
        ),
    };
    let topbar_action_label: Option<&'static str> = None;
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
    let home_recent_logs: Vec<LogEntry> = log_items.iter().take(4).cloned().collect();
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
    let current_cert_label = if !imported_cert_name_snapshot.trim().is_empty() {
        imported_cert_name_snapshot.clone()
    } else if let Some(file) = ca_catalog_snapshot
        .files
        .iter()
        .find(|file| file.path == form_snapshot.ca_path)
    {
        file.name.clone()
    } else if !form_snapshot.ca_path.trim().is_empty() {
        Path::new(form_snapshot.ca_path.trim())
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(form_snapshot.ca_path.trim())
            .to_string()
    } else {
        "No certificate imported".to_string()
    };
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
            div {
                style: "position: relative; z-index: 1; width: min(100%, 560px); margin: 0 auto; display: flex; flex-direction: column; gap: 18px;",
                TopBar {
                    title: page_title,
                    subtitle: page_subtitle,
                    action_label: topbar_action_label,
                    on_action: move |_| {},
                }

                match active_tab_snapshot {
                    AppTab::Home => rsx! {
                        div {
                            style: page_stack_style(&prefs_snapshot),
                            section {
                                style: status_card_style(connection_tone, &prefs_snapshot),
                                div {
                                    style: "display: flex; justify-content: space-between; gap: 16px; align-items: flex-start; flex-wrap: wrap;",
                                    div {
                                        style: "display: flex; flex-direction: column; gap: 6px;",
                                        h1 {
                                            style: "margin: 0; font-size: 24px; line-height: 1.15; font-weight: 600;",
                                            "{status_snapshot.phase}"
                                        }
                                        p {
                                            style: "margin: 0; color: #a8b3c7; font-size: 14px; line-height: 1.5;",
                                            "{status_snapshot.detail}"
                                        }
                                    }
                                    div {
                                        style: "display: flex; flex-wrap: wrap; gap: 8px; justify-content: flex-end;",
                                        StatusPill { label: format!("VPN {}", vpn_badge_label(&status_snapshot)), tone: if status_snapshot.vpn_active { AccentTone::Positive } else { AccentTone::Neutral } }
                                        StatusPill { label: summarize_protocol(&status_snapshot), tone: if status_snapshot.udp_enabled { AccentTone::Accent } else { AccentTone::Neutral } }
                                    }
                                }

                                div {
                                    style: "display: flex; flex-direction: column; gap: 0; border-radius: 18px; background: rgba(255,255,255,0.03); border: 1px solid rgba(255,255,255,0.06); padding: 4px 16px;",
                                    StatusLine { label: "Config", value: current_config_label }
                                    StatusLine { label: "Endpoint", value: config_value_or_empty(&form_snapshot.server) }
                                    StatusLine { label: "Certificate", value: current_cert_label }
                                    StatusLine { label: "Remote", value: display_or_dash(&status_snapshot.remote) }
                                    StatusLine { label: "Session", value: online_duration.clone() }
                                    StatusLine { label: "Last connected", value: last_connected.clone() }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Import",
                                    subtitle: "Load a client config and an optional CA certificate.".to_string(),
                                }
                                div {
                                    style: "display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px;",
                                    PrimaryButton {
                                        label: "Import Config",
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
                                if !ca_catalog_snapshot.directory.trim().is_empty() {
                                    p {
                                        style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                        "App CA directory: {ca_catalog_snapshot.directory}"
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Connection",
                                    subtitle: if !status_snapshot.vpn_permission_granted {
                                        "The first tap requests Android VPN permission, then the app continues connecting automatically.".to_string()
                                    } else {
                                        "Connect starts the managed Android VPN runtime and routes traffic through hysteria-core.".to_string()
                                    },
                                }
                                button {
                                    style: button_style_with_state(button_surface_style(false), can_connect || should_offer_disconnect(&status_snapshot.phase)),
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
                                if !can_connect && !should_offer_disconnect(&status_snapshot.phase) {
                                    p {
                                        style: "margin: 14px 0 0; color: #fca5a5; font-size: 13px; line-height: 1.5;",
                                        "Import a valid config first. A usable profile must resolve to both server and auth."
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Recent Activity",
                                    subtitle: "Newest runtime events first.".to_string(),
                                }
                                div {
                                    style: "display: flex; flex-direction: column; gap: 10px;",
                                    if home_recent_logs.is_empty() {
                                        EmptyState {
                                            title: "No runtime events yet".to_string(),
                                            detail: "Import a profile or connect to populate this feed.".to_string(),
                                        }
                                    } else {
                                        for entry in home_recent_logs.iter() {
                                            ActivityRow {
                                                message: entry.message.clone(),
                                                age: format_relative_time(entry.recorded_at, now),
                                            }
                                        }
                                    }
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
                                    subtitle: "Keep the mobile flow minimal: server, auth, CA, then connect. Expert transport controls stay hidden unless you explicitly open them.".to_string(),
                                }
                                FieldRow { label: "Server", placeholder: "Host:port or hy2:// URI", value: form_snapshot.server.clone(), oninput: move |value| form.write().server = value }
                                FieldRow { label: "Auth", placeholder: "Password or auth token", value: form_snapshot.auth.clone(), oninput: move |value| form.write().auth = value }
                                FieldRow { label: "CA path", placeholder: "PEM file path inside app storage", value: form_snapshot.ca_path.clone(), oninput: move |value| form.write().ca_path = value }
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
                                if prefs_snapshot.show_advanced_fields {
                                    div {
                                        style: "display: flex; flex-direction: column;",
                                        p {
                                            style: "margin: 12px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                            "Expert controls are intended for CLI parity and transport debugging, not the normal Android flow."
                                        }
                                        FieldRow { label: "Salamander", placeholder: "Optional obfs password", value: form_snapshot.obfs_password.clone(), oninput: move |value| form.write().obfs_password = value }
                                        FieldRow { label: "TLS SNI", placeholder: "Optional SNI override", value: form_snapshot.sni.clone(), oninput: move |value| form.write().sni = value }
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
                                        style: filter_chip_style(form_snapshot.insecure_tls),
                                        onclick: move |_| {
                                            let next = !form().insecure_tls;
                                            form.write().insecure_tls = next;
                                        },
                                        if form_snapshot.insecure_tls { "TLS insecure: ON" } else { "TLS insecure: OFF" }
                                    }
                                    button {
                                        style: filter_chip_style(prefs_snapshot.show_advanced_fields),
                                        onclick: move |_| prefs.with_mut(|current| current.show_advanced_fields = !current.show_advanced_fields),
                                        if prefs_snapshot.show_advanced_fields { "Expert mode: On" } else { "Expert mode: Off" }
                                    }
                                    if prefs_snapshot.show_advanced_fields {
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
                                        "Server and Auth must be set before Connect or Start System VPN can run."
                                    }
                                } else if !prefs_snapshot.show_advanced_fields {
                                    p {
                                        style: "margin: 14px 0 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                                        "Normal mobile flow is ready. Leave Expert mode off unless you are testing obfs, pinning, bandwidth, or QUIC tuning."
                                    }
                                }
                            }

                            section {
                                style: panel_style(&prefs_snapshot),
                                SectionHeader {
                                    title: "Profile Actions",
                                    subtitle: "Persist the draft locally or execute the connection workflow.".to_string(),
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
                                    PrimaryButton {
                                        label: "Connect",
                                        disabled: !can_connect,
                                        secondary: false,
                                        onclick: {
                                            let controller = controller.clone();
                                            let snapshot = form();
                                            move |_| controller.send(AppCommand::Connect(snapshot.clone()))
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Start System VPN",
                                        disabled: !can_connect,
                                        secondary: true,
                                        onclick: {
                                            let controller = controller.clone();
                                            let snapshot = form();
                                            move |_| controller.send(AppCommand::StartManagedVpn(snapshot.clone()))
                                        },
                                    }
                                    PrimaryButton {
                                        label: "Disconnect",
                                        disabled: false,
                                        secondary: true,
                                        onclick: {
                                            let controller = controller.clone();
                                            move |_| controller.send(AppCommand::Disconnect)
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
                                    style: "display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); gap: 12px;",
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
                                ProgressMetric {
                                    label: "Stability",
                                    value: health_score,
                                    detail: format!("{} successful connects, {} reconnects, {} errors.", metrics_snapshot.successful_connections, metrics_snapshot.reconnect_count, metrics_snapshot.error_count),
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
                                    tone: if status_snapshot.udp_enabled { AccentTone::Accent } else { AccentTone::Neutral },
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
                                StatusLine { label: "UDP enabled", value: bool_word(status_snapshot.udp_enabled).to_string() }
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
                                    title: "Connection Settings",
                                    subtitle: "Only the mobile features that are complete enough to be part of the main path stay here.".to_string(),
                                }
                                SettingRow {
                                    label: "TLS insecure",
                                    detail: (if form_snapshot.insecure_tls {
                                        "Enabled for easier bring-up while CA or pinning is not ready."
                                    } else {
                                        "TLS verification is enabled."
                                    })
                                    .to_string(),
                                    control: rsx! {
                                        button {
                                            style: filter_chip_style(form_snapshot.insecure_tls),
                                            onclick: move |_| {
                                                let next = !form().insecure_tls;
                                                form.write().insecure_tls = next;
                                            },
                                            if form_snapshot.insecure_tls { "On" } else { "Off" }
                                        }
                                    },
                                }
                                SettingRow {
                                    label: "Saved profile",
                                    detail: (if can_load_saved {
                                        "A local profile is stored in Android SharedPreferences."
                                    } else {
                                        "No local profile is stored yet."
                                    })
                                    .to_string(),
                                    control: rsx! {
                                        button {
                                            style: filter_chip_style(can_load_saved),
                                            onclick: move |_| {
                                                settings_return_tab.set(AppTab::Nodes);
                                                active_tab.set(AppTab::Nodes);
                                            },
                                            if can_load_saved { "Manage" } else { "Create" }
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
                                    label: "Transport bridge",
                                    detail: "When active, Android routes traffic through tun2socks into the local SOCKS runtime.".to_string(),
                                    control: rsx! {
                                        StatusPill {
                                            label: if status_snapshot.vpn_active { "Attached".to_string() } else { "Idle".to_string() },
                                            tone: if status_snapshot.vpn_active { AccentTone::Positive } else { AccentTone::Neutral },
                                        }
                                    },
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
            style: "position: sticky; top: 0; z-index: 3; display: flex; flex-direction: column; gap: 4px; padding: 10px 0 12px; background: linear-gradient(180deg, rgba(16,19,26,0.96) 0%, rgba(16,19,26,0.88) 70%, rgba(16,19,26,0) 100%); backdrop-filter: blur(12px);",
            div {
                style: "display: flex; justify-content: space-between; gap: 12px; align-items: center;",
                div {
                    style: "display: flex; flex-direction: column; gap: 4px; min-width: 0;",
                    span {
                        style: "font-size: 22px; line-height: 1.2; font-weight: 600; letter-spacing: -0.01em;",
                        "{title}"
                    }
                    p {
                        style: "margin: 0; color: #a8b3c7; font-size: 14px; line-height: 1.5;",
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
            style: "display: flex; flex-direction: column; gap: 6px; margin-bottom: 14px;",
            h2 {
                style: "margin: 0; font-size: 20px; font-weight: 600; letter-spacing: -0.02em;",
                "{title}"
            }
            p {
                style: "margin: 0; color: #a8b3c7; font-size: 13px; line-height: 1.5;",
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
            style: "display: flex; flex-direction: column; gap: 8px; margin-top: 12px;",
            span { style: "color: #a8b3c7; font-size: 13px;", "{label}" }
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
            style: "display: flex; flex-direction: column; gap: 10px; margin-top: 12px;",
            div {
                style: "display: flex; justify-content: space-between; gap: 12px; align-items: flex-start; flex-wrap: wrap;",
                div {
                    style: "display: flex; flex-direction: column; gap: 6px;",
                    span {
                        style: "color: #a8b3c7; font-size: 13px;",
                        "Installed CAs"
                    }
                    p {
                        style: "margin: 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                        if has_directory {
                            "ADB directory: {directory}"
                        } else {
                            "ADB directory will appear when the Android bridge is ready."
                        }
                    }
                }
                div {
                    style: "display: flex; gap: 8px; flex-wrap: wrap;",
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
                    style: "margin: 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
                    "No CA files found. Push a .crt or .pem file into the directory above, then refresh."
                }
            } else {
                div {
                    style: "display: flex; flex-direction: column; gap: 10px;",
                    for file in files {
                        button {
                            style: selectable_item_style(selected_path == file.path),
                            onclick: {
                                let file_path = file.path.clone();
                                move |_| onselect.call(file_path.clone())
                            },
                            div {
                                style: "display: flex; flex-direction: column; align-items: flex-start; gap: 4px; width: 100%;",
                                strong {
                                    style: "font-size: 14px; color: #f3f7ff; font-weight: 600;",
                                    if selected_path == file.path {
                                        "Using {file.name}"
                                    } else {
                                        "Use {file.name}"
                                    }
                                }
                                span {
                                    style: "font-size: 12px; color: #a8b3c7; line-height: 1.5; text-align: left;",
                                    "{file.path}"
                                }
                            }
                        }
                    }
                }
            }
            if has_selected_path {
                p {
                    style: "margin: 0; color: #6e7b91; font-size: 12px; line-height: 1.5;",
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
            style: "display: flex; justify-content: space-between; gap: 16px; padding: 10px 0; border-bottom: 1px solid rgba(255,255,255,0.06); align-items: flex-start;",
            strong {
                style: "min-width: 112px; color: #f3f7ff; font-size: 14px; font-weight: 500;",
                "{label}"
            }
            span {
                style: "text-align: right; color: #a8b3c7; font-size: 14px; line-height: 1.5;",
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
fn QuickActionCard(
    title: &'static str,
    detail: &'static str,
    tone: AccentTone,
    disabled: bool,
    onclick: EventHandler<()>,
) -> Element {
    rsx! {
        button {
            style: button_style_with_state(quick_action_style(tone), !disabled),
            disabled: disabled,
            onclick: move |_| onclick.call(()),
            div {
                style: "display: flex; flex-direction: column; align-items: flex-start; gap: 8px; width: 100%;",
                strong {
                    style: "font-size: 15px; color: #f3f7ff; font-weight: 600;",
                    "{title}"
                }
                span {
                    style: "font-size: 12px; color: #a8b3c7; line-height: 1.5; text-align: left;",
                    "{detail}"
                }
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
            style: button_style_with_state(button_surface_style(secondary), !disabled),
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
                style: "display: flex; justify-content: space-between; gap: 12px; align-items: flex-start;",
                div {
                    style: "display: flex; flex-direction: column; gap: 6px;",
                    strong {
                        style: "font-size: 16px; color: #f3f7ff; font-weight: 600;",
                        "{title}"
                    }
                    span {
                        style: "font-size: 13px; color: #a8b3c7; line-height: 1.5;",
                        "{subtitle}"
                    }
                }
                StatusPill {
                    label: if selected { "Selected".to_string() } else { "Available".to_string() },
                    tone: if selected { AccentTone::Accent } else { tone },
                }
            }
            p {
                style: "margin: 12px 0 0; color: #6e7b91; font-size: 13px; line-height: 1.5;",
                "{meta}"
            }
            div {
                style: "display: flex; gap: 8px; flex-wrap: wrap; margin-top: 14px;",
                for tag in tags {
                    StatusPill { label: tag, tone: tone }
                }
            }
            button {
                style: button_style_with_state(button_surface_style(true), !disabled),
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
            style: "display: flex; flex-direction: column; gap: 8px; margin-top: 14px;",
            div {
                style: "display: flex; justify-content: space-between; gap: 12px;",
                strong { style: "font-size: 14px; color: #f3f7ff;", "{label}" }
                span { style: "font-size: 13px; color: #a8b3c7;", "{value}%" }
            }
            div {
                style: progress_track_style(),
                div { style: progress_fill_style(value, tone) }
            }
            p {
                style: "margin: 0; color: #6e7b91; font-size: 13px; line-height: 1.5;",
                "{detail}"
            }
        }
    }
}

#[component]
fn ActivityRow(message: String, age: String) -> Element {
    rsx! {
        div {
            style: "display: flex; justify-content: space-between; gap: 14px; padding: 12px 14px; border-radius: 16px; background: rgba(255,255,255,0.03); border: 1px solid rgba(255,255,255,0.05);",
            span {
                style: "font-size: 14px; color: #f3f7ff; line-height: 1.5;",
                "{message}"
            }
            span {
                style: "font-size: 12px; color: #6e7b91; white-space: nowrap;",
                "{age}"
            }
        }
    }
}

#[component]
fn EmptyState(title: String, detail: String) -> Element {
    rsx! {
        div {
            style: "display: flex; flex-direction: column; gap: 6px; padding: 18px; border-radius: 18px; background: rgba(255,255,255,0.03); border: 1px dashed rgba(255,255,255,0.08);",
            strong {
                style: "font-size: 15px; color: #f3f7ff;",
                "{title}"
            }
            p {
                style: "margin: 0; font-size: 13px; color: #a8b3c7; line-height: 1.5;",
                "{detail}"
            }
        }
    }
}

#[component]
fn SettingRow(label: &'static str, detail: String, control: Element) -> Element {
    rsx! {
        div {
            style: "display: flex; justify-content: space-between; gap: 16px; padding: 14px 0; border-bottom: 1px solid rgba(255,255,255,0.06); align-items: center;",
            div {
                style: "display: flex; flex-direction: column; gap: 6px;",
                strong {
                    style: "font-size: 14px; color: #f3f7ff; font-weight: 500;",
                    "{label}"
                }
                p {
                    style: "margin: 0; color: #a8b3c7; font-size: 13px; line-height: 1.5;",
                    "{detail}"
                }
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
    let root_certificates = load_optional_certificates(&normalized.ca_path)?;
    let pinned = parse_optional_pinned_sha256(&normalized.pin_sha256)?;

    if !normalized.insecure_tls && root_certificates.is_empty() && pinned.is_none() {
        bail!("set a CA path, pinSHA256, or enable TLS insecure for MVP bring-up");
    }

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
        udp_enabled: info.udp_enabled,
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

fn load_optional_certificates(path: &str) -> Result<Vec<CertificateDer<'static>>> {
    if path.trim().is_empty() {
        return Ok(Vec::new());
    }
    let file = File::open(Path::new(path))
        .with_context(|| format!("failed to open certificate file {path}"))?;
    let mut reader = BufReader::new(file);
    let certs = rustls_pemfile::certs(&mut reader)
        .with_context(|| format!("failed to parse PEM certificates from {path}"))?;
    if certs.is_empty() {
        bail!("no certificates found in {path}");
    }
    Ok(certs.into_iter().map(CertificateDer::from).collect())
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
                if status.udp_enabled {
                    "UDP".to_string()
                } else {
                    "TCP only".to_string()
                },
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

fn summarize_protocol(status: &UiStatus) -> String {
    let udp = if status.udp_enabled {
        "UDP relay ready"
    } else {
        "UDP unavailable"
    };
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

fn present_absent_label(value: bool) -> &'static str {
    if value { "Present" } else { "Absent" }
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
    let mut value = bytes_per_second as f64;
    let units = ["B/s", "KB/s", "MB/s", "GB/s"];
    let mut unit_index = 0usize;
    while value >= 1024.0 && unit_index < units.len() - 1 {
        value /= 1024.0;
        unit_index += 1;
    }
    format!("{value:.1} {}", units[unit_index])
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

fn app_shell_style(prefs: &UiPrefs) -> String {
    format!(
        concat!(
            "--bg-main: #10131A;",
            "--surface: #171B22;",
            "--surface-container: #1D222B;",
            "--surface-container-high: #232A34;",
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
            "background: linear-gradient(180deg, #10131A 0%, #0E1117 100%);",
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
            "background: var(--surface-container);",
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

fn quick_action_style(tone: AccentTone) -> String {
    format!(
        concat!(
            "display: flex; width: 100%; min-height: 84px; box-sizing: border-box;",
            "padding: 16px; border-radius: 18px;",
            "border: 1px solid {};",
            "background: linear-gradient(180deg, {}, var(--surface-container-high));",
            "transition: background var(--motion), border-color var(--motion);"
        ),
        tone_border(tone),
        tone_fill(tone)
    )
}

fn button_surface_style(secondary: bool) -> &'static str {
    if secondary {
        "padding: 14px 16px; border-radius: 20px; border: 1px solid var(--border); background: var(--surface-container-high); color: var(--text-primary); font-size: 14px; font-weight: 500; transition: background var(--motion), border-color var(--motion);"
    } else {
        "padding: 14px 16px; border-radius: 20px; border: 1px solid transparent; background: #8AB4F8; color: #0B1220; font-size: 14px; font-weight: 600; transition: background var(--motion), border-color var(--motion);"
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
