use super::event::{EventKind, EventLog, ProbeKind};
use crate::Stamp;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default)]
pub struct ProbeHealth {
    failed: BTreeMap<ProbeKind, String>,
    answered: BTreeSet<ProbeKind>,
}

impl ProbeHealth {
    pub fn ok(&mut self, probe: ProbeKind, now: Stamp, log: &mut EventLog) {
        self.answered.insert(probe);
        if self.failed.remove(&probe).is_some() {
            log.push(now, EventKind::ProbeRecovered { probe });
        }
    }

    pub fn failed(&mut self, probe: ProbeKind, reason: &str, now: Stamp, log: &mut EventLog) {
        if !self.failed.contains_key(&probe) {
            log.push(
                now,
                EventKind::ProbeFailed {
                    probe,
                    reason: reason.to_string(),
                },
            );
        }
        self.failed.insert(probe, reason.to_string());
    }

    pub fn is_failed(&self, probe: ProbeKind) -> bool {
        self.failed.contains_key(&probe)
    }

    /// Answered at least once and not failing now: its values are current.
    pub fn is_current(&self, probe: ProbeKind) -> bool {
        self.answered.contains(&probe) && !self.is_failed(probe)
    }

    pub fn reason(&self, probe: ProbeKind) -> Option<&str> {
        self.failed.get(&probe).map(String::as_str)
    }

    pub fn failed_probes(&self) -> impl Iterator<Item = ProbeKind> + '_ {
        self.failed.keys().copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fires_once_per_outage_and_once_per_recovery() {
        let mut p = ProbeHealth::default();
        let mut log = EventLog::new();
        assert!(!p.is_current(ProbeKind::Path), "never answered");
        p.ok(ProbeKind::Path, Stamp::at_secs(1), &mut log);
        assert!(log.take_fresh().is_empty(), "ok while ok: silent");
        assert!(p.is_current(ProbeKind::Path));
        p.failed(ProbeKind::Path, "exit 1", Stamp::at_secs(2), &mut log);
        p.failed(ProbeKind::Path, "exit 2", Stamp::at_secs(3), &mut log);
        let fresh = log.take_fresh();
        assert_eq!(fresh.len(), 1);
        assert_eq!(
            fresh[0].kind,
            EventKind::ProbeFailed {
                probe: ProbeKind::Path,
                reason: "exit 1".into()
            }
        );
        assert_eq!(
            p.reason(ProbeKind::Path),
            Some("exit 2"),
            "latest reason kept"
        );
        assert!(p.is_failed(ProbeKind::Path));
        assert_eq!(p.failed_probes().collect::<Vec<_>>(), vec![ProbeKind::Path]);
        p.ok(ProbeKind::Path, Stamp::at_secs(4), &mut log);
        assert_eq!(
            log.take_fresh()[0].kind,
            EventKind::ProbeRecovered {
                probe: ProbeKind::Path
            }
        );
        assert!(!p.is_failed(ProbeKind::Path));
    }
}
