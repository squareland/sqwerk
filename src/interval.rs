use std::time::Duration;
use tokio::time::{Instant, Interval};

pub(crate) struct TimedInterval {
    interval: Interval,
    times: u32,
    timeout: Option<Instant>,
}

impl TimedInterval {
    pub(crate) fn from(interval: Interval, times: u32) -> Self {
        Self { interval, times, timeout: None }
    }

    pub(crate) async fn check_expired(&mut self) -> bool {
        self.interval.tick().await;
        if let Some(timeout) = self.timeout.as_ref() {
            Instant::now() >= *timeout
        } else {
            false
        }
    }

    pub(crate) fn begin(&mut self) {
        let total = self.interval.period().as_secs() * self.times as u64;
        self.timeout = Some(Instant::now() + Duration::from_secs(total));
    }

    pub(crate) fn reset(&mut self) {
        self.timeout = None;
    }
}