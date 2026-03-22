use std::any::Any;
use std::env;
use std::fmt::Debug;
use std::sync::{Arc, OnceLock};

use rand::{Rng, SeedableRng};

use crate::congestion::bbr::min_max::MinMax;
use crate::congestion::ControllerMetrics;
use crate::connection::RttEstimator;
use crate::{Duration, Instant};

use super::{AckEvent, Controller, ControllerFactory, BASE_DATAGRAM_SIZE};

mod bw_estimation;
mod min_max;

/// Experimental! Use at your own risk.
///
/// Aims for reduced buffer bloat and improved performance over high bandwidth-delay product networks.
/// Based on google's quiche implementation <https://source.chromium.org/chromium/chromium/src/+/master:net/third_party/quiche/src/quic/core/congestion_control/bbr_sender.cc>
/// of BBR <https://datatracker.ietf.org/doc/html/draft-cardwell-iccrg-bbr-congestion-control>.
/// More discussion and links at <https://groups.google.com/g/bbr-dev>.
#[derive(Debug, Clone)]
pub struct Bbr {
    config: Arc<BbrConfig>,
    current_mtu: u64,
    sampler: bw_estimation::BandwidthSampler,
    max_bandwidth: MinMax,
    acked_bytes: u64,
    mode: Mode,
    loss_state: LossState,
    recovery_state: RecoveryState,
    recovery_window: u64,
    is_at_full_bandwidth: bool,
    pacing_gain: f32,
    high_gain: f32,
    drain_gain: f32,
    cwnd_gain: f32,
    high_cwnd_gain: f32,
    last_cycle_start: Option<Instant>,
    current_cycle_offset: u8,
    init_cwnd: u64,
    min_cwnd: u64,
    exit_probe_rtt_at: Option<Instant>,
    probe_rtt_round_passed: bool,
    min_rtt: Duration,
    min_rtt_timestamp: Option<Instant>,
    exiting_quiescence: bool,
    pacing_rate: u64,
    bytes_in_flight: u64,
    max_sent_packet_number: u64,
    end_recovery_at_packet_number: u64,
    num_loss_events_in_round: u64,
    bytes_lost_in_round: u64,
    cwnd: u64,
    current_round_trip_end_packet_number: Option<u64>,
    round_count: u64,
    bw_at_last_round: u64,
    round_wo_bw_gain: u64,
    last_sample_is_app_limited: bool,
    has_no_app_limited_sample: bool,
    app_limited_trace: AppLimitedTraceState,
    startup_trace: StartupTraceState,
    random_number_generator: rand::rngs::StdRng,
}

#[derive(Debug, Clone, Default)]
struct AppLimitedTraceState {
    ack_events: u64,
    maybe_calls: u64,
    sampler_calls_total: u64,
    sampler_calls_from_maybe: u64,
    sampler_calls_from_probe_rtt: u64,
    sampler_skipped_retriggers: u64,
    conn_true: u64,
    heuristic_true: u64,
    both_true: u64,
    selected_true: u64,
    app_limited_samples: u64,
    non_app_limited_samples: u64,
    last_conn_app_limited: bool,
    last_heuristic_app_limited: bool,
    last_selected_app_limited: bool,
    last_target_cwnd: u64,
}

#[derive(Debug, Clone, Default)]
struct StartupTraceState {
    ack_events: u64,
    checks: u64,
    skipped_by_gate: u64,
    target_hits: u64,
    no_gain_rounds: u64,
    exits_by_rounds: u64,
    exits_by_loss: u64,
    startup_to_drain: u64,
    drain_to_probe_bw: u64,
}

impl Bbr {
    /// Construct a state using the given `config` and current time `now`
    pub fn new(config: Arc<BbrConfig>, current_mtu: u16, _now: Instant) -> Self {
        let initial_window = config.initial_window;
        Self {
            config,
            current_mtu: current_mtu as u64,
            sampler: Default::default(),
            max_bandwidth: MinMax::default(),
            acked_bytes: 0,
            mode: Mode::Startup,
            loss_state: Default::default(),
            recovery_state: RecoveryState::NotInRecovery,
            recovery_window: 0,
            is_at_full_bandwidth: false,
            pacing_gain: K_DEFAULT_HIGH_GAIN,
            high_gain: K_DEFAULT_HIGH_GAIN,
            drain_gain: 1.0 / K_DEFAULT_HIGH_GAIN,
            cwnd_gain: K_DERIVED_HIGH_CWNDGAIN,
            high_cwnd_gain: K_DERIVED_HIGH_CWNDGAIN,
            last_cycle_start: None,
            current_cycle_offset: 0,
            init_cwnd: initial_window,
            min_cwnd: calculate_min_window(current_mtu as u64),
            exit_probe_rtt_at: None,
            probe_rtt_round_passed: false,
            min_rtt: Default::default(),
            min_rtt_timestamp: None,
            exiting_quiescence: false,
            pacing_rate: 0,
            bytes_in_flight: 0,
            max_sent_packet_number: 0,
            end_recovery_at_packet_number: 0,
            num_loss_events_in_round: 0,
            bytes_lost_in_round: 0,
            cwnd: initial_window,
            current_round_trip_end_packet_number: None,
            round_count: 0,
            bw_at_last_round: 0,
            round_wo_bw_gain: 0,
            last_sample_is_app_limited: false,
            has_no_app_limited_sample: false,
            app_limited_trace: Default::default(),
            startup_trace: Default::default(),
            random_number_generator: rand::rngs::StdRng::from_os_rng(),
        }
    }

    fn bandwidth_estimate(&self) -> u64 {
        self.max_bandwidth.get()
    }

    fn enter_startup_mode(&mut self) {
        self.mode = Mode::Startup;
        self.pacing_gain = self.high_gain;
        self.cwnd_gain = self.high_cwnd_gain;
    }

    fn enter_probe_bandwidth_mode(&mut self, now: Instant) {
        self.mode = Mode::ProbeBw;
        self.cwnd_gain = K_DERIVED_HIGH_CWNDGAIN;
        self.last_cycle_start = Some(now);
        let mut rand_index = self
            .random_number_generator
            .random_range(0..K_PACING_GAIN.len() as u8 - 1);
        if rand_index >= 1 {
            rand_index += 1;
        }
        self.current_cycle_offset = rand_index;
        self.pacing_gain = K_PACING_GAIN[rand_index as usize];
    }

    fn update_round_trip_counter(&mut self, last_acked_packet: u64) -> bool {
        if self
            .current_round_trip_end_packet_number
            .is_none_or(|end| last_acked_packet > end)
        {
            self.round_count = self.round_count.saturating_add(1);
            self.current_round_trip_end_packet_number = Some(self.max_sent_packet_number);
            return true;
        }
        false
    }

    fn maybe_app_limited(&mut self, bytes_in_flight: u64, conn_app_limited: bool) {
        let target_cwnd = self.get_target_cwnd(1.0);
        let heuristic_app_limited =
            heuristic_app_limited_with_pct(bytes_in_flight, target_cwnd, app_limited_target_pct());
        let source = *app_limited_source();
        let app_limited = source.is_app_limited(conn_app_limited, heuristic_app_limited);
        self.trace_app_limited_decision(
            source,
            conn_app_limited,
            heuristic_app_limited,
            app_limited,
            bytes_in_flight,
            target_cwnd,
        );
        if app_limited {
            self.mark_sampler_app_limited("maybe_app_limited");
        }
    }

    fn mark_sampler_app_limited(&mut self, source: &'static str) {
        let was_app_limited = self.sampler.is_app_limited();
        if source == "maybe_app_limited" && app_limited_debounce_enabled() && was_app_limited {
            if app_limited_trace_enabled() {
                self.app_limited_trace.sampler_skipped_retriggers += 1;
                eprintln!(
                    "BBR_APP_LIMITED_SKIP pid={} source={} ack_event={} mode={:?} round={} sampler_skipped_retriggers={} conn_app_limited={} heuristic_app_limited={} selected_app_limited={} target_cwnd={} cwnd={} bytes_in_flight={} bw={} pacing_rate={}",
                    std::process::id(),
                    source,
                    self.app_limited_trace.ack_events,
                    self.mode,
                    self.round_count,
                    self.app_limited_trace.sampler_skipped_retriggers,
                    self.app_limited_trace.last_conn_app_limited,
                    self.app_limited_trace.last_heuristic_app_limited,
                    self.app_limited_trace.last_selected_app_limited,
                    self.app_limited_trace.last_target_cwnd,
                    self.cwnd,
                    self.bytes_in_flight,
                    self.bandwidth_estimate(),
                    self.pacing_rate,
                );
            }
            return;
        }
        self.sampler.on_app_limited();
        if !app_limited_trace_enabled() {
            return;
        }

        match source {
            "maybe_app_limited" => self.app_limited_trace.sampler_calls_from_maybe += 1,
            "probe_rtt" => self.app_limited_trace.sampler_calls_from_probe_rtt += 1,
            _ => {}
        }
        self.app_limited_trace.sampler_calls_total += 1;
        eprintln!(
            "BBR_APP_LIMITED_TRIGGER pid={} source={} ack_event={} mode={:?} round={} was_sampler_app_limited={} now_sampler_app_limited={} sampler_calls_total={} sampler_calls_from_maybe={} sampler_calls_from_probe_rtt={} conn_app_limited={} heuristic_app_limited={} selected_app_limited={} target_cwnd={} cwnd={} bytes_in_flight={} bw={} pacing_rate={}",
            std::process::id(),
            source,
            self.app_limited_trace.ack_events,
            self.mode,
            self.round_count,
            was_app_limited,
            self.sampler.is_app_limited(),
            self.app_limited_trace.sampler_calls_total,
            self.app_limited_trace.sampler_calls_from_maybe,
            self.app_limited_trace.sampler_calls_from_probe_rtt,
            self.app_limited_trace.last_conn_app_limited,
            self.app_limited_trace.last_heuristic_app_limited,
            self.app_limited_trace.last_selected_app_limited,
            self.app_limited_trace.last_target_cwnd,
            self.cwnd,
            self.bytes_in_flight,
            self.bandwidth_estimate(),
            self.pacing_rate,
        );
    }

