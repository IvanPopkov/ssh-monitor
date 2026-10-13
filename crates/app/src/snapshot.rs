//! `--once`: the monitor state as one JSON document (SPEC F7).

use crate::ui::format;
use serde_json::{Value, json};
use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{Badge, Breaker, Monitor, State};

/// SPEC F7 exit codes: 0/1/2 = UP/DEGRADED/DOWN, 3 = error (including "no status answer").
pub const EXIT_ERROR: u8 = 3;

pub fn exit_code(state: Option<State>) -> u8 {
    match state {
        Some(State::Up) => 0,
        Some(State::Degraded) => 1,
        Some(State::Down) => 2,
        None => EXIT_ERROR,
    }
}

pub fn state_name(state: Option<State>) -> &'static str {
    state.map_or("n/a", State::name)
}

pub fn json(m: &Monitor, now: Stamp) -> Value {
    let state = m.known_state();
    let health = m.known_health();
    let stats = m.latency.stats();
    let (rx, tx) = match m.traffic.current {
        Some((rx, tx)) => (Some(rx), Some(tx)),
        None => (None, None),
    };
    let clients: Option<Vec<Value>> = m.known_clients().map(|known| {
        known
            .list
            .iter()
            .map(|c| {
                json!({
                    "pid": c.pid,
                    "label": format::client_line(c),
                    "age_s": c.age_at(now).as_secs(),
                    "direct": c.direct,
                })
            })
            .collect()
    });
    let events: Vec<String> = m
        .events
        .oldest_first()
        .map(|e| format::event_text(&e.kind))
        .collect();
    let probes_failed: Vec<&str> = m.probes.failed_probes().map(format::probe_label).collect();
    json!({
        "host": m.cfg.host_alias,
        "hostname": m.cfg.hostname,
        "state": state_name(state),
        "exit_code": exit_code(state),
        "since_s": state.map(|_| now.mono.saturating_sub(m.health.since.mono).as_secs()),
        "pid": m.known_master_pid(),
        "master_uptime_s": health.and_then(|h| h.master.as_ref()?.uptime_at(now)).map(|d| d.as_secs()),
        "was_up_s": health.and_then(|h| h.was_up_for).map(|d| d.as_secs()),
        "degraded_reason": health.and_then(|h| h.reason.as_ref()).map(format::degrade_reason),
        "rx_bps": rx,
        "tx_bps": tx,
        "rx_peak_bps": m.traffic.peak_rx(),
        "tx_peak_bps": m.traffic.peak_tx(),
        "rtt_ms": stats.now_ms,
        "rtt": {
            "min_ms": stats.min_ms,
            "avg_ms": stats.avg_ms,
            "max_ms": stats.max_ms,
            "jitter_ms": stats.jitter_ms,
            "replies": stats.replies,
            "lost": stats.lost,
        },
        "loss": stats.loss_pct,
        "baseline_ms": m.latency.baseline(),
        "path": format::path_short(m.path.kind()),
        "clients": clients,
        "client_count": m.known_clients().map(|c| c.list.len()),
        "ssh_procs": m.known_ssh_procs(),
        "storm": m.known_storm(),
        "breaker": m.supervisor.available.then_some(match m.supervisor.breaker {
            Breaker::Closed => "closed",
            Breaker::Open => "open",
            Breaker::HalfOpen => "half-open",
        }),
        "supervisor_log": m.supervisor.available,
        "retry_loop": m.supervisor.available.then(|| m.supervisor.loop_active()),
        "badges": m.badges().into_iter().map(Badge::name).collect::<Vec<_>>(),
        "probes_failed": probes_failed,
        "events": events,
        "sampled_s": now.mono.saturating_sub(m.started.unwrap_or(now).mono).as_secs(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_monitor_core::monitor::{Input, MonitorConfig, ProbeKind};
    use ssh_monitor_core::parse::{
        ByteCounters, MasterStatus, PingResult, Process, parse_supervisor_log,
    };

    fn monitor() -> Monitor {
        Monitor::new(MonitorConfig {
            host_alias: "fedora".into(),
            hostname: "100.100.1.10".into(),
            server_alive_interval: Some(15),
        })
    }

    #[test]
    fn exit_codes_follow_spec_and_unknown_is_an_error() {
        assert_eq!(exit_code(Some(State::Up)), 0);
        assert_eq!(exit_code(Some(State::Degraded)), 1);
        assert_eq!(exit_code(Some(State::Down)), 2);
        assert_eq!(exit_code(None), 3);
        assert_eq!(state_name(None), "n/a");
    }

    #[test]
    fn unknown_status_is_not_reported_as_down() {
        let mut m = monitor();
        let v = json(&m, Stamp::at_secs(0));
        assert_eq!(v["state"], "n/a");
        assert_eq!(v["exit_code"], 3);
        assert!(v["since_s"].is_null());
        m.apply(
            Input::Failed {
                probe: ProbeKind::Status,
                reason: "timed out".into(),
            },
            Stamp::at_secs(2),
        );
        let v = json(&m, Stamp::at_secs(3));
        assert_eq!(v["state"], "n/a");
        assert_eq!(v["probes_failed"], json!(["ssh -O check"]));
        m.apply(
            Input::Status(MasterStatus::Down { reason: "x".into() }),
            Stamp::at_secs(4),
        );
        let v = json(&m, Stamp::at_secs(5));
        assert_eq!(v["state"], "DOWN");
        assert_eq!(v["exit_code"], 2);
        assert_eq!(v["since_s"], 1);
    }

    #[test]
    fn snapshot_has_the_spec_fields_and_nulls_for_unknowns() {
        let mut m = monitor();
        m.apply(
            Input::Status(MasterStatus::Down { reason: "x".into() }),
            Stamp::at_secs(0),
        );
        let v = json(&m, Stamp::at_secs(0));
        assert_eq!(v["state"], "DOWN");
        assert_eq!(v["exit_code"], 2);
        assert!(v["rx_bps"].is_null());
        assert!(v["pid"].is_null());
        assert_eq!(v["path"], "n/a");
        assert_eq!(v["badges"], json!([]));
        m.apply(
            Input::Status(MasterStatus::Running { pid: 9 }),
            Stamp::at_secs(1),
        );
        m.apply(
            Input::Traffic(Some(ByteCounters {
                bytes_in: 0,
                bytes_out: 0,
            })),
            Stamp::at_secs(1),
        );
        m.apply(
            Input::Traffic(Some(ByteCounters {
                bytes_in: 2000,
                bytes_out: 1000,
            })),
            Stamp::at_secs(2),
        );
        m.apply(
            Input::Latency(PingResult::Reply { rtt_ms: 9.5 }),
            Stamp::at_secs(2),
        );
        let v = json(&m, Stamp::at_secs(3));
        assert_eq!(v["state"], "UP");
        assert_eq!(v["exit_code"], 0);
        assert_eq!(v["pid"], 9);
        assert_eq!(v["rx_bps"], 2000.0);
        assert_eq!(v["tx_bps"], 1000.0);
        assert_eq!(v["rtt_ms"], 9.5);
        assert_eq!(v["loss"], 0.0);
        assert_eq!(v["events"][0], "master down");
        assert_eq!(v["events"][1], "master up (pid 9)");
        assert_eq!(v["sampled_s"], 3);
        assert!(
            v["master_uptime_s"].is_null(),
            "no etime reading: n/a, not a guess"
        );
        assert!(
            v["storm"].is_null(),
            "ps/lsof never answered: unknown, not \"no storm\""
        );
    }

    #[test]
    fn failing_probes_show_na_instead_of_stale_values() {
        let mut m = monitor();
        m.apply(
            Input::Status(MasterStatus::Running { pid: 9 }),
            Stamp::at_secs(0),
        );
        let v = json(&m, Stamp::at_secs(1));
        assert_eq!(v["state"], "UP");
        assert!(v["master_uptime_s"].is_null(), "no etime reading yet");
        assert!(v["breaker"].is_null(), "supervisor log never read yet");
        assert!(v["retry_loop"].is_null());
        m.apply(Input::Supervisor(vec![]), Stamp::at_secs(2));
        m.apply(
            Input::Uptime(std::time::Duration::from_secs(100)),
            Stamp::at_secs(2),
        );
        let v = json(&m, Stamp::at_secs(3));
        assert_eq!(v["breaker"], "closed");
        assert_eq!(v["retry_loop"], false);
        assert_eq!(v["master_uptime_s"], 101);
        m.apply(
            Input::Failed {
                probe: ProbeKind::Status,
                reason: "timed out".into(),
            },
            Stamp::at_secs(4),
        );
        let v = json(&m, Stamp::at_secs(5));
        assert_eq!(v["state"], "n/a");
        assert_eq!(v["exit_code"], 3);
        assert!(v["pid"].is_null(), "not the last pid");
        assert!(v["master_uptime_s"].is_null());
        m.apply(
            Input::Status(MasterStatus::Running { pid: 9 }),
            Stamp::at_secs(6),
        );
        assert_eq!(json(&m, Stamp::at_secs(6))["state"], "UP");
        // Clients and the supervisor log failing: null, not the last list.
        m.apply(
            Input::Failed {
                probe: ProbeKind::Clients,
                reason: "exit 1".into(),
            },
            Stamp::at_secs(7),
        );
        m.apply(
            Input::Failed {
                probe: ProbeKind::Supervisor,
                reason: "missing".into(),
            },
            Stamp::at_secs(7),
        );
        let v = json(&m, Stamp::at_secs(8));
        assert!(v["clients"].is_null());
        assert!(v["client_count"].is_null());
        assert!(v["ssh_procs"].is_null());
        assert!(v["storm"].is_null());
        assert!(v["breaker"].is_null());
        assert_eq!(v["supervisor_log"], false);
    }

    #[test]
    fn badges_need_their_probe_answering() {
        let mut m = monitor();
        m.apply(
            Input::Status(MasterStatus::Running { pid: 9 }),
            Stamp::at_secs(0),
        );
        m.apply(
            Input::Processes(vec![Process {
                pid: 1,
                ppid: 1,
                etime: std::time::Duration::from_secs(5),
                command: "/usr/local/bin/autossh -M 0 fedora".into(),
            }]),
            Stamp::at_secs(1),
        );
        m.apply(Input::Connections(vec![]), Stamp::at_secs(1));
        m.apply(
            Input::Supervisor(parse_supervisor_log(
                "2026-10-11 03:01:11  CIRCUIT OPEN — 8 rapid failures; standing down for 300s\n",
            )),
            Stamp::at_secs(1),
        );
        assert_eq!(
            json(&m, Stamp::at_secs(1))["badges"],
            json!(["STORM", "BREAKER"])
        );
        assert_eq!(json(&m, Stamp::at_secs(1))["ssh_procs"], 0);
        for probe in [ProbeKind::Rogues, ProbeKind::Supervisor] {
            m.apply(
                Input::Failed {
                    probe,
                    reason: "exit 1".into(),
                },
                Stamp::at_secs(2),
            );
        }
        assert!(
            m.badges().is_empty(),
            "a stale STORM / BREAKER is not shown while its probe fails"
        );
        let v = json(&m, Stamp::at_secs(2));
        assert_eq!(v["badges"], json!([]));
        assert!(v["storm"].is_null() && v["breaker"].is_null());
        assert_eq!(v["ssh_procs"], 0, "ps still answers");
    }
}
