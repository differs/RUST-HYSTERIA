use std::any::Any;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use crate::connection::RttEstimator;
use crate::{Duration, Instant};

use super::{AckEvent, BbrConfig, Controller, ControllerFactory, ControllerMetrics, PacingBehavior};

const SLOT_COUNT: usize = 5;
const MIN_SAMPLE_COUNT: u64 = 50;
const MIN_ACK_RATE: f64 = 0.8;
const CWND_MULTIPLIER: f64 = 2.0;
const INITIAL_WINDOW_NO_RTT: u64 = 10_240;

/// Hysteria-style Brutal congestion controller.
///
/// The configured target bandwidth is read dynamically from `bandwidth_target`, allowing the
/// connection to run a fallback controller until the authenticated target bandwidth becomes known.
#[derive(Debug, Clone)]
pub struct BrutalConfig {
    bandwidth_target: Arc<AtomicU64>,
    fallback: BbrConfig,
}

impl BrutalConfig {
    /// Construct a new configuration with a dynamically updatable target bandwidth in bytes/s.
    pub fn new(bandwidth_target: Arc<AtomicU64>) -> Self {
        Self {
            bandwidth_target,
            fallback: BbrConfig::default(),
        }
    }

    /// Replace the fallback controller configuration used before the target bandwidth is known.
    pub fn fallback_config(&mut self, config: BbrConfig) -> &mut Self {
        self.fallback = config;
        self
    }

    /// Access the fallback controller configuration.
    pub fn fallback_config_mut(&mut self) -> &mut BbrConfig {
        &mut self.fallback
    }
}

impl ControllerFactory for BrutalConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Brutal::new(self, now, current_mtu))
    }
}

/// Hysteria-style Brutal congestion controller state.
pub struct Brutal {
    config: Arc<BrutalConfig>,
    fallback: Box<dyn Controller>,
    current_mtu: u64,
    has_rtt_sample: bool,
    smoothed_rtt: Duration,
    ack_rate: f64,
    sample_epoch: Instant,
    slots: [PacketSlot; SLOT_COUNT],
}

impl Brutal {
    fn new(config: Arc<BrutalConfig>, now: Instant, current_mtu: u16) -> Self {
        let fallback = Arc::new(config.fallback.clone()).build(now, current_mtu);
        Self {
            config,
            fallback,
            current_mtu: current_mtu as u64,
            has_rtt_sample: false,
            smoothed_rtt: Duration::ZERO,
            ack_rate: 1.0,
            sample_epoch: now,
            slots: [PacketSlot::default(); SLOT_COUNT],
        }
    }

    fn target_bytes_per_sec(&self) -> u64 {
        self.config.bandwidth_target.load(Ordering::Relaxed)
    }

    fn using_fallback(&self) -> bool {
        self.target_bytes_per_sec() == 0
    }

    fn update_ack_rate(&mut self, now: Instant, acked_packets: u64, lost_packets: u64) {
        let current_second = now.saturating_duration_since(self.sample_epoch).as_secs();
        let slot = &mut self.slots[(current_second as usize) % SLOT_COUNT];
        if slot.second == current_second {
            slot.acked_packets = slot.acked_packets.saturating_add(acked_packets);
            slot.lost_packets = slot.lost_packets.saturating_add(lost_packets);
        } else {
            *slot = PacketSlot {
                second: current_second,
                acked_packets,
                lost_packets,
            };
        }

        let min_second = current_second.saturating_sub(SLOT_COUNT as u64);
        let (acked, lost) = self
            .slots
            .iter()
            .filter(|slot| slot.second >= min_second)
            .fold((0_u64, 0_u64), |(acked, lost), slot| {
                (
                    acked.saturating_add(slot.acked_packets),
                    lost.saturating_add(slot.lost_packets),
                )
            });
        let total = acked.saturating_add(lost);
        self.ack_rate = if total < MIN_SAMPLE_COUNT {
            1.0
        } else {
            (acked as f64 / total as f64).max(MIN_ACK_RATE)
        };
    }

    fn brutal_window(&self, target_bytes_per_sec: u64) -> u64 {
        if !self.has_rtt_sample || self.smoothed_rtt.is_zero() {
            return INITIAL_WINDOW_NO_RTT.max(self.current_mtu);
        }
        let cwnd = (target_bytes_per_sec as f64 * self.smoothed_rtt.as_secs_f64() * CWND_MULTIPLIER
            / self.ack_rate.max(MIN_ACK_RATE)) as u64;
        cwnd.max(self.current_mtu)
    }

    fn pacing_rate_bps(&self, target_bytes_per_sec: u64) -> u64 {
        (((target_bytes_per_sec as f64) / self.ack_rate.max(MIN_ACK_RATE)) as u64) * 8
    }
}

impl Controller for Brutal {
    fn on_sent(
        &mut self,
        now: Instant,
        bytes: u64,
        last_packet_number: u64,
        bytes_in_flight: u64,
    ) {
        if self.using_fallback() {
            self.fallback
                .on_sent(now, bytes, last_packet_number, bytes_in_flight);
        }
    }

