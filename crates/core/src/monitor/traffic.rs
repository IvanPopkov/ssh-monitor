use crate::Stamp;
use crate::parse::ByteCounters;
use crate::ring::Ring;
use std::time::Duration;

pub const WINDOW: Duration = Duration::from_secs(300);
pub const CAPACITY: usize = 600;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RateSample {
    pub stamp: Stamp,
    /// Bytes per second; `None` = gap (DOWN, first sample, counter reset, probe failed).
    pub bps: Option<f64>,
}

#[derive(Debug, Clone)]
pub struct Traffic {
    pub rx: Ring<RateSample>,
    pub tx: Ring<RateSample>,
    pub current: Option<(f64, f64)>,
    /// Last time bytes_in grew (or the clock was (re)started by a master/resume event).
    pub last_rx_activity: Option<Stamp>,
    last: Option<(Stamp, ByteCounters)>,
}

impl Default for Traffic {
    fn default() -> Self {
        Self::new()
    }
}

impl Traffic {
    pub fn new() -> Self {
        Self {
            rx: Ring::new(CAPACITY),
            tx: Ring::new(CAPACITY),
            current: None,
            last_rx_activity: None,
            last: None,
        }
    }

    /// `None` counters = nettop had no row for the PID (dead process): a gap, not zero.
    pub fn on_sample(&mut self, counters: Option<ByteCounters>, now: Stamp) {
        self.expire(now);
        let Some(c) = counters else {
            self.push_gap(now);
            self.restart_silence(now);
            self.last = None;
            return;
        };
        match self.last {
            Some((t0, c0))
                if c.bytes_in >= c0.bytes_in
                    && c.bytes_out >= c0.bytes_out
                    && now.mono > t0.mono =>
            {
                let dt = (now.mono - t0.mono).as_secs_f64();
                let rx = (c.bytes_in - c0.bytes_in) as f64 / dt;
                let tx = (c.bytes_out - c0.bytes_out) as f64 / dt;
                self.rx.push(RateSample {
                    stamp: now,
                    bps: Some(rx),
                });
                self.tx.push(RateSample {
                    stamp: now,
                    bps: Some(tx),
                });
                self.current = Some((rx, tx));
                if c.bytes_in > c0.bytes_in {
                    self.last_rx_activity = Some(now);
                }
            }
            _ => {
                // First sample after (re)start or a reset: nothing to diff, and measured silence starts here.
                self.push_gap(now);
                self.last_rx_activity = Some(now);
            }
        }
        self.last = Some((now, c));
    }

    /// Probe failed or master is DOWN: record a gap for this tick.
    pub fn on_gap(&mut self, now: Stamp) {
        self.expire(now);
        self.push_gap(now);
        self.restart_silence(now);
    }

    /// Only contiguous measured time counts: a failed nettop must never report a stall.
    fn restart_silence(&mut self, now: Stamp) {
        if self.last_rx_activity.is_some() {
            self.last_rx_activity = Some(now);
        }
    }

    /// A (new) master appeared: restart the diff and the silence clock.
    pub fn on_master_changed(&mut self, now: Stamp) {
        self.last = None;
        self.current = None;
        self.last_rx_activity = Some(now);
    }

    pub fn on_master_lost(&mut self) {
        self.last = None;
        self.current = None;
        self.last_rx_activity = None;
    }

    /// After sleep the old counters would produce a bogus rate over a tiny monotonic gap.
    pub fn on_resume(&mut self, now: Stamp) {
        self.last = None;
        self.current = None;
        if self.last_rx_activity.is_some() {
            self.last_rx_activity = Some(now);
        }
    }

    pub fn peak_rx(&self) -> Option<f64> {
        self.rx.iter().filter_map(|s| s.bps).reduce(f64::max)
    }

    pub fn peak_tx(&self) -> Option<f64> {
        self.tx.iter().filter_map(|s| s.bps).reduce(f64::max)
    }

    fn push_gap(&mut self, now: Stamp) {
        self.rx.push(RateSample {
            stamp: now,
            bps: None,
        });
        self.tx.push(RateSample {
            stamp: now,
            bps: None,
        });
        self.current = None;
    }

