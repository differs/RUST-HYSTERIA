//! Logic for controlling the rate at which data is sent

use crate::connection::RttEstimator;
use crate::Instant;
use std::any::Any;
use std::sync::Arc;

mod bbr;
mod brutal;
mod cubic;
mod new_reno;

pub use bbr::{Bbr, BbrConfig};
pub use brutal::{Brutal, BrutalConfig};
pub use cubic::{Cubic, CubicConfig};
pub use new_reno::{NewReno, NewRenoConfig};

/// Metadata for an acked packet in a single ACK/loss processing batch.
#[derive(Debug, Clone, Copy)]
pub struct AckedPacketInfo {
    pub packet_number: u64,
    pub bytes_acked: u64,
}

/// Metadata for a lost packet in a single ACK/loss processing batch.
#[derive(Debug, Clone, Copy)]
pub struct LostPacketInfo {
    pub packet_number: u64,
    pub bytes_lost: u64,
}

/// Summary of one ACK processing batch.
#[derive(Clone, Copy)]
pub struct AckEvent<'a> {
    pub prior_in_flight: u64,
    pub acked_packets: &'a [AckedPacketInfo],
    pub lost_packets: &'a [LostPacketInfo],
    pub app_limited: bool,
    pub largest_packet_num_acked: Option<u64>,
    pub rtt: &'a RttEstimator,
}

/// Common interface for different congestion controllers
pub trait Controller: Send + Sync {
    /// One or more packets were just sent
    #[allow(unused_variables)]
    fn on_sent(
        &mut self,
        now: Instant,
        bytes: u64,
        last_packet_number: u64,
        bytes_in_flight: u64,
    ) {
    }

    /// Packet deliveries were confirmed
    ///
    /// `app_limited` indicates whether the connection was blocked on outgoing
    /// application data prior to receiving these acknowledgements.
    #[allow(unused_variables)]
    fn on_ack(
        &mut self,
        now: Instant,
        packet_number: u64,
        sent: Instant,
        bytes: u64,
        app_limited: bool,
        rtt: &RttEstimator,
    ) {
    }

    /// Packet deliveries and ACK-driven losses were observed in one ACK batch.
    #[allow(unused_variables)]
    fn on_ack_event(&mut self, now: Instant, event: &AckEvent<'_>) {}

    /// Packets are acked in batches, all with the same `now` argument. This indicates one of those batches has completed.
    #[allow(unused_variables)]
    fn on_end_acks(
        &mut self,
        now: Instant,
        in_flight: u64,
        prior_in_flight: u64,
        app_limited: bool,
        largest_packet_num_acked: Option<u64>,
    ) {
    }

    /// Packets were deemed lost or marked congested
    ///
    /// `in_persistent_congestion` indicates whether all packets sent within the persistent
    /// congestion threshold period ending when the most recent packet in this batch was sent were
    /// lost.
    /// `lost_bytes` indicates how many bytes were lost. This value will be 0 for ECN triggers.
    fn on_congestion_event(
        &mut self,
        now: Instant,
        sent: Instant,
        is_persistent_congestion: bool,
        lost_packets: u64,
        lost_bytes: u64,
        bytes_in_flight_before_loss: u64,
    );

    /// The known MTU for the current network path has been updated
    fn on_mtu_update(&mut self, new_mtu: u16);

    /// Number of ack-eliciting bytes that may be in flight
    fn window(&self) -> u64;

    /// Retrieve implementation-specific metrics used to populate `qlog` traces when they are enabled
    fn metrics(&self) -> ControllerMetrics {
        ControllerMetrics {
            congestion_window: self.window(),
            ssthresh: None,
            pacing_rate: None,
            pacing_behavior: PacingBehavior::Window,
        }
    }

    /// Duplicate the controller's state
    fn clone_box(&self) -> Box<dyn Controller>;

    /// Initial congestion window
    fn initial_window(&self) -> u64;

    /// Returns Self for use in down-casting to extract implementation details
    fn into_any(self: Box<Self>) -> Box<dyn Any>;
}

/// Common congestion controller metrics
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub enum PacingBehavior {
    /// Use Quinn's existing window-based pacing behavior.
    #[default]
    Window,
    /// Use a rate-based token bucket pacing behavior.
    RateTokenBucket,
}

/// Common congestion controller metrics.
#[derive(Default)]
#[non_exhaustive]
pub struct ControllerMetrics {
    /// Congestion window (bytes)
    pub congestion_window: u64,
    /// Slow start threshold (bytes)
    pub ssthresh: Option<u64>,
    /// Pacing rate (bits/s)
    pub pacing_rate: Option<u64>,
    /// Preferred pacing implementation for this controller.
    pub pacing_behavior: PacingBehavior,
}

/// Constructs controllers on demand
pub trait ControllerFactory {
    /// Construct a fresh `Controller`
    fn build(self: Arc<Self>, now: Instant, current_mtu: u16) -> Box<dyn Controller>;
}

const BASE_DATAGRAM_SIZE: u64 = 1200;
