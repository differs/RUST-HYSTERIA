use std::{
    cmp,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

#[derive(Debug)]
pub(crate) struct BandwidthLimiter {
    rate_bytes_per_sec: u64,
    burst_bytes: u64,
    state: Mutex<LimiterState>,
}

#[derive(Debug)]
struct LimiterState {
    tokens: f64,
    last_refill: Instant,
}

const MIN_BURST_BYTES: u64 = 10 * 1500;
const MAX_WRITE_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum LimitDecision {
    Ready(usize),
    Wait(Duration),
}

impl BandwidthLimiter {
    pub(crate) fn optional(rate_bytes_per_sec: u64) -> Option<Arc<Self>> {
        (rate_bytes_per_sec > 0).then(|| Arc::new(Self::new(rate_bytes_per_sec)))
    }

    pub(crate) fn new(rate_bytes_per_sec: u64) -> Self {
        let burst_bytes = cmp::max(rate_bytes_per_sec / 250, MIN_BURST_BYTES);
        Self {
            rate_bytes_per_sec,
            burst_bytes,
            state: Mutex::new(LimiterState {
                tokens: burst_bytes as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    pub(crate) fn take_stream_budget(&self, requested: usize) -> LimitDecision {
        if requested == 0 {
            return LimitDecision::Ready(0);
        }

        let mut state = self.state.lock().expect("bandwidth limiter mutex poisoned");
        self.refill(&mut state);
        if state.tokens >= 1.0 {
            let available = state.tokens.floor().max(1.0) as usize;
            let granted = requested.min(available).min(MAX_WRITE_CHUNK);
            state.tokens -= granted as f64;
            LimitDecision::Ready(granted)
        } else {
            LimitDecision::Wait(self.wait_duration(1.0 - state.tokens))
        }
    }

    pub(crate) async fn wait_for_chunk(&self, requested: usize) -> usize {
        if requested == 0 {
            return 0;
        }

        loop {
            match self.take_stream_budget(requested) {
                LimitDecision::Ready(granted) => return granted,
                LimitDecision::Wait(duration) => tokio::time::sleep(duration).await,
            }
        }
    }

    pub(crate) async fn wait_for(&self, requested: usize) {
        if requested == 0 {
            return;
        }

        loop {
            let decision = {
                let mut state = self.state.lock().expect("bandwidth limiter mutex poisoned");
                self.refill(&mut state);
                if state.tokens >= requested as f64 {
                    state.tokens -= requested as f64;
                    LimitDecision::Ready(requested)
                } else {
                    LimitDecision::Wait(self.wait_duration(requested as f64 - state.tokens))
                }
            };

            match decision {
                LimitDecision::Ready(_) => return,
                LimitDecision::Wait(duration) => tokio::time::sleep(duration).await,
            }
        }
    }

    fn refill(&self, state: &mut LimiterState) {
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.last_refill);
        state.last_refill = now;

        let replenished = self.rate_bytes_per_sec as f64 * elapsed.as_secs_f64();
        state.tokens = (state.tokens + replenished).min(self.burst_bytes as f64);
    }

    fn wait_duration(&self, missing_bytes: f64) -> Duration {
        let seconds = missing_bytes.max(1.0) / self.rate_bytes_per_sec.max(1) as f64;
        Duration::from_secs_f64(seconds)
    }
}
