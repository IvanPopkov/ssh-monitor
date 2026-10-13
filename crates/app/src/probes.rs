//! One tokio task per probe on its own cadence, feeding `(Stamp, Input)` to the main loop (A5).

use crate::clock::Clock;
use crate::runner::{CmdOutput, CmdStatus, CommandRunner};
use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{Input, ProbeKind};
use ssh_monitor_core::parse::{
    parse_etime, parse_lsof, parse_nettop, parse_ping, parse_ps, parse_ssh_check,
    parse_supervisor_log, parse_tailscale_status,
};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

#[derive(Debug, Clone)]
pub struct ProbeContext {
    pub alias: String,
    pub hostname: String,
    pub supervisor_log: Option<PathBuf>,
    pub interval: Duration,
}

/// SPEC F7: 1× traffic/ping, 2× status/clients/processes/log, 10× tailscale.
pub fn cadence(kind: ProbeKind) -> u32 {
    match kind {
        ProbeKind::Traffic | ProbeKind::Latency => 1,
        ProbeKind::Status
        | ProbeKind::Uptime
        | ProbeKind::Clients
        | ProbeKind::Rogues
        | ProbeKind::Supervisor => 2,
        ProbeKind::Path => 10,
    }
}

/// A hung command is killed and reported failed (ping: lost); `ssh -O check` answers in ~10ms.
pub fn timeout(kind: ProbeKind) -> Duration {
    match kind {
        ProbeKind::Status => Duration::from_secs(2),
        ProbeKind::Latency | ProbeKind::Uptime | ProbeKind::Supervisor => Duration::from_secs(3),
        // `nettop -L 1` sometimes waits ~5s for the next kernel stats update (measured live).
        ProbeKind::Traffic => Duration::from_secs(8),
        _ => Duration::from_secs(5),
    }
}

/// `None` when it cannot run yet (no master PID, no log path).
pub fn argv(kind: ProbeKind, ctx: &ProbeContext, master_pid: Option<u32>) -> Option<Vec<String>> {
    let s = |v: &[&str]| v.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
    Some(match kind {
        ProbeKind::Status => s(&["ssh", "-O", "check", &ctx.alias]),
        ProbeKind::Traffic => {
            let pid = master_pid?.to_string();
            s(&[
                "nettop",
                "-p",
                &pid,
                "-P",
                "-L",
                "1",
                "-x",
                "-J",
                "bytes_in,bytes_out",
            ])
        }
        ProbeKind::Uptime => {
            let pid = master_pid?.to_string();
            s(&["ps", "-o", "etime=", "-p", &pid])
        }
        ProbeKind::Latency => s(&["ping", "-c", "1", "-t", "1", &ctx.hostname]),
        ProbeKind::Clients => s(&["ps", "-axo", "pid=,ppid=,etime=,command="]),
        ProbeKind::Rogues => s(&["lsof", "-n", "-P", &format!("-iTCP@{}:22", ctx.hostname)]),
        ProbeKind::Supervisor => {
            let log = ctx.supervisor_log.as_ref()?.to_string_lossy().into_owned();
            s(&["tail", "-n", "50", &log])
        }
        ProbeKind::Path => s(&["tailscale", "status", "--json"]),
    })
}

fn first_line(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .chars()
        .take(120)
        .collect()
}

fn failed(kind: ProbeKind, reason: impl Into<String>) -> Input {
    Input::Failed {
        probe: kind,
        reason: reason.into(),
    }
}

/// Never panics; anything unreadable is `Input::Failed`.
pub fn parse_output(kind: ProbeKind, out: &CmdOutput, hostname: &str) -> Input {
    match &out.status {
        CmdStatus::SpawnFailed(reason) => return failed(kind, reason.clone()),
        CmdStatus::TimedOut if kind != ProbeKind::Latency && kind != ProbeKind::Status => {
            return failed(kind, format!("timed out after {}s", out.elapsed.as_secs()));
        }
        _ => {}
    }
    let exit = out.exit_code();
    let exit_reason = || {
        let msg = first_line(&out.stderr);
        if msg.is_empty() {
            format!("exit {exit}")
        } else {
            format!("exit {exit}: {msg}")
        }
    };
    match kind {
        ProbeKind::Status => parse_ssh_check(exit, &out.stderr)
            .map(Input::Status)
            .unwrap_or_else(|e| failed(kind, e.reason)),
        ProbeKind::Traffic => {
            if exit != 0 {
                return failed(kind, exit_reason());
            }
            parse_nettop(&out.stdout)
                .map(Input::Traffic)
                .unwrap_or_else(|e| failed(kind, e.reason))
        }
        ProbeKind::Uptime => {
            // A vanished PID makes ps exit 1, silent or with the "process id" complaint: the status
            // probe reports that. Any other complaint is a real failure.
            if exit != 0 && out.stdout.trim().is_empty() {
                let err = out.stderr.trim();
                if err.is_empty() || err.contains("process id") {
                    return Input::Tick;
                }
                return failed(kind, exit_reason());
            }
            parse_etime(&out.stdout)
                .map(Input::Uptime)
                .unwrap_or_else(|e| failed(kind, e.reason))
        }
        ProbeKind::Latency => parse_ping(exit, &out.stdout)
            .map(Input::Latency)
            .unwrap_or_else(|e| failed(kind, e.reason)),
        ProbeKind::Clients => {
            if exit != 0 {
                return failed(kind, exit_reason());
            }
            parse_ps(&out.stdout)
                .map(Input::Processes)
                .unwrap_or_else(|e| failed(kind, e.reason))
        }
        ProbeKind::Rogues => parse_lsof(exit, &out.stdout, &out.stderr)
            .map(Input::Connections)
            .unwrap_or_else(|e| failed(kind, e.reason)),
        ProbeKind::Supervisor => {
            if exit != 0 {
                return failed(kind, exit_reason());
            }
            Input::Supervisor(parse_supervisor_log(&out.stdout))
        }
        ProbeKind::Path => {
            if exit != 0 {
                return failed(kind, exit_reason());
            }
            parse_tailscale_status(&out.stdout, hostname)
                .map(Input::Path)
                .unwrap_or_else(|e| failed(kind, e.reason))
        }
    }
}