    fn trace_state_snapshot(
        &self,
        acked_bytes: u64,
        lost_bytes: u64,
        prior_in_flight: u64,
        event_app_limited: bool,
        sample: &bw_estimation::CongestionEventSample,
        sample_max_bw: u64,
        sample_is_app_limited: bool,
        is_round_start: bool,
    ) {
        if !state_trace_enabled() {
            return;
        }

        eprintln!(
            "BBR_STATE pid={} stage=ack_event round={} round_start={} mode={:?} recovery={:?} event_app_limited={} sampler_is_app_limited={} last_sample_is_app_limited={} has_non_app_limited_sample={} sample_bw={} sample_bw_non_app_limited={} sample_is_app_limited={} bytes_acked={} bytes_lost={} prior_in_flight={} bytes_in_flight={} cwnd={} recovery_window={} bw={} pacing_rate={} is_at_full_bandwidth={} bytes_lost_in_round={} num_loss_events_in_round={}",
            std::process::id(),
            self.round_count,
            is_round_start,
            self.mode,
            self.recovery_state,
            event_app_limited,
            self.sampler.is_app_limited(),
            self.last_sample_is_app_limited,
            sample.has_non_app_limited_sample,
            sample_max_bw,
            sample.sample_max_bandwidth_non_app_limited,
            sample_is_app_limited,
            acked_bytes,
            lost_bytes,
            prior_in_flight,
            self.bytes_in_flight,
            self.cwnd,
            self.recovery_window,
            self.bandwidth_estimate(),
            self.pacing_rate,
            self.is_at_full_bandwidth,
            self.bytes_lost_in_round,
            self.num_loss_events_in_round,
        );
    }

    fn trace_state_transition(
        &self,
        stage: &'static str,
        detail: &'static str,
        last_acked_packet: Option<u64>,
    ) {
        if !state_trace_enabled() {
            return;
        }

        eprintln!(
            "BBR_STATE pid={} stage={} detail={} round={} mode={:?} recovery={:?} last_acked_packet={} bytes_in_flight={} cwnd={} recovery_window={} bw={} pacing_rate={} is_at_full_bandwidth={} bytes_lost_in_round={} num_loss_events_in_round={} sampler_is_app_limited={} last_sample_is_app_limited={}",
            std::process::id(),
            stage,
            detail,
            self.round_count,
            self.mode,
            self.recovery_state,
            last_acked_packet.unwrap_or_default(),
            self.bytes_in_flight,
            self.cwnd,
            self.recovery_window,
            self.bandwidth_estimate(),
            self.pacing_rate,
            self.is_at_full_bandwidth,
            self.bytes_lost_in_round,
            self.num_loss_events_in_round,
            self.sampler.is_app_limited(),
            self.last_sample_is_app_limited,
        );
    }

    fn trace_app_limited_decision(
        &mut self,
        source: AppLimitedSource,
        conn_app_limited: bool,
        heuristic_app_limited: bool,
        selected: bool,
        bytes_in_flight: u64,
        target_cwnd: u64,
    ) {
        if !app_limited_trace_enabled() {
            return;
        }

        self.app_limited_trace.maybe_calls += 1;
        self.app_limited_trace.last_conn_app_limited = conn_app_limited;
        self.app_limited_trace.last_heuristic_app_limited = heuristic_app_limited;
        self.app_limited_trace.last_selected_app_limited = selected;
        self.app_limited_trace.last_target_cwnd = target_cwnd;
        if conn_app_limited {
            self.app_limited_trace.conn_true += 1;
        }
        if heuristic_app_limited {
            self.app_limited_trace.heuristic_true += 1;
        }
        if conn_app_limited && heuristic_app_limited {
            self.app_limited_trace.both_true += 1;
        }
        if selected {
            self.app_limited_trace.selected_true += 1;
        }

        if selected || self.app_limited_trace.ack_events % app_limited_trace_every() == 0 {
            eprintln!(
                "BBR_APP_LIMITED_DECISION pid={} ack_event={} source={:?} conn_app_limited={} heuristic_app_limited={} selected={} conn_true={} heuristic_true={} both_true={} selected_true={} maybe_calls={} mode={:?} round={} bytes_in_flight={} target_cwnd={} cwnd={} bw={} pacing_rate={}",
                std::process::id(),
                self.app_limited_trace.ack_events,
                source,
                conn_app_limited,
                heuristic_app_limited,
                selected,
                self.app_limited_trace.conn_true,
                self.app_limited_trace.heuristic_true,
                self.app_limited_trace.both_true,
                self.app_limited_trace.selected_true,
                self.app_limited_trace.maybe_calls,
                self.mode,
                self.round_count,
                bytes_in_flight,
                target_cwnd,
                self.cwnd,
                self.bandwidth_estimate(),
                self.pacing_rate,
            );
        }
    }

    fn trace_app_limited_sample(
        &mut self,
        event: &AckEvent<'_>,
        sample: &bw_estimation::CongestionEventSample,
        sample_max_bw: u64,
        sample_is_app_limited: bool,
        measurement: Option<u64>,
        current_bw: u64,
    ) {
        if !app_limited_trace_enabled() {
            return;
        }

        if sample_is_app_limited {
            self.app_limited_trace.app_limited_samples += 1;
        } else {
            self.app_limited_trace.non_app_limited_samples += 1;
        }

        let low_sample =
            current_bw != 0 && u128::from(sample_max_bw) * 100 < u128::from(current_bw) * 75;
        if sample_is_app_limited
            || low_sample
            || self.app_limited_trace.ack_events % app_limited_trace_every() == 0
        {
            eprintln!(
                "BBR_APP_LIMITED_SAMPLE pid={} ack_event={} mode={:?} round={} event_app_limited={} last_conn_app_limited={} last_heuristic_app_limited={} last_selected_app_limited={} sampler_is_app_limited={} bytes_acked={} bytes_lost={} prior_in_flight={} sample_bw={} sample_bw_non_app_limited={} sample_is_app_limited={} has_non_app_limited_sample={} measurement={} current_bw={} low_sample={} app_limited_samples={} non_app_limited_samples={} last_sample_is_app_limited={} cwnd={} pacing_rate={}",
                std::process::id(),
                self.app_limited_trace.ack_events,
                self.mode,
                self.round_count,
                event.app_limited,
                self.app_limited_trace.last_conn_app_limited,
                self.app_limited_trace.last_heuristic_app_limited,
                self.app_limited_trace.last_selected_app_limited,
                self.sampler.is_app_limited(),
                sample.bytes_acked,
                sample.bytes_lost,
                event.prior_in_flight,
                sample_max_bw,
                sample.sample_max_bandwidth_non_app_limited,
                sample_is_app_limited,
                sample.has_non_app_limited_sample,
                measurement.unwrap_or(0),
                current_bw,
                low_sample,
                self.app_limited_trace.app_limited_samples,
                self.app_limited_trace.non_app_limited_samples,
                self.last_sample_is_app_limited,
                self.cwnd,
                self.pacing_rate,
            );
        }
    }

