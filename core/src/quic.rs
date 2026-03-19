use std::{
    any::Any,
    convert::TryInto,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use quinn::{
    IdleTimeout, TransportConfig, VarInt,
    congestion::{BbrConfig, Controller, ControllerFactory},
};
use quinn_proto::RttEstimator;

use crate::{CoreError, CoreResult};

pub const DEFAULT_STREAM_RECEIVE_WINDOW: u64 = 8 * 1024 * 1024;
pub const DEFAULT_CONNECTION_RECEIVE_WINDOW: u64 = DEFAULT_STREAM_RECEIVE_WINDOW * 5 / 2;
pub const DEFAULT_MAX_IDLE_TIMEOUT: Duration = Duration::from_secs(30);
pub const DEFAULT_KEEP_ALIVE_PERIOD: Duration = Duration::from_secs(10);
pub const DEFAULT_MAX_INCOMING_STREAMS: u64 = 1024;
const MIN_QUIC_RECEIVE_WINDOW: u64 = 16 * 1024;
const BRUTAL_SLOT_COUNT: usize = 5;
const BRUTAL_MIN_SAMPLE_COUNT: u64 = 50;
const BRUTAL_MIN_ACK_RATE: f64 = 0.8;
const BRUTAL_CONGESTION_WINDOW_MULTIPLIER: f64 = 2.0;

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
        .initial_rtt(Duration::from_millis(250))
        .persistent_congestion_threshold(5)
        .max_idle_timeout(Some(idle_timeout(config.max_idle_timeout)?))
        .keep_alive_interval(config.keep_alive_interval)
        .congestion_controller_factory(Arc::new(BandwidthAwareControllerFactory::new(
            bandwidth_target,
        )));

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

#[derive(Debug, Clone)]
struct BandwidthAwareControllerFactory {
    bandwidth_target: Arc<AtomicU64>,
    fallback: BbrConfig,
}

impl BandwidthAwareControllerFactory {
    fn new(bandwidth_target: Arc<AtomicU64>) -> Self {
        Self {
            bandwidth_target,
            fallback: BbrConfig::default(),
        }
    }
}

impl ControllerFactory for BandwidthAwareControllerFactory {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(BandwidthAwareController {
            bandwidth_target: self.bandwidth_target.clone(),
            fallback: Arc::new(self.fallback.clone()).build(now, current_mtu),
            current_mtu,
            smoothed_rtt: Duration::from_millis(333),
            sample_epoch: now,
            packet_samples: [PacketSample::default(); BRUTAL_SLOT_COUNT],
            ack_rate: 1.0,
        })
    }
}

struct BandwidthAwareController {
    bandwidth_target: Arc<AtomicU64>,
    fallback: Box<dyn Controller>,
    current_mtu: u16,
    smoothed_rtt: Duration,
    sample_epoch: Instant,
    packet_samples: [PacketSample; BRUTAL_SLOT_COUNT],
    ack_rate: f64,
}

impl BandwidthAwareController {
    fn brutal_window(&self, target_bytes_per_sec: u64) -> u64 {
        let rtt = if self.smoothed_rtt.is_zero() {
            Duration::from_millis(333)
        } else {
            self.smoothed_rtt
        };
        let cwnd =
            (target_bytes_per_sec as f64 * rtt.as_secs_f64() * BRUTAL_CONGESTION_WINDOW_MULTIPLIER
                / self.ack_rate.max(BRUTAL_MIN_ACK_RATE)) as u64;
        cwnd.max(self.fallback.initial_window())
            .max(self.current_mtu as u64)
    }

    fn using_fallback(&self) -> bool {
        self.bandwidth_target.load(Ordering::Relaxed) == 0
    }

    fn record_ack(&mut self, now: Instant) {
        self.record_samples(now, 1, 0);
    }

    fn record_loss(&mut self, now: Instant, lost_bytes: u64) {
        let packets = lost_bytes
            .max(self.current_mtu as u64)
            .div_ceil(self.current_mtu.max(1) as u64)
            .max(1);
        self.record_samples(now, 0, packets);
    }

    fn record_samples(&mut self, now: Instant, acked_packets: u64, lost_packets: u64) {
        let current_second = now.saturating_duration_since(self.sample_epoch).as_secs();
        let slot = (current_second as usize) % BRUTAL_SLOT_COUNT;
        let sample = &mut self.packet_samples[slot];
        if sample.second == current_second {
            sample.acked_packets = sample.acked_packets.saturating_add(acked_packets);
            sample.lost_packets = sample.lost_packets.saturating_add(lost_packets);
        } else {
            *sample = PacketSample {
                second: current_second,
                acked_packets,
                lost_packets,
            };
        }
        self.ack_rate = self.calculate_ack_rate(current_second);
    }

    fn calculate_ack_rate(&self, current_second: u64) -> f64 {
        let min_second = current_second.saturating_sub(BRUTAL_SLOT_COUNT as u64);
        let (acked_packets, lost_packets) = self
            .packet_samples
            .iter()
            .filter(|sample| sample.second >= min_second)
            .fold((0_u64, 0_u64), |(acked, lost), sample| {
                (
                    acked.saturating_add(sample.acked_packets),
                    lost.saturating_add(sample.lost_packets),
                )
            });
        let total_packets = acked_packets.saturating_add(lost_packets);
        if total_packets < BRUTAL_MIN_SAMPLE_COUNT {
            1.0
        } else {
            (acked_packets as f64 / total_packets as f64).max(BRUTAL_MIN_ACK_RATE)
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PacketSample {
    second: u64,
    acked_packets: u64,
    lost_packets: u64,
}

impl Controller for BandwidthAwareController {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64) {
        if self.using_fallback() {
            self.fallback.on_sent(now, bytes, last_packet_number);
        }
    }

    fn on_ack(
        &mut self,
        now: Instant,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        self.smoothed_rtt = rtt.get();
        if self.using_fallback() {
            self.fallback.on_ack(now, sent, bytes, app_limited, rtt);
        } else {
            self.record_ack(now);
        }
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        if self.using_fallback() {
            self.fallback
                .on_end_acks(now, in_flight, app_limited, largest_packet_num_acked);
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_bytes: u64,
    ) {
        if self.using_fallback() {
            self.fallback
                .on_congestion_event(now, sent, is_persistent_congestion, lost_bytes);
        } else {
            self.record_loss(now, lost_bytes);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu;
        self.fallback.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        let target_bytes_per_sec = self.bandwidth_target.load(Ordering::Relaxed);
        if target_bytes_per_sec == 0 {
            self.fallback.window()
        } else {
            self.brutal_window(target_bytes_per_sec)
        }
    }

    fn metrics(&self) -> quinn::congestion::ControllerMetrics {
        if self.using_fallback() {
            self.fallback.metrics()
        } else {
            let mut metrics = quinn::congestion::ControllerMetrics::default();
            metrics.congestion_window = self.window();
            metrics
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            bandwidth_target: self.bandwidth_target.clone(),
            fallback: self.fallback.clone_box(),
            current_mtu: self.current_mtu,
            smoothed_rtt: self.smoothed_rtt,
            sample_epoch: self.sample_epoch,
            packet_samples: self.packet_samples,
            ack_rate: self.ack_rate,
        })
    }

    fn initial_window(&self) -> u64 {
        let target_bytes_per_sec = self.bandwidth_target.load(Ordering::Relaxed);
        if target_bytes_per_sec == 0 {
            self.fallback.initial_window()
        } else {
            self.brutal_window(target_bytes_per_sec)
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}
