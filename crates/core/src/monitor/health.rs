use super::event::{DegradeReason, EventKind, EventLog};
use super::latency::Latency;
use super::traffic::Traffic;
use crate::Stamp;
use crate::parse::MasterStatus;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Up,
    Degraded,
    Down,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            State::Up => "UP",
            State::Degraded => "DEGRADED",
            State::Down => "DOWN",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterInfo {
    pub pid: u32,
    pub since: Stamp,
    pub uptime: Option<(Duration, Stamp)>,
}

impl MasterInfo {
    /// `None` until `ps -o etime` answers: never guessed from when we first saw the PID.
    pub fn uptime_at(&self, now: Stamp) -> Option<Duration> {
        self.uptime
            .map(|(d, at)| d.saturating_add(now.mono.saturating_sub(at.mono)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    None,
    /// Master appeared (from DOWN or at start).
    Up,
    /// PID changed without an observed DOWN.
    Restarted,
    /// Carries the `master down` event id for a later supervisor merge.
    Down {
        event_id: u64,
    },
}

/// Hysteresis: DEGRADED → UP needs this long without any degrade condition.
pub const RECOVERY_CLEAN: Duration = Duration::from_secs(5);
pub const LOSS_THRESHOLD: u32 = 3;
pub const SLOW_WINDOW: Duration = Duration::from_secs(5);

#[derive(Debug, Clone)]
pub struct Health {
    pub state: State,
    pub since: Stamp,
    pub master: Option<MasterInfo>,
    /// Last stamp at which `ssh -O check` reported a running master.
    pub last_up_seen: Option<Stamp>,
    /// Last stamp at which `ssh -O check` answered at all (up or down).
    pub last_status_at: Option<Stamp>,
    /// For `down X (was up Y)`.
    pub was_up_for: Option<Duration>,
    /// Reason the connection is (or was last) DEGRADED.
    pub reason: Option<DegradeReason>,
    degraded_since: Option<Stamp>,
    last_bad: Option<Stamp>,
    seen_any_master: bool,
    initialized: bool,
}

impl Default for Health {
    fn default() -> Self {
        Self::new()
    }
}

impl Health {
    pub fn new() -> Self {
        Self {
            state: State::Down,
            since: Stamp::default(),
            master: None,
            last_up_seen: None,
            last_status_at: None,
            was_up_for: None,
            reason: None,
            degraded_since: None,
            last_bad: None,
            seen_any_master: false,
            initialized: false,
        }
    }

    pub fn master_pid(&self) -> Option<u32> {
        self.master.as_ref().map(|m| m.pid)
    }

    pub fn initialized(&self) -> bool {
        self.initialized
    }

    pub fn on_status(
        &mut self,
        status: &MasterStatus,
        now: Stamp,
        log: &mut EventLog,
    ) -> Transition {
        self.last_status_at = Some(now);
        let first = !self.initialized;
        self.initialized = true;
        match status {
            MasterStatus::Running { pid } => {
                self.last_up_seen = Some(now);
                match &self.master {
                    Some(m) if m.pid == *pid => Transition::None,
                    Some(_) => {
                        self.master = Some(MasterInfo {
                            pid: *pid,
                            since: now,
                            uptime: None,
                        });
                        log.push(now, EventKind::MasterRestarted { pid: *pid });
                        Transition::Restarted
                    }
                    None => {
                        let kind = if self.seen_any_master {
                            EventKind::MasterRestarted { pid: *pid }
                        } else {
                            EventKind::MasterUp { pid: *pid }
                        };
                        self.seen_any_master = true;
                        self.master = Some(MasterInfo {
                            pid: *pid,
                            since: now,
                            uptime: None,
                        });
                        self.set_state(State::Up, now);
                        self.was_up_for = None;
                        self.reason = None;
                        self.last_bad = None;
                        log.push(now, kind);
                        Transition::Up
                    }
                }
            }
            MasterStatus::Down { .. } => {
                if self.master.is_none() && !first {
                    return Transition::None;
                }
                let was_up = self.master.as_ref().and_then(|m| m.uptime_at(now));
                self.master = None;
                self.was_up_for = was_up;
                self.set_state(State::Down, now);
                self.reason = None;
                let event_id = log.push(
                    now,
                    EventKind::MasterDown {
                        was_up,
                        supervisor: None,
                    },
                );
                Transition::Down { event_id }
            }
        }
    }

    pub fn set_uptime(&mut self, uptime: Duration, now: Stamp) {
        if let Some(m) = &mut self.master {
            m.uptime = Some((uptime, now));
        }
    }

    /// Re-evaluates the degrade conditions; call after every input.
    pub fn evaluate(
        &mut self,
        now: Stamp,
        latency: &Latency,
        traffic: &Traffic,
        server_alive_interval: Option<u32>,
        log: &mut EventLog,
    ) {
        if self.master.is_none() {
            return;
        }
        let reason = degrade_reason(now, latency, traffic, server_alive_interval);
        match (self.state, reason) {
            (State::Up, Some(r)) => {
                self.set_state(State::Degraded, now);
                self.degraded_since = Some(now);
                self.last_bad = Some(now);
                self.reason = Some(r.clone());
                log.push(now, EventKind::Degraded(r));
            }
            (State::Degraded, Some(r)) => {
                self.last_bad = Some(now);
                self.reason = Some(r);
            }
            (State::Degraded, None) => {
                // A lost ping is not a "clean ping" even before three of them make a reason.
                let lost_now = latency
                    .samples
                    .latest()
                    .is_some_and(|s| s.lost && s.stamp == now);
                if lost_now {
                    self.last_bad = Some(now);
                    return;
                }
                let clean_for = now
                    .mono
                    .saturating_sub(self.last_bad.map_or(now.mono, |b| b.mono));
                // "5s of clean pings": replies since the last bad moment, and the latest sample is a reply.
                let replied_since_bad = latency
                    .last_reply_at()
                    .is_some_and(|r| self.last_bad.is_none_or(|b| r.mono > b.mono));
                if clean_for >= RECOVERY_CLEAN && replied_since_bad && latency.latest_is_reply() {
                    let degraded_for = now
                        .mono
                        .saturating_sub(self.degraded_since.map_or(now.mono, |d| d.mono));
                    self.set_state(State::Up, now);
                    self.degraded_since = None;
                    log.push(now, EventKind::Recovered { degraded_for });
                }
            }
            _ => {}
        }
    }

    fn set_state(&mut self, state: State, now: Stamp) {
        self.state = state;
        self.since = now;
    }
}

/// Loss first (most specific), then stall, then slow.
fn degrade_reason(
    now: Stamp,
    latency: &Latency,
    traffic: &Traffic,
    server_alive_interval: Option<u32>,
) -> Option<DegradeReason> {
    if latency.consecutive_lost >= LOSS_THRESHOLD {
        return Some(DegradeReason::Loss {
            consecutive: latency.consecutive_lost,
        });
    }
    if let Some(sai) = server_alive_interval
        && let Some(last) = traffic.last_rx_activity
    {
        let limit = Duration::from_secs(u64::from(sai) * 2 + 5);
        let silent_for = now.mono.saturating_sub(last.mono);
        if silent_for > limit {
            return Some(DegradeReason::Stall { silent_for });
        }
    }
    if let Some(baseline) = latency.baseline()
        && let Some(median_ms) = latency.median_last(SLOW_WINDOW, now)
    {
        let threshold_ms = Latency::slow_bar(baseline);
        if median_ms > threshold_ms {
            return Some(DegradeReason::Slow {
                median_ms,
                threshold_ms,
            });
        }
    }
    None
}