    fn update_recovery_state(
        &mut self,
        last_acked_packet: u64,
        _has_losses: bool,
        is_round_start: bool,
    ) {
        if !self.is_at_full_bandwidth {
            return;
        }

        let enter_recovery = self.loss_state.should_enter_recovery(&self.config);
        let previous_state = self.recovery_state;

        if enter_recovery {
            self.end_recovery_at_packet_number = self.max_sent_packet_number;
        }

        match self.recovery_state {
            RecoveryState::NotInRecovery if enter_recovery => {
                self.recovery_state = RecoveryState::Conservation;
                self.recovery_window = 0;
                self.current_round_trip_end_packet_number = Some(self.max_sent_packet_number);
            }
            RecoveryState::Conservation | RecoveryState::Growth => {
                if self.recovery_state == RecoveryState::Conservation && is_round_start {
                    self.recovery_state = RecoveryState::Growth;
                }
                if !enter_recovery && last_acked_packet > self.end_recovery_at_packet_number {
                    self.recovery_state = RecoveryState::NotInRecovery;
                }
            }
            _ => {}
        }

        if self.recovery_state != previous_state {
            let detail = match (previous_state, self.recovery_state) {
                (RecoveryState::NotInRecovery, RecoveryState::Conservation) => "enter_recovery",
                (RecoveryState::Conservation, RecoveryState::Growth) => "recovery_growth",
                (_, RecoveryState::NotInRecovery) => "exit_recovery",
                _ => "recovery_transition",
            };
            self.trace_state_transition(
                detail,
                if is_round_start { "round_start" } else { "ack" },
                Some(last_acked_packet),
            );
        }
    }

    fn update_gain_cycle_phase(&mut self, now: Instant, prior_in_flight: u64, has_losses: bool) {
        let mut should_advance_gain_cycling = self
            .last_cycle_start
            .map(|last_cycle_start| now.duration_since(last_cycle_start) > self.min_rtt)
            .unwrap_or(false);

        if self.pacing_gain > 1.0
            && !has_losses
            && prior_in_flight < self.get_target_cwnd(self.pacing_gain)
        {
            should_advance_gain_cycling = false;
        }

        if self.pacing_gain < 1.0 && self.bytes_in_flight <= self.get_target_cwnd(1.0) {
            should_advance_gain_cycling = true;
        }

        if should_advance_gain_cycling {
            self.current_cycle_offset = (self.current_cycle_offset + 1) % K_PACING_GAIN.len() as u8;
            self.last_cycle_start = Some(now);
            if DRAIN_TO_TARGET
                && self.pacing_gain < 1.0
                && (K_PACING_GAIN[self.current_cycle_offset as usize] - 1.0).abs() < f32::EPSILON
                && self.bytes_in_flight > self.get_target_cwnd(1.0)
            {
                return;
            }
            self.pacing_gain = K_PACING_GAIN[self.current_cycle_offset as usize];
        }
    }

    fn maybe_exit_startup_or_drain(&mut self, now: Instant) {
        if self.mode == Mode::Startup && self.is_at_full_bandwidth {
            if startup_trace_enabled() {
                self.startup_trace.startup_to_drain += 1;
                eprintln!(
                    "BBR_STARTUP_MODE pid={} ack_event={} transition=startup_to_drain count={} round={} bw={} bw_at_last_round={} round_wo_bw_gain={} bytes_lost_in_round={} num_loss_events_in_round={} last_sample_is_app_limited={} bytes_in_flight={} target_cwnd={} cwnd={} pacing_rate={}",
                    std::process::id(),
                    self.startup_trace.ack_events,
                    self.startup_trace.startup_to_drain,
                    self.round_count,
                    self.bandwidth_estimate(),
                    self.bw_at_last_round,
                    self.round_wo_bw_gain,
                    self.bytes_lost_in_round,
                    self.num_loss_events_in_round,
                    self.last_sample_is_app_limited,
                    self.bytes_in_flight,
                    self.get_target_cwnd(1.0),
                    self.cwnd,
                    self.pacing_rate,
                );
            }
            self.mode = Mode::Drain;
            self.pacing_gain = self.drain_gain;
            self.cwnd_gain = self.high_cwnd_gain;
            self.trace_state_transition("mode", "startup_to_drain", None);
        }
        if self.mode == Mode::Drain && self.bytes_in_flight <= self.get_target_cwnd(1.0) {
            if startup_trace_enabled() {
                self.startup_trace.drain_to_probe_bw += 1;
                eprintln!(
                    "BBR_STARTUP_MODE pid={} ack_event={} transition=drain_to_probe_bw count={} round={} bw={} bw_at_last_round={} round_wo_bw_gain={} bytes_in_flight={} target_cwnd={} cwnd={} pacing_rate={}",
                    std::process::id(),
                    self.startup_trace.ack_events,
                    self.startup_trace.drain_to_probe_bw,
                    self.round_count,
                    self.bandwidth_estimate(),
                    self.bw_at_last_round,
                    self.round_wo_bw_gain,
                    self.bytes_in_flight,
                    self.get_target_cwnd(1.0),
                    self.cwnd,
                    self.pacing_rate,
                );
            }
            self.enter_probe_bandwidth_mode(now);
            self.trace_state_transition("mode", "drain_to_probe_bw", None);
        }
    }

    fn maybe_update_min_rtt(&mut self, now: Instant, sample_min_rtt: Duration) -> bool {
        let min_rtt_expired = self
            .min_rtt_timestamp
            .map(|timestamp| now.saturating_duration_since(timestamp) > Duration::from_secs(10))
            .unwrap_or(false);
        if min_rtt_expired || self.min_rtt.is_zero() || sample_min_rtt < self.min_rtt {
            self.min_rtt = sample_min_rtt;
            self.min_rtt_timestamp = Some(now);
        }
        min_rtt_expired
    }

    fn maybe_enter_or_exit_probe_rtt(
        &mut self,
        now: Instant,
        is_round_start: bool,
        min_rtt_expired: bool,
        event_app_limited: bool,
        sampler_is_app_limited: bool,
    ) {
        if min_rtt_expired && !self.exiting_quiescence && self.mode != Mode::ProbeRtt {
            let probe_rtt_cwnd = self.get_probe_rtt_cwnd() + self.current_mtu;
            if probe_rtt_entry_strategy().should_enter(
                self.bytes_in_flight,
                probe_rtt_cwnd,
                self.last_sample_is_app_limited,
                event_app_limited,
                sampler_is_app_limited,
            ) {
                self.mode = Mode::ProbeRtt;
                self.pacing_gain = 1.0;
                self.exit_probe_rtt_at = None;
                self.trace_state_transition("mode", "enter_probe_rtt", None);
            }
        }

        if self.mode == Mode::ProbeRtt {
            self.mark_sampler_app_limited("probe_rtt");

            match self.exit_probe_rtt_at {
                None => {
                    if self.bytes_in_flight < self.get_probe_rtt_cwnd() + self.current_mtu {
                        self.exit_probe_rtt_at = Some(now + Duration::from_millis(200));
                        self.probe_rtt_round_passed = false;
                    }
                }
                Some(exit_time) => {
                    if is_round_start {
                        self.probe_rtt_round_passed = true;
                    }
                    if now >= exit_time && self.probe_rtt_round_passed {
                        self.min_rtt_timestamp = Some(now);
                        if !self.is_at_full_bandwidth {
                            self.enter_startup_mode();
                            self.trace_state_transition("mode", "exit_probe_rtt_to_startup", None);
                        } else {
                            self.enter_probe_bandwidth_mode(now);
                            self.trace_state_transition("mode", "exit_probe_rtt_to_probe_bw", None);
                        }
                    }
                }
            }
        }

        self.exiting_quiescence = false;
    }

    fn min_rtt_for_model(&self) -> Duration {
        if !self.min_rtt.is_zero() {
            self.min_rtt
        } else {
            Duration::from_millis(100)
        }
    }

    fn get_target_cwnd(&self, gain: f32) -> u64 {
        let bw = self.bandwidth_estimate();
        let bdp = self.min_rtt_for_model().as_micros() as u64 * bw;
        let cwnd = ((gain as f64 * bdp as f64) / 1_000_000f64) as u64;
        if cwnd == 0 {
            return ((gain as f64 * self.init_cwnd as f64) as u64).max(self.min_cwnd);
        }
        cwnd.max(self.min_cwnd)
    }

    fn get_probe_rtt_cwnd(&self) -> u64 {
        self.min_cwnd
    }

    fn calculate_pacing_rate(&mut self, _bytes_lost: u64) {
        let bw = self.bandwidth_estimate();
        if bw == 0 {
            return;
        }
        let target_rate = (bw as f64 * self.pacing_gain as f64) as u64;
        if self.is_at_full_bandwidth {
            self.pacing_rate = target_rate;
            return;
        }

        if self.pacing_rate == 0 && !self.min_rtt.is_zero() {
            if let Some(rate) =
                bw_estimation::BandwidthSampler::bw_from_delta(self.init_cwnd, self.min_rtt)
            {
                self.pacing_rate = rate;
            }
            return;
        }

        self.pacing_rate = self.pacing_rate.max(target_rate);
    }

    fn calculate_cwnd(&mut self, bytes_acked: u64, _excess_acked: u64) {
        if self.mode == Mode::ProbeRtt {
            return;
        }
        let mut target_window = self.get_target_cwnd(self.cwnd_gain);
        if self.is_at_full_bandwidth {
            target_window = target_window.saturating_add(self.sampler.max_ack_height());
        }

        if self.is_at_full_bandwidth {
            self.cwnd = target_window.min(self.cwnd.saturating_add(bytes_acked));
        } else if self.cwnd < target_window || self.acked_bytes < self.init_cwnd {
            self.cwnd = self.cwnd.saturating_add(bytes_acked);
        }

        self.cwnd = self.cwnd.max(self.min_cwnd);
    }