    fn expire(&mut self, now: Stamp) {
        let from = now.mono.saturating_sub(WINDOW);
        self.rx.drop_oldest_while(|s| s.stamp.mono < from);
        self.tx.drop_oldest_while(|s| s.stamp.mono < from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(bytes_in: u64, bytes_out: u64) -> Option<ByteCounters> {
        Some(ByteCounters {
            bytes_in,
            bytes_out,
        })
    }

    #[test]
    fn rates_are_counter_diffs_over_monotonic_time() {
        let mut t = Traffic::new();
        t.on_sample(c(1000, 500), Stamp::at_secs(10));
        assert_eq!(t.current, None, "first sample has nothing to diff against");
        assert_eq!(t.rx.latest().unwrap().bps, None);
        t.on_sample(c(3000, 500), Stamp::at_secs(12));
        assert_eq!(t.current, Some((1000.0, 0.0)));
        assert_eq!(t.last_rx_activity, Some(Stamp::at_secs(12)));
        t.on_sample(c(3000, 1500), Stamp::at_secs(13));
        assert_eq!(t.current, Some((0.0, 1000.0)));
        assert_eq!(
            t.last_rx_activity,
            Some(Stamp::at_secs(12)),
            "tx only: rx clock untouched"
        );
        assert_eq!(t.peak_rx(), Some(1000.0));
        assert_eq!(t.peak_tx(), Some(1000.0));
    }

    #[test]
    fn counter_reset_and_dead_pid_are_gaps_not_zero() {
        let mut t = Traffic::new();
        t.on_sample(c(5000, 5000), Stamp::at_secs(1));
        t.on_sample(c(6000, 6000), Stamp::at_secs(2));
        assert!(t.current.is_some());
        t.on_sample(c(10, 10), Stamp::at_secs(3));
        assert_eq!(t.current, None, "counters went backwards: gap");
        t.on_sample(c(20, 10), Stamp::at_secs(4));
        assert_eq!(t.current, Some((10.0, 0.0)), "diff restarts from the reset");
        t.on_sample(None, Stamp::at_secs(5));
        assert_eq!(t.current, None);
        t.on_sample(c(30, 10), Stamp::at_secs(6));
        assert_eq!(
            t.current, None,
            "nothing to diff against after a dead-PID sample"
        );
        let gaps = t.rx.iter().filter(|s| s.bps.is_none()).count();
        assert_eq!(gaps, 4);
    }

    #[test]
    fn master_lifecycle_and_resume_restart_the_silence_clock() {
        let mut t = Traffic::new();
        t.on_master_changed(Stamp::at_secs(1));
        assert_eq!(t.last_rx_activity, Some(Stamp::at_secs(1)));
        t.on_sample(c(1, 1), Stamp::at_secs(2));
        t.on_sample(c(1, 1), Stamp::at_secs(3));
        assert_eq!(
            t.last_rx_activity,
            Some(Stamp::at_secs(2)),
            "no rx growth since the first measurement"
        );
        t.on_resume(Stamp::at_secs(500));
        assert_eq!(t.last_rx_activity, Some(Stamp::at_secs(500)));
        assert_eq!(t.current, None);
        t.on_sample(c(1_000_000, 1), Stamp::at_secs(501));
        assert_eq!(t.current, None, "no rate across the sleep gap");
        t.on_master_lost();
        assert_eq!(t.last_rx_activity, None);
        t.on_gap(Stamp::at_secs(502));
        assert_eq!(t.rx.latest().unwrap().bps, None);
    }

    #[test]
    fn window_expires() {
        let mut t = Traffic::new();
        t.on_sample(c(1, 1), Stamp::at_secs(0));
        t.on_sample(c(2, 2), Stamp::at_secs(1));
        t.on_sample(c(3, 3), Stamp::at_secs(301));
        assert_eq!(t.rx.len(), 2);
        assert_eq!(t.rx.oldest().unwrap().stamp, Stamp::at_secs(1));
    }
}
