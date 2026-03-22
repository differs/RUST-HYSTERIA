use std::{
    convert::TryInto,
    env,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

use quinn::{
    AckFrequencyConfig, IdleTimeout, TransportConfig, VarInt,
    congestion::{BbrConfig, BrutalConfig},
};

use crate::{CoreError, CoreResult};

pub const DEFAULT_STREAM_RECEIVE_WINDOW: u64 = 8 * 1024 * 1024;
pub const DEFAULT_CONNECTION_RECEIVE_WINDOW: u64 = DEFAULT_STREAM_RECEIVE_WINDOW * 5 / 2;
pub const DEFAULT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_KEEP_ALIVE_PERIOD: Duration = Duration::from_secs(10);
pub const DEFAULT_MAX_INCOMING_STREAMS: u64 = 1024;
const MIN_QUIC_RECEIVE_WINDOW: u64 = 16 * 1024;

#[derive(Debug, Clone)]
pub struct QuicTransportConfig {
    pub stream_receive_window: u64,
    pub receive_window: u64,
    pub max_idle_timeout: Duration,
    pub keep_alive_interval: Option<Duration>,
    pub max_concurrent_bidi_streams: Option<u64>,
    pub disable_path_mtu_discovery: bool,
}

impl QuicTransportConfig {
    pub fn client_default() -> Self {
        Self {
            stream_receive_window: DEFAULT_STREAM_RECEIVE_WINDOW,
            receive_window: DEFAULT_CONNECTION_RECEIVE_WINDOW,
            max_idle_timeout: DEFAULT_MAX_IDLE_TIMEOUT,
            keep_alive_interval: Some(DEFAULT_KEEP_ALIVE_PERIOD),
            max_concurrent_bidi_streams: None,
            disable_path_mtu_discovery: false,
        }
    }

    pub fn server_default() -> Self {
        Self {
            stream_receive_window: DEFAULT_STREAM_RECEIVE_WINDOW,
            receive_window: DEFAULT_CONNECTION_RECEIVE_WINDOW,
            max_idle_timeout: DEFAULT_MAX_IDLE_TIMEOUT,
            keep_alive_interval: None,
            max_concurrent_bidi_streams: Some(DEFAULT_MAX_INCOMING_STREAMS),
            disable_path_mtu_discovery: false,
        }
    }
}

impl Default for QuicTransportConfig {
    fn default() -> Self {
        Self::client_default()
    }
}

pub(crate) fn build_transport_config(
    config: &QuicTransportConfig,
    bandwidth_target: Arc<AtomicU64>,
) -> CoreResult<TransportConfig> {
    if config.stream_receive_window < MIN_QUIC_RECEIVE_WINDOW {
        return Err(CoreError::Config(
            "quic.stream_receive_window must be at least 16384".into(),
        ));
    }
    if config.receive_window < MIN_QUIC_RECEIVE_WINDOW {
        return Err(CoreError::Config(
            "quic.receive_window must be at least 16384".into(),
        ));
    }

    let mut transport = TransportConfig::default();
    transport
        .stream_receive_window(varint(
            config.stream_receive_window,
            "quic.stream_receive_window",
        )?)
        .receive_window(varint(config.receive_window, "quic.receive_window")?)
        .send_window(
            config
                .receive_window
                .max(config.stream_receive_window.saturating_mul(8)),
        )
        .ack_frequency_config(ack_frequency_override())
        .initial_rtt(Duration::from_millis(250))
        .persistent_congestion_threshold(5)
        .max_idle_timeout(Some(idle_timeout(config.max_idle_timeout)?))
        .keep_alive_interval(config.keep_alive_interval)
        .congestion_controller_factory(Arc::new(build_brutal_config(bandwidth_target)));

    if let Some(value) = env_u64("HY_RS_PACKET_THRESHOLD") {
        transport.packet_threshold(value.clamp(3, u32::MAX as u64) as u32);
    }
    if let Some(value) = env_f32("HY_RS_TIME_THRESHOLD") {
        transport.time_threshold(value.max(1.0));
    }
    if let Some(value) = env_u64("HY_RS_PERSISTENT_CONGESTION_THRESHOLD") {
        transport.persistent_congestion_threshold(value.clamp(1, u32::MAX as u64) as u32);
    }
    if let Some(value) = env_bool("HY_RS_DISABLE_GSO") {
        transport.enable_segmentation_offload(!value);
    }

    if let Some(max_concurrent_bidi_streams) = config.max_concurrent_bidi_streams {
        transport.max_concurrent_bidi_streams(varint(
            max_concurrent_bidi_streams,
            "quic.max_concurrent_bidi_streams",
        )?);
    }
    if config.disable_path_mtu_discovery {
        transport.mtu_discovery_config(None);
    }

    Ok(transport)
}

fn varint(value: u64, field: &str) -> CoreResult<VarInt> {
    VarInt::try_from(value)
        .map_err(|_| CoreError::Config(format!("{field} is too large for QUIC varint")))
}

fn idle_timeout(value: Duration) -> CoreResult<IdleTimeout> {
    value
        .try_into()
        .map_err(|_| CoreError::Config("quic.max_idle_timeout is too large".into()))
}

fn ack_frequency_override() -> Option<AckFrequencyConfig> {
    if !env_bool("HY_RS_ACK_ENABLE").unwrap_or(false) {
        return None;
    }

    let mut config = AckFrequencyConfig::default();
    if let Some(value) = env_u64("HY_RS_ACK_THRESH") {
        config.ack_eliciting_threshold(VarInt::from_u32(value.min(u32::MAX as u64) as u32));
    }
    if let Some(value) = env_u64("HY_RS_ACK_MAX_DELAY_MS") {
        config.max_ack_delay(Some(Duration::from_millis(value)));
    }
    if let Some(value) = env_u64("HY_RS_ACK_REORDER_THRESHOLD") {
        config.reordering_threshold(VarInt::from_u32(value.min(u32::MAX as u64) as u32));
    }
    Some(config)
}

fn build_brutal_config(bandwidth_target: Arc<AtomicU64>) -> BrutalConfig {
    let mut fallback = BbrConfig::default();
    let initial_window = env_u64("HY_RS_BBR_INITIAL_WINDOW")
        .or_else(|| env_u64("HY_RS_BBR_INITIAL_WINDOW_PKTS").map(|value| value * 1200))
        .unwrap_or(512 * 1200);
    fallback
        .initial_window(initial_window)
        .startup_growth_target(env_f32("HY_RS_BBR_STARTUP_GROWTH").unwrap_or(1.25))
        .startup_rounds_without_growth_before_exit(
            env_u64("HY_RS_BBR_STARTUP_ROUNDS")
                .unwrap_or(6)
                .clamp(1, u8::MAX as u64) as u8,
        )
        .exit_startup_on_recovery(env_bool("HY_RS_BBR_EXIT_ON_RECOVERY").unwrap_or(false))
        .recover_on_non_persistent_loss(
            env_bool("HY_RS_BBR_RECOVER_NON_PERSISTENT").unwrap_or(false),
        )
        .non_persistent_loss_reduction_factor(
            env_f32("HY_RS_BBR_NON_PERSISTENT_LOSS_FACTOR").unwrap_or(0.25),
        );

    let mut brutal = BrutalConfig::new(bandwidth_target);
    brutal.fallback_config(fallback);
    brutal
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name).ok()?.parse().ok()
}

fn env_f32(name: &str) -> Option<f32> {
    env::var(name).ok()?.parse().ok()
}

fn env_bool(name: &str) -> Option<bool> {
    let value = env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}