    fn calculate_recovery_window(&mut self, bytes_acked: u64, bytes_lost: u64) {
        if !self.recovery_state.in_recovery() {
            return;
        }

        if self.recovery_window == 0 {
            self.recovery_window = self
                .bytes_in_flight
                .saturating_add(bytes_acked)
                .max(self.min_cwnd);
            return;
        }

        let effective_loss = self
            .loss_state
            .effective_recovery_loss(&self.config, bytes_lost);

        if self.recovery_window >= effective_loss {
            self.recovery_window -= effective_loss;
        } else {
            self.recovery_window = self.current_mtu;
        }
        if self.recovery_state == RecoveryState::Growth {
            self.recovery_window = self.recovery_window.saturating_add(bytes_acked);
        }

        self.recovery_window = self
            .recovery_window
            .max(self.bytes_in_flight.saturating_add(bytes_acked))
            .max(self.min_cwnd);
    }

    fn check_if_full_bw_reached(
        &mut self,
        last_packet_send_state: &bw_estimation::SendTimeState,
        sample_is_app_limited: bool,
        has_non_app_limited_sample: bool,
        event_app_limited: bool,
        sampler_is_app_limited: bool,
    ) {
        let gate = startup_full_bw_gate_strategy();
        let target =
            (self.bw_at_last_round as f64 * self.config.startup_growth_target as f64) as u64;
        let bw = self.bandwidth_estimate();
        if gate.skip_check(
            self.last_sample_is_app_limited,
            sample_is_app_limited,
            event_app_limited,
            sampler_is_app_limited,
        ) {
            if startup_trace_enabled() {
                self.startup_trace.checks += 1;
                self.startup_trace.skipped_by_gate += 1;
                eprintln!(
                    "BBR_STARTUP_CHECK pid={} ack_event={} stage=skip gate={:?} checks={} skipped_by_gate={} target_hits={} no_gain_rounds={} exits_by_rounds={} exits_by_loss={} round={} mode={:?} bw={} bw_at_last_round={} target={} growth_target={} round_wo_bw_gain={} startup_round_limit={} last_sample_is_app_limited={} sample_is_app_limited={} has_non_app_limited_sample={} event_app_limited={} sampler_is_app_limited={} send_state_valid={} inflight_at_send={} bytes_lost_in_round={} num_loss_events_in_round={} is_at_full_bandwidth={} bytes_in_flight={} cwnd={} pacing_rate={}",
                    std::process::id(),
                    self.startup_trace.ack_events,
                    gate,
                    self.startup_trace.checks,
                    self.startup_trace.skipped_by_gate,
                    self.startup_trace.target_hits,
                    self.startup_trace.no_gain_rounds,
                    self.startup_trace.exits_by_rounds,
                    self.startup_trace.exits_by_loss,
                    self.round_count,
                    self.mode,
                    bw,
                    self.bw_at_last_round,
                    target,
                    self.config.startup_growth_target,
                    self.round_wo_bw_gain,
                    self.config.startup_rounds_without_growth_before_exit,
                    self.last_sample_is_app_limited,
                    sample_is_app_limited,
                    has_non_app_limited_sample,
                    event_app_limited,
                    sampler_is_app_limited,
                    last_packet_send_state.is_valid,
                    last_packet_send_state.bytes_in_flight,
                    self.bytes_lost_in_round,
                    self.num_loss_events_in_round,
                    self.is_at_full_bandwidth,
                    self.bytes_in_flight,
                    self.cwnd,
                    self.pacing_rate,
                );
            }
            return;
        }
        if startup_trace_enabled() {
            self.startup_trace.checks += 1;
        }
        if bw >= target {
            self.bw_at_last_round = bw;
            self.round_wo_bw_gain = 0;
            if startup_trace_enabled() {
                self.startup_trace.target_hits += 1;
                eprintln!(
                    "BBR_STARTUP_CHECK pid={} ack_event={} stage=target_hit gate={:?} checks={} skipped_by_gate={} target_hits={} no_gain_rounds={} exits_by_rounds={} exits_by_loss={} round={} mode={:?} bw={} bw_at_last_round={} target={} growth_target={} round_wo_bw_gain={} startup_round_limit={} last_sample_is_app_limited={} sample_is_app_limited={} has_non_app_limited_sample={} event_app_limited={} sampler_is_app_limited={} send_state_valid={} inflight_at_send={} bytes_lost_in_round={} num_loss_events_in_round={} is_at_full_bandwidth={} bytes_in_flight={} cwnd={} pacing_rate={}",
                    std::process::id(),
                    self.startup_trace.ack_events,
                    gate,
                    self.startup_trace.checks,
                    self.startup_trace.skipped_by_gate,
                    self.startup_trace.target_hits,
                    self.startup_trace.no_gain_rounds,
                    self.startup_trace.exits_by_rounds,
                    self.startup_trace.exits_by_loss,
                    self.round_count,
                    self.mode,
                    bw,
                    self.bw_at_last_round,
                    target,
                    self.config.startup_growth_target,
                    self.round_wo_bw_gain,
                    self.config.startup_rounds_without_growth_before_exit,
                    self.last_sample_is_app_limited,
                    sample_is_app_limited,
                    has_non_app_limited_sample,
                    event_app_limited,
                    sampler_is_app_limited,
                    last_packet_send_state.is_valid,
                    last_packet_send_state.bytes_in_flight,
                    self.bytes_lost_in_round,
                    self.num_loss_events_in_round,
                    self.is_at_full_bandwidth,
                    self.bytes_in_flight,
                    self.cwnd,
                    self.pacing_rate,
                );
            }
            return;
        }

        self.round_wo_bw_gain = self.round_wo_bw_gain.saturating_add(1);
        let exit_due_to_rounds =
            self.round_wo_bw_gain >= self.config.startup_rounds_without_growth_before_exit as u64;
        let exit_due_to_loss = self.should_exit_startup_due_to_loss(last_packet_send_state);
        if startup_trace_enabled() {
            self.startup_trace.no_gain_rounds += 1;
            if exit_due_to_rounds {
                self.startup_trace.exits_by_rounds += 1;
            }
            if exit_due_to_loss {
                self.startup_trace.exits_by_loss += 1;
            }
            eprintln!(
                "BBR_STARTUP_CHECK pid={} ack_event={} stage=no_gain gate={:?} checks={} skipped_by_gate={} target_hits={} no_gain_rounds={} exits_by_rounds={} exits_by_loss={} round={} mode={:?} bw={} bw_at_last_round={} target={} growth_target={} round_wo_bw_gain={} startup_round_limit={} last_sample_is_app_limited={} sample_is_app_limited={} has_non_app_limited_sample={} event_app_limited={} sampler_is_app_limited={} send_state_valid={} inflight_at_send={} bytes_lost_in_round={} num_loss_events_in_round={} exit_due_to_rounds={} exit_due_to_loss={} is_at_full_bandwidth={} bytes_in_flight={} cwnd={} pacing_rate={}",
                std::process::id(),
                self.startup_trace.ack_events,
                gate,
                self.startup_trace.checks,
                self.startup_trace.skipped_by_gate,
                self.startup_trace.target_hits,
                self.startup_trace.no_gain_rounds,
                self.startup_trace.exits_by_rounds,
                self.startup_trace.exits_by_loss,
                self.round_count,
                self.mode,
                bw,
                self.bw_at_last_round,
                target,
                self.config.startup_growth_target,
                self.round_wo_bw_gain,
                self.config.startup_rounds_without_growth_before_exit,
                self.last_sample_is_app_limited,
                sample_is_app_limited,
                has_non_app_limited_sample,
                event_app_limited,
                sampler_is_app_limited,
                last_packet_send_state.is_valid,
                last_packet_send_state.bytes_in_flight,
                self.bytes_lost_in_round,
                self.num_loss_events_in_round,
                exit_due_to_rounds,
                exit_due_to_loss,
                self.is_at_full_bandwidth,
                self.bytes_in_flight,
                self.cwnd,
                self.pacing_rate,
            );
        }
        if exit_due_to_rounds || exit_due_to_loss {
            self.is_at_full_bandwidth = true;
        }
    }

    fn should_exit_startup_due_to_loss(
        &self,
        last_packet_send_state: &bw_estimation::SendTimeState,
    ) -> bool {
        if self.num_loss_events_in_round < K_STARTUP_FULL_LOSS_COUNT
            || !last_packet_send_state.is_valid
        {
            return false;
        }
        let inflight_at_send = last_packet_send_state.bytes_in_flight;
        inflight_at_send > 0
            && self.bytes_lost_in_round
                > ((inflight_at_send as f64) * K_STARTUP_LOSS_THRESHOLD) as u64
    }
}

