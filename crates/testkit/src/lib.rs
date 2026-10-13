//! Dev-only test kit (decision A8): a `Scenario` says what every probe answers at a given moment.
//! L1 tests query it directly; L2 tests replay it as a JSONL tape through `fake-cmd`.

pub use ssh_monitor_core::tape::TapeEntry;
use std::time::Duration;

/// What a fake command answers. `exit < 0` = the command hangs (the runner's timeout fires).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    pub delay_ms: u64,
}

impl Response {
    pub fn ok(stdout: impl Into<String>) -> Self {
        Self {
            exit: 0,
            stdout: stdout.into(),
            stderr: String::new(),
            delay_ms: 0,
        }
    }

    pub fn fail(exit: i32, stderr: impl Into<String>) -> Self {
        Self {
            exit,
            stdout: String::new(),
            stderr: stderr.into(),
            delay_ms: 0,
        }
    }

    pub fn hang() -> Self {
        Self {
            exit: -1,
            stdout: String::new(),
            stderr: String::new(),
            delay_ms: 60_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Ping {
    Reply(f64),
    Lost,
    /// Never answers: the runner times it out.
    Hang,
    /// `ping` cannot run (unknown host).
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Tailscale {
    DirectLan,
    DirectWan,
    Derp(String),
    PeerAbsent,
    Fail,
}

/// One `ssh` process targeting the host (not the master).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientSpec {
    pub pid: u32,
    pub ppid: u32,
    pub command: String,
    /// Holds its own TCP connection (non-mux) instead of sharing the master socket.
    pub own_tcp: bool,
}

impl ClientSpec {
    pub fn dev_session(pid: u32, n: u32) -> Self {
        Self {
            pid,
            ppid: 1,
            command: format!("ssh -t fedora -- /home/user/.local/bin/dev-session {n}"),
            own_tcp: false,
        }
    }

    pub fn vscode(pid: u32, own_tcp: bool) -> Self {
        Self {
            pid,
            ppid: 1,
            command: "ssh -v -T -D 52124 -o ConnectTimeout=15 fedora".into(),
            own_tcp,
        }
    }

    pub fn plain(pid: u32, remote: &str, own_tcp: bool) -> Self {
        Self {
            pid,
            ppid: 1,
            command: format!("ssh fedora {remote}").trim_end().to_string(),
            own_tcp,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Phase {
    pub master: Option<u32>,
    pub ping: Ping,
    pub rx_bps: u64,
    pub tx_bps: u64,
    /// `false` → nettop exits 1.
    pub nettop_ok: bool,
    pub clients: Vec<ClientSpec>,
    pub autossh: Option<u32>,
    /// `None` → the log file does not exist (tail exits 1).
    pub supervisor_log: Option<String>,
    pub tailscale: Tailscale,
    pub status_hang: bool,
    /// `false` → the `ps -axo` listing exits 1.
    pub ps_ok: bool,
}

impl Default for Phase {
    fn default() -> Self {
        Self {
            master: Some(1000),
            ping: Ping::Reply(10.0),
            rx_bps: 1000,
            tx_bps: 500,
            nettop_ok: true,
            clients: Vec::new(),
            autossh: None,
            supervisor_log: Some(String::new()),
            tailscale: Tailscale::DirectLan,
            status_hang: false,
            ps_ok: true,
        }
    }
}

impl Phase {
    pub fn down() -> Self {
        Self {
            master: None,
            ..Self::default()
        }
    }

    pub fn with_master(mut self, pid: Option<u32>) -> Self {
        self.master = pid;
        self
    }

    pub fn with_ping(mut self, ping: Ping) -> Self {
        self.ping = ping;
        self
    }

    pub fn with_rates(mut self, rx_bps: u64, tx_bps: u64) -> Self {
        self.rx_bps = rx_bps;
        self.tx_bps = tx_bps;
        self
    }

    pub fn with_clients(mut self, clients: Vec<ClientSpec>) -> Self {
        self.clients = clients;
        self
    }

    pub fn with_supervisor_log(mut self, log: Option<&str>) -> Self {
        self.supervisor_log = log.map(str::to_string);
        self
    }

    pub fn with_tailscale(mut self, ts: Tailscale) -> Self {
        self.tailscale = ts;
        self
    }

    pub fn with_autossh(mut self, pid: Option<u32>) -> Self {
        self.autossh = pid;
        self
    }

    pub fn with_nettop_ok(mut self, ok: bool) -> Self {
        self.nettop_ok = ok;
        self
    }

    pub fn with_status_hang(mut self, hang: bool) -> Self {
        self.status_hang = hang;
        self
    }

    pub fn with_ps_ok(mut self, ok: bool) -> Self {
        self.ps_ok = ok;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub alias: String,
    pub hostname: String,
    pub control_path: String,
    pub server_alive_interval: u32,
    /// Where the app expects the supervisor log (`$HOME/.local/state/ssh-master-<alias>.log`).
    pub supervisor_log_path: String,
    /// Sorted by start time; the first starts at 0.
    phases: Vec<(Duration, Phase)>,
}

impl Default for Scenario {
    fn default() -> Self {
        Self::new()
    }
}

impl Scenario {
    pub fn new() -> Self {
        Self {
            alias: "fedora".into(),
            hostname: "100.100.1.10".into(),
            control_path: "/Users/user/.ssh/cm-user@100.100.1.10:22".into(),
            server_alive_interval: 15,
            supervisor_log_path: "/Users/user/.local/state/ssh-master-fedora.log".into(),
            phases: vec![(Duration::ZERO, Phase::default())],
        }
    }

    pub fn starting_with(mut self, phase: Phase) -> Self {
        self.phases[0].1 = phase;
        self
    }

    pub fn then_at(mut self, from: Duration, phase: Phase) -> Self {
        self.phases.push((from, phase));
        self.phases.sort_by_key(|(t, _)| *t);
        self
    }

    pub fn with_control_path(mut self, cp: Option<&str>) -> Self {
        self.control_path = cp.unwrap_or("none").to_string();
        self
    }

    pub fn with_home(mut self, home: &str) -> Self {
        self.supervisor_log_path = format!("{home}/.local/state/ssh-master-{}.log", self.alias);
        self
    }

    pub fn phase_at(&self, now: Duration) -> &Phase {
        let idx = self
            .phases
            .iter()
            .rposition(|(t, _)| *t <= now)
            .unwrap_or(0);
        &self.phases[idx].1
    }

    /// Cumulative bytes (in, out) at `now`: rates integrated over the phases so far.
    pub fn counters_at(&self, now: Duration) -> (u64, u64) {
        let mut acc = (0u64, 0u64);
        for (i, (start, phase)) in self.phases.iter().enumerate() {
            if *start >= now {
                break;
            }
            let end = self.phases.get(i + 1).map_or(now, |(t, _)| (*t).min(now));
            let secs = end.saturating_sub(*start).as_secs_f64();
            acc.0 += (phase.rx_bps as f64 * secs) as u64;
            acc.1 += (phase.tx_bps as f64 * secs) as u64;
        }
        acc
    }

    /// When the current master PID first appeared (for `ps -o etime`).
    fn master_since(&self, now: Duration) -> Duration {
        let pid = self.phase_at(now).master;
        let mut since = Duration::ZERO;
        for (start, phase) in &self.phases {
            if *start > now {
                break;
            }
            if phase.master != pid {
                since = *start;
            }
        }
        // `since` is the start of the last phase with a different master; the current master
        // began at the next phase boundary after it.
        self.phases
            .iter()
            .map(|(t, _)| *t)
            .find(|t| *t >= since && self.phase_at(*t).master == pid)
            .unwrap_or(Duration::ZERO)
    }

    pub fn respond(&self, argv: &[String], now: Duration) -> Response {
        let phase = self.phase_at(now);
        let prog = argv
            .first()
            .map(|a| a.rsplit('/').next().unwrap_or(a))
            .unwrap_or("");
        let args: Vec<&str> = argv.iter().skip(1).map(String::as_str).collect();
        match prog {
            "ssh" => self.ssh(&args, phase),
            "nettop" => {
                if !phase.nettop_ok {
                    return Response::fail(1, "nettop: fake failure\n");
                }
                let pid = args
                    .iter()
                    .position(|a| *a == "-p")
                    .and_then(|i| args.get(i + 1));
                let header = ",bytes_in,bytes_out,\n";
                match (pid, phase.master) {
                    (Some(p), Some(m)) if p.parse::<u32>().ok() == Some(m) => {
                        let (rx, tx) = self.counters_at(now);
                        Response::ok(format!("{header}ssh.{m},{rx},{tx},\n"))
                    }
                    _ => Response::ok(header),
                }
            }
            "ping" => match &phase.ping {
                Ping::Reply(ms) => Response::ok(format!(
                    "PING {h} ({h}): 56 data bytes\n64 bytes from {h}: icmp_seq=0 ttl=64 time={ms:.3} ms\n\n--- {h} ping statistics ---\n1 packets transmitted, 1 packets received, 0.0% packet loss\n",
                    h = self.hostname
                )),
                // A real `ping -c 1 -t 1` gives up after ~1s (ARCHITECTURE probe table).
                Ping::Lost => Response {
                    exit: 2,
                    stdout: format!(
                        "PING {h} ({h}): 56 data bytes\n\n--- {h} ping statistics ---\n1 packets transmitted, 0 packets received, 100.0% packet loss\n",
                        h = self.hostname
                    ),
                    stderr: String::new(),
                    delay_ms: 1000,
                },
                Ping::Hang => Response::hang(),
                Ping::Fail => Response::fail(68, "ping: cannot resolve host: Unknown host\n"),
            },
            "ps" => {
                if args.first() == Some(&"-o") {
                    let pid = args.get(3).and_then(|p| p.parse::<u32>().ok());
                    return match (pid, phase.master) {
                        (Some(p), Some(m)) if p == m => {
                            Response::ok(format!("{}\n", etime(now - self.master_since(now))))
                        }
                        _ => Response::fail(1, "ps: process id too large: 99999999\n"),
                    };
                }
                if !phase.ps_ok {
                    return Response::fail(1, "ps: fake failure\n");
                }
                Response::ok(self.ps_listing(phase, now))
            }
            "lsof" => {
                let rows = self.lsof_rows(phase);
                if rows.is_empty() {
                    Response::fail(1, "")
                } else {
                    Response::ok(format!(
                        "COMMAND   PID USER   FD   TYPE             DEVICE SIZE/OFF NODE NAME\n{}",
                        rows.join("")
                    ))
                }
            }
            "tail" => match &phase.supervisor_log {
                Some(log) => {
                    let lines: Vec<&str> = log.lines().collect();
                    let from = lines.len().saturating_sub(50);
                    let mut out = lines[from..].join("\n");
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    Response::ok(out)
                }
                None => Response::fail(
                    1,
                    format!(
                        "tail: {}: No such file or directory\n",
                        self.supervisor_log_path
                    ),
                ),
            },
            "tailscale" => match &phase.tailscale {
                Tailscale::DirectLan => {
                    Response::ok(self.tailscale_json("192.168.1.192:41641", "par"))
                }
                Tailscale::DirectWan => {
                    Response::ok(self.tailscale_json("203.0.113.46:38205", "par"))
                }
                Tailscale::Derp(region) => Response::ok(self.tailscale_json("", region)),
                Tailscale::PeerAbsent => Response::ok("{\"Peer\":{}}\n"),
                Tailscale::Fail => Response::fail(1, "tailscale: not running\n"),
            },
            _ => Response::fail(127, format!("fake: unknown program {prog}\n")),
        }
    }

    fn ssh(&self, args: &[&str], phase: &Phase) -> Response {
        match args {
            ["-G", alias] if *alias == self.alias => Response::ok(format!(
                "host {alias}\nuser user\nhostname {h}\nport 22\ncontrolmaster auto\nserveraliveinterval {sai}\ncontrolpath {cp}\n",
                h = self.hostname,
                sai = self.server_alive_interval,
                cp = self.control_path
            )),
            ["-G", alias] => Response::ok(format!(
                "host {alias}\nhostname {alias}\nport 22\ncontrolpath none\n"
            )),
            ["-O", "check", _] => {
                if phase.status_hang {
                    return Response::hang();
                }
                match phase.master {
                    Some(pid) => Response::fail(0, format!("Master running (pid={pid})\r\n")),
                    None => Response::fail(
                        255,
                        format!(
                            "Control socket connect({}): No such file or directory\r\n",
                            self.control_path
                        ),
                    ),
                }
            }
            _ => Response::fail(
                1,
                "fake ssh: refused (only -G / -O check are ever expected)\n",
            ),
        }
    }

    fn ps_listing(&self, phase: &Phase, now: Duration) -> String {
        let mut out = String::new();
        out.push_str("    1     0 02-00:48:27 /sbin/launchd\n");
        out.push_str("  538     1 02-00:47:22 /usr/libexec/logd\n");
        out.push_str(" 2775     1 02-00:37:47 bash /Users/user/.local/bin/ssh-master fedora\n");
        if let Some(pid) = phase.master {
            out.push_str(&format!(
                "{pid:>5}  2775 {:>11} ssh -N -o ControlMaster=yes -o ControlPersist=no -o BatchMode=yes {}\n",
                etime(now - self.master_since(now)),
                self.alias
            ));
        }
        for c in &phase.clients {
            out.push_str(&format!(
                "{:>5} {:>5} {:>11} {}\n",
                c.pid,
                c.ppid,
                etime(now),
                c.command
            ));
        }
        if let Some(pid) = phase.autossh {
            out.push_str(&format!(
                "{pid:>5}     1 {:>11} autossh -M 0 -N {}\n",
                etime(now),
                self.alias
            ));
        }
        out
    }

    fn lsof_rows(&self, phase: &Phase) -> Vec<String> {
        let row = |pid: u32, dev: &str, port: u32| {
            format!(
                "ssh     {pid:>5} user    3u  IPv4 {dev}      0t0  TCP 100.100.1.20:{port}->{}:22 (ESTABLISHED)\n",
                self.hostname
            )
        };
        let mut rows = Vec::new();
        if let Some(m) = phase.master {
            rows.push(row(m, "0x00000000000aaaa0", 52103));
        }
        for c in &phase.clients {
            match (c.own_tcp, phase.master) {
                (true, _) => rows.push(row(
                    c.pid,
                    &format!("0x{:016x}", u64::from(c.pid)),
                    60000 + c.pid % 1000,
                )),
                (false, Some(_)) => rows.push(row(c.pid, "0x00000000000aaaa0", 52103)),
                (false, None) => {}
            }
        }
        rows
    }

    fn tailscale_json(&self, cur_addr: &str, relay: &str) -> String {
        format!(
            r#"{{"Version":"1.104.1","BackendState":"Running","Self":{{"HostName":"laptop","TailscaleIPs":["100.100.1.20"],"Online":true}},"Peer":{{"nodekey:0":{{"HostName":"deb","DNSName":"deb.tailnet.example.","OS":"linux","TailscaleIPs":["{h}","fd7a:115c:a1e0::d139:476f"],"Online":true,"Active":true,"Relay":"{relay}","PeerRelay":"","CurAddr":"{cur_addr}","RxBytes":1,"TxBytes":1}}}}}}
"#,
            h = self.hostname
        )
    }

    /// Every probe command the app would run at `now` (same table as `app::probes::argv`).
    pub fn commands_at(&self, now: Duration) -> Vec<Vec<String>> {
        let phase = self.phase_at(now);
        let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        let mut out = vec![
            s(&["ssh", "-O", "check", &self.alias]),
            s(&["ping", "-c", "1", "-t", "1", &self.hostname]),
            s(&["ps", "-axo", "pid=,ppid=,etime=,command="]),
            s(&["lsof", "-n", "-P", &format!("-iTCP@{}:22", self.hostname)]),
            s(&["tailscale", "status", "--json"]),
            s(&["tail", "-n", "50", &self.supervisor_log_path]),
        ];
        if let Some(pid) = phase.master {
            let pid = pid.to_string();
            out.push(s(&[
                "nettop",
                "-p",
                &pid,
                "-P",
                "-L",
                "1",
                "-x",
                "-J",
                "bytes_in,bytes_out",
            ]));
            out.push(s(&["ps", "-o", "etime=", "-p", &pid]));
        }
        out
    }

    /// One entry per command per `interval` from 0 to `until`, plus `ssh -G` at the start.
    pub fn to_tape(&self, until: Duration, interval: Duration) -> Vec<TapeEntry> {
        let mut entries = Vec::new();
        let g = vec!["ssh".to_string(), "-G".to_string(), self.alias.clone()];
        entries.push(self.entry(&g, Duration::ZERO));
        let mut t = Duration::ZERO;
        while t <= until {
            for argv in self.commands_at(t) {
                entries.push(self.entry(&argv, t));
            }
            t += interval;
        }
        entries
    }

    fn entry(&self, argv: &[String], at: Duration) -> TapeEntry {
        let r = self.respond(argv, at);
        TapeEntry {
            at_ms: at.as_millis() as u64,
            cmd: argv.to_vec(),
            exit: r.exit,
            stdout: r.stdout,
            stderr: r.stderr,
            delay_ms: r.delay_ms,
        }
    }
}

/// `ps` elapsed-time format.
pub fn etime(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 86_400 {
        format!(
            "{}-{:02}:{:02}:{:02}",
            s / 86_400,
            (s % 86_400) / 3600,
            (s % 3600) / 60,
            s % 60
        )
    } else if s >= 3600 {
        format!("{:02}:{:02}:{:02}", s / 3600, (s % 3600) / 60, s % 60)
    } else {
        format!("{:02}:{:02}", s / 60, s % 60)
    }
}

/// Supervisor log for a reconnect loop of `attempts` × `each` seconds from a fixed timestamp.
pub fn reconnect_loop_log(attempts: u32, each: u64) -> String {
    let mut out = String::new();
    let mut t = 0u64;
    for _ in 0..attempts {
        out.push_str(&format!(
            "2026-10-09 16:{:02}:{:02}  opening master connection\n",
            (t / 60) % 60,
            t % 60
        ));
        t += each;
        out.push_str(&format!(
            "2026-10-09 16:{:02}:{:02}  master ended after {each}s (rc=255) — healthy run, backoff reset\n",
            (t / 60) % 60,
            t % 60
        ));
        out.push_str(&format!(
            "2026-10-09 16:{:02}:{:02}  reconnecting in 2s\n",
            (t / 60) % 60,
            t % 60
        ));
        t += 2;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_monitor_core::parse::{
        MasterStatus, PingResult, parse_lsof, parse_nettop, parse_ping, parse_ps, parse_ssh_check,
        parse_ssh_config, parse_tailscale_status,
    };

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn default_scenario_answers_every_probe_parseably() {
        let s = Scenario::new();
        let t = Duration::from_secs(5);
        let cfg = parse_ssh_config(&s.respond(&argv(&["ssh", "-G", "fedora"]), t).stdout).unwrap();
        assert_eq!(cfg.hostname, "100.100.1.10");
        assert_eq!(cfg.server_alive_interval, Some(15));
        let r = s.respond(&argv(&["ssh", "-O", "check", "fedora"]), t);
        assert_eq!(
            parse_ssh_check(r.exit, &r.stderr).unwrap(),
            MasterStatus::Running { pid: 1000 }
        );
        let r = s.respond(
            &argv(&[
                "nettop",
                "-p",
                "1000",
                "-P",
                "-L",
                "1",
                "-x",
                "-J",
                "bytes_in,bytes_out",
            ]),
            t,
        );
        let c = parse_nettop(&r.stdout).unwrap().unwrap();
        assert_eq!((c.bytes_in, c.bytes_out), (5000, 2500));
        let r = s.respond(&argv(&["ping", "-c", "1", "-t", "1", "100.100.1.10"]), t);
        assert_eq!(
            parse_ping(r.exit, &r.stdout).unwrap(),
            PingResult::Reply { rtt_ms: 10.0 }
        );
        let r = s.respond(&argv(&["ps", "-o", "etime=", "-p", "1000"]), t);
        assert_eq!(r.stdout.trim(), "00:05");
        let r = s.respond(&argv(&["ps", "-axo", "pid=,ppid=,etime=,command="]), t);
        assert_eq!(parse_ps(&r.stdout).unwrap().len(), 4);
        let r = s.respond(&argv(&["lsof", "-n", "-P", "-iTCP@100.100.1.10:22"]), t);
        assert_eq!(parse_lsof(r.exit, &r.stdout, &r.stderr).unwrap().len(), 1);
        let r = s.respond(&argv(&["tail", "-n", "50", "/x"]), t);
        assert_eq!((r.exit, r.stdout.as_str()), (0, ""));
        let r = s.respond(&argv(&["tailscale", "status", "--json"]), t);
        let p = parse_tailscale_status(&r.stdout, "100.100.1.10")
            .unwrap()
            .unwrap();
        assert!(p.kind.is_direct());
        assert_eq!(s.respond(&argv(&["nope"]), t).exit, 127);
        assert_eq!(
            s.respond(&argv(&["ssh", "fedora"]), t).exit,
            1,
            "never a real session"
        );
    }

    #[test]
    fn phases_switch_the_world_and_counters_integrate() {
        let s = Scenario::new()
            .then_at(Duration::from_secs(10), Phase::down())
            .then_at(
                Duration::from_secs(20),
                Phase::default().with_master(Some(2000)).with_rates(10, 0),
            );
        assert_eq!(s.phase_at(Duration::from_secs(9)).master, Some(1000));
        assert_eq!(s.phase_at(Duration::from_secs(10)).master, None);
        assert_eq!(s.phase_at(Duration::from_secs(25)).master, Some(2000));
        assert_eq!(s.counters_at(Duration::from_secs(10)), (10_000, 5000));
        assert_eq!(
            s.counters_at(Duration::from_secs(25)),
            (10_000 + 10_000 + 50, 10_000)
        );
        let r = s.respond(
            &argv(&["ssh", "-O", "check", "fedora"]),
            Duration::from_secs(12),
        );
        assert_eq!(r.exit, 255);
        let r = s.respond(
            &argv(&[
                "nettop",
                "-p",
                "1000",
                "-P",
                "-L",
                "1",
                "-x",
                "-J",
                "bytes_in,bytes_out",
            ]),
            Duration::from_secs(12),
        );
        assert_eq!(
            parse_nettop(&r.stdout).unwrap(),
            None,
            "dead pid: header only"
        );
        let r = s.respond(
            &argv(&["ps", "-o", "etime=", "-p", "2000"]),
            Duration::from_secs(25),
        );
        assert_eq!(r.stdout.trim(), "00:05", "master 2000 started at t=20");
        let r = s.respond(
            &argv(&["ps", "-o", "etime=", "-p", "1000"]),
            Duration::from_secs(25),
        );
        assert_eq!(r.exit, 1);
    }

    #[test]
    fn clients_show_up_in_ps_and_lsof() {
        let s = Scenario::new().starting_with(Phase::default().with_clients(vec![
            ClientSpec::dev_session(2001, 1),
            ClientSpec::vscode(2002, true),
            ClientSpec::plain(2003, "scp -t /tmp", false),
        ]));
        let t = Duration::from_secs(1);
        let procs = parse_ps(
            &s.respond(&argv(&["ps", "-axo", "pid=,ppid=,etime=,command="]), t)
                .stdout,
        )
        .unwrap();
        let ssh = ssh_monitor_core::parse::ssh_processes(&procs, "fedora", "100.100.1.10");
        assert_eq!(
            ssh.iter().map(|p| p.pid).collect::<Vec<_>>(),
            vec![1000, 2001, 2002, 2003]
        );
        let r = s.respond(&argv(&["lsof", "-n", "-P", "-iTCP@100.100.1.10:22"]), t);
        let conns = parse_lsof(r.exit, &r.stdout, &r.stderr).unwrap();
        let dev = |pid: u32| {
            conns
                .iter()
                .find(|c| c.pid == pid)
                .map(|c| c.device.clone())
        };
        assert_eq!(dev(2001), dev(1000), "mux client shares the master socket");
        assert_ne!(dev(2002), dev(1000), "own TCP");
        assert_eq!(dev(2003), dev(1000));
    }

    #[test]
    fn tape_covers_every_command_per_tick_and_round_trips() {
        let s = Scenario::new();
        let tape = s.to_tape(Duration::from_secs(2), Duration::from_secs(1));
        assert_eq!(tape[0].cmd[..2], ["ssh", "-G"]);
        let checks = tape
            .iter()
            .filter(|e| e.cmd.starts_with(&argv(&["ssh", "-O"])))
            .count();
        assert_eq!(checks, 3, "t=0,1,2");
        let text = ssh_monitor_core::tape::write_jsonl(&tape);
        assert_eq!(ssh_monitor_core::tape::read_jsonl(&text).unwrap(), tape);
    }

    #[test]
    fn helpers() {
        assert_eq!(etime(Duration::from_secs(65)), "01:05");
        assert_eq!(etime(Duration::from_secs(3661)), "01:01:01");
        assert_eq!(etime(Duration::from_secs(90_000)), "1-01:00:00");
        let log = reconnect_loop_log(3, 75);
        assert_eq!(log.lines().count(), 9);
        assert_eq!(ssh_monitor_core::parse::parse_supervisor_log(&log).len(), 9);
    }
}

/// Tape replay by time (shared by `fake-cmd` and the L1 tape runner): the entry for `argv` is
/// the latest one recorded at or before `now_ms` (the first one before any was), among the
/// entries with exactly this argv. Without an exact match, entries of the same program are
/// used — except for `ssh`, which never falls back: an unexpected ssh form must fail loudly.
pub fn select_entry<'a>(
    entries: &'a [TapeEntry],
    argv: &[String],
    now_ms: u64,
) -> Option<&'a TapeEntry> {
    let exact: Vec<&TapeEntry> = entries.iter().filter(|e| e.cmd == argv).collect();
    let program = argv.first().map(String::as_str).unwrap_or("");
    let candidates: Vec<&TapeEntry> = if exact.is_empty() {
        if program == "ssh" {
            return None;
        }
        entries.iter().filter(|e| e.program() == program).collect()
    } else {
        exact
    };
    candidates
        .iter()
        .rev()
        .find(|e| e.at_ms <= now_ms)
        .or(candidates.first())
        .copied()
}

#[cfg(test)]
mod tape_tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn select_entry_is_latest_at_or_before_now_with_fallbacks() {
        let s = Scenario::new().then_at(Duration::from_secs(5), Phase::down());
        let tape = s.to_tape(Duration::from_secs(8), Duration::from_secs(1));
        let check = argv(&["ssh", "-O", "check", "fedora"]);
        assert_eq!(select_entry(&tape, &check, 0).unwrap().exit, 0);
        assert_eq!(select_entry(&tape, &check, 4_999).unwrap().at_ms, 4_000);
        assert_eq!(
            select_entry(&tape, &check, 5_000).unwrap().exit,
            255,
            "DOWN from t=5"
        );
        assert_eq!(
            select_entry(&tape, &check, 60_000).unwrap().at_ms,
            8_000,
            "last repeats"
        );
        let other_ps = argv(&["ps", "-o", "etime=", "-p", "4242"]);
        assert!(select_entry(&tape, &other_ps, 1_000).is_some());
        assert!(select_entry(&tape, &argv(&["ssh", "-O", "exit", "fedora"]), 1_000).is_none());
        assert!(select_entry(&tape, &argv(&["ssh", "fedora"]), 1_000).is_none());
        assert!(select_entry(&tape, &argv(&["nope"]), 1_000).is_none());
        // An entry recorded later than `now` is still answered (the first one).
        let late = vec![TapeEntry::new(9_000, &["x"], 0, "late", "")];
        assert_eq!(
            select_entry(&late, &argv(&["x"]), 0).unwrap().stdout,
            "late"
        );
    }
}
