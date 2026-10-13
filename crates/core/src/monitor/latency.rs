use super::event::{EventKind, EventLog};
use super::health::State;
use crate::Stamp;
use crate::parse::PingResult;
use crate::ring::Ring;
use std::time::Duration;

pub const WINDOW: Duration = Duration::from_secs(300);
/// Samples a 5-minute window can hold at the minimum interval (0.5s).
pub const CAPACITY: usize = 600;
/// Baseline needs this many UP samples before the slow rule and spikes are enabled.
pub const BASELINE_MIN_SAMPLES: usize = 10;
/// At most one spike event per this long; later spikes bump its ×N.
pub const SPIKE_COOLDOWN: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RttSample {
    pub stamp: Stamp,
    /// `None` = no reply (lost) or no measurement (probe failed, sleep gap).
    pub rtt_ms: Option<f64>,
    /// A real lost ping, as opposed to a probe that could not run.
    pub lost: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LatencyStats {
    /// Latest reply, if the latest sample was a reply.
    pub now_ms: Option<f64>,
    pub min_ms: Option<f64>,
    pub avg_ms: Option<f64>,
    pub max_ms: Option<f64>,
    /// Mean |ΔRTT| between consecutive replies.
    pub jitter_ms: Option<f64>,
    /// Lost ÷ (lost + replies) × 100; `None` without any ping.
    pub loss_pct: Option<f64>,
    pub replies: usize,
    pub lost: usize,
}

#[derive(Debug, Clone)]
pub struct Latency {
    pub samples: Ring<RttSample>,
    /// UP replies in the window; frozen while not UP so a long DEGRADED stretch cannot erase the baseline.
    baseline_samples: Ring<(Stamp, f64)>,
    /// Last full baseline; kept until 10 fresh UP samples replace it (or the path changes).
    last_baseline: Option<f64>,
    pub consecutive_lost: u32,
    last_spike: Option<(u64, Stamp)>,
}

impl Default for Latency {
    fn default() -> Self {
        Self::new()
    }
}

impl Latency {
    pub fn new() -> Self {
        Self {
            samples: Ring::new(CAPACITY),
            baseline_samples: Ring::new(CAPACITY),
            last_baseline: None,
            consecutive_lost: 0,
            last_spike: None,
        }
    }

    pub fn on_ping(&mut self, ping: PingResult, now: Stamp, state: State, log: &mut EventLog) {
        self.expire(now, state);
        match ping {
            PingResult::Reply { rtt_ms } => {
                // A spike is judged against the baseline of the samples *before* it.
                let baseline_before = self.baseline();
                self.samples.push(RttSample {
                    stamp: now,
                    rtt_ms: Some(rtt_ms),
                    lost: false,
                });
                self.consecutive_lost = 0;
                if state == State::Up {
                    self.baseline_samples.push((now, rtt_ms));
                    if self.baseline_samples.len() >= BASELINE_MIN_SAMPLES {
                        self.last_baseline = median(self.baseline_samples.iter().map(|(_, r)| *r));
                    }
                }
                // Spikes only while UP: DEGRADED mutes them (SPEC), DOWN has no connection.
                if state == State::Up
                    && let Some(baseline) = baseline_before
                {
                    let threshold_ms = Self::slow_bar(baseline);
                    if rtt_ms > threshold_ms {
                        self.spike(rtt_ms, threshold_ms, now, log);
                    }
                }
            }
            PingResult::Lost => {
                self.samples.push(RttSample {
                    stamp: now,
                    rtt_ms: None,
                    lost: true,
                });
                self.consecutive_lost += 1;
            }
        }
    }

    /// The ping probe could not run: a gap, not a loss.
    pub fn on_failed(&mut self, now: Stamp, state: State) {
        self.expire(now, state);
        self.samples.push(RttSample {
            stamp: now,
            rtt_ms: None,
            lost: false,
        });
    }

    /// After sleep: mark the gap so pre-sleep samples are not adjacent to post-sleep ones.
    pub fn on_resume(&mut self, now: Stamp, state: State) {
        self.on_failed(now, state);
    }

    /// A master just appeared: pings lost while it was DOWN say nothing about it.
    pub fn on_master_up(&mut self) {
        self.consecutive_lost = 0;
    }

    /// Path changed: the old RTT level means nothing any more.
    pub fn reset_baseline(&mut self) {
        self.baseline_samples.clear();
        self.last_baseline = None;
    }

    /// Median of the window's UP samples once there are enough, else the last full baseline.
    pub fn baseline(&self) -> Option<f64> {
        if self.baseline_samples.len() >= BASELINE_MIN_SAMPLES {
            return median(self.baseline_samples.iter().map(|(_, r)| *r));
        }
        self.last_baseline
    }

    pub fn baseline_sample_count(&self) -> usize {
        self.baseline_samples.len()
    }

    /// The slow bar: max(3× baseline, baseline + 100ms).
    pub fn slow_bar(baseline: f64) -> f64 {
        (3.0 * baseline).max(baseline + 100.0)
    }

    /// Median of replies in `(now - window, now]` (clipped at the origin, which is included).
    pub fn median_last(&self, window: Duration, now: Stamp) -> Option<f64> {
        let from = now.mono.saturating_sub(window);
        median(
            self.samples
                .iter()
                .filter(|s| s.stamp.mono > from || (from.is_zero() && s.stamp.mono == from))
                .filter_map(|s| s.rtt_ms),
        )
    }

    pub fn last_reply_at(&self) -> Option<Stamp> {
        self.samples
            .iter()
            .rev()
            .find(|s| s.rtt_ms.is_some())
            .map(|s| s.stamp)
    }

    /// Was the most recent sample a reply (not lost, not a gap)?
    pub fn latest_is_reply(&self) -> bool {
        self.samples.latest().is_some_and(|s| s.rtt_ms.is_some())
    }

    pub fn stats(&self) -> LatencyStats {
        let replies: Vec<f64> = self.samples.iter().filter_map(|s| s.rtt_ms).collect();
        let lost = self.samples.iter().filter(|s| s.lost).count();
        let n = replies.len();
        let mut st = LatencyStats {
            now_ms: self.samples.latest().and_then(|s| s.rtt_ms),
            replies: n,
            lost,
            ..LatencyStats::default()
        };
        if n > 0 {
            st.min_ms = replies.iter().copied().reduce(f64::min);
            st.max_ms = replies.iter().copied().reduce(f64::max);
            st.avg_ms = Some(replies.iter().sum::<f64>() / n as f64);
            if n > 1 {
                let diffs: f64 = replies.windows(2).map(|w| (w[1] - w[0]).abs()).sum();
                st.jitter_ms = Some(diffs / (n - 1) as f64);
            }
        }
        if n + lost > 0 {
            st.loss_pct = Some(lost as f64 * 100.0 / (n + lost) as f64);
        }
        st
    }

    fn spike(&mut self, rtt_ms: f64, threshold_ms: f64, now: Stamp, log: &mut EventLog) {
        if let Some((id, at)) = self.last_spike
            && now.mono.saturating_sub(at.mono) < SPIKE_COOLDOWN
            && log.amend(id, |k| {
                if let EventKind::RttSpike {
                    count,
                    rtt_ms: worst,
                    ..
                } = k
                {
                    *count += 1;
                    if rtt_ms > *worst {
                        *worst = rtt_ms;
                    }
                }
            })
        {
            return;
        }
        let id = log.push(
            now,
            EventKind::RttSpike {
                rtt_ms,
                threshold_ms,
                count: 1,
            },
        );
        self.last_spike = Some((id, now));
    }

    fn expire(&mut self, now: Stamp, state: State) {
        let from = now.mono.saturating_sub(WINDOW);
        self.samples.drop_oldest_while(|s| s.stamp.mono < from);
        if state == State::Up {
            self.baseline_samples
                .drop_oldest_while(|(s, _)| s.mono < from);
        }
    }
}

pub fn median(values: impl Iterator<Item = f64>) -> Option<f64> {
    let mut v: Vec<f64> = values.collect();
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply(ms: f64) -> PingResult {
        PingResult::Reply { rtt_ms: ms }
    }

    #[test]
    fn median_of_odd_even_and_empty() {
        assert_eq!(median([3.0, 1.0, 2.0].into_iter()), Some(2.0));
        assert_eq!(median([4.0, 1.0, 2.0, 3.0].into_iter()), Some(2.5));
        assert_eq!(median(std::iter::empty()), None);
    }

    #[test]
    fn stats_cover_min_avg_max_jitter_and_loss() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        for (i, p) in [reply(10.0), reply(20.0), PingResult::Lost, reply(14.0)]
            .into_iter()
            .enumerate()
        {
            l.on_ping(p, Stamp::at_secs(i as u64), State::Up, &mut log);
        }
        l.on_failed(Stamp::at_secs(4), State::Up);
        let s = l.stats();
        assert_eq!((s.replies, s.lost), (3, 1));
        assert_eq!((s.min_ms, s.max_ms), (Some(10.0), Some(20.0)));
        assert!((s.avg_ms.unwrap() - 44.0 / 3.0).abs() < 1e-9);
        assert!(
            (s.jitter_ms.unwrap() - 8.0).abs() < 1e-9,
            "(|20-10| + |14-20|) / 2"
        );
        assert_eq!(s.loss_pct, Some(25.0));
        assert_eq!(s.now_ms, None, "latest sample is a probe failure");
        assert!(!l.latest_is_reply());
        assert_eq!(l.last_reply_at(), Some(Stamp::at_secs(3)));
        assert_eq!(l.samples.len(), 5);
        assert_eq!(Latency::new().stats(), LatencyStats::default());
    }

    #[test]
    fn consecutive_lost_resets_on_reply_master_up_and_ignores_probe_failures() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        l.on_ping(PingResult::Lost, Stamp::at_secs(1), State::Up, &mut log);
        l.on_ping(PingResult::Lost, Stamp::at_secs(2), State::Up, &mut log);
        l.on_failed(Stamp::at_secs(3), State::Up);
        assert_eq!(l.consecutive_lost, 2);
        l.on_ping(reply(5.0), Stamp::at_secs(4), State::Up, &mut log);
        assert_eq!(l.consecutive_lost, 0);
        l.on_ping(PingResult::Lost, Stamp::at_secs(5), State::Down, &mut log);
        l.on_master_up();
        assert_eq!(l.consecutive_lost, 0);
    }

    #[test]
    fn baseline_needs_ten_up_samples_and_ignores_degraded_ones() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        for i in 0..9 {
            l.on_ping(reply(10.0), Stamp::at_secs(i), State::Up, &mut log);
        }
        l.on_ping(reply(500.0), Stamp::at_secs(9), State::Degraded, &mut log);
        assert_eq!(l.baseline(), None, "9 UP samples only");
        l.on_ping(reply(12.0), Stamp::at_secs(10), State::Up, &mut log);
        assert_eq!(l.baseline(), Some(10.0));
        assert_eq!(Latency::slow_bar(10.0), 110.0);
        assert_eq!(Latency::slow_bar(80.0), 240.0);
        l.reset_baseline();
        assert_eq!(l.baseline(), None);
    }

    #[test]
    fn baseline_is_frozen_while_not_up_and_survives_until_ten_fresh_samples() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        for i in 0..10 {
            l.on_ping(reply(10.0), Stamp::at_secs(i), State::Up, &mut log);
        }
        // Six minutes DEGRADED: the UP samples are older than the window but must not expire.
        for i in 10..370 {
            l.on_ping(reply(300.0), Stamp::at_secs(i), State::Degraded, &mut log);
        }
        assert_eq!(l.baseline(), Some(10.0), "frozen");
        // Back UP: the old samples expire on the first UP ping, the value is kept meanwhile.
        l.on_ping(reply(11.0), Stamp::at_secs(370), State::Up, &mut log);
        assert_eq!(l.baseline_sample_count(), 1);
        assert_eq!(
            l.baseline(),
            Some(10.0),
            "last full baseline until 10 fresh ones"
        );
        for i in 371..380 {
            l.on_ping(reply(11.0), Stamp::at_secs(i), State::Up, &mut log);
        }
        assert_eq!(l.baseline(), Some(11.0));
    }

    #[test]
    fn window_expires_and_median_window_is_half_open() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        l.on_ping(reply(10.0), Stamp::at_secs(0), State::Up, &mut log);
        l.on_ping(reply(10.0), Stamp::at_secs(299), State::Up, &mut log);
        assert_eq!(l.samples.len(), 2);
        l.on_ping(reply(10.0), Stamp::at_secs(301), State::Up, &mut log);
        assert_eq!(l.samples.len(), 2, "sample at t=0 expired");
        assert_eq!(
            l.median_last(Duration::from_secs(5), Stamp::at_secs(301)),
            Some(10.0)
        );
        assert_eq!(
            l.median_last(Duration::from_secs(1), Stamp::at_secs(310)),
            None
        );
        // (now-5s, now]: at 1 Hz that is five samples, not six.
        let mut l = Latency::new();
        for i in 0..=10u64 {
            let ms = if i <= 5 { 10.0 } else { 100.0 };
            l.on_ping(reply(ms), Stamp::at_secs(i), State::Up, &mut log);
        }
        assert_eq!(
            l.median_last(Duration::from_secs(5), Stamp::at_secs(10)),
            Some(100.0),
            "samples 6..=10 only"
        );
        assert_eq!(
            l.median_last(Duration::from_secs(5), Stamp::at_secs(5)),
            Some(10.0),
            "window clipped at t=0 includes t=0"
        );
    }

    #[test]
    fn spikes_only_while_up() {
        let mut l = Latency::new();
        let mut log = EventLog::new();
        for i in 0..10 {
            l.on_ping(reply(10.0), Stamp::at_secs(i), State::Up, &mut log);
        }
        l.on_ping(reply(500.0), Stamp::at_secs(10), State::Down, &mut log);
        l.on_ping(reply(500.0), Stamp::at_secs(11), State::Degraded, &mut log);
        assert!(log.take_fresh().is_empty());
        l.on_ping(reply(500.0), Stamp::at_secs(12), State::Up, &mut log);
        assert_eq!(log.take_fresh().len(), 1);
    }
}
