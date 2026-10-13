use super::event::{ClientRef, EventKind, EventLog};
use crate::Stamp;
use crate::parse::{ClientKind, Process, TcpConn, ssh_processes};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Client {
    pub pid: u32,
    pub kind: ClientKind,
    pub remote_argv: Vec<String>,
    /// `ps` etime and the stamp it was sampled at.
    pub etime: Duration,
    pub seen: Stamp,
    /// Own TCP connection (not multiplexed through the master) → red `⚠`.
    pub direct: bool,
}

impl Client {
    pub fn age_at(&self, now: Stamp) -> Duration {
        self.etime
            .saturating_add(now.mono.saturating_sub(self.seen.mono))
    }

    pub fn as_ref(&self) -> ClientRef {
        ClientRef {
            pid: self.pid,
            kind: self.kind.clone(),
            remote_argv: self.remote_argv.clone(),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Clients {
    /// Sorted: `#N` by N, then `vscode`, then others by PID.
    pub list: Vec<Client>,
    own_tcp: Vec<u32>,
    /// Master included (CSV `ssh_procs`).
    pub ssh_process_count: usize,
    initialized: bool,
}

impl Clients {
    pub fn on_processes(
        &mut self,
        procs: &[Process],
        alias: &str,
        hostname: &str,
        master_pid: Option<u32>,
        now: Stamp,
        log: &mut EventLog,
    ) {
        let all = ssh_processes(procs, alias, hostname);
        self.ssh_process_count = all.len();
        let mut next: Vec<Client> = all
            .into_iter()
            .filter(|p| Some(p.pid) != master_pid)
            .map(|p| Client {
                pid: p.pid,
                direct: self.own_tcp.contains(&p.pid),
                kind: p.kind,
                remote_argv: p.invocation.remote_argv,
                etime: p.etime,
                seen: now,
            })
            .collect();
        next.sort_by_key(sort_key);
        let added: Vec<ClientRef> = next
            .iter()
            .filter(|n| !self.list.iter().any(|o| o.pid == n.pid))
            .map(Client::as_ref)
            .collect();
        let removed: Vec<ClientRef> = self
            .list
            .iter()
            .filter(|o| !next.iter().any(|n| n.pid == o.pid))
            .map(Client::as_ref)
            .collect();
        if !added.is_empty() || !removed.is_empty() {
            log.push(now, EventKind::ClientsChanged { added, removed });
        }
        self.list = next;
        self.initialized = true;
    }

    /// Without a master every connection is direct; a master missing from lsof keeps the flags.
    pub fn on_connections(&mut self, conns: &[TcpConn], master_pid: Option<u32>) {
        let master_dev = match master_pid {
            None => None,
            Some(m) => match conns.iter().find(|c| c.pid == m) {
                Some(c) => Some(c.device.as_str()),
                None => return,
            },
        };
        self.own_tcp = conns
            .iter()
            .filter(|c| Some(c.pid) != master_pid && master_dev.is_none_or(|d| c.device != d))
            .map(|c| c.pid)
            .collect();
        for c in &mut self.list {
            c.direct = self.own_tcp.contains(&c.pid);
        }
    }

    pub fn initialized(&self) -> bool {
        self.initialized
    }
}

fn sort_key(c: &Client) -> (u8, u32, u32) {
    match c.kind {
        ClientKind::DevSession(n) => (0, n, c.pid),
        ClientKind::VsCode => (1, 0, c.pid),
        ClientKind::Other => (2, 0, c.pid),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_ps;

    const NORMAL: &str = include_str!("../../tests/fixtures/ps_axo_normal.txt");
    const POST: &str = include_str!("../../tests/fixtures/ps_axo_postoutage.txt");
    const LSOF_POST: &str = include_str!("../../tests/fixtures/lsof_tcp_postoutage.txt");

    #[test]
    fn normal_listing_minus_master_is_two_panes_and_vscode_sorted() {
        let mut c = Clients::default();
        let mut log = EventLog::new();
        c.on_processes(
            &parse_ps(NORMAL).unwrap(),
            "fedora",
            "100.100.1.10",
            Some(34299),
            Stamp::at_secs(1),
            &mut log,
        );
        let kinds: Vec<ClientKind> = c.list.iter().map(|x| x.kind.clone()).collect();
        assert_eq!(
            kinds,
            vec![
                ClientKind::DevSession(1),
                ClientKind::DevSession(4),
                ClientKind::VsCode
            ]
        );
        assert_eq!(c.ssh_process_count, 4, "master counted in ssh_procs");
        let fresh = log.take_fresh();
        assert_eq!(fresh.len(), 1);
        let EventKind::ClientsChanged { added, removed } = &fresh[0].kind else {
            panic!()
        };
        assert_eq!((added.len(), removed.len()), (3, 0));
        assert_eq!(
            c.list[0].age_at(Stamp::at_secs(11)),
            Duration::from_secs(43 * 60 + 45 + 10)
        );
    }

    #[test]
    fn diff_emits_one_event_per_poll_only_when_something_changed() {
        let mut c = Clients::default();
        let mut log = EventLog::new();
        let procs = parse_ps(NORMAL).unwrap();
        c.on_processes(
            &procs,
            "fedora",
            "100.100.1.10",
            Some(34299),
            Stamp::at_secs(1),
            &mut log,
        );
        log.take_fresh();
        c.on_processes(
            &procs,
            "fedora",
            "100.100.1.10",
            Some(34299),
            Stamp::at_secs(3),
            &mut log,
        );
        assert!(log.take_fresh().is_empty());
        let fewer: Vec<Process> = procs.iter().filter(|p| p.pid != 34345).cloned().collect();
        c.on_processes(
            &fewer,
            "fedora",
            "100.100.1.10",
            Some(34299),
            Stamp::at_secs(5),
            &mut log,
        );
        let fresh = log.take_fresh();
        let EventKind::ClientsChanged { added, removed } = &fresh[0].kind else {
            panic!()
        };
        assert!(added.is_empty());
        assert_eq!(removed[0].kind, ClientKind::DevSession(4));
    }

    #[test]
    fn direct_flag_comes_from_lsof_devices() {
        let mut c = Clients::default();
        let mut log = EventLog::new();
        let conns = crate::parse::parse_lsof(0, LSOF_POST, "").unwrap();
        c.on_connections(&conns, Some(22489));
        c.on_processes(
            &parse_ps(POST).unwrap(),
            "fedora",
            "100.100.1.10",
            Some(22489),
            Stamp::at_secs(1),
            &mut log,
        );
        let direct: Vec<(u32, bool)> = c.list.iter().map(|x| (x.pid, x.direct)).collect();
        assert_eq!(
            direct,
            vec![(22340, false), (22475, true), (22508, true)],
            "pane shares the master socket; VS Code and the duplicate master are direct"
        );
        c.on_connections(&conns, None);
        assert!(c.list.iter().all(|x| x.direct));
        c.on_connections(&[], Some(22489));
        assert!(c.list.iter().all(|x| x.direct));
    }

    #[test]
    fn while_down_rogue_ssh_processes_are_listed() {
        let mut c = Clients::default();
        let mut log = EventLog::new();
        c.on_processes(
            &parse_ps(POST).unwrap(),
            "fedora",
            "100.100.1.10",
            None,
            Stamp::at_secs(1),
            &mut log,
        );
        assert_eq!(c.list.len(), 3, "nothing excluded without a master");
    }
}