    fn on_ack(
        &mut self,
        now: Instant,
        _packet_number: u64,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
        if self.using_fallback() {
            self.fallback
                .on_ack(now, _packet_number, sent, bytes, app_limited, rtt);
        }
    }

    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        prior_in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
        if self.using_fallback() {
            self.fallback.on_end_acks(
                now,
                in_flight,
                prior_in_flight,
                app_limited,
                largest_packet_num_acked,
            );
        }
    }

    fn on_ack_event(&mut self, now: Instant, event: &AckEvent<'_>) {
        self.has_rtt_sample = event.rtt.has_sample();
        self.smoothed_rtt = event.rtt.get();
        if !self.using_fallback() {
            self.update_ack_rate(
                now,
                event.acked_packets.len() as u64,
                event.lost_packets.len() as u64,
            );
        } else {
            self.fallback.on_ack_event(now, event);
        }
    }

    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_packets: u64,
        lost_bytes: u64,
        bytes_in_flight_before_loss: u64,
    ) {
        if self.using_fallback() {
            self.fallback.on_congestion_event(
                now,
                sent,
                is_persistent_congestion,
                lost_packets,
                lost_bytes,
                bytes_in_flight_before_loss,
            );
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu as u64;
        self.fallback.on_mtu_update(new_mtu);
    }

    fn window(&self) -> u64 {
        let target_bytes_per_sec = self.target_bytes_per_sec();
        if target_bytes_per_sec == 0 {
            self.fallback.window()
        } else {
            self.brutal_window(target_bytes_per_sec)
        }
    }

    fn metrics(&self) -> ControllerMetrics {
        let target_bytes_per_sec = self.target_bytes_per_sec();
        if target_bytes_per_sec == 0 {
            self.fallback.metrics()
        } else {
            ControllerMetrics {
                congestion_window: self.window(),
                ssthresh: None,
                pacing_rate: Some(self.pacing_rate_bps(target_bytes_per_sec)),
                pacing_behavior: PacingBehavior::RateTokenBucket,
            }
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(Self {
            config: self.config.clone(),
            fallback: self.fallback.clone_box(),
            current_mtu: self.current_mtu,
            has_rtt_sample: self.has_rtt_sample,
            smoothed_rtt: self.smoothed_rtt,
            ack_rate: self.ack_rate,
            sample_epoch: self.sample_epoch,
            slots: self.slots,
        })
    }

    fn initial_window(&self) -> u64 {
        if self.using_fallback() {
            self.fallback.initial_window()
        } else {
            INITIAL_WINDOW_NO_RTT.max(self.current_mtu)
        }
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct PacketSlot {
    second: u64,
    acked_packets: u64,
    lost_packets: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_rollover_discards_old_samples() {
        let mut brutal = Brutal {
            config: Arc::new(BrutalConfig::new(Arc::new(AtomicU64::new(1)))),
            fallback: Arc::new(BbrConfig::default()).build(Instant::now(), 1200),
            current_mtu: 1200,
            has_rtt_sample: true,
            smoothed_rtt: Duration::from_millis(250),
            ack_rate: 1.0,
            sample_epoch: Instant::now(),
            slots: [PacketSlot::default(); SLOT_COUNT],
        };
        let epoch = brutal.sample_epoch;
        brutal.update_ack_rate(epoch + Duration::from_secs(0), 20, 0);
        brutal.update_ack_rate(epoch + Duration::from_secs(6), 40, 10);
        assert_eq!(brutal.ack_rate, 0.8);
    }

    #[test]
    fn low_sample_count_keeps_ack_rate_at_one() {
        let mut brutal = Brutal {
            config: Arc::new(BrutalConfig::new(Arc::new(AtomicU64::new(1)))),
            fallback: Arc::new(BbrConfig::default()).build(Instant::now(), 1200),
            current_mtu: 1200,
            has_rtt_sample: true,
            smoothed_rtt: Duration::from_millis(250),
            ack_rate: 1.0,
            sample_epoch: Instant::now(),
            slots: [PacketSlot::default(); SLOT_COUNT],
        };
        let epoch = brutal.sample_epoch;
        brutal.update_ack_rate(epoch + Duration::from_secs(1), 30, 10);
        assert_eq!(brutal.ack_rate, 1.0);
    }

    #[test]
    fn congestion_window_matches_formula() {
        let brutal = Brutal {
            config: Arc::new(BrutalConfig::new(Arc::new(AtomicU64::new(100_000_000)))),
            fallback: Arc::new(BbrConfig::default()).build(Instant::now(), 1200),
            current_mtu: 1200,
            has_rtt_sample: true,
            smoothed_rtt: Duration::from_millis(250),
            ack_rate: 0.8,
            sample_epoch: Instant::now(),
            slots: [PacketSlot::default(); SLOT_COUNT],
        };
        assert_eq!(brutal.brutal_window(100_000_000), 62_500_000);
    }
}