/// `None` until the first `ssh -O check` answered; then the master PID (`None` = DOWN).
pub type PidState = Option<Option<u32>>;

/// Non-status probes wait for the first status answer (≤ 2 ticks) and skip ticks with no master PID.
pub fn spawn_all(
    ctx: ProbeContext,
    runner: Arc<dyn CommandRunner>,
    clock: Arc<dyn Clock>,
    master_pid: watch::Receiver<PidState>,
    stop: watch::Receiver<bool>,
    tx: mpsc::Sender<(Stamp, Input)>,
) -> Vec<JoinHandle<()>> {
    ProbeKind::ALL
        .into_iter()
        .filter(|kind| *kind != ProbeKind::Supervisor || ctx.supervisor_log.is_some())
        .map(|kind| {
            let ctx = ctx.clone();
            let runner = Arc::clone(&runner);
            let clock = Arc::clone(&clock);
            let mut master_pid = master_pid.clone();
            let mut stop = stop.clone();
            let tx = tx.clone();
            tokio::spawn(async move {
                if kind != ProbeKind::Status {
                    let _ = tokio::time::timeout(
                        ctx.interval * 2,
                        master_pid.wait_for(|state| state.is_some()),
                    )
                    .await;
                }
                let period = ctx.interval * cadence(kind);
                let mut ticker = tokio::time::interval(period);
                ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = ticker.tick() => {}
                        _ = stop.changed() => return,
                    }
                    if *stop.borrow() {
                        return;
                    }
                    let pid = master_pid.borrow().flatten();
                    let Some(argv) = argv(kind, &ctx, pid) else {
                        continue;
                    };
                    let out = runner.run(argv, timeout(kind)).await;
                    let input = parse_output(kind, &out, &ctx.hostname);
                    if tx.send((clock.now(), input)).await.is_err() {
                        return;
                    }
                }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_monitor_core::parse::{MasterStatus, PingResult};

    fn out(exit: i32, stdout: &str, stderr: &str) -> CmdOutput {
        CmdOutput {
            status: CmdStatus::Exited(exit),
            stdout: stdout.into(),
            stderr: stderr.into(),
            elapsed: Duration::from_millis(5),
        }
    }

    fn ctx() -> ProbeContext {
        ProbeContext {
            alias: "fedora".into(),
            hostname: "100.100.1.10".into(),
            supervisor_log: Some(PathBuf::from("/tmp/sup.log")),
            interval: Duration::from_secs(1),
        }
    }

    #[test]
    fn commands_match_the_architecture_table() {
        let c = ctx();
        let j = |k| argv(k, &c, Some(42)).unwrap().join(" ");
        assert_eq!(j(ProbeKind::Status), "ssh -O check fedora");
        assert_eq!(
            j(ProbeKind::Traffic),
            "nettop -p 42 -P -L 1 -x -J bytes_in,bytes_out"
        );
        assert_eq!(j(ProbeKind::Uptime), "ps -o etime= -p 42");
        assert_eq!(j(ProbeKind::Latency), "ping -c 1 -t 1 100.100.1.10");
        assert_eq!(j(ProbeKind::Clients), "ps -axo pid=,ppid=,etime=,command=");
        assert_eq!(j(ProbeKind::Rogues), "lsof -n -P -iTCP@100.100.1.10:22");
        assert_eq!(j(ProbeKind::Supervisor), "tail -n 50 /tmp/sup.log");
        assert_eq!(j(ProbeKind::Path), "tailscale status --json");
        assert_eq!(
            argv(ProbeKind::Traffic, &c, None),
            None,
            "no master, no nettop"
        );
        assert_eq!(argv(ProbeKind::Uptime, &c, None), None);
        let no_log = ProbeContext {
            supervisor_log: None,
            ..c
        };
        assert_eq!(argv(ProbeKind::Supervisor, &no_log, Some(1)), None);
        // Only ever `ssh -O check` / `ssh -G`: no other ssh form exists in this module.
        for k in ProbeKind::ALL {
            if let Some(a) = argv(k, &ctx(), Some(1))
                && a[0] == "ssh"
            {
                assert_eq!(&a[1..3], ["-O", "check"]);
            }
        }
    }

    #[test]
    fn cadences_and_timeouts() {
        assert_eq!(cadence(ProbeKind::Traffic), 1);
        assert_eq!(cadence(ProbeKind::Latency), 1);
        assert_eq!(cadence(ProbeKind::Status), 2);
        assert_eq!(cadence(ProbeKind::Supervisor), 2);
        assert_eq!(cadence(ProbeKind::Path), 10);
        assert_eq!(timeout(ProbeKind::Latency), Duration::from_secs(3));
        assert_eq!(timeout(ProbeKind::Status), Duration::from_secs(2));
        assert_eq!(timeout(ProbeKind::Traffic), Duration::from_secs(8));
    }

    #[test]
    fn parses_each_probe_or_reports_failure() {
        let h = "100.100.1.10";
        assert_eq!(
            parse_output(
                ProbeKind::Status,
                &out(0, "", "Master running (pid=7)\r\n"),
                h
            ),
            Input::Status(MasterStatus::Running { pid: 7 })
        );
        assert!(matches!(
            parse_output(
                ProbeKind::Status,
                &out(255, "", "Control socket connect(x): No such file\r\n"),
                h
            ),
            Input::Status(MasterStatus::Down { .. })
        ));
        assert_eq!(
            parse_output(ProbeKind::Traffic, &out(0, ",bytes_in,bytes_out,\n", ""), h),
            Input::Traffic(None)
        );
        assert!(matches!(
            parse_output(ProbeKind::Traffic, &out(1, "", "nettop: bad\n"), h),
            Input::Failed {
                probe: ProbeKind::Traffic,
                ..
            }
        ));
        assert_eq!(
            parse_output(ProbeKind::Uptime, &out(0, "   01:02\n", ""), h),
            Input::Uptime(Duration::from_secs(62))
        );
        assert_eq!(
            parse_output(
                ProbeKind::Uptime,
                &out(1, "", "ps: process id too large\n"),
                h
            ),
            Input::Tick,
            "vanished PID is not a probe failure"
        );
        assert_eq!(
            parse_output(ProbeKind::Uptime, &out(1, "", ""), h),
            Input::Tick,
            "ps says nothing at all for a missing PID"
        );
        assert!(matches!(
            parse_output(
                ProbeKind::Uptime,
                &out(1, "", "ps: illegal option -- z\n"),
                h
            ),
            Input::Failed {
                probe: ProbeKind::Uptime,
                ..
            }
        ));
        assert_eq!(
            parse_output(ProbeKind::Latency, &out(0, "x time=1.5 ms\n", ""), h),
            Input::Latency(PingResult::Reply { rtt_ms: 1.5 })
        );
        assert!(matches!(
            parse_output(
                ProbeKind::Clients,
                &out(0, "1 0 00:01 /sbin/launchd\n", ""),
                h
            ),
            Input::Processes(_)
        ));
        assert_eq!(
            parse_output(ProbeKind::Rogues, &out(1, "", ""), h),
            Input::Connections(vec![])
        );
        assert!(matches!(
            parse_output(ProbeKind::Supervisor, &out(1, "", "tail: /x: No such file or directory\n"), h),
            Input::Failed { probe: ProbeKind::Supervisor, reason } if reason.contains("No such file")
        ));
        assert!(matches!(
            parse_output(ProbeKind::Supervisor, &out(0, "2026-10-11 03:00:00  opening master connection\n", ""), h),
            Input::Supervisor(lines) if lines.len() == 1
        ));
        assert_eq!(
            parse_output(ProbeKind::Path, &out(0, r#"{"Peer":{}}"#, ""), h),
            Input::Path(None)
        );
        assert!(matches!(
            parse_output(ProbeKind::Path, &out(0, "{", ""), h),
            Input::Failed {
                probe: ProbeKind::Path,
                ..
            }
        ));
    }

    #[test]
    fn spawn_failure_and_timeouts() {
        let h = "h";
        let spawn = CmdOutput::spawn_failed("nettop: No such file or directory".into());
        assert!(matches!(
            parse_output(ProbeKind::Traffic, &spawn, h),
            Input::Failed { probe: ProbeKind::Traffic, reason } if reason.contains("nettop")
        ));
        let timed = CmdOutput {
            status: CmdStatus::TimedOut,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::from_secs(3),
        };
        assert_eq!(
            parse_output(ProbeKind::Latency, &timed, h),
            Input::Latency(PingResult::Lost),
            "a ping that never came back is lost"
        );
        assert!(matches!(
            parse_output(ProbeKind::Status, &timed, h),
            Input::Failed {
                probe: ProbeKind::Status,
                ..
            }
        ));
        assert!(matches!(
            parse_output(ProbeKind::Clients, &timed, h),
            Input::Failed { probe: ProbeKind::Clients, reason } if reason.contains("timed out")
        ));
    }
}
