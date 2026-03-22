//! Pacing of packet transmissions.

use std::{env, sync::OnceLock};

use crate::{congestion::PacingBehavior, Duration, Instant};

use tracing::warn;

/// A simple pacer with two modes:
/// - Quinn's existing window-based pacing
/// - a Hysteria/Brutal-style rate token bucket
pub(super) struct Pacer {
    capacity: u64,
    last_window: u64,
    last_mtu: u16,
    last_rate_bps: Option<u64>,
    tokens: u64,
    prev: Instant,
    last_behavior: PacingBehavior,
    rate_bucket: RateBucket,
}

#[derive(Clone, Copy, Debug, Default)]
struct RateBucket {
    budget_at_last_sent: u64,
    max_datagram_size: u64,
    last_sent_time: Option<Instant>,
    bytes_per_sec: u64,
}

#[derive(Clone, Copy, Debug)]
struct PacingTuning {
    use_rate_based_pacing: bool,
    burst_interval_nanos: u128,
    min_burst_packets: u64,
    max_burst_packets: u64,
    rate_burst_interval_nanos: u128,
    min_rate_based_delay_nanos: u64,
    rate_based_min_burst_packets: u64,
}

fn pacing_tuning() -> &'static PacingTuning {
    static TUNING: OnceLock<PacingTuning> = OnceLock::new();
    TUNING.get_or_init(|| PacingTuning {
        use_rate_based_pacing: env_bool("HY_RS_PACING_USE_RATE").unwrap_or(true),
        burst_interval_nanos: env_u128("HY_RS_PACING_BURST_NS").unwrap_or(BURST_INTERVAL_NANOS),
        min_burst_packets: env_u64("HY_RS_PACING_MIN_BURST_PKTS").unwrap_or(MIN_BURST_SIZE),
        max_burst_packets: env_u64("HY_RS_PACING_MAX_BURST_PKTS").unwrap_or(MAX_BURST_SIZE),
        rate_burst_interval_nanos: env_u128("HY_RS_RATE_PACING_BURST_NS")
            .unwrap_or(RATE_BASED_BURST_INTERVAL_NANOS),
        min_rate_based_delay_nanos: env_u64("HY_RS_RATE_PACING_MIN_DELAY_NS")
            .unwrap_or(MIN_RATE_BASED_DELAY_NANOS),
        rate_based_min_burst_packets: env_u64("HY_RS_RATE_PACING_MIN_BURST_PKTS")
            .unwrap_or(RATE_BASED_MIN_BURST_PACKETS),
    })
}

fn env_u64(name: &str) -> Option<u64> {
    env::var(name).ok()?.parse().ok()
}

