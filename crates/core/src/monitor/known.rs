//! SPEC F9: a value whose probe has not answered, or last failed, is unknown, never stale.

use super::{Breaker, Clients, Health, Monitor, ProbeKind, State};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Badge {
    Storm,
    Breaker,
    Retry,
}

impl Badge {
    pub fn name(self) -> &'static str {
        match self {
            Badge::Storm => "STORM",
            Badge::Breaker => "BREAKER",
            Badge::Retry => "RETRY",
        }
    }
}

impl Monitor {
    /// `None` before the first answer or while failing: `n/a`, never DOWN by default.
    pub fn known_state(&self) -> Option<State> {
        (self.health.initialized() && !self.probes.is_failed(ProbeKind::Status))
            .then(|| self.state())
    }

    /// While the state is known.
    pub fn known_health(&self) -> Option<&Health> {
        self.known_state().map(|_| &self.health)
    }

    pub fn known_master_pid(&self) -> Option<u32> {
        self.known_health().and_then(Health::master_pid)
    }

    /// Current only when the last `ps` and `lsof` calls both succeeded.
    pub fn known_clients(&self) -> Option<&Clients> {
        (self.probes.is_current(ProbeKind::Clients) && self.probes.is_current(ProbeKind::Rogues))
            .then_some(&self.clients)
    }

    /// Storm verdict, from the same two probes as the client list.
    pub fn known_storm(&self) -> Option<bool> {
        self.known_clients().map(|_| self.storm.active())
    }

    /// `ssh_procs` comes from `ps` alone.
    pub fn known_ssh_procs(&self) -> Option<usize> {
        self.probes
            .is_current(ProbeKind::Clients)
            .then_some(self.clients.ssh_process_count)
    }

    /// STORM needs the process/connection probes; BREAKER and RETRY the supervisor log.
    pub fn badges(&self) -> Vec<Badge> {
        let log = self.supervisor.available;
        [
            (self.known_storm() == Some(true), Badge::Storm),
            (
                log && self.supervisor.breaker == Breaker::Open,
                Badge::Breaker,
            ),
            (log && self.supervisor.loop_active(), Badge::Retry),
        ]
        .into_iter()
        .filter_map(|(on, badge)| on.then_some(badge))
        .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stamp;
    use crate::monitor::{Input, MonitorConfig};
    use crate::parse::{MasterStatus, Process, parse_supervisor_log};
    use std::time::Duration;

    fn monitor() -> Monitor {
        Monitor::new(MonitorConfig {
            host_alias: "fedora".into(),
            hostname: "100.100.1.10".into(),
            server_alive_interval: Some(15),
        })
    }

    fn fail(m: &mut Monitor, probe: ProbeKind, at: u64) {
        m.apply(
            Input::Failed {
                probe,
                reason: "exit 1".into(),
            },
            Stamp::at_secs(at),
        );
    }

    #[test]
    fn state_and_master_are_unknown_before_the_first_answer_and_while_status_fails() {
        let mut m = monitor();
        assert_eq!(m.known_state(), None);
        m.apply(
            Input::Status(MasterStatus::Running { pid: 9 }),
            Stamp::at_secs(0),
        );
        assert_eq!(m.known_state(), Some(State::Up));
        assert_eq!(m.known_master_pid(), Some(9));
        fail(&mut m, ProbeKind::Status, 1);
        assert_eq!(m.known_state(), None);
        assert_eq!(m.known_master_pid(), None, "not the last pid");
        assert!(m.known_health().is_none());
        assert_eq!(
            m.health.master_pid(),
            Some(9),
            "the model keeps it for recovery"
        );
    }

    #[test]
    fn clients_and_storm_are_unknown_until_ps_and_lsof_answer_and_while_either_fails() {
        let mut m = monitor();
        assert!(m.known_clients().is_none());
        assert_eq!(m.known_storm(), None);
        assert_eq!(m.known_ssh_procs(), None);
        m.apply(Input::Processes(vec![]), Stamp::at_secs(0));
        assert!(m.known_clients().is_none(), "lsof has not answered yet");
        assert_eq!(
            m.known_ssh_procs(),
            Some(0),
            "ps alone is enough for ssh_procs"
        );
        m.apply(Input::Connections(vec![]), Stamp::at_secs(0));
        assert_eq!(m.known_clients().map(|c| c.list.len()), Some(0));
        assert_eq!(m.known_storm(), Some(false));
        fail(&mut m, ProbeKind::Rogues, 1);
        assert!(m.known_clients().is_none());
        assert_eq!(m.known_storm(), None);
        fail(&mut m, ProbeKind::Clients, 1);
        assert_eq!(m.known_ssh_procs(), None);
    }

    #[test]
    fn badges_need_their_probe_answering() {
        let mut m = monitor();
        m.apply(
            Input::Processes(vec![Process {
                pid: 1,
                ppid: 1,
                etime: Duration::from_secs(5),
                command: "/usr/local/bin/autossh -M 0 fedora".into(),
            }]),
            Stamp::at_secs(0),
        );
        assert!(m.badges().is_empty(), "storm needs lsof too");
        m.apply(Input::Connections(vec![]), Stamp::at_secs(0));
        m.apply(
            Input::Supervisor(parse_supervisor_log(
                "2026-10-11 03:01:11  CIRCUIT OPEN — 8 rapid failures; standing down for 300s\n",
            )),
            Stamp::at_secs(0),
        );
        assert_eq!(m.badges(), vec![Badge::Storm, Badge::Breaker]);
        fail(&mut m, ProbeKind::Supervisor, 1);
        assert_eq!(m.badges(), vec![Badge::Storm]);
        fail(&mut m, ProbeKind::Rogues, 1);
        assert!(m.badges().is_empty());
        assert_eq!(Badge::Retry.name(), "RETRY");
    }
}
