//! Storm: an `autossh` process, or a non-master process with its own TCP connection to `<ip>:22`.

use super::event::{EventKind, EventLog, StormCulprit};
use crate::Stamp;
use crate::parse::{Process, TcpConn};

#[derive(Debug, Clone, Default)]
pub struct Storm {
    pub culprits: Vec<StormCulprit>,
    pub since: Option<Stamp>,
}

impl Storm {
    pub fn active(&self) -> bool {
        self.since.is_some()
    }

    /// Rule (a) from the process listing.
    pub fn on_processes(&mut self, procs: &[Process], now: Stamp, log: &mut EventLog) {
        let found: Vec<StormCulprit> = procs
            .iter()
            .filter(|p| {
                p.command
                    .split_whitespace()
                    .next()
                    .and_then(|a| a.rsplit('/').next())
                    == Some("autossh")
            })
            .map(|p| StormCulprit::Autossh { pid: p.pid })
            .collect();
        self.reconcile(
            found,
            |c| matches!(c, StormCulprit::Autossh { .. }),
            now,
            log,
        );
    }

    /// Rule (b). Skipped without a master or when lsof does not show it (inconclusive).
    pub fn on_connections(
        &mut self,
        conns: &[TcpConn],
        master_pid: Option<u32>,
        now: Stamp,
        log: &mut EventLog,
    ) {
        let Some(master) = master_pid else {
            return;
        };
        let Some(master_dev) = conns
            .iter()
            .find(|c| c.pid == master)
            .map(|c| c.device.as_str())
        else {
            return;
        };
        let mut found: Vec<StormCulprit> = Vec::new();
        for c in conns
            .iter()
            .filter(|c| c.pid != master && c.device != master_dev)
        {
            if !found.iter().any(|x| x.pid() == c.pid) {
                found.push(StormCulprit::NonMux {
                    pid: c.pid,
                    command: c.command.clone(),
                });
            }
        }
        self.reconcile(
            found,
            |c| matches!(c, StormCulprit::NonMux { .. }),
            now,
            log,
        );
    }

    /// Emits onset per new PID and `cleared` when the whole set becomes empty.
    fn reconcile(
        &mut self,
        found: Vec<StormCulprit>,
        same_rule: impl Fn(&StormCulprit) -> bool,
        now: Stamp,
        log: &mut EventLog,
    ) {
        let mut next: Vec<StormCulprit> = self
            .culprits
            .iter()
            .filter(|c| !same_rule(c))
            .cloned()
            .collect();
        for c in found {
            if !self.culprits.iter().any(|old| old.pid() == c.pid()) {
                if self.since.is_none() {
                    self.since = Some(now);
                }
                log.push(now, EventKind::StormStarted(c.clone()));
            }
            next.push(c);
        }
        self.culprits = next;
        if self.culprits.is_empty()
            && let Some(since) = self.since.take()
        {
            log.push(
                now,
                EventKind::StormCleared {
                    lasted: now.mono.saturating_sub(since.mono),
                },
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn conn(pid: u32, device: &str) -> TcpConn {
        TcpConn {
            command: "ssh".into(),
            pid,
            fd: "3u".into(),
            device: device.into(),
            local: "100.100.1.20:1".into(),
            remote: "100.100.1.10:22".into(),
            state: Some("ESTABLISHED".into()),
        }
    }

    fn proc_(pid: u32, command: &str) -> Process {
        Process {
            pid,
            ppid: 1,
            etime: Duration::ZERO,
            command: command.into(),
        }
    }

    #[test]
    fn non_mux_connection_is_a_culprit_once_and_clears_when_gone() {
        let mut s = Storm::default();
        let mut log = EventLog::new();
        let master = Some(10);
        s.on_connections(
            &[conn(10, "0xa"), conn(11, "0xa")],
            master,
            Stamp::at_secs(1),
            &mut log,
        );
        assert!(
            !s.active(),
            "pid 11 shares the master socket (pane that became master)"
        );
        s.on_connections(
            &[conn(10, "0xa"), conn(12, "0xb")],
            master,
            Stamp::at_secs(2),
            &mut log,
        );
        s.on_connections(
            &[conn(10, "0xa"), conn(12, "0xb")],
            master,
            Stamp::at_secs(3),
            &mut log,
        );
        let fresh = log.take_fresh();
        assert_eq!(fresh.len(), 1, "once per culprit");
        assert_eq!(
            fresh[0].kind,
            EventKind::StormStarted(StormCulprit::NonMux {
                pid: 12,
                command: "ssh".into()
            })
        );
        assert!(s.active());
        s.on_connections(&[conn(10, "0xa")], master, Stamp::at_secs(9), &mut log);
        assert_eq!(
            log.take_fresh()[0].kind,
            EventKind::StormCleared {
                lasted: Duration::from_secs(7)
            }
        );
        assert!(!s.active());
    }

    #[test]
    fn no_master_means_no_non_mux_storm_and_missing_master_row_is_skipped() {
        let mut s = Storm::default();
        let mut log = EventLog::new();
        s.on_connections(&[conn(5, "0xa")], None, Stamp::at_secs(1), &mut log);
        assert!(log.take_fresh().is_empty());
        s.on_connections(&[conn(5, "0xa")], Some(10), Stamp::at_secs(2), &mut log);
        assert!(
            log.take_fresh().is_empty(),
            "master not in lsof output: inconclusive"
        );
        assert!(!s.active());
    }

    #[test]
    fn autossh_is_rule_a_and_rules_are_independent() {
        let mut s = Storm::default();
        let mut log = EventLog::new();
        s.on_processes(
            &[
                proc_(1, "/usr/local/bin/autossh -M 0 fedora"),
                proc_(2, "ssh fedora"),
            ],
            Stamp::at_secs(1),
            &mut log,
        );
        assert_eq!(
            log.take_fresh()[0].kind,
            EventKind::StormStarted(StormCulprit::Autossh { pid: 1 })
        );
        s.on_connections(
            &[conn(10, "0xa"), conn(12, "0xb")],
            Some(10),
            Stamp::at_secs(2),
            &mut log,
        );
        assert_eq!(s.culprits.len(), 2);
        log.take_fresh();
        s.on_processes(&[proc_(2, "ssh fedora")], Stamp::at_secs(3), &mut log);
        assert_eq!(
            s.culprits,
            vec![StormCulprit::NonMux {
                pid: 12,
                command: "ssh".into()
            }]
        );
        assert!(log.take_fresh().is_empty(), "still active via rule (b)");
        s.on_connections(&[conn(10, "0xa")], Some(10), Stamp::at_secs(4), &mut log);
        assert!(matches!(
            log.take_fresh()[0].kind,
            EventKind::StormCleared { .. }
        ));
    }
}