fn env_u128(name: &str) -> Option<u128> {
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

impl RateBucket {
    fn configure(&mut self, mtu: u16, rate_bps: u64, reset_budget: bool) {
        let previous_budget = self.budget_at_last_sent;
        let previous_last_sent_time = self.last_sent_time;
        self.max_datagram_size = mtu as u64;
        self.bytes_per_sec = rate_bps.div_ceil(8);
        if reset_budget || previous_last_sent_time.is_none() {
            self.budget_at_last_sent = self.max_burst_size();
            self.last_sent_time = None;
        } else {
            self.budget_at_last_sent = previous_budget.min(self.max_burst_size());
            self.last_sent_time = previous_last_sent_time;
        }
    }

    fn max_burst_size(&self) -> u64 {
        if self.bytes_per_sec == 0 {
            return self
                .max_datagram_size
                .saturating_mul(RATE_BASED_MIN_BURST_PACKETS);
        }
        let tuning = pacing_tuning();
        let rate_burst = (u128::from(self.bytes_per_sec) * tuning.rate_burst_interval_nanos)
            .div_ceil(1_000_000_000) as u64;
        rate_burst.max(tuning.rate_based_min_burst_packets * self.max_datagram_size)
    }

    fn budget(&self, now: Instant) -> u64 {
        let max_burst_size = self.max_burst_size();
        let Some(last_sent_time) = self.last_sent_time else {
            return max_burst_size;
        };
        let elapsed = now.saturating_duration_since(last_sent_time).as_nanos();
        let budget = self.budget_at_last_sent.saturating_add(
            ((u128::from(self.bytes_per_sec) * elapsed) / 1_000_000_000).min(u128::from(u64::MAX))
                as u64,
        );
        max_burst_size.min(budget)
    }

    fn on_transmit(&mut self, now: Instant, packet_length: u16) {
        let budget = self.budget(now);
        self.budget_at_last_sent = budget.saturating_sub(packet_length as u64);
        self.last_sent_time = Some(now);
    }
}

impl Pacer {
    /// Obtains a new [`Pacer`].
    pub(super) fn new(smoothed_rtt: Duration, window: u64, mtu: u16, now: Instant) -> Self {
        let capacity = optimal_capacity(smoothed_rtt, window, mtu, None);
        Self {
            capacity,
            last_window: window,
            last_mtu: mtu,
            last_rate_bps: None,
            tokens: capacity,
            prev: now,
            last_behavior: PacingBehavior::Window,
            rate_bucket: RateBucket {
                budget_at_last_sent: 0,
                max_datagram_size: mtu as u64,
                last_sent_time: None,
                bytes_per_sec: 0,
            },
        }
    }

    /// Record that a packet has been transmitted.
    pub(super) fn on_transmit(&mut self, now: Instant, packet_length: u16) {
        if self.last_behavior == PacingBehavior::RateTokenBucket
            && self.rate_bucket.bytes_per_sec > 0
        {
            self.rate_bucket.on_transmit(now, packet_length);
        } else {
            self.tokens = self.tokens.saturating_sub(packet_length.into());
        }
    }

    /// Return how long we need to wait before sending `bytes_to_send`.
    pub(super) fn delay(
        &mut self,
        smoothed_rtt: Duration,
        bytes_to_send: u64,
        mtu: u16,
        window: u64,
        pacing_rate_bps: Option<u64>,
        pacing_behavior: PacingBehavior,
        now: Instant,
    ) -> Option<Instant> {
        debug_assert_ne!(
            window, 0,
            "zero-sized congestion control window is nonsense"
        );
        let tuning = pacing_tuning();
        let rate_bps = pacing_rate_bps
            .filter(|value| *value > 0)
            .filter(|_| tuning.use_rate_based_pacing)
            .filter(|_| pacing_behavior == PacingBehavior::RateTokenBucket);

        if pacing_behavior == PacingBehavior::RateTokenBucket {
            let rate_bps = rate_bps?;
            let reconfigure = self.last_behavior != PacingBehavior::RateTokenBucket
                || self.last_mtu != mtu
                || self.rate_bucket.bytes_per_sec != rate_bps.div_ceil(8);
            if reconfigure {
                self.rate_bucket.configure(
                    mtu,
                    rate_bps,
                    self.last_behavior != PacingBehavior::RateTokenBucket,
                );
                self.last_mtu = mtu;
            }
            self.last_behavior = PacingBehavior::RateTokenBucket;
            self.last_rate_bps = None;
            let bytes_needed = u64::from(mtu);
            let budget = self.rate_bucket.budget(now);
            if budget >= bytes_needed {
                return None;
            }
            if self.rate_bucket.bytes_per_sec == 0 {
                return None;
            }
            let deficit = bytes_needed - budget;
            let delay_nanos = (u128::from(deficit) * 1_000_000_000)
                .div_ceil(u128::from(self.rate_bucket.bytes_per_sec));
            let delay_nanos = delay_nanos.max(u128::from(tuning.min_rate_based_delay_nanos));
            let delay_nanos = delay_nanos.min(u128::from(u64::MAX));
            let base = self.rate_bucket.last_sent_time.unwrap_or(now);
            return Some(base + Duration::from_nanos(delay_nanos as u64));
        }

        let active_rate_bps =
            pacing_rate_bps.filter(|value| *value > 0 && tuning.use_rate_based_pacing);
        let bytes_required = if active_rate_bps.is_some() {
            u64::from(mtu)
        } else {
            bytes_to_send
        };
        if window != self.last_window
            || mtu != self.last_mtu
            || active_rate_bps != self.last_rate_bps
            || self.last_behavior != PacingBehavior::Window
        {
            self.capacity = optimal_capacity(smoothed_rtt, window, mtu, active_rate_bps);
            self.tokens = if self.last_behavior != PacingBehavior::Window {
                self.capacity
            } else {
                self.capacity.min(self.tokens)
            };
            self.last_window = window;
            self.last_mtu = mtu;
            self.last_rate_bps = active_rate_bps;
            self.last_behavior = PacingBehavior::Window;
            self.prev = now;
        }

        if self.tokens >= bytes_required {
            return None;
        }

        if window > u64::from(u32::MAX) {
            return None;
        }

        let window = window as u32;
        let time_elapsed = now.checked_duration_since(self.prev).unwrap_or_else(|| {
            warn!("received a timestamp early than a previous recorded time, ignoring");
            Default::default()
        });

        if smoothed_rtt.as_nanos() == 0 && active_rate_bps.unwrap_or_default() == 0 {
            return None;
        }

        let new_tokens = if let Some(rate_bps) = active_rate_bps {
            let bytes_per_sec = rate_bps as f64 / 8.0;
            bytes_per_sec * time_elapsed.as_secs_f64()
        } else {
            let elapsed_rtts = time_elapsed.as_secs_f64() / smoothed_rtt.as_secs_f64();
            window as f64 * 1.25 * elapsed_rtts
        };
        self.tokens = self
            .tokens
            .saturating_add(new_tokens as _)
            .min(self.capacity);
        self.prev = now;

        if self.tokens >= bytes_required {
            return None;
        }

        if let Some(rate_bps) = active_rate_bps {
            let bytes_per_sec = (u128::from(rate_bps)).div_ceil(8);
            if bytes_per_sec == 0 {
                return None;
            }
            let deficit = u128::from(bytes_required.saturating_sub(self.tokens));
            let delay_nanos = (deficit * 1_000_000_000).div_ceil(bytes_per_sec);
            let delay_nanos = delay_nanos.max(u128::from(tuning.min_rate_based_delay_nanos));
            let delay_nanos = delay_nanos.min(u128::from(u64::MAX));
            return Some(self.prev + Duration::from_nanos(delay_nanos as u64));
        }

        let unscaled_delay = smoothed_rtt
            .checked_mul((bytes_required.max(self.capacity) - self.tokens) as _)
            .unwrap_or(Duration::MAX)
            / window;
        Some(self.prev + (unscaled_delay / 5) * 4)
    }
}

/// Calculates a pacer capacity for a certain window and RTT.
fn optimal_capacity(
    smoothed_rtt: Duration,
    window: u64,
    mtu: u16,
    pacing_rate_bps: Option<u64>,
) -> u64 {
    let tuning = pacing_tuning();
    if let Some(rate_bps) = pacing_rate_bps
        .filter(|value| *value > 0)
        .filter(|_| tuning.use_rate_based_pacing)
    {
        let bytes_per_sec = (u128::from(rate_bps)).div_ceil(8);
        let rate_burst =
            (bytes_per_sec * tuning.rate_burst_interval_nanos).div_ceil(1_000_000_000) as u64;
        return rate_burst.max(tuning.rate_based_min_burst_packets * mtu as u64);
    }
    let rtt = smoothed_rtt.as_nanos().max(1);
    let capacity = ((window as u128 * tuning.burst_interval_nanos) / rtt) as u64;
    capacity.clamp(
        tuning.min_burst_packets * mtu as u64,
        tuning.max_burst_packets * mtu as u64,
    )
}

/// The burst interval for window-based pacing.
const BURST_INTERVAL_NANOS: u128 = 2_000_000; // 2ms
const RATE_BASED_BURST_INTERVAL_NANOS: u128 = 4_000_000; // 4ms
const MIN_RATE_BASED_DELAY_NANOS: u64 = 1_000_000; // 1ms

const MIN_BURST_SIZE: u64 = 10;
const RATE_BASED_MIN_BURST_PACKETS: u64 = 10;
const MAX_BURST_SIZE: u64 = 256;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn does_not_panic_on_bad_instant() {
        let old_instant = Instant::now();
        let new_instant = old_instant + Duration::from_micros(15);
        let rtt = Duration::from_micros(400);

        assert!(Pacer::new(rtt, 30000, 1500, new_instant)
            .delay(
                Duration::from_micros(0),
                0,
                1500,
                1,
                None,
                PacingBehavior::Window,
                old_instant,
            )
            .is_none());
        assert!(Pacer::new(rtt, 30000, 1500, new_instant)
            .delay(
                Duration::from_micros(0),
                1600,
                1500,
                1,
                None,
                PacingBehavior::Window,
                old_instant,
            )
            .is_none());
        assert!(Pacer::new(rtt, 30000, 1500, new_instant)
            .delay(
                Duration::from_micros(0),
                1500,
                1500,
                3000,
                None,
                PacingBehavior::Window,
                old_instant,
            )
            .is_none());
    }

    #[test]
    fn derives_initial_capacity() {
        let window = 2_000_000;
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let now = Instant::now();

        let pacer = Pacer::new(rtt, window, mtu, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, pacer.capacity);

        let pacer = Pacer::new(Duration::from_millis(0), window, mtu, now);
        assert_eq!(pacer.capacity, MAX_BURST_SIZE * mtu as u64);
        assert_eq!(pacer.tokens, pacer.capacity);

        let pacer = Pacer::new(rtt, 1, mtu, now);
        assert_eq!(pacer.capacity, MIN_BURST_SIZE * mtu as u64);
        assert_eq!(pacer.tokens, pacer.capacity);
    }

    #[test]
    fn adjusts_capacity() {
        let window = 2_000_000;
        let mtu = 1500;
        let rtt = Duration::from_millis(50);
        let now = Instant::now();

        let mut pacer = Pacer::new(rtt, window, mtu, now);
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, pacer.capacity);
        let initial_tokens = pacer.tokens;

        pacer.delay(
            rtt,
            mtu as u64,
            mtu,
            window * 2,
            None,
            PacingBehavior::Window,
            now,
        );
        assert_eq!(
            pacer.capacity,
            (2 * window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, initial_tokens);

        pacer.delay(
            rtt,
            mtu as u64,
            mtu,
            window / 2,
            None,
            PacingBehavior::Window,
            now,
        );
        assert_eq!(
            pacer.capacity,
            (window as u128 / 2 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );
        assert_eq!(pacer.tokens, initial_tokens / 2);

        pacer.delay(
            rtt,
            mtu as u64,
            mtu * 2,
            window,
            None,
            PacingBehavior::Window,
            now,
        );
        assert_eq!(
            pacer.capacity,
            (window as u128 * BURST_INTERVAL_NANOS / rtt.as_nanos()) as u64
        );

        pacer.delay(
            rtt,
            mtu as u64,
            20_000,
            window,
            None,
            PacingBehavior::Window,
            now,
        );
        assert_eq!(pacer.capacity, 20_000_u64 * MIN_BURST_SIZE);
    }

    #[test]
    fn computes_pause_correctly() {
        let window = 2_000_000u64;
        let mtu = 1000;
        let rtt = Duration::from_millis(50);
        let old_instant = Instant::now();

        let mut pacer = Pacer::new(rtt, window, mtu, old_instant);
        let packet_capacity = pacer.capacity / mtu as u64;

        for _ in 0..packet_capacity {
            assert_eq!(
                pacer.delay(
                    rtt,
                    mtu as u64,
                    mtu,
                    window,
                    None,
                    PacingBehavior::Window,
                    old_instant,
                ),
                None,
                "When capacity is available packets should be sent immediately"
            );

            pacer.on_transmit(old_instant, mtu);
        }

        let pace_duration = Duration::from_nanos((BURST_INTERVAL_NANOS * 4 / 5) as u64);

        assert_eq!(
            pacer
                .delay(
                    rtt,
                    mtu as u64,
                    mtu,
                    window,
                    None,
                    PacingBehavior::Window,
                    old_instant,
                )
                .expect("Send must be delayed")
                .duration_since(old_instant),
            pace_duration
        );

        assert_eq!(
            pacer.delay(
                rtt,
                mtu as u64,
                mtu,
                window,
                None,
                PacingBehavior::Window,
                old_instant + pace_duration / 2,
            ),
            None
        );
        assert_eq!(pacer.tokens, pacer.capacity / 2);

        for _ in 0..packet_capacity / 2 {
            assert_eq!(
                pacer.delay(
                    rtt,
                    mtu as u64,
                    mtu,
                    window,
                    None,
                    PacingBehavior::Window,
                    old_instant,
                ),
                None,
                "When capacity is available packets should be sent immediately"
            );

            pacer.on_transmit(old_instant, mtu);
        }

        assert_eq!(
            pacer.delay(
                rtt,
                mtu as u64,
                mtu,
                window,
                None,
                PacingBehavior::Window,
                old_instant + pace_duration * 3 / 2,
            ),
            None
        );
        assert_eq!(pacer.tokens, pacer.capacity);
    }

    #[test]
    fn rate_token_bucket_uses_hysteria_burst_and_deficit() {
        let mtu = 1500;
        let rate_bps = 8_000_000;
        let now = Instant::now();
        let mut pacer = Pacer::new(Duration::from_millis(50), 2_000_000, mtu, now);

        assert_eq!(
            pacer.delay(
                Duration::from_millis(50),
                mtu as u64,
                mtu,
                2_000_000,
                Some(rate_bps),
                PacingBehavior::RateTokenBucket,
                now,
            ),
            None,
        );
        assert_eq!(pacer.rate_bucket.max_burst_size(), 10 * mtu as u64);

        pacer.on_transmit(now, mtu);
        pacer.rate_bucket.budget_at_last_sent = 0;
        pacer.rate_bucket.last_sent_time = Some(now);

        let delay = pacer
            .delay(
                Duration::from_millis(50),
                mtu as u64,
                mtu,
                2_000_000,
                Some(rate_bps),
                PacingBehavior::RateTokenBucket,
                now,
            )
            .expect("rate bucket should delay when empty");

        assert_eq!(delay.duration_since(now), Duration::from_nanos(1_500_000));
    }
}
