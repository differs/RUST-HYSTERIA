use std::{
    collections::{BTreeMap, VecDeque},
    fmt::{Display, Formatter},
    sync::OnceLock,
};

use crate::{
    congestion::{AckedPacketInfo, LostPacketInfo},
    Duration, Instant,
};

const DEFAULT_CANDIDATES_BUFFER_SIZE: usize = 256;
const DEFAULT_ACK_HEIGHT_WINDOW: u64 = 10;
const ACK_AGGREGATION_BANDWIDTH_THRESHOLD: f64 = 1.0;
const ACK_AGGREGATION_BANDWIDTH_THRESHOLD_OVER_ESTIMATE: f64 = 2.0;
const INF_BANDWIDTH: u64 = u64::MAX;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SampleBandwidthStrategy {
    #[default]
    Min,
    Send,
    Max,
}

impl SampleBandwidthStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if matches!(value.as_str(), "send" | "send_rate") => Self::Send,
            Some(value) if matches!(value.as_str(), "max" | "max_rate") => Self::Max,
            _ => Self::Min,
        }
    }

    fn select(self, send_rate: u64, ack_rate: u64) -> u64 {
        let send_rate = (send_rate != INF_BANDWIDTH).then_some(send_rate);
        let ack_rate = (ack_rate != INF_BANDWIDTH).then_some(ack_rate);
        match self {
            Self::Min => match (send_rate, ack_rate) {
                (Some(send_rate), Some(ack_rate)) => send_rate.min(ack_rate),
                (Some(send_rate), None) => send_rate,
                (None, Some(ack_rate)) => ack_rate,
                (None, None) => 0,
            },
            Self::Send => send_rate.or(ack_rate).unwrap_or(0),
            Self::Max => match (send_rate, ack_rate) {
                (Some(send_rate), Some(ack_rate)) => send_rate.max(ack_rate),
                (Some(send_rate), None) => send_rate,
                (None, Some(ack_rate)) => ack_rate,
                (None, None) => 0,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SendRateAnchorStrategy {
    #[default]
    SentState,
    AckEvent,
}

impl SendRateAnchorStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if matches!(value.as_str(), "event" | "ack_event") => Self::AckEvent,
            _ => Self::SentState,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum AckEventBandwidthFusionStrategy {
    #[default]
    Off,
    SendCap,
}

impl AckEventBandwidthFusionStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
            Some(value) if matches!(value.as_str(), "send_cap" | "cap" | "on" | "true" | "1") => {
                Self::SendCap
            }
            _ => Self::Off,
        }
    }

    fn fuse(self, legacy_bandwidth: u64, max_send_rate: u64) -> u64 {
        match self {
            Self::Off => legacy_bandwidth,
            Self::SendCap => {
                if max_send_rate == 0 || max_send_rate == INF_BANDWIDTH {
                    legacy_bandwidth
                } else {
                    legacy_bandwidth.min(max_send_rate)
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SamplerAppLimitedExitStrategy {
    #[default]
    Legacy,
    AckTimeExitOk,
}

impl SamplerAppLimitedExitStrategy {
    fn from_env_value(value: Option<&str>) -> Self {
        match value.map(|value| value.trim().to_ascii_lowercase()) {
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

    fn should_exit(
        self,
        packet_number: u64,
        end_of_app_limited_phase: Option<u64>,
        event_app_limited: bool,
    ) -> bool {
        match self {
            Self::Legacy => {
                end_of_app_limited_phase.is_none() || Some(packet_number) > end_of_app_limited_phase
            }
            Self::AckTimeExitOk => {
                end_of_app_limited_phase.is_none()
                    || Some(packet_number) > end_of_app_limited_phase
                    || (!event_app_limited && Some(packet_number) == end_of_app_limited_phase)
            }
        }
    }
}

fn sample_bandwidth_strategy() -> &'static SampleBandwidthStrategy {
    static STRATEGY: OnceLock<SampleBandwidthStrategy> = OnceLock::new();
    STRATEGY.get_or_init(|| {
        SampleBandwidthStrategy::from_env_value(
            std::env::var("HY_RS_BBR_SAMPLE_BANDWIDTH").ok().as_deref(),
        )
    })
}

fn send_rate_anchor_strategy() -> &'static SendRateAnchorStrategy {
    static STRATEGY: OnceLock<SendRateAnchorStrategy> = OnceLock::new();
    STRATEGY.get_or_init(|| {
        SendRateAnchorStrategy::from_env_value(
            std::env::var("HY_RS_BBR_SEND_RATE_ANCHOR").ok().as_deref(),
        )
    })
}

fn ack_event_bandwidth_fusion_strategy() -> &'static AckEventBandwidthFusionStrategy {
    static STRATEGY: OnceLock<AckEventBandwidthFusionStrategy> = OnceLock::new();
    STRATEGY.get_or_init(|| {
        AckEventBandwidthFusionStrategy::from_env_value(
            std::env::var("HY_RS_BBR_ACK_EVENT_BW_FUSION")
                .ok()
                .as_deref(),
        )
    })
}

fn sampler_app_limited_exit_strategy() -> &'static SamplerAppLimitedExitStrategy {
    static STRATEGY: OnceLock<SamplerAppLimitedExitStrategy> = OnceLock::new();
    STRATEGY.get_or_init(|| {
        SamplerAppLimitedExitStrategy::from_env_value(
            std::env::var("HY_RS_BBR_SAMPLER_APP_LIMITED_EXIT")
                .ok()
                .as_deref(),
        )
    })
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct SendTimeState {
    pub(crate) is_valid: bool,
    pub(crate) is_app_limited: bool,
    pub(crate) total_bytes_sent: u64,
    pub(crate) total_bytes_acked: u64,
    pub(crate) total_bytes_lost: u64,
    pub(crate) bytes_in_flight: u64,
}

#[derive(Clone, Copy, Debug)]
struct ConnectionStateOnSentPacket {
    sent_time: Instant,
    size: u64,
    total_bytes_sent_at_last_acked_packet: u64,
    last_acked_packet_sent_time: Option<Instant>,
    last_acked_packet_ack_time: Option<Instant>,
    send_time_state: SendTimeState,
}

#[derive(Clone, Copy, Debug, Default)]
struct BandwidthSample {
    bandwidth: u64,
    rtt: Option<Duration>,
    send_rate: u64,
    state_at_send: SendTimeState,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct CongestionEventSample {
    pub(crate) sample_max_bandwidth: u64,
    pub(crate) sample_max_bandwidth_non_app_limited: u64,
    pub(crate) sample_is_app_limited: bool,
    pub(crate) has_non_app_limited_sample: bool,
    pub(crate) sample_rtt: Option<Duration>,
    pub(crate) sample_max_inflight: u64,
    pub(crate) last_packet_send_state: SendTimeState,
    pub(crate) extra_acked: u64,
    pub(crate) bytes_acked: u64,
    pub(crate) bytes_lost: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct AckPoint {
    ack_time: Option<Instant>,
    total_bytes_acked: u64,
}

#[derive(Clone, Copy, Debug, Default)]
struct RecentAckPoints {
    ack_points: [AckPoint; 2],
}

impl RecentAckPoints {
    fn update(&mut self, ack_time: Instant, total_bytes_acked: u64) {
        match self.ack_points[1].ack_time {
            Some(latest) if ack_time < latest => {
                self.ack_points[1].ack_time = Some(ack_time);
            }
            Some(latest) if ack_time > latest => {
                self.ack_points[0] = self.ack_points[1];
                self.ack_points[1].ack_time = Some(ack_time);
            }
            None => {
                self.ack_points[1].ack_time = Some(ack_time);
            }
            _ => {}
        }
        self.ack_points[1].total_bytes_acked = total_bytes_acked;
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    fn most_recent_point(&self) -> AckPoint {
        self.ack_points[1]
    }

    fn less_recent_point(&self) -> AckPoint {
        if self.ack_points[0].total_bytes_acked != 0 {
            self.ack_points[0]
        } else {
            self.ack_points[1]
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct ExtraAckedEvent {
    extra_acked: u64,
    bytes_acked: u64,
    time_delta: Duration,
    round: u64,
}

#[derive(Clone, Copy, Debug)]
struct ExtraAckedFilter {
    window_length: u64,
    estimates: [ExtraAckedEvent; 3],
}

impl Default for ExtraAckedFilter {
    fn default() -> Self {
        Self {
            window_length: DEFAULT_ACK_HEIGHT_WINDOW,
            estimates: [ExtraAckedEvent::default(); 3],
        }
    }
}

impl ExtraAckedFilter {
    fn get_best(&self) -> ExtraAckedEvent {
        self.estimates[0]
    }

    fn get_second_best(&self) -> ExtraAckedEvent {
        self.estimates[1]
    }

    fn get_third_best(&self) -> ExtraAckedEvent {
        self.estimates[2]
    }

    fn update(&mut self, sample: ExtraAckedEvent, round: u64) {
        let mut sample = sample;
        sample.round = round;

        if self.estimates[0].extra_acked == 0
            || sample.extra_acked >= self.estimates[0].extra_acked
            || round.saturating_sub(self.estimates[2].round) > self.window_length
        {
            self.reset(sample, round);
            return;
        }

        if sample.extra_acked >= self.estimates[1].extra_acked {
            self.estimates[1] = sample;
            self.estimates[2] = sample;
        } else if sample.extra_acked >= self.estimates[2].extra_acked {
            self.estimates[2] = sample;
        }

        if round.saturating_sub(self.estimates[0].round) > self.window_length {
            self.estimates[0] = self.estimates[1];
            self.estimates[1] = self.estimates[2];
            self.estimates[2] = sample;
            if round.saturating_sub(self.estimates[0].round) > self.window_length {
                self.estimates[0] = self.estimates[1];
                self.estimates[1] = self.estimates[2];
            }
            return;
        }

        if self.estimates[1].extra_acked == self.estimates[0].extra_acked
            && round.saturating_sub(self.estimates[1].round) > self.window_length / 4
        {
            self.estimates[1] = sample;
            self.estimates[2] = sample;
            return;
        }

        if self.estimates[2].extra_acked == self.estimates[1].extra_acked
            && round.saturating_sub(self.estimates[2].round) > self.window_length / 2
        {
            self.estimates[2] = sample;
        }
    }

    fn reset(&mut self, sample: ExtraAckedEvent, round: u64) {
        let mut sample = sample;
        sample.round = round;
        self.estimates = [sample; 3];
    }

    fn clear(&mut self) {
        self.estimates = [ExtraAckedEvent::default(); 3];
    }
}

#[derive(Clone, Debug)]
struct MaxAckHeightTracker {
    max_ack_height_filter: ExtraAckedFilter,
    aggregation_epoch_start_time: Option<Instant>,
    aggregation_epoch_bytes: u64,
    last_sent_packet_number_before_epoch: Option<u64>,
    num_ack_aggregation_epochs: u64,
    ack_aggregation_bandwidth_threshold: f64,
    start_new_aggregation_epoch_after_full_round: bool,
    reduce_extra_acked_on_bandwidth_increase: bool,
}

impl MaxAckHeightTracker {
    fn get(&self) -> u64 {
        self.max_ack_height_filter.get_best().extra_acked
    }

    fn update(
        &mut self,
        bandwidth_estimate: u64,
        is_new_max_bandwidth: bool,
        round_trip_count: u64,
        last_sent_packet_number: Option<u64>,
        last_acked_packet_number: Option<u64>,
        ack_time: Instant,
        bytes_acked: u64,
    ) -> u64 {
        if self.reduce_extra_acked_on_bandwidth_increase && is_new_max_bandwidth {
            let best = self.max_ack_height_filter.get_best();
            let second_best = self.max_ack_height_filter.get_second_best();
            let third_best = self.max_ack_height_filter.get_third_best();
            self.max_ack_height_filter.clear();
            for mut event in [best, second_best, third_best] {
                let expected =
                    bytes_from_bandwidth_and_time_delta(bandwidth_estimate, event.time_delta);
                if expected < event.bytes_acked {
                    event.extra_acked = event.bytes_acked - expected;
                    self.max_ack_height_filter.update(event, event.round);
                }
            }
        }

        let force_new_epoch = self.start_new_aggregation_epoch_after_full_round
            && self.last_sent_packet_number_before_epoch.is_some()
            && last_acked_packet_number.is_some()
            && last_acked_packet_number > self.last_sent_packet_number_before_epoch;

        if self.aggregation_epoch_start_time.is_none() || force_new_epoch {
            self.aggregation_epoch_bytes = bytes_acked;
            self.aggregation_epoch_start_time = Some(ack_time);
            self.last_sent_packet_number_before_epoch = last_sent_packet_number;
            self.num_ack_aggregation_epochs = self.num_ack_aggregation_epochs.saturating_add(1);
            return 0;
        }

        let aggregation_delta = ack_time
            .saturating_duration_since(self.aggregation_epoch_start_time.unwrap_or(ack_time));
        let expected_bytes_acked =
            bytes_from_bandwidth_and_time_delta(bandwidth_estimate, aggregation_delta);
        if (self.aggregation_epoch_bytes as f64)
            <= self.ack_aggregation_bandwidth_threshold * (expected_bytes_acked as f64)
        {
            self.aggregation_epoch_bytes = bytes_acked;
            self.aggregation_epoch_start_time = Some(ack_time);
            self.last_sent_packet_number_before_epoch = last_sent_packet_number;
            self.num_ack_aggregation_epochs = self.num_ack_aggregation_epochs.saturating_add(1);
            return 0;
        }

        self.aggregation_epoch_bytes = self.aggregation_epoch_bytes.saturating_add(bytes_acked);
        let extra_acked = self
            .aggregation_epoch_bytes
            .saturating_sub(expected_bytes_acked);
        self.max_ack_height_filter.update(
            ExtraAckedEvent {
                extra_acked,
                bytes_acked: self.aggregation_epoch_bytes,
                time_delta: aggregation_delta,
                round: round_trip_count,
            },
            round_trip_count,
        );
        extra_acked
    }

    fn reset(&mut self, new_height: u64, round_trip_count: u64) {
        self.max_ack_height_filter.reset(
            ExtraAckedEvent {
                extra_acked: new_height,
                bytes_acked: 0,
                time_delta: Duration::ZERO,
                round: round_trip_count,
            },
            round_trip_count,
        );
    }

    fn set_ack_aggregation_bandwidth_threshold(&mut self, threshold: f64) {
        self.ack_aggregation_bandwidth_threshold = threshold;
    }

    fn set_start_new_aggregation_epoch_after_full_round(&mut self, value: bool) {
        self.start_new_aggregation_epoch_after_full_round = value;
    }

    fn set_reduce_extra_acked_on_bandwidth_increase(&mut self, value: bool) {
        self.reduce_extra_acked_on_bandwidth_increase = value;
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct BandwidthSampler {
    total_bytes_sent: u64,
    total_bytes_acked: u64,
    total_bytes_lost: u64,
    total_bytes_neutered: u64,
    total_bytes_sent_at_last_acked_packet: u64,
    last_acked_packet_sent_time: Option<Instant>,
    last_acked_packet_ack_time: Option<Instant>,
    last_sent_packet: Option<u64>,
    last_acked_packet: Option<u64>,
    is_app_limited: bool,
    end_of_app_limited_phase: Option<u64>,
    connection_state_map: BTreeMap<u64, ConnectionStateOnSentPacket>,
    recent_ack_points: RecentAckPoints,
    a0_candidates: VecDeque<AckPoint>,
    max_ack_height_tracker: MaxAckHeightTracker,
    total_bytes_acked_after_last_ack_event: u64,
    overestimate_avoidance: bool,
    limit_max_ack_height_tracker_by_send_rate: bool,
    last_legacy_ack_time: Option<Instant>,
    last_legacy_total_acked: u64,
}

impl BandwidthSampler {
    pub(crate) fn on_packet_sent(
        &mut self,
        sent_time: Instant,
        packet_number: u64,
        bytes: u64,
        bytes_in_flight: u64,
        is_retransmittable: bool,
    ) {
        self.last_sent_packet = Some(packet_number);

        if !is_retransmittable {
            return;
        }

        self.total_bytes_sent = self.total_bytes_sent.saturating_add(bytes);

        if bytes_in_flight == 0 {
            self.last_acked_packet_ack_time = Some(sent_time);
            if self.overestimate_avoidance {
                self.recent_ack_points.clear();
                self.recent_ack_points
                    .update(sent_time, self.total_bytes_acked);
                self.a0_candidates.clear();
                self.push_a0_candidate(self.recent_ack_points.most_recent_point());
            }
            self.total_bytes_sent_at_last_acked_packet = self.total_bytes_sent;
            self.last_acked_packet_sent_time = Some(sent_time);
        }

        self.connection_state_map.insert(
            packet_number,
            ConnectionStateOnSentPacket {
                sent_time,
                size: bytes,
                total_bytes_sent_at_last_acked_packet: self.total_bytes_sent_at_last_acked_packet,
                last_acked_packet_sent_time: self.last_acked_packet_sent_time,
                last_acked_packet_ack_time: self.last_acked_packet_ack_time,
                send_time_state: SendTimeState {
                    is_valid: true,
                    is_app_limited: self.is_app_limited,
                    total_bytes_sent: self.total_bytes_sent,
                    total_bytes_acked: self.total_bytes_acked,
                    total_bytes_lost: self.total_bytes_lost,
                    bytes_in_flight: bytes_in_flight.saturating_add(bytes),
                },
            },
        );
    }

    pub(crate) fn on_congestion_event(
        &mut self,
        ack_time: Instant,
        acked_packets: &[AckedPacketInfo],
        lost_packets: &[LostPacketInfo],
        max_bandwidth: u64,
        est_bandwidth_upper_bound: u64,
        round_trip_count: u64,
        event_app_limited: bool,
    ) -> CongestionEventSample {
        let total_bytes_acked_before = self.total_bytes_acked;
        let total_bytes_lost_before = self.total_bytes_lost;
        let mut event_sample = CongestionEventSample::default();
        let mut last_lost_packet_send_state = SendTimeState::default();
        let mut largest_acked_packet = None;

        for packet in lost_packets {
            let send_state = self.on_packet_lost(packet.packet_number, packet.bytes_lost);
            if send_state.is_valid {
                last_lost_packet_send_state = send_state;
            }
        }

        if acked_packets.is_empty() {
            event_sample.last_packet_send_state = last_lost_packet_send_state;
            event_sample.bytes_lost = self
                .total_bytes_lost
                .saturating_sub(total_bytes_lost_before);
            return event_sample;
        }

        let mut last_acked_packet_send_state = SendTimeState::default();
        let mut max_send_rate = 0;
        for packet in acked_packets {
            largest_acked_packet = Some(packet.packet_number);
            let sample = self.on_packet_acknowledged(ack_time, packet.packet_number);
            if !sample.state_at_send.is_valid {
                continue;
            }

            last_acked_packet_send_state = sample.state_at_send;

            if let Some(sample_rtt) = sample.rtt {
                event_sample.sample_rtt = Some(
                    event_sample
                        .sample_rtt
                        .map_or(sample_rtt, |current| current.min(sample_rtt)),
                );
            }
            if !sample.state_at_send.is_app_limited {
                event_sample.has_non_app_limited_sample = true;
                event_sample.sample_max_bandwidth_non_app_limited = event_sample
                    .sample_max_bandwidth_non_app_limited
                    .max(sample.bandwidth);
            }
            if sample.bandwidth > event_sample.sample_max_bandwidth {
                event_sample.sample_max_bandwidth = sample.bandwidth;
                event_sample.sample_is_app_limited = sample.state_at_send.is_app_limited;
            }
            if sample.send_rate != INF_BANDWIDTH {
                max_send_rate = max_send_rate.max(sample.send_rate);
            }
            let inflight_sample = self
                .total_bytes_acked
                .saturating_sub(last_acked_packet_send_state.total_bytes_acked);
            event_sample.sample_max_inflight =
                event_sample.sample_max_inflight.max(inflight_sample);
        }

        event_sample.last_packet_send_state = match (
            last_lost_packet_send_state.is_valid,
            last_acked_packet_send_state.is_valid,
        ) {
            (false, true) => last_acked_packet_send_state,
            (true, false) => last_lost_packet_send_state,
            (true, true) => {
                if lost_packets
                    .last()
                    .map(|packet| packet.packet_number)
                    .unwrap_or_default()
                    > acked_packets
                        .last()
                        .map(|packet| packet.packet_number)
                        .unwrap_or_default()
                {
                    last_lost_packet_send_state
                } else {
                    last_acked_packet_send_state
                }
            }
            (false, false) => SendTimeState::default(),
        };

        let is_new_max_bandwidth = event_sample.sample_max_bandwidth > max_bandwidth;
        let mut bandwidth_estimate = max_bandwidth.max(event_sample.sample_max_bandwidth);
        if self.limit_max_ack_height_tracker_by_send_rate {
            bandwidth_estimate = bandwidth_estimate.max(max_send_rate);
        }
        let legacy_acked_bytes = self
            .total_bytes_acked
            .saturating_sub(self.last_legacy_total_acked);
        if let Some(last_ack_time) = self.last_legacy_ack_time {
            if let Some(legacy_bandwidth) = Self::bw_from_delta(
                legacy_acked_bytes,
                ack_time.saturating_duration_since(last_ack_time),
            ) {
                let fused_bandwidth =
                    ack_event_bandwidth_fusion_strategy().fuse(legacy_bandwidth, max_send_rate);
                if fused_bandwidth > event_sample.sample_max_bandwidth {
                    event_sample.sample_max_bandwidth = fused_bandwidth;
                    if !matches!(
                        ack_event_bandwidth_fusion_strategy(),
                        AckEventBandwidthFusionStrategy::Off
                    ) {
                        event_sample.sample_is_app_limited = event_app_limited;
                    }
                }
                bandwidth_estimate = bandwidth_estimate.max(fused_bandwidth);
            }
        }
        event_sample.extra_acked = self.on_ack_event_end(
            est_bandwidth_upper_bound.min(bandwidth_estimate),
            is_new_max_bandwidth,
            round_trip_count,
        );
        event_sample.bytes_acked = self
            .total_bytes_acked
            .saturating_sub(total_bytes_acked_before);
        event_sample.bytes_lost = self
            .total_bytes_lost
            .saturating_sub(total_bytes_lost_before);
        if event_sample.bytes_acked > 0 {
            self.last_legacy_ack_time = Some(ack_time);
            self.last_legacy_total_acked = self.total_bytes_acked;
        }
        if self.is_app_limited
            && largest_acked_packet.is_some_and(|packet_number| {
                sampler_app_limited_exit_strategy().should_exit(
                    packet_number,
                    self.end_of_app_limited_phase,
                    event_app_limited,
                )
            })
        {
            self.is_app_limited = false;
            self.end_of_app_limited_phase = None;
        }
        event_sample
    }

    pub(crate) fn on_app_limited(&mut self) {
        self.is_app_limited = true;
        self.end_of_app_limited_phase = self.last_sent_packet;
    }

    pub(crate) fn remove_obsolete_packets(&mut self, least_unacked: u64) {
        self.connection_state_map
            .retain(|packet_number, _| *packet_number >= least_unacked);
    }

    pub(crate) fn total_bytes_acked(&self) -> u64 {
        self.total_bytes_acked
    }

    pub(crate) fn total_bytes_lost(&self) -> u64 {
        self.total_bytes_lost
    }

    pub(crate) fn is_app_limited(&self) -> bool {
        self.is_app_limited
    }

    pub(crate) fn max_ack_height(&self) -> u64 {
        self.max_ack_height_tracker.get()
    }

    pub(crate) fn reset_max_ack_height_tracker(&mut self, new_height: u64, round_trip_count: u64) {
        self.max_ack_height_tracker
            .reset(new_height, round_trip_count);
    }

    pub(crate) fn set_start_new_aggregation_epoch_after_full_round(&mut self, value: bool) {
        self.max_ack_height_tracker
            .set_start_new_aggregation_epoch_after_full_round(value);
    }

    pub(crate) fn set_limit_max_ack_height_tracker_by_send_rate(&mut self, value: bool) {
        self.limit_max_ack_height_tracker_by_send_rate = value;
    }

    pub(crate) fn set_reduce_extra_acked_on_bandwidth_increase(&mut self, value: bool) {
        self.max_ack_height_tracker
            .set_reduce_extra_acked_on_bandwidth_increase(value);
    }

    pub(crate) fn enable_overestimate_avoidance(&mut self) {
        if self.overestimate_avoidance {
            return;
        }
        self.overestimate_avoidance = true;
        self.max_ack_height_tracker
            .set_ack_aggregation_bandwidth_threshold(
                ACK_AGGREGATION_BANDWIDTH_THRESHOLD_OVER_ESTIMATE,
            );
    }

    fn on_packet_lost(&mut self, packet_number: u64, bytes_lost: u64) -> SendTimeState {
        self.total_bytes_lost = self.total_bytes_lost.saturating_add(bytes_lost);
        self.connection_state_map
            .get(&packet_number)
            .map(|packet| packet.send_time_state)
            .unwrap_or_default()
    }

    fn on_packet_acknowledged(&mut self, ack_time: Instant, packet_number: u64) -> BandwidthSample {
        let mut sample = BandwidthSample {
            send_rate: INF_BANDWIDTH,
            ..Default::default()
        };
        self.last_acked_packet = Some(packet_number);
        let Some(sent_packet) = self.connection_state_map.get(&packet_number).copied() else {
            return sample;
        };
        let prior_last_acked_packet_sent_time = self.last_acked_packet_sent_time;
        let prior_total_bytes_sent_at_last_acked_packet =
            self.total_bytes_sent_at_last_acked_packet;

        self.total_bytes_acked = self.total_bytes_acked.saturating_add(sent_packet.size);
        self.total_bytes_sent_at_last_acked_packet = sent_packet.send_time_state.total_bytes_sent;
        self.last_acked_packet_sent_time = Some(sent_packet.sent_time);
        self.last_acked_packet_ack_time = Some(ack_time);
        if self.overestimate_avoidance {
            self.recent_ack_points
                .update(ack_time, self.total_bytes_acked);
        }

        let (Some(last_acked_packet_sent_time), Some(last_acked_packet_ack_time)) = (
            sent_packet.last_acked_packet_sent_time,
            sent_packet.last_acked_packet_ack_time,
        ) else {
            return sample;
        };

        let (send_rate_anchor_sent_time, send_rate_anchor_total_sent) =
            match send_rate_anchor_strategy() {
                SendRateAnchorStrategy::AckEvent => prior_last_acked_packet_sent_time
                    .map(|sent_time| (sent_time, prior_total_bytes_sent_at_last_acked_packet))
                    .unwrap_or((
                        last_acked_packet_sent_time,
                        sent_packet.total_bytes_sent_at_last_acked_packet,
                    )),
                SendRateAnchorStrategy::SentState => (
                    last_acked_packet_sent_time,
                    sent_packet.total_bytes_sent_at_last_acked_packet,
                ),
            };

        let send_rate = if sent_packet.sent_time > send_rate_anchor_sent_time {
            Self::bw_from_delta(
                sent_packet
                    .send_time_state
                    .total_bytes_sent
                    .saturating_sub(send_rate_anchor_total_sent),
                sent_packet.sent_time - send_rate_anchor_sent_time,
            )
            .unwrap_or(INF_BANDWIDTH)
        } else {
            INF_BANDWIDTH
        };

        let (a0_ack_time, a0_total_bytes_acked) = if self.overestimate_avoidance {
            self.choose_a0_point(sent_packet.send_time_state.total_bytes_acked)
                .and_then(|point| {
                    point
                        .ack_time
                        .map(|ack_time| (ack_time, point.total_bytes_acked))
                })
                .unwrap_or((
                    last_acked_packet_ack_time,
                    sent_packet.send_time_state.total_bytes_acked,
                ))
        } else {
            (
                last_acked_packet_ack_time,
                sent_packet.send_time_state.total_bytes_acked,
            )
        };

        let ack_elapsed = ack_time.saturating_duration_since(a0_ack_time);
        let Some(ack_rate) = Self::bw_from_delta(
            self.total_bytes_acked.saturating_sub(a0_total_bytes_acked),
            ack_elapsed,
        ) else {
            return sample;
        };

        sample.bandwidth = sample_bandwidth_strategy().select(send_rate, ack_rate);
        sample.rtt = Some(ack_time.saturating_duration_since(sent_packet.sent_time));
        sample.send_rate = send_rate;
        sample.state_at_send = sent_packet.send_time_state;
        sample
    }

    fn choose_a0_point(&mut self, total_bytes_acked: u64) -> Option<AckPoint> {
        if self.a0_candidates.is_empty() {
            return None;
        }

        if self.a0_candidates.len() == 1 {
            return self.a0_candidates.front().copied();
        }

        for i in 1..self.a0_candidates.len() {
            if self.a0_candidates.get(i)?.total_bytes_acked > total_bytes_acked {
                let point = *self.a0_candidates.get(i - 1)?;
                for _ in 0..i.saturating_sub(1) {
                    self.a0_candidates.pop_front();
                }
                return Some(point);
            }
        }

        let point = *self.a0_candidates.back()?;
        while self.a0_candidates.len() > 1 {
            self.a0_candidates.pop_front();
        }
        Some(point)
    }

    fn push_a0_candidate(&mut self, point: AckPoint) {
        if self.a0_candidates.len() >= DEFAULT_CANDIDATES_BUFFER_SIZE {
            self.a0_candidates.pop_front();
        }
        self.a0_candidates.push_back(point);
    }

    fn on_ack_event_end(
        &mut self,
        bandwidth_estimate: u64,
        is_new_max_bandwidth: bool,
        round_trip_count: u64,
    ) -> u64 {
        let newly_acked_bytes = self
            .total_bytes_acked
            .saturating_sub(self.total_bytes_acked_after_last_ack_event);
        if newly_acked_bytes == 0 {
            return 0;
        }
        self.total_bytes_acked_after_last_ack_event = self.total_bytes_acked;
        let extra_acked = self.max_ack_height_tracker.update(
            bandwidth_estimate,
            is_new_max_bandwidth,
            round_trip_count,
            self.last_sent_packet,
            self.last_acked_packet,
            self.last_acked_packet_ack_time.unwrap_or_else(Instant::now),
            newly_acked_bytes,
        );
        if self.overestimate_avoidance && extra_acked == 0 {
            self.push_a0_candidate(self.recent_ack_points.less_recent_point());
        }
        extra_acked
    }

    pub(crate) const fn bw_from_delta(bytes: u64, delta: Duration) -> Option<u64> {
        let window_duration_ns = delta.as_nanos();
        if window_duration_ns == 0 {
            return None;
        }
        let b_ns = bytes.saturating_mul(1_000_000_000);
        let bytes_per_second = b_ns / (window_duration_ns as u64);
        Some(bytes_per_second)
    }
}

fn bytes_from_bandwidth_and_time_delta(bandwidth: u64, delta: Duration) -> u64 {
    ((bandwidth as u128) * delta.as_nanos() / 1_000_000_000u128).min(u64::MAX as u128) as u64
}

impl Default for MaxAckHeightTracker {
    fn default() -> Self {
        Self {
            max_ack_height_filter: ExtraAckedFilter::default(),
            aggregation_epoch_start_time: None,
            aggregation_epoch_bytes: 0,
            last_sent_packet_number_before_epoch: None,
            num_ack_aggregation_epochs: 0,
            ack_aggregation_bandwidth_threshold: ACK_AGGREGATION_BANDWIDTH_THRESHOLD,
            start_new_aggregation_epoch_after_full_round: false,
            reduce_extra_acked_on_bandwidth_increase: false,
        }
    }
}

impl Display for BandwidthSampler {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:.3} MB/s",
            self.total_bytes_acked as f32 / (1024 * 1024) as f32
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AckEventBandwidthFusionStrategy, SampleBandwidthStrategy, SamplerAppLimitedExitStrategy,
        SendRateAnchorStrategy, INF_BANDWIDTH,
    };

    #[test]
    fn sample_bandwidth_strategy_defaults_to_min() {
        assert_eq!(
            SampleBandwidthStrategy::from_env_value(None),
            SampleBandwidthStrategy::Min
        );
        assert_eq!(
            SampleBandwidthStrategy::from_env_value(Some("unknown")),
            SampleBandwidthStrategy::Min
        );
    }

    #[test]
    fn sample_bandwidth_strategy_parses_overrides() {
        assert_eq!(
            SampleBandwidthStrategy::from_env_value(Some("send")),
            SampleBandwidthStrategy::Send
        );
        assert_eq!(
            SampleBandwidthStrategy::from_env_value(Some("max_rate")),
            SampleBandwidthStrategy::Max
        );
    }

    #[test]
    fn send_rate_anchor_strategy_parses_overrides() {
        assert_eq!(
            SendRateAnchorStrategy::from_env_value(None),
            SendRateAnchorStrategy::SentState
        );
        assert_eq!(
            SendRateAnchorStrategy::from_env_value(Some("ack_event")),
            SendRateAnchorStrategy::AckEvent
        );
    }

    #[test]
    fn ack_event_bandwidth_fusion_strategy_parses_overrides() {
        assert_eq!(
            AckEventBandwidthFusionStrategy::from_env_value(None),
            AckEventBandwidthFusionStrategy::Off
        );
        assert_eq!(
            AckEventBandwidthFusionStrategy::from_env_value(Some("send_cap")),
            AckEventBandwidthFusionStrategy::SendCap
        );
    }

    #[test]
    fn sampler_app_limited_exit_strategy_parses_overrides() {
        assert_eq!(
            SamplerAppLimitedExitStrategy::from_env_value(None),
            SamplerAppLimitedExitStrategy::Legacy
        );
        assert_eq!(
            SamplerAppLimitedExitStrategy::from_env_value(Some("tail_clear")),
            SamplerAppLimitedExitStrategy::AckTimeExitOk
        );
    }

    #[test]
    fn sample_bandwidth_strategy_selects_expected_rate() {
        assert_eq!(SampleBandwidthStrategy::Min.select(10, 20), 10);
        assert_eq!(SampleBandwidthStrategy::Send.select(10, 20), 10);
        assert_eq!(SampleBandwidthStrategy::Max.select(10, 20), 20);
        assert_eq!(SampleBandwidthStrategy::Send.select(INF_BANDWIDTH, 20), 20);
        assert_eq!(SampleBandwidthStrategy::Max.select(10, INF_BANDWIDTH), 10);
    }

    #[test]
    fn ack_event_bandwidth_fusion_strategy_caps_legacy_bandwidth() {
        assert_eq!(AckEventBandwidthFusionStrategy::Off.fuse(20, 10), 20);
        assert_eq!(AckEventBandwidthFusionStrategy::SendCap.fuse(20, 10), 10);
        assert_eq!(AckEventBandwidthFusionStrategy::SendCap.fuse(20, 0), 20);
    }

    #[test]
    fn sampler_app_limited_exit_strategy_ack_time_exit_ok_clears_on_boundary_ack() {
        assert!(!SamplerAppLimitedExitStrategy::Legacy.should_exit(7, Some(7), false));
        assert!(SamplerAppLimitedExitStrategy::Legacy.should_exit(8, Some(7), true));
        assert!(SamplerAppLimitedExitStrategy::AckTimeExitOk.should_exit(7, Some(7), false));
        assert!(!SamplerAppLimitedExitStrategy::AckTimeExitOk.should_exit(7, Some(7), true));
    }
}