impl Controller for Bbr {
    fn on_sent(&mut self, now: Instant, bytes: u64, last_packet_number: u64, bytes_in_flight: u64) {
        self.max_sent_packet_number = last_packet_number;
        let bytes_in_flight_before_send =
            if env_bool("HY_RS_BBR_ON_SENT_POST_FLIGHT").unwrap_or(false) {
                bytes_in_flight
            } else {
                bytes_in_flight.saturating_sub(bytes)
            };
        if bytes_in_flight_before_send == 0 {
            self.exiting_quiescence = true;
        }
        self.bytes_in_flight = bytes_in_flight_before_send;
        self.sampler.on_packet_sent(
            now,
            last_packet_number,
            bytes,
            bytes_in_flight_before_send,
            bytes != 0,
        );
    }

    fn on_ack(
        &mut self,
        _now: Instant,
        _packet_number: u64,
        _sent: Instant,
        _bytes: u64,
        _app_limited: bool,
        _rtt: &RttEstimator,
    ) {
    }

    fn on_ack_event(&mut self, now: Instant, event: &AckEvent<'_>) {
        if app_limited_trace_enabled() {
            self.app_limited_trace.ack_events += 1;
        }
        if startup_trace_enabled() {
            self.startup_trace.ack_events += 1;
        }
        self.maybe_app_limited(event.prior_in_flight, event.app_limited);
        self.bytes_in_flight = event.prior_in_flight;
        for packet in event.acked_packets {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(packet.bytes_acked);
        }
        for packet in event.lost_packets {
            self.bytes_in_flight = self.bytes_in_flight.saturating_sub(packet.bytes_lost);
        }

        let is_round_start = round_gating_strategy()
            .round_packet_number(event)
            .map(|packet| self.update_round_trip_counter(packet))
            .unwrap_or(false);
        if let Some(last_acked_packet) = event
            .acked_packets
            .last()
            .map(|packet| packet.packet_number)
        {
            self.update_recovery_state(
                last_acked_packet,
                !event.lost_packets.is_empty(),
                is_round_start,
            );
        }
        let sample = self.sampler.on_congestion_event(
            now,
            event.acked_packets,
            event.lost_packets,
            self.bandwidth_estimate(),
            u64::MAX,
            self.round_count,
            event.app_limited,
        );
        if sample.last_packet_send_state.is_valid {
            self.last_sample_is_app_limited = sample.last_packet_send_state.is_app_limited;
            self.has_no_app_limited_sample |= !self.last_sample_is_app_limited;
        }
        let current_bw = self.bandwidth_estimate();
        let (sample_max_bw, sample_is_app_limited) = max_bw_app_limited_strategy().candidate(
            sample.sample_max_bandwidth,
            sample.sample_max_bandwidth_non_app_limited,
            sample.sample_is_app_limited,
            sample.has_non_app_limited_sample,
            event.app_limited,
            self.sampler.is_app_limited(),
        );
        let measurement = max_bw_update_strategy().measurement(
            current_bw,
            sample_max_bw,
            sample_is_app_limited,
            sample.bytes_acked,
        );
        self.trace_app_limited_sample(
            event,
            &sample,
            sample_max_bw,
            sample_is_app_limited,
            measurement,
            current_bw,
        );
        if let Some(measurement) = measurement {
            self.max_bandwidth.update_max(self.round_count, measurement);
        }
        let min_rtt_expired = sample
            .sample_rtt
            .map(|sample_rtt| self.maybe_update_min_rtt(now, sample_rtt))
            .unwrap_or(false);

        if self.mode == Mode::ProbeBw {
            self.update_gain_cycle_phase(
                now,
                event.prior_in_flight,
                !event.lost_packets.is_empty(),
            );
        }

        if is_round_start && !self.is_at_full_bandwidth {
            self.check_if_full_bw_reached(
                &sample.last_packet_send_state,
                sample.sample_is_app_limited,
                sample.has_non_app_limited_sample,
                event.app_limited,
                self.sampler.is_app_limited(),
            );
        }

        let bytes_acked = sample.bytes_acked;
        self.acked_bytes = self.acked_bytes.saturating_add(bytes_acked);

        self.maybe_exit_startup_or_drain(now);
        self.maybe_enter_or_exit_probe_rtt(
            now,
            is_round_start,
            min_rtt_expired,
            event.app_limited,
            self.sampler.is_app_limited(),
        );

        self.calculate_pacing_rate(sample.bytes_lost);
        self.calculate_cwnd(bytes_acked, sample.extra_acked);
        self.calculate_recovery_window(bytes_acked, sample.bytes_lost);
        if is_round_start || sample.bytes_lost > 0 {
            self.trace_state_snapshot(
                bytes_acked,
                sample.bytes_lost,
                event.prior_in_flight,
                event.app_limited,
                &sample,
                sample_max_bw,
                sample_is_app_limited,
                is_round_start,
            );
        }

        let least_unacked = event
            .acked_packets
            .last()
            .map(|packet| packet.packet_number)
            .map(|packet| packet.saturating_sub(2))
            .or_else(|| {
                event
                    .lost_packets
                    .last()
                    .map(|packet| packet.packet_number.saturating_add(1))
            });
        if let Some(least_unacked) = least_unacked {
            self.sampler.remove_obsolete_packets(least_unacked);
        }

        self.loss_state.reset();
        if is_round_start {
            self.num_loss_events_in_round = 0;
            self.bytes_lost_in_round = 0;
        }
    }

    fn on_end_acks(
        &mut self,
        _now: Instant,
        in_flight: u64,
        _prior_in_flight: u64,
        _app_limited: bool,
        _largest_packet_num_acked: Option<u64>,
    ) {
        self.bytes_in_flight = in_flight;
    }

    fn on_congestion_event(
        &mut self,
        _now: Instant,
        _sent: Instant,
        is_persistent_congestion: bool,
        _lost_packets: u64,
        lost_bytes: u64,
        _bytes_in_flight_before_loss: u64,
    ) {
        self.loss_state.lost_bytes = self.loss_state.lost_bytes.saturating_add(lost_bytes);
        self.loss_state.persistent_congestion |= is_persistent_congestion;
        if lost_bytes > 0 && self.loss_state.should_enter_recovery(&self.config) {
            self.num_loss_events_in_round = self.num_loss_events_in_round.saturating_add(1);
            self.bytes_lost_in_round = self.bytes_lost_in_round.saturating_add(lost_bytes);
        }
    }

    fn on_mtu_update(&mut self, new_mtu: u16) {
        self.current_mtu = new_mtu as u64;
        self.min_cwnd = calculate_min_window(self.current_mtu);
        self.init_cwnd = self.config.initial_window.max(self.min_cwnd);
        self.cwnd = self.cwnd.max(self.min_cwnd);
    }

    fn window(&self) -> u64 {
        if self.mode == Mode::ProbeRtt {
            return self.get_probe_rtt_cwnd();
        } else if self.recovery_state.in_recovery() && self.mode != Mode::Startup {
            return self.cwnd.min(self.recovery_window);
        }
        self.cwnd
    }

    fn metrics(&self) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: None,
            pacing_rate: (self.pacing_rate != 0).then_some(
                self.pacing_rate
                    .max(K_MIN_BBR_PACER_BYTES_PER_SECOND)
                    .saturating_mul(8),
            ),
            pacing_behavior: if env_bool("HY_RS_BBR_USE_RATE_BUCKET").unwrap_or(false) {
                super::PacingBehavior::RateTokenBucket
            } else {
                super::PacingBehavior::Window
            },
        }
    }

    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(self.clone())
    }

    fn initial_window(&self) -> u64 {
        self.config.initial_window
    }

    fn into_any(self: Box<Self>) -> Box<dyn Any> {
        self
    }
}

/// Configuration for the [`Bbr`] congestion controller
#[derive(Debug, Clone)]
pub struct BbrConfig {
    initial_window: u64,
    startup_growth_target: f32,
    startup_rounds_without_growth_before_exit: u8,
    exit_startup_on_recovery: bool,
    recover_on_non_persistent_loss: bool,
    non_persistent_loss_reduction_factor: f32,
}

impl BbrConfig {
    /// Default limit on the amount of outstanding data in bytes.
    ///
    /// Recommended value: `min(10 * max_datagram_size, max(2 * max_datagram_size, 14720))`
    pub fn initial_window(&mut self, value: u64) -> &mut Self {
        self.initial_window = value;
        self
    }

    /// Required bandwidth-growth ratio to stay in STARTUP.
    pub fn startup_growth_target(&mut self, value: f32) -> &mut Self {
        self.startup_growth_target = value.max(1.0);
        self
    }

    /// Number of consecutive rounds without enough growth before exiting STARTUP.
    pub fn startup_rounds_without_growth_before_exit(&mut self, value: u8) -> &mut Self {
        self.startup_rounds_without_growth_before_exit = value.max(1);
        self
    }

