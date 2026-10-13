//! Events (SPEC F5) as typed data; core never formats.

use crate::Stamp;
use crate::parse::{ClientKind, PathKind};
use crate::ring::Ring;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Green,
    Yellow,
    Red,
    Dim,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ProbeKind {
    Status,
    Traffic,
    Uptime,
    Latency,
    Clients,
    Rogues,
    Supervisor,
    Path,
}

impl ProbeKind {
    pub const ALL: [ProbeKind; 8] = [
        ProbeKind::Status,
        ProbeKind::Traffic,
        ProbeKind::Uptime,
        ProbeKind::Latency,
        ProbeKind::Clients,
        ProbeKind::Rogues,
        ProbeKind::Supervisor,
        ProbeKind::Path,
    ];
}

#[derive(Debug, Clone, PartialEq)]
pub enum DegradeReason {
    /// ≥3 consecutive lost pings.
    Loss { consecutive: u32 },
    /// 5s median RTT above max(3× baseline, baseline + 100ms).
    Slow { median_ms: f64, threshold_ms: f64 },
    /// No bytes received for more than 2× ServerAliveInterval + 5s.
    Stall { silent_for: Duration },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StormCulprit {
    Autossh {
        pid: u32,
    },
    /// A process other than the master with its own TCP connection to `<ip>:22`.
    NonMux {
        pid: u32,
        command: String,
    },
}

impl StormCulprit {
    pub fn pid(&self) -> u32 {
        match self {
            StormCulprit::Autossh { pid } | StormCulprit::NonMux { pid, .. } => *pid,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientRef {
    pub pid: u32,
    pub kind: ClientKind,
    pub remote_argv: Vec<String>,
}

/// Supervisor's account of a master exit, merged into `master down`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MasterExit {
    pub after_s: u64,
    pub rc: i32,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EventKind {
    /// First master seen since start.
    MasterUp {
        pid: u32,
    },
    /// A master seen again after DOWN, or the PID changed between two polls.
    MasterRestarted {
        pid: u32,
    },
    MasterDown {
        was_up: Option<Duration>,
        supervisor: Option<MasterExit>,
    },
    Degraded(DegradeReason),
    Recovered {
        degraded_for: Duration,
    },
    /// `count` grows in place while the 30s cooldown lasts (×N).
    RttSpike {
        rtt_ms: f64,
        threshold_ms: f64,
        count: u32,
    },
    StormStarted(StormCulprit),
    StormCleared {
        lasted: Duration,
    },
    SupervisorReconnecting {
        in_s: u64,
    },
    CircuitOpen {
        standing_down_s: Option<u64>,
    },
    CircuitHalfOpen,
    /// `attempts` grows in place while the loop continues.
    ReconnectLoop {
        attempts: u32,
        avg_secs: u64,
    },
    ReconnectLoopEnded {
        attempts: u32,
    },
    PathChanged {
        from: Option<PathKind>,
        to: Option<PathKind>,
    },
    ClientsChanged {
        added: Vec<ClientRef>,
        removed: Vec<ClientRef>,
    },
    ProbeFailed {
        probe: ProbeKind,
        reason: String,
    },
    ProbeRecovered {
        probe: ProbeKind,
    },
    SystemResumed {
        gap: Duration,
    },
}

impl EventKind {
    pub fn severity(&self) -> Severity {
        use EventKind::*;
        match self {
            MasterUp { .. }
            | Recovered { .. }
            | StormCleared { .. }
            | ReconnectLoopEnded { .. } => Severity::Green,
            MasterRestarted { .. }
            | Degraded(_)
            | RttSpike { .. }
            | CircuitHalfOpen
            | ReconnectLoop { .. }
            | ProbeFailed { .. }
            | ProbeRecovered { .. } => Severity::Yellow,
            MasterDown { .. } | StormStarted(_) | CircuitOpen { .. } => Severity::Red,
            SupervisorReconnecting { .. }
            | PathChanged { .. }
            | ClientsChanged { .. }
            | SystemResumed { .. } => Severity::Dim,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Monotonic per monitor; lets detectors amend an event they emitted (×N, merges).
    pub id: u64,
    pub at: Stamp,
    pub kind: EventKind,
}

impl Event {
    pub fn severity(&self) -> Severity {
        self.kind.severity()
    }
}

/// SPEC F5: 200 kept, plus the events fired during the current `apply`.
#[derive(Debug, Clone)]
pub struct EventLog {
    ring: Ring<Event>,
    next_id: u64,
    fresh: Vec<Event>,
}

pub const EVENTS_KEPT: usize = 200;

impl Default for EventLog {
    fn default() -> Self {
        Self::new()
    }
}

impl EventLog {
    pub fn new() -> Self {
        Self {
            ring: Ring::new(EVENTS_KEPT),
            next_id: 1,
            fresh: Vec::new(),
        }
    }

    pub fn push(&mut self, at: Stamp, kind: EventKind) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        let ev = Event { id, at, kind };
        self.fresh.push(ev.clone());
        self.ring.push(ev);
        id
    }

    /// Mutates an earlier event in place; `false` if it has already been evicted or cleared.
    pub fn amend(&mut self, id: u64, f: impl FnOnce(&mut EventKind)) -> bool {
        match self.ring.iter_mut().rev().find(|e| e.id == id) {
            Some(ev) => {
                f(&mut ev.kind);
                true
            }
            None => false,
        }
    }

    /// Events pushed since the last call (what one `Monitor::apply` fired).
    pub fn take_fresh(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.fresh)
    }

    pub fn newest_first(&self) -> impl Iterator<Item = &Event> {
        self.ring.iter().rev()
    }

    pub fn oldest_first(&self) -> impl Iterator<Item = &Event> {
        self.ring.iter()
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ring.is_empty()
    }

    /// `c` key: clears the on-screen log; ids keep counting so pending amendments become no-ops.
    pub fn clear(&mut self) {
        self.ring.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_keeps_200_newest_and_hands_out_fresh_events_once() {
        let mut log = EventLog::new();
        for i in 0..205u32 {
            log.push(Stamp::at_secs(u64::from(i)), EventKind::MasterUp { pid: i });
        }
        assert_eq!(log.len(), EVENTS_KEPT);
        assert_eq!(
            log.newest_first().next().unwrap().kind,
            EventKind::MasterUp { pid: 204 }
        );
        assert_eq!(
            log.oldest_first().next().unwrap().kind,
            EventKind::MasterUp { pid: 5 }
        );
        assert_eq!(log.take_fresh().len(), 205);
        assert!(log.take_fresh().is_empty());
    }

    #[test]
    fn amend_updates_in_place_and_fails_after_clear() {
        let mut log = EventLog::new();
        let id = log.push(
            Stamp::at_secs(1),
            EventKind::RttSpike {
                rtt_ms: 1.0,
                threshold_ms: 1.0,
                count: 1,
            },
        );
        assert!(log.amend(id, |k| {
            if let EventKind::RttSpike { count, .. } = k {
                *count += 1;
            }
        }));
        assert!(matches!(
            log.newest_first().next().unwrap().kind,
            EventKind::RttSpike { count: 2, .. }
        ));
        log.clear();
        assert!(log.is_empty());
        assert!(!log.amend(id, |_| {}));
        assert_eq!(
            log.push(Stamp::at_secs(2), EventKind::CircuitHalfOpen),
            id + 1,
            "ids keep counting"
        );
    }

    #[test]
    fn severities_follow_spec_table() {
        assert_eq!(
            EventKind::MasterDown {
                was_up: None,
                supervisor: None
            }
            .severity(),
            Severity::Red
        );
        assert_eq!(
            EventKind::MasterRestarted { pid: 1 }.severity(),
            Severity::Yellow
        );
        assert_eq!(
            EventKind::SupervisorReconnecting { in_s: 2 }.severity(),
            Severity::Dim
        );
        assert_eq!(
            EventKind::CircuitOpen {
                standing_down_s: None
            }
            .severity(),
            Severity::Red
        );
        assert_eq!(EventKind::CircuitHalfOpen.severity(), Severity::Yellow);
        assert_eq!(
            EventKind::ProbeRecovered {
                probe: ProbeKind::Path
            }
            .severity(),
            Severity::Yellow
        );
        assert_eq!(ProbeKind::ALL.len(), 8);
    }
}
