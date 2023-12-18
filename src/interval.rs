use tokio::time::Interval;

pub(crate) struct TimedInterval {
    interval: Interval,
    times: u32,
    current_time: u32,
}

impl TimedInterval {
    pub(crate) fn from(interval: Interval, times: u32) -> Self {
        Self { interval, times, current_time: times }
    }

    pub(crate) async fn check_expired(&mut self) -> bool {
        self.interval.tick().await;
        self.current_time -= 1;
        self.current_time == 0
    }

    pub(crate) fn reset(&mut self) {
        self.current_time = self.times;
    }
}