    /// Whether entering recovery should immediately mark the connection as bandwidth-saturated.
    pub fn exit_startup_on_recovery(&mut self, value: bool) -> &mut Self {
        self.exit_startup_on_recovery = value;
        self
    }

    /// Whether non-persistent loss should trigger BBR recovery behavior.
    pub fn recover_on_non_persistent_loss(&mut self, value: bool) -> &mut Self {
        self.recover_on_non_persistent_loss = value;
        self
    }

    /// The fraction of non-persistent loss that should reduce the recovery window.
    pub fn non_persistent_loss_reduction_factor(&mut self, value: f32) -> &mut Self {
        self.non_persistent_loss_reduction_factor = value.clamp(0.0, 1.0);
        self
    }
}

impl Default for BbrConfig {
    fn default() -> Self {
        Self {
            initial_window: K_INITIAL_CONGESTION_WINDOW_PACKETS * BASE_DATAGRAM_SIZE,
            startup_growth_target: K_STARTUP_GROWTH_TARGET,
            startup_rounds_without_growth_before_exit:
                K_ROUND_TRIPS_WITHOUT_GROWTH_BEFORE_EXITING_STARTUP,
            exit_startup_on_recovery: true,
            recover_on_non_persistent_loss: false,
            non_persistent_loss_reduction_factor: 1.0,
        }
    }
}

impl ControllerFactory for BbrConfig {
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller> {
        Box::new(Bbr::new(self, current_mtu, now))
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum Mode {
    // Startup phase of the connection.
    Startup,
    // After achieving the highest possible bandwidth during the startup, lower
    // the pacing rate in order to drain the queue.
    Drain,
    // Cruising mode.
    ProbeBw,
    // Temporarily slow down sending in order to empty the buffer and measure
    // the real minimum RTT.
    ProbeRtt,
}

// Indicates how the congestion control limits the amount of bytes in flight.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum RecoveryState {
    // Do not limit.
    NotInRecovery,
    // Allow an extra outstanding byte for each byte acknowledged.
    Conservation,
    // Allow two extra outstanding bytes for each byte acknowledged (slow
    // start).
    Growth,
}

impl RecoveryState {
    pub(super) fn in_recovery(&self) -> bool {
        !matches!(self, Self::NotInRecovery)
    }
}

#[derive(Debug, Clone, Default)]
struct LossState {
    lost_bytes: u64,
    persistent_congestion: bool,
}

impl LossState {
    pub(super) fn reset(&mut self) {
        self.lost_bytes = 0;
        self.persistent_congestion = false;
    }

    pub(super) fn has_losses(&self) -> bool {
        self.lost_bytes != 0
    }

    fn should_enter_recovery(&self, config: &BbrConfig) -> bool {
        self.has_losses() && (self.persistent_congestion || config.recover_on_non_persistent_loss)
    }

    fn effective_recovery_loss(&self, config: &BbrConfig, bytes_lost: u64) -> u64 {
        if !self.has_losses() {
            return 0;
        }
        if self.persistent_congestion || config.recover_on_non_persistent_loss {
            if self.persistent_congestion {
                return bytes_lost;
            }
            return ((bytes_lost as f64) * config.non_persistent_loss_reduction_factor as f64)
                .round() as u64;
        }
        0
    }
}

fn calculate_min_window(current_mtu: u64) -> u64 {
    4 * current_mtu
}

// The gain used for the STARTUP, equal to 2/ln(2).
const K_DEFAULT_HIGH_GAIN: f32 = 2.885;
// The newly derived CWND gain for STARTUP, 2.
const K_DERIVED_HIGH_CWNDGAIN: f32 = 2.0;
// The cycle of gains used during the ProbeBw stage.
const K_PACING_GAIN: [f32; 8] = [1.25, 0.75, 1.0, 1.0, 1.0, 1.0, 1.0, 1.0];

const K_STARTUP_GROWTH_TARGET: f32 = 1.25;
const K_ROUND_TRIPS_WITHOUT_GROWTH_BEFORE_EXITING_STARTUP: u8 = 3;
const K_STARTUP_FULL_LOSS_COUNT: u64 = 8;
const K_STARTUP_LOSS_THRESHOLD: f64 = 0.02;
const K_MIN_BBR_PACER_BYTES_PER_SECOND: u64 = 65_536;

// Do not allow initial congestion window to be greater than 200 packets.
const K_INITIAL_CONGESTION_WINDOW_PACKETS: u64 = 32;

const DRAIN_TO_TARGET: bool = true;

fn env_bool(name: &str) -> Option<bool> {
    let value = env::var(name).ok()?;
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

fn connection_app_limited_value(value: Option<&str>) -> bool {
    match value.and_then(|value| match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }) {
        Some(value) => value,
        None => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AppLimitedSource {
    #[default]
    Heuristic,
    Connection,
    HybridAnd,
}

impl AppLimitedSource {
    fn from_env_value(value: Option<&str>, legacy_connection_value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(
                    value.as_str(),
                    "heuristic" | "legacy" | "default" | "inflight"
                ) =>
            {
                Self::Heuristic
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "connection" | "conn" | "connection_only" | "conn_only"
                ) =>
            {
                Self::Connection
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "hybrid" | "hybrid_and" | "and" | "conn_and_heuristic"
                ) =>
            {
                Self::HybridAnd
            }
            _ if connection_app_limited_value(legacy_connection_value) => Self::Connection,
            _ => Self::Heuristic,
        }
    }

    fn is_app_limited(self, conn_app_limited: bool, heuristic_app_limited: bool) -> bool {
        match self {
            Self::Heuristic => heuristic_app_limited,
            Self::Connection => conn_app_limited,
            Self::HybridAnd => conn_app_limited && heuristic_app_limited,
        }
    }
}

fn app_limited_source() -> &'static AppLimitedSource {
    static VALUE: OnceLock<AppLimitedSource> = OnceLock::new();
    VALUE.get_or_init(|| {
        AppLimitedSource::from_env_value(
            env::var("HY_RS_BBR_APP_LIMITED_SOURCE").ok().as_deref(),
            env::var("HY_RS_BBR_USE_CONN_APP_LIMITED").ok().as_deref(),
        )
    })
}

fn app_limited_trace_enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| env_bool("HY_RS_BBR_APP_LIMITED_TRACE").unwrap_or(false))
}

fn startup_trace_enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| env_bool("HY_RS_BBR_STARTUP_TRACE").unwrap_or(false))
}

fn state_trace_enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| env_bool("HY_RS_BBR_STATE_TRACE").unwrap_or(false))
}

fn app_limited_debounce_enabled() -> bool {
    static VALUE: OnceLock<bool> = OnceLock::new();
    *VALUE.get_or_init(|| env_bool("HY_RS_BBR_APP_LIMITED_DEBOUNCE").unwrap_or(false))
}

fn app_limited_trace_every_value(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(64)
        .clamp(1, 1_000_000)
}

fn app_limited_trace_every() -> u64 {
    static VALUE: OnceLock<u64> = OnceLock::new();
    *VALUE.get_or_init(|| {
        app_limited_trace_every_value(
            env::var("HY_RS_BBR_APP_LIMITED_TRACE_EVERY")
                .ok()
                .as_deref(),
        )
    })
}

fn app_limited_target_pct_value(value: Option<&str>) -> u64 {
    value
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(100)
        .clamp(1, 1000)
}

fn app_limited_target_pct() -> u64 {
    static VALUE: OnceLock<u64> = OnceLock::new();
    *VALUE.get_or_init(|| {
        app_limited_target_pct_value(env::var("HY_RS_BBR_APP_LIMITED_TARGET_PCT").ok().as_deref())
    })
}

fn heuristic_app_limited_with_pct(bytes_in_flight: u64, target_cwnd: u64, pct: u64) -> bool {
    u128::from(bytes_in_flight) * 100 < u128::from(target_cwnd) * u128::from(pct)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MaxBwUpdateStrategy {
    #[default]
    Default,
    RefreshCurrentOnAppLimited,
}

impl MaxBwUpdateStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(
                    value.as_str(),
                    "refresh_current" | "refresh_current_on_app_limited"
                ) =>
            {
                Self::RefreshCurrentOnAppLimited
            }
            _ => Self::Default,
        }
    }

    fn measurement(
        self,
        current_bw: u64,
        sample_max_bw: u64,
        sample_is_app_limited: bool,
        bytes_acked: u64,
    ) -> Option<u64> {
        if bytes_acked == 0 {
            return None;
        }
        if !sample_is_app_limited || sample_max_bw > current_bw {
            return Some(sample_max_bw);
        }
        match self {
            Self::Default => None,
            Self::RefreshCurrentOnAppLimited => (current_bw != 0).then_some(current_bw),
        }
    }
}

