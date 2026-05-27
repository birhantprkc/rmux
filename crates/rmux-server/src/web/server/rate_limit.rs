use std::time::{Duration, Instant};

const OPERATOR_RATE_LIMIT: u16 = 200;

pub(super) struct OperatorRateLimiter {
    remaining: u16,
    window_started: Instant,
}

impl OperatorRateLimiter {
    pub(super) fn new() -> Self {
        Self {
            remaining: OPERATOR_RATE_LIMIT,
            window_started: Instant::now(),
        }
    }

    pub(super) fn try_acquire(&mut self) -> bool {
        if self.window_started.elapsed() >= Duration::from_secs(1) {
            self.remaining = OPERATOR_RATE_LIMIT;
            self.window_started = Instant::now();
        }
        if self.remaining == 0 {
            return false;
        }
        self.remaining -= 1;
        true
    }
}