fn max_bw_update_strategy() -> &'static MaxBwUpdateStrategy {
    static VALUE: OnceLock<MaxBwUpdateStrategy> = OnceLock::new();
    VALUE.get_or_init(|| {
        MaxBwUpdateStrategy::from_env_value(env::var("HY_RS_BBR_MAX_BW_UPDATE").ok().as_deref())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum MaxBwAppLimitedStrategy {
    #[default]
    Legacy,
    PreferNonAppLimited,
    EventNonAppLimitedOk,
    AckTimeExitOk,
}

impl MaxBwAppLimitedStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(
                    value.as_str(),
                    "legacy" | "default" | "winning_sample" | "winning_sample_gate"
                ) =>
            {
                Self::Legacy
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "prefer_non_app_limited"
                        | "non_app_limited_fallback"
                        | "prefer_non_app_limited_sample"
                ) =>
            {
                Self::PreferNonAppLimited
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "event_non_app_limited_ok"
                        | "event_non_app_limited"
                        | "event_gate"
                        | "event_app_limited_gate"
                ) =>
            {
                Self::EventNonAppLimitedOk
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "ack_time_exit_ok" | "ack_exit_ok" | "ack_time_clear" | "tail_clear"
                ) =>
            {
                Self::AckTimeExitOk
            }
            _ => Self::Legacy,
        }
    }

    fn candidate(
        self,
        sample_max_bw: u64,
        sample_max_bw_non_app_limited: u64,
        sample_is_app_limited: bool,
        has_non_app_limited_sample: bool,
        event_app_limited: bool,
        sampler_is_app_limited: bool,
    ) -> (u64, bool) {
        match self {
            Self::Legacy => (sample_max_bw, sample_is_app_limited),
            Self::PreferNonAppLimited
                if sample_is_app_limited && sample_max_bw_non_app_limited != 0 =>
            {
                (sample_max_bw_non_app_limited, false)
            }
            Self::EventNonAppLimitedOk if sample_is_app_limited && has_non_app_limited_sample => {
                (sample_max_bw, false)
            }
            Self::PreferNonAppLimited => (sample_max_bw, sample_is_app_limited),
            Self::EventNonAppLimitedOk => (sample_max_bw, sample_is_app_limited),
            Self::AckTimeExitOk
                if sample_is_app_limited && !event_app_limited && !sampler_is_app_limited =>
            {
                (sample_max_bw, false)
            }
            Self::AckTimeExitOk => (sample_max_bw, sample_is_app_limited),
        }
    }
}

fn max_bw_app_limited_strategy() -> &'static MaxBwAppLimitedStrategy {
    static VALUE: OnceLock<MaxBwAppLimitedStrategy> = OnceLock::new();
    VALUE.get_or_init(|| {
        MaxBwAppLimitedStrategy::from_env_value(
            env::var("HY_RS_BBR_MAX_BW_APP_LIMITED").ok().as_deref(),
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum StartupFullBwGateStrategy {
    #[default]
    Default,
    IgnoreAppLimited,
    EventAppLimitedOnly,
    AckTimeExitOk,
}

impl StartupFullBwGateStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(
                    value.as_str(),
                    "ignore_app_limited" | "ignore_app_limited_gate"
                ) =>
            {
                Self::IgnoreAppLimited
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "event_app_limited_only" | "event_only" | "event_gate"
                ) =>
            {
                Self::EventAppLimitedOnly
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "ack_time_exit_ok" | "ack_exit_ok" | "ack_time_clear" | "tail_clear"
                ) =>
            {
                Self::AckTimeExitOk
            }
            _ => Self::Default,
        }
    }

    fn skip_check(
        self,
        last_sample_is_app_limited: bool,
        sample_is_app_limited: bool,
        event_app_limited: bool,
        sampler_is_app_limited: bool,
    ) -> bool {
        match self {
            Self::Default => last_sample_is_app_limited,
            Self::IgnoreAppLimited => false,
            Self::EventAppLimitedOnly => event_app_limited,
            Self::AckTimeExitOk => {
                sample_is_app_limited && (event_app_limited || sampler_is_app_limited)
            }
        }
    }
}

fn startup_full_bw_gate_strategy() -> &'static StartupFullBwGateStrategy {
    static VALUE: OnceLock<StartupFullBwGateStrategy> = OnceLock::new();
    VALUE.get_or_init(|| {
        StartupFullBwGateStrategy::from_env_value(
            env::var("HY_RS_BBR_STARTUP_FULL_BW_GATE").ok().as_deref(),
        )
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ProbeRttEntryStrategy {
    #[default]
    Legacy,
    IdleOrDrain,
    LastSampleOrDrain,
}

impl ProbeRttEntryStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(
                    value.as_str(),
                    "idle_or_drain" | "idle" | "idle_only" | "app_limited_or_drain"
                ) =>
            {
                Self::IdleOrDrain
            }
            Some(value)
                if matches!(
                    value.as_str(),
                    "last_sample_or_drain"
                        | "last_sample_or_idle"
                        | "recent_app_limited_or_drain"
                        | "sample_hint_or_drain"
                ) =>
            {
                Self::LastSampleOrDrain
            }
            _ => Self::Legacy,
        }
    }

    fn should_enter(
        self,
        bytes_in_flight: u64,
        probe_rtt_cwnd: u64,
        last_sample_is_app_limited: bool,
        event_app_limited: bool,
        sampler_is_app_limited: bool,
    ) -> bool {
        match self {
            Self::Legacy => true,
            Self::IdleOrDrain => {
                event_app_limited || sampler_is_app_limited || bytes_in_flight <= probe_rtt_cwnd
            }
            Self::LastSampleOrDrain => {
                last_sample_is_app_limited
                    || event_app_limited
                    || sampler_is_app_limited
                    || bytes_in_flight <= probe_rtt_cwnd
            }
        }
    }
}

fn probe_rtt_entry_strategy() -> &'static ProbeRttEntryStrategy {
    static VALUE: OnceLock<ProbeRttEntryStrategy> = OnceLock::new();
    VALUE.get_or_init(|| {
        ProbeRttEntryStrategy::from_env_value(env::var("HY_RS_BBR_PROBE_RTT_ENTRY").ok().as_deref())
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum RoundGatingStrategy {
    #[default]
    AckedOnly,
    LargestObserved,
}

impl RoundGatingStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value)
                if matches!(value.as_str(), "largest_observed" | "include_loss" | "loss") =>
            {
                Self::LargestObserved
            }
            _ => Self::AckedOnly,
        }
    }

    fn round_packet_number(self, event: &AckEvent<'_>) -> Option<u64> {
        let acked = event
            .acked_packets
            .last()
            .map(|packet| packet.packet_number);
        match self {
            Self::AckedOnly => acked,
            Self::LargestObserved => acked
                .into_iter()
                .chain(event.lost_packets.last().map(|packet| packet.packet_number))
                .max(),
        }
    }
}

fn round_gating_strategy() -> &'static RoundGatingStrategy {
    static VALUE: OnceLock<RoundGatingStrategy> = OnceLock::new();
    VALUE.get_or_init(|| {
        RoundGatingStrategy::from_env_value(env::var("HY_RS_BBR_ROUND_GATING").ok().as_deref())
    })
}

#[cfg(test)]
mod tests {
    use super::{
        MaxBwAppLimitedStrategy, MaxBwUpdateStrategy, ProbeRttEntryStrategy, RoundGatingStrategy,
        StartupFullBwGateStrategy,
    };
    use crate::congestion::{AckEvent, AckedPacketInfo, LostPacketInfo};

    #[test]
    fn max_bw_update_strategy_parses_overrides() {
        assert_eq!(
            MaxBwUpdateStrategy::from_env_value(None),
            MaxBwUpdateStrategy::Default
        );
        assert_eq!(
            MaxBwUpdateStrategy::from_env_value(Some("refresh_current")),
            MaxBwUpdateStrategy::RefreshCurrentOnAppLimited
        );
    }

    #[test]
    fn connection_app_limited_value_parses_and_defaults_to_false() {
        assert!(!super::connection_app_limited_value(None));
        assert!(super::connection_app_limited_value(Some("1")));
        assert!(!super::connection_app_limited_value(Some("0")));
        assert!(!super::connection_app_limited_value(Some("bad")));
    }

    #[test]
    fn app_limited_source_parses_and_uses_legacy_fallback() {
        assert_eq!(
            super::AppLimitedSource::from_env_value(None, None),
            super::AppLimitedSource::Heuristic
        );
        assert_eq!(
            super::AppLimitedSource::from_env_value(Some("connection"), None),
            super::AppLimitedSource::Connection
        );
        assert_eq!(
            super::AppLimitedSource::from_env_value(Some("hybrid"), None),
            super::AppLimitedSource::HybridAnd
        );
        assert_eq!(
            super::AppLimitedSource::from_env_value(None, Some("1")),
            super::AppLimitedSource::Connection
        );
        assert_eq!(
            super::AppLimitedSource::from_env_value(Some("bad"), None),
            super::AppLimitedSource::Heuristic
        );

        assert!(super::AppLimitedSource::Heuristic.is_app_limited(false, true));
        assert!(!super::AppLimitedSource::Connection.is_app_limited(false, true));
        assert!(super::AppLimitedSource::HybridAnd.is_app_limited(true, true));
        assert!(!super::AppLimitedSource::HybridAnd.is_app_limited(true, false));
    }

    #[test]
    fn app_limited_trace_every_parses_and_clamps() {
        assert_eq!(super::app_limited_trace_every_value(None), 64);
        assert_eq!(super::app_limited_trace_every_value(Some("8")), 8);
        assert_eq!(super::app_limited_trace_every_value(Some("0")), 1);
        assert_eq!(
            super::app_limited_trace_every_value(Some("999999999")),
            1_000_000
        );
        assert_eq!(super::app_limited_trace_every_value(Some("bad")), 64);
    }

    #[test]
    fn env_bool_parses_app_limited_debounce_style_values() {
        assert_eq!(super::env_bool("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST"), None);
        std::env::set_var("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST", "1");
        assert_eq!(
            super::env_bool("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST"),
            Some(true)
        );
        std::env::set_var("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST", "off");
        assert_eq!(
            super::env_bool("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST"),
            Some(false)
        );
        std::env::remove_var("HY_RS_BBR_APP_LIMITED_DEBOUNCE_TEST");
    }

    #[test]
    fn env_bool_parses_startup_trace_style_values() {
        assert_eq!(super::env_bool("HY_RS_BBR_STARTUP_TRACE_TEST"), None);
        std::env::set_var("HY_RS_BBR_STARTUP_TRACE_TEST", "true");
        assert_eq!(super::env_bool("HY_RS_BBR_STARTUP_TRACE_TEST"), Some(true));
        std::env::set_var("HY_RS_BBR_STARTUP_TRACE_TEST", "0");
        assert_eq!(super::env_bool("HY_RS_BBR_STARTUP_TRACE_TEST"), Some(false));
        std::env::remove_var("HY_RS_BBR_STARTUP_TRACE_TEST");
    }

    #[test]
    fn max_bw_update_strategy_selects_measurement() {
        assert_eq!(
            MaxBwUpdateStrategy::Default.measurement(100, 120, false, 1),
            Some(120)
        );
        assert_eq!(
            MaxBwUpdateStrategy::Default.measurement(100, 90, true, 1),
            None
        );
        assert_eq!(
            MaxBwUpdateStrategy::RefreshCurrentOnAppLimited.measurement(100, 90, true, 1),
            Some(100)
        );
        assert_eq!(
            MaxBwUpdateStrategy::RefreshCurrentOnAppLimited.measurement(0, 90, true, 1),
            Some(90)
        );
    }

    #[test]
    fn app_limited_target_pct_parses_and_applies() {
        assert_eq!(super::app_limited_target_pct_value(None), 100);
        assert_eq!(super::app_limited_target_pct_value(Some("50")), 50);
        assert_eq!(super::app_limited_target_pct_value(Some("0")), 1);
        assert_eq!(super::app_limited_target_pct_value(Some("2000")), 1000);
        assert_eq!(super::app_limited_target_pct_value(Some("bad")), 100);

        assert!(super::heuristic_app_limited_with_pct(49, 100, 50));
        assert!(!super::heuristic_app_limited_with_pct(50, 100, 50));
        assert!(super::heuristic_app_limited_with_pct(99, 100, 100));
        assert!(!super::heuristic_app_limited_with_pct(100, 100, 100));
    }

    #[test]
    fn max_bw_app_limited_strategy_parses_and_selects_candidate() {
        assert_eq!(
            MaxBwAppLimitedStrategy::from_env_value(None),
            MaxBwAppLimitedStrategy::Legacy
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::from_env_value(Some("legacy")),
            MaxBwAppLimitedStrategy::Legacy
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::from_env_value(Some("prefer_non_app_limited")),
            MaxBwAppLimitedStrategy::PreferNonAppLimited
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::from_env_value(Some("event_non_app_limited_ok")),
            MaxBwAppLimitedStrategy::EventNonAppLimitedOk
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::from_env_value(Some("ack_time_exit_ok")),
            MaxBwAppLimitedStrategy::AckTimeExitOk
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::Legacy.candidate(120, 110, true, true, true, true),
            (120, true)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::PreferNonAppLimited
                .candidate(120, 110, true, true, true, true),
            (110, false)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::PreferNonAppLimited.candidate(120, 0, true, true, true, true),
            (120, true)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::EventNonAppLimitedOk
                .candidate(120, 110, true, true, true, true),
            (120, false)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::EventNonAppLimitedOk
                .candidate(120, 0, true, false, true, true),
            (120, true)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::AckTimeExitOk.candidate(120, 0, true, false, false, false),
            (120, false)
        );
        assert_eq!(
            MaxBwAppLimitedStrategy::AckTimeExitOk.candidate(120, 0, true, false, true, false),
            (120, true)
        );
    }

    #[test]
    fn startup_full_bw_gate_strategy_parses_and_applies() {
        assert_eq!(
            StartupFullBwGateStrategy::from_env_value(None),
            StartupFullBwGateStrategy::Default
        );
        assert_eq!(
            StartupFullBwGateStrategy::from_env_value(Some("ignore_app_limited")),
            StartupFullBwGateStrategy::IgnoreAppLimited
        );
        assert_eq!(
            StartupFullBwGateStrategy::from_env_value(Some("event_only")),
            StartupFullBwGateStrategy::EventAppLimitedOnly
        );
        assert_eq!(
            StartupFullBwGateStrategy::from_env_value(Some("ack_time_exit_ok")),
            StartupFullBwGateStrategy::AckTimeExitOk
        );
        assert!(StartupFullBwGateStrategy::Default.skip_check(true, true, true, true));
        assert!(!StartupFullBwGateStrategy::IgnoreAppLimited.skip_check(true, true, true, true));
        assert!(StartupFullBwGateStrategy::EventAppLimitedOnly.skip_check(true, true, true, false));
        assert!(!StartupFullBwGateStrategy::EventAppLimitedOnly.skip_check(true, true, false, true));
        assert!(!StartupFullBwGateStrategy::AckTimeExitOk.skip_check(true, true, false, false));
        assert!(StartupFullBwGateStrategy::AckTimeExitOk.skip_check(true, true, true, false));
        assert!(!StartupFullBwGateStrategy::AckTimeExitOk.skip_check(true, false, false, false));
    }

    #[test]
    fn round_gating_strategy_parses_and_selects_packet() {
        assert_eq!(
            RoundGatingStrategy::from_env_value(None),
            RoundGatingStrategy::AckedOnly
        );
        assert_eq!(
            RoundGatingStrategy::from_env_value(Some("largest_observed")),
            RoundGatingStrategy::LargestObserved
        );

        let acked_packets = [AckedPacketInfo {
            packet_number: 10,
            bytes_acked: 1200,
        }];
        let lost_packets = [LostPacketInfo {
            packet_number: 12,
            bytes_lost: 1200,
        }];
        let rtt = crate::connection::RttEstimator::new(std::time::Duration::from_millis(100));
        let event = AckEvent {
            prior_in_flight: 0,
            acked_packets: &acked_packets,
            lost_packets: &lost_packets,
            app_limited: false,
            largest_packet_num_acked: Some(10),
            rtt: &rtt,
        };

        assert_eq!(
            RoundGatingStrategy::AckedOnly.round_packet_number(&event),
            Some(10)
        );
        assert_eq!(
            RoundGatingStrategy::LargestObserved.round_packet_number(&event),
            Some(12)
        );
    }

    #[test]
    fn probe_rtt_entry_strategy_parses_and_applies() {
        assert_eq!(
            ProbeRttEntryStrategy::from_env_value(None),
            ProbeRttEntryStrategy::Legacy
        );
        assert_eq!(
            ProbeRttEntryStrategy::from_env_value(Some("idle_or_drain")),
            ProbeRttEntryStrategy::IdleOrDrain
        );
        assert_eq!(
            ProbeRttEntryStrategy::from_env_value(Some("last_sample_or_drain")),
            ProbeRttEntryStrategy::LastSampleOrDrain
        );
        assert!(ProbeRttEntryStrategy::Legacy.should_enter(10_000, 1_000, false, false, false));
        assert!(
            !ProbeRttEntryStrategy::IdleOrDrain.should_enter(10_000, 1_000, false, false, false)
        );
        assert!(ProbeRttEntryStrategy::IdleOrDrain.should_enter(10_000, 1_000, false, true, false));
        assert!(ProbeRttEntryStrategy::IdleOrDrain.should_enter(500, 1_000, false, false, false));
        assert!(ProbeRttEntryStrategy::LastSampleOrDrain
            .should_enter(10_000, 1_000, true, false, false));
        assert!(!ProbeRttEntryStrategy::LastSampleOrDrain
            .should_enter(10_000, 1_000, false, false, false));
    }
}
