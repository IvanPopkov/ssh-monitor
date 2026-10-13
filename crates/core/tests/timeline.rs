//! Monitor timeline tests: time is explicit (`Stamp::at_secs`), never slept.

use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{
    Breaker, DegradeReason, Event, EventKind, Input, MasterExit, Monitor, MonitorConfig, ProbeKind,
    Severity, State, StormCulprit,
};
use ssh_monitor_core::parse::{
    ByteCounters, MasterStatus, PathKind, PingResult, TcpConn, parse_lsof, parse_ps,
    parse_supervisor_log, parse_tailscale_status,
};
use ssh_monitor_core::stamp::WALL_ORIGIN_MS;
use std::time::Duration;

const PS_NORMAL: &str = include_str!("fixtures/ps_axo_normal.txt");
const PS_POST: &str = include_str!("fixtures/ps_axo_postoutage.txt");
const LSOF_NORMAL: &str = include_str!("fixtures/lsof_tcp_normal.txt");
const LSOF_POST: &str = include_str!("fixtures/lsof_tcp_postoutage.txt");
const SUP_OUTAGE: &str = include_str!("fixtures/supervisor_outage.log");
const SUP_BREAKER: &str = include_str!("fixtures/supervisor_breaker.log");
const TS_WAN: &str = include_str!("fixtures/tailscale_status_direct_wan.json");
const TS_LAN: &str = include_str!("fixtures/tailscale_status_direct_lan.json");
const TS_DERP: &str = include_str!("fixtures/tailscale_status_derp.json");

fn monitor() -> Monitor {
    Monitor::new(MonitorConfig {
        host_alias: "fedora".into(),
        hostname: "100.100.1.10".into(),
        server_alive_interval: Some(15),
    })
}

/// Ping-centric tests feed no traffic.
fn monitor_no_stall() -> Monitor {
    Monitor::new(MonitorConfig {
        server_alive_interval: None,
        ..monitor().cfg
    })
}

fn up(pid: u32) -> Input {
    Input::Status(MasterStatus::Running { pid })
}

fn down() -> Input {
    Input::Status(MasterStatus::Down {
        reason: "no socket".into(),
    })
}

fn ping(ms: f64) -> Input {
    Input::Latency(PingResult::Reply { rtt_ms: ms })
}

fn lost() -> Input {
    Input::Latency(PingResult::Lost)
}

fn traffic(bytes_in: u64, bytes_out: u64) -> Input {
    Input::Traffic(Some(ByteCounters {
        bytes_in,
        bytes_out,
    }))
}

fn tail(text: &str) -> Input {
    Input::Supervisor(parse_supervisor_log(text))
}

fn path(json: &str) -> Input {
    Input::Path(parse_tailscale_status(json, "100.100.1.10").unwrap())
}

fn kinds(events: &[Event]) -> Vec<EventKind> {
    events.iter().map(|e| e.kind.clone()).collect()
}

fn at(secs: u64) -> Stamp {
    Stamp::at_secs(secs)
}

fn run(m: &mut Monitor, start: u64, inputs: impl IntoIterator<Item = Input>) -> Vec<Event> {
    let mut out = Vec::new();
    for (i, input) in inputs.into_iter().enumerate() {
        out.extend(m.apply(input, at(start + i as u64)));
    }
    out
}

#[test]
fn down_until_a_master_is_reported_then_up() {
    let mut m = monitor();
    assert_eq!(m.state(), State::Down);
    let ev = m.apply(down(), at(1));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::MasterDown {
            was_up: None,
            supervisor: None
        }]
    );
    assert_eq!(m.state(), State::Down);
    assert!(m.apply(down(), at(3)).is_empty(), "still down: no repeat");
    let ev = m.apply(up(42), at(5));
    assert_eq!(kinds(&ev), vec![EventKind::MasterUp { pid: 42 }]);
    assert_eq!(ev[0].severity(), Severity::Green);
    assert_eq!((m.state(), m.health.since), (State::Up, at(5)));
    assert_eq!(m.health.master_pid(), Some(42));
}

#[test]
fn degraded_on_third_consecutive_lost_ping_not_before() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    assert!(run(&mut m, 1, [lost(), lost()]).is_empty());
    assert_eq!(m.state(), State::Up, "two lost pings are not enough");
    let ev = m.apply(lost(), at(3));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::Degraded(DegradeReason::Loss { consecutive: 3 })]
    );
    assert_eq!(ev[0].severity(), Severity::Yellow);
    assert_eq!((m.state(), m.health.since), (State::Degraded, at(3)));
}

#[test]
fn degraded_to_up_needs_five_seconds_of_clean_pings() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, [lost(), lost(), lost()]);
    assert_eq!(m.state(), State::Degraded);
    // Replies at 4..7: clean for 4s only.
    assert!(run(&mut m, 4, [ping(10.0), ping(10.0), ping(10.0), ping(10.0)]).is_empty());
    assert_eq!(m.state(), State::Degraded);
    let ev = m.apply(ping(10.0), at(8));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::Recovered {
            degraded_for: Duration::from_secs(5)
        }]
    );
    assert_eq!(ev[0].severity(), Severity::Green);
    assert_eq!((m.state(), m.health.since), (State::Up, at(8)));
}

#[test]
fn a_single_lost_ping_restarts_the_recovery_timer() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, [lost(), lost(), lost()]);
    run(&mut m, 4, [ping(10.0), ping(10.0), ping(10.0)]);
    m.apply(lost(), at(7)); // one lost ping: not a reason, but not clean either
    assert!(run(&mut m, 8, [ping(10.0), ping(10.0), ping(10.0), ping(10.0)]).is_empty());
    assert_eq!(
        m.state(),
        State::Degraded,
        "only 4s clean since the lost ping at 7"
    );
    let ev = m.apply(ping(10.0), at(12));
    assert!(matches!(kinds(&ev)[..], [EventKind::Recovered { .. }]));
}

#[test]
fn slow_rule_waits_for_ten_baseline_samples_then_uses_five_second_median() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    // 9 UP samples: no baseline yet, a huge ping is neither a spike nor a degrade.
    let ev = run(&mut m, 1, (0..9).map(|_| ping(10.0)).chain([ping(900.0)]));
    assert!(ev.is_empty(), "{ev:?}");
    assert_eq!(
        m.latency.baseline(),
        Some(10.0),
        "the 10th UP sample completes the baseline"
    );
    assert_eq!(m.state(), State::Up);
    // Let the 900ms sample leave the 5s window, then go slow.
    assert!(run(&mut m, 11, (0..5).map(|_| ping(10.0))).is_empty());
    // Threshold = max(3×10, 10+100) = 110ms. Slow pings from t=16; the (now-5s, now] median
    // holds 5 samples at 1 Hz and crosses the bar at t=18.
    let ev = run(&mut m, 16, [ping(200.0), ping(200.0)]);
    assert!(
        matches!(kinds(&ev)[..], [EventKind::RttSpike { count: 1, .. }]),
        "{ev:?}"
    );
    assert_eq!(
        m.state(),
        State::Up,
        "median of [10,10,10,200,200] is 10 < 110"
    );
    let ev = m.apply(ping(200.0), at(18));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::Degraded(DegradeReason::Slow {
            median_ms: 200.0,
            threshold_ms: 110.0
        })]
    );
    assert_eq!(m.state(), State::Degraded);
}

#[test]
fn baseline_resets_on_path_change() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    m.apply(path(TS_WAN), at(0));
    run(&mut m, 1, (0..10).map(|_| ping(10.0)));
    assert!(m.latency.baseline().is_some());
    let ev = m.apply(path(TS_DERP), at(11));
    assert!(matches!(kinds(&ev)[..], [EventKind::PathChanged { .. }]));
    assert_eq!(
        m.latency.baseline(),
        None,
        "old RTT level means nothing over DERP"
    );
    let ev = run(
        &mut m,
        12,
        [
            ping(500.0),
            ping(500.0),
            ping(500.0),
            ping(500.0),
            ping(500.0),
        ],
    );
    assert!(
        ev.is_empty(),
        "slow rule and spikes disabled until 10 new samples: {ev:?}"
    );
    assert_eq!(m.state(), State::Up);
}

#[test]
fn stall_threshold_is_twice_server_alive_interval_plus_five_seconds() {
    let mut m = monitor(); // ServerAliveInterval 15 → 35s
    m.apply(up(1), at(0));
    m.apply(traffic(1000, 1000), at(1));
    // Silence is measured from the first counter sample (t=1).
    for t in 2..=36 {
        m.apply(ping(10.0), at(t));
        let ev = m.apply(traffic(1000, 1000 + t), at(t));
        assert!(ev.is_empty(), "t={t}: {ev:?}");
    }
    assert_eq!(m.state(), State::Up, "35s silent is not > 35s");
    let ev = m.apply(traffic(1000, 2000), at(37));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::Degraded(DegradeReason::Stall {
            silent_for: Duration::from_secs(36)
        })]
    );
    // Bytes arrive again: the stall clears; recovery needs 5s of clean pings after t=37.
    for t in 38..=41 {
        m.apply(traffic(5000 + t, 2000), at(t));
        let ev = m.apply(ping(10.0), at(t));
        assert!(ev.is_empty(), "t={t}: {ev:?}");
    }
    let ev = m.apply(ping(10.0), at(42));
    assert!(
        matches!(kinds(&ev)[..], [EventKind::Recovered { .. }]),
        "{ev:?}"
    );
}

#[test]
fn stall_rule_is_off_without_server_alive_interval() {
    let mut m = Monitor::new(MonitorConfig {
        server_alive_interval: None,
        ..monitor().cfg
    });
    m.apply(up(1), at(0));
    m.apply(traffic(1, 1), at(1));
    let ev = run(&mut m, 2, (0..100).map(|_| traffic(1, 1)));
    assert!(ev.is_empty());
    assert_eq!(m.state(), State::Up);
}

#[test]
fn master_restarted_after_down_and_on_pid_change() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    m.apply(Input::Uptime(Duration::from_secs(100)), at(1));
    let ev = m.apply(down(), at(5));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::MasterDown {
            was_up: Some(Duration::from_secs(104)),
            supervisor: None
        }]
    );
    assert_eq!(ev[0].severity(), Severity::Red);
    assert_eq!(m.health.was_up_for, Some(Duration::from_secs(104)));
    let ev = m.apply(up(2), at(9));
    assert_eq!(kinds(&ev), vec![EventKind::MasterRestarted { pid: 2 }]);
    assert_eq!(ev[0].severity(), Severity::Yellow);
    let ev = m.apply(up(3), at(11));
    assert_eq!(kinds(&ev), vec![EventKind::MasterRestarted { pid: 3 }]);
    assert_eq!(m.health.master_pid(), Some(3));
    assert_eq!(m.state(), State::Up);
}

#[test]
fn master_down_merges_supervisor_line_within_five_seconds_either_order() {
    let mut m = monitor();
    m.apply(tail(""), at(0));
    m.apply(up(1), at(0));
    m.apply(down(), at(10));
    let line1 =
        "2026-10-09 16:46:40  master ended after 5736s (rc=255) — healthy run, backoff reset\n";
    assert!(
        m.apply(tail(line1), at(13)).is_empty(),
        "merge is an amendment, not a new event"
    );
    let merged = m
        .events
        .newest_first()
        .find(|e| matches!(e.kind, EventKind::MasterDown { .. }))
        .unwrap();
    assert!(matches!(
        merged.kind,
        EventKind::MasterDown {
            supervisor: Some(MasterExit {
                after_s: 5736,
                rc: 255
            }),
            ..
        }
    ));
    m.apply(up(2), at(20));
    let line2 = format!(
        "{line1}2026-10-09 16:47:56  master ended after 75s (rc=255) — healthy run, backoff reset\n"
    );
    m.apply(tail(&line2), at(30));
    m.apply(down(), at(32));
    let merged = m
        .events
        .newest_first()
        .find(|e| matches!(e.kind, EventKind::MasterDown { .. }))
        .unwrap();
    assert!(matches!(
        merged.kind,
        EventKind::MasterDown {
            supervisor: Some(MasterExit {
                after_s: 75,
                rc: 255
            }),
            ..
        }
    ));
}

#[test]
fn master_down_stays_plain_when_the_supervisor_line_is_too_late() {
    let mut m = monitor();
    m.apply(tail(""), at(0));
    m.apply(up(1), at(0));
    m.apply(down(), at(10));
    let line =
        "2026-10-09 16:46:40  master ended after 5736s (rc=255) — healthy run, backoff reset\n";
    m.apply(tail(line), at(16));
    let ev = m
        .events
        .newest_first()
        .find(|e| matches!(e.kind, EventKind::MasterDown { .. }))
        .unwrap();
    assert!(matches!(
        ev.kind,
        EventKind::MasterDown {
            supervisor: None,
            ..
        }
    ));
}

#[test]
fn rtt_spike_at_most_one_event_per_30s_with_count_and_muted_while_degraded() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, (0..10).map(|_| ping(10.0)));
    let ev = m.apply(ping(300.0), at(11));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::RttSpike {
            rtt_ms: 300.0,
            threshold_ms: 110.0,
            count: 1
        }]
    );
    let first_id = ev[0].id;
    // Back to normal between spikes so the median never degrades.
    run(&mut m, 12, (0..8).map(|_| ping(10.0)));
    let ev = m.apply(ping(400.0), at(20));
    assert!(ev.is_empty(), "within 30s: amended, not re-emitted");
    let spike = m.events.newest_first().find(|e| e.id == first_id).unwrap();
    assert_eq!(
        spike.kind,
        EventKind::RttSpike {
            rtt_ms: 400.0,
            threshold_ms: 110.0,
            count: 2
        }
    );
    run(&mut m, 21, (0..20).map(|_| ping(10.0)));
    let ev = m.apply(ping(300.0), at(42));
    assert!(
        matches!(kinds(&ev)[..], [EventKind::RttSpike { count: 1, .. }]),
        "cooldown over: {ev:?}"
    );
    run(&mut m, 43, [lost(), lost(), lost()]);
    assert_eq!(m.state(), State::Degraded);
    run(&mut m, 46, (0..20).map(|_| ping(10.0)));
    let before = m.events.len();
    assert_eq!(m.state(), State::Up);
    run(&mut m, 70, [lost(), lost(), lost()]);
    let ev = m.apply(ping(900.0), at(73));
    assert!(ev.is_empty(), "spike while DEGRADED is muted: {ev:?}");
    assert_eq!(m.events.len(), before + 1, "only `degraded` was added");
}

#[test]
fn storm_non_mux_connection_once_per_culprit_then_cleared() {
    let mut m = monitor();
    m.apply(up(22489), at(0));
    let conns = parse_lsof(0, LSOF_POST, "").unwrap();
    let ev = m.apply(Input::Connections(conns.clone()), at(1));
    let culprits: Vec<u32> = ev
        .iter()
        .filter_map(|e| match &e.kind {
            EventKind::StormStarted(c) => Some(c.pid()),
            _ => None,
        })
        .collect();
    assert_eq!(
        culprits,
        vec![22475, 22508],
        "VS Code direct + duplicate supervisor master; the pane shares the master socket"
    );
    assert!(ev.iter().all(|e| e.severity() == Severity::Red));
    assert!(m.storm.active());
    assert!(
        m.apply(Input::Connections(conns), at(3)).is_empty(),
        "once per culprit"
    );
    let only_master: Vec<TcpConn> = parse_lsof(0, LSOF_POST, "")
        .unwrap()
        .into_iter()
        .filter(|c| c.pid == 22489)
        .collect();
    let ev = m.apply(Input::Connections(only_master), at(9));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::StormCleared {
            lasted: Duration::from_secs(8)
        }]
    );
    assert!(!m.storm.active());
}

#[test]
fn storm_autossh_present() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    let mut procs = parse_ps(PS_NORMAL).unwrap();
    procs.push(ssh_monitor_core::parse::Process {
        pid: 777,
        ppid: 1,
        etime: Duration::from_secs(3),
        command: "autossh -M 0 -N fedora".into(),
    });
    let ev = m.apply(Input::Processes(procs.clone()), at(1));
    assert!(
        ev.iter()
            .any(|e| e.kind == EventKind::StormStarted(StormCulprit::Autossh { pid: 777 }))
    );
    procs.retain(|p| p.pid != 777);
    let ev = m.apply(Input::Processes(procs), at(4));
    assert!(
        ev.iter()
            .any(|e| matches!(e.kind, EventKind::StormCleared { .. }))
    );
}

#[test]
fn supervisor_lines_become_events_and_breaker_state() {
    let mut m = monitor();
    let lines: Vec<&str> = SUP_BREAKER.lines().collect();
    let history = format!("{}\n", lines[..2].join("\n"));
    assert!(
        m.apply(tail(&history), at(0)).is_empty(),
        "history fires nothing"
    );
    assert_eq!(m.supervisor.breaker, Breaker::Closed);
    let mut text = history;
    let mut fired = Vec::new();
    for (i, l) in lines[2..].iter().enumerate() {
        text.push_str(l);
        text.push('\n');
        fired.extend(m.apply(tail(&text), at(1 + i as u64)));
    }
    let k = kinds(&fired);
    assert_eq!(
        k,
        vec![
            EventKind::SupervisorReconnecting { in_s: 61 },
            EventKind::CircuitOpen {
                standing_down_s: Some(300)
            },
            EventKind::CircuitHalfOpen,
        ]
    );
    assert_eq!(fired[0].severity(), Severity::Dim);
    assert_eq!(fired[1].severity(), Severity::Red);
    assert_eq!(fired[2].severity(), Severity::Yellow);
    assert_eq!(m.supervisor.breaker, Breaker::HalfOpen);
    m.apply(up(5), at(20));
    assert_eq!(
        m.supervisor.breaker,
        Breaker::Closed,
        "half-open closes once a master runs"
    );
}

#[test]
fn supervisor_log_missing_is_a_failed_probe_and_n_a() {
    let mut m = monitor();
    let ev = m.apply(
        Input::Failed {
            probe: ProbeKind::Supervisor,
            reason: "no such file".into(),
        },
        at(1),
    );
    assert_eq!(
        kinds(&ev),
        vec![EventKind::ProbeFailed {
            probe: ProbeKind::Supervisor,
            reason: "no such file".into()
        }]
    );
    assert!(!m.supervisor.available);
    let ev = m.apply(tail(""), at(3));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::ProbeRecovered {
            probe: ProbeKind::Supervisor
        }]
    );
    assert!(m.supervisor.available);
}

#[test]
fn reconnect_loop_after_three_attempts_that_never_reached_up_and_loop_ended_on_up() {
    let mut m = monitor();
    m.apply(tail(""), at(0));
    m.apply(down(), at(0));
    let mut text = String::new();
    let mut t = 1;
    let mut fired = Vec::new();
    for n in 0..5u64 {
        text.push_str(&format!(
            "2026-10-09 16:5{n}:00  opening master connection\n"
        ));
        fired.extend(m.apply(tail(&text), at(t)));
        t += 75;
        text.push_str(&format!(
            "2026-10-09 16:5{n}:75  master ended after 75s (rc=255) — healthy run, backoff reset\n"
        ));
        fired.extend(m.apply(tail(&text), at(t)));
        fired.extend(m.apply(down(), at(t)));
        text.push_str(&format!("2026-10-09 16:5{n}:76  reconnecting in 2s\n"));
        fired.extend(m.apply(tail(&text), at(t + 1)));
        t += 2;
        if n == 1 {
            assert!(!m.supervisor.loop_active(), "two attempts are not a loop");
        }
        if n == 2 {
            assert!(m.supervisor.loop_active(), "third attempt opens the loop");
        }
    }
    let loops: Vec<&Event> = fired
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReconnectLoop { .. }))
        .collect();
    assert_eq!(loops.len(), 1, "fires once");
    assert_eq!(
        loops[0].kind,
        EventKind::ReconnectLoop {
            attempts: 3,
            avg_secs: 75
        }
    );
    assert_eq!(loops[0].severity(), Severity::Yellow);
    let stored = m
        .events
        .newest_first()
        .find(|e| e.id == loops[0].id)
        .unwrap();
    assert_eq!(
        stored.kind,
        EventKind::ReconnectLoop {
            attempts: 5,
            avg_secs: 75
        },
        "grows in place"
    );
    assert_eq!(
        fired
            .iter()
            .filter(|e| matches!(e.kind, EventKind::SupervisorReconnecting { .. }))
            .count(),
        5
    );
    let ev = m.apply(up(9), at(t + 10));
    assert_eq!(
        kinds(&ev),
        vec![
            EventKind::MasterUp { pid: 9 },
            EventKind::ReconnectLoopEnded { attempts: 5 }
        ],
        "first master this monitor ever saw: `master up`"
    );
    assert_eq!(ev[1].severity(), Severity::Green);
    assert!(!m.supervisor.loop_active());
}

#[test]
fn reconnect_loop_fires_when_replaying_the_real_outage_log() {
    // The supervisor labelled 75s connect timeouts "healthy run"; we ignore that label.
    let lines: Vec<&str> = SUP_OUTAGE.lines().collect();
    let parsed = parse_supervisor_log(SUP_OUTAGE);
    let t0 = parsed[0].naive_secs;
    let stamp_of = |i: usize| at(u64::try_from(parsed[i].naive_secs - t0).unwrap() + 1);
    let mut m = monitor();
    // Monitor starts right after the 5736s healthy run died (line index 5 = "opening").
    let history = format!("{}\n", lines[..6].join("\n"));
    m.apply(tail(&history), stamp_of(5));
    m.apply(down(), stamp_of(5));
    let mut fired = Vec::new();
    let mut loop_at_line = None;
    for i in 6usize..60 {
        let text = format!("{}\n", lines[i.saturating_sub(49)..=i].join("\n"));
        let stamp = stamp_of(i);
        let ev = m.apply(tail(&text), stamp);
        if ev
            .iter()
            .any(|e| matches!(e.kind, EventKind::ReconnectLoop { .. }))
            && loop_at_line.is_none()
        {
            loop_at_line = Some(i);
        }
        fired.extend(ev);
        fired.extend(m.apply(down(), stamp));
        assert_eq!(m.state(), State::Down);
    }
    assert_eq!(
        loop_at_line,
        Some(12),
        "third `master ended after 75s` line (0-based index 12)"
    );
    let stored = m
        .events
        .newest_first()
        .find(|e| matches!(e.kind, EventKind::ReconnectLoop { .. }))
        .unwrap();
    let EventKind::ReconnectLoop { attempts, avg_secs } = stored.kind else {
        unreachable!()
    };
    assert_eq!(attempts, 18, "every `master ended` line in lines 6..60");
    assert_eq!(avg_secs, 75);
    assert!(m.supervisor.loop_active());
    assert_eq!(
        fired
            .iter()
            .filter(|e| matches!(e.kind, EventKind::ReconnectLoop { .. }))
            .count(),
        1
    );
}

#[test]
fn path_changed_only_between_direct_and_derp() {
    let mut m = monitor();
    assert!(m.apply(path(TS_WAN), at(1)).is_empty());
    assert!(
        m.apply(path(TS_LAN), at(2)).is_empty(),
        "LAN ↔ WAN is title-only"
    );
    assert_eq!(m.path.kind(), Some(&PathKind::Direct { lan: true }));
    let ev = m.apply(path(TS_DERP), at(3));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::PathChanged {
            from: Some(PathKind::Direct { lan: true }),
            to: Some(PathKind::Derp {
                region: "par".into()
            })
        }]
    );
    assert_eq!(ev[0].severity(), Severity::Dim);
    let ev = m.apply(path(TS_WAN), at(4));
    assert!(matches!(kinds(&ev)[..], [EventKind::PathChanged { .. }]));
}

#[test]
fn clients_changed_one_line_per_poll() {
    let mut m = monitor();
    m.apply(up(34299), at(0));
    let procs = parse_ps(PS_NORMAL).unwrap();
    let ev = m.apply(Input::Processes(procs.clone()), at(1));
    assert_eq!(ev.len(), 1);
    let EventKind::ClientsChanged { added, removed } = &ev[0].kind else {
        panic!("{ev:?}")
    };
    assert_eq!(added.len(), 3);
    assert!(removed.is_empty());
    assert_eq!(ev[0].severity(), Severity::Dim);
    assert_eq!(m.clients.list.len(), 3);
    assert_eq!(m.clients.ssh_process_count, 4);
    let mut next: Vec<_> = procs
        .iter()
        .filter(|p| p.pid != 34340 && p.pid != 34345)
        .cloned()
        .collect();
    next.push(ssh_monitor_core::parse::Process {
        pid: 500,
        ppid: 1,
        etime: Duration::from_secs(1),
        command: "ssh fedora rsync --server".into(),
    });
    let ev = m.apply(Input::Processes(next), at(3));
    assert_eq!(ev.len(), 1);
    let EventKind::ClientsChanged { added, removed } = &ev[0].kind else {
        panic!()
    };
    assert_eq!((added.len(), removed.len()), (1, 2));
    assert_eq!(added[0].remote_argv, vec!["rsync", "--server"]);
}

#[test]
fn direct_clients_are_flagged_from_lsof_and_down_lists_rogues() {
    let mut m = monitor();
    m.apply(up(22489), at(0));
    m.apply(
        Input::Connections(parse_lsof(0, LSOF_POST, "").unwrap()),
        at(1),
    );
    m.apply(Input::Processes(parse_ps(PS_POST).unwrap()), at(2));
    let flags: Vec<(u32, bool)> = m.clients.list.iter().map(|c| (c.pid, c.direct)).collect();
    assert_eq!(flags, vec![(22340, false), (22475, true), (22508, true)]);
    m.apply(down(), at(4));
    m.apply(Input::Processes(parse_ps(PS_POST).unwrap()), at(5));
    assert_eq!(
        m.clients.list.len(),
        3,
        "no master: every ssh process to the host is listed"
    );
    m.apply(
        Input::Connections(parse_lsof(0, LSOF_NORMAL, "").unwrap()),
        at(6),
    );
    assert!(
        m.clients.list.iter().all(|c| !c.direct),
        "none of them is in this lsof output"
    );
}

#[test]
fn probe_failed_and_recovered_with_n_a_gaps() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    let ev = m.apply(
        Input::Failed {
            probe: ProbeKind::Traffic,
            reason: "exit 1".into(),
        },
        at(1),
    );
    assert_eq!(
        kinds(&ev),
        vec![EventKind::ProbeFailed {
            probe: ProbeKind::Traffic,
            reason: "exit 1".into()
        }]
    );
    assert_eq!(ev[0].severity(), Severity::Yellow);
    assert!(m.probes.is_failed(ProbeKind::Traffic));
    assert_eq!(m.traffic.current, None);
    assert_eq!(m.traffic.rx.latest().unwrap().bps, None, "gap, not zero");
    assert!(
        m.apply(
            Input::Failed {
                probe: ProbeKind::Traffic,
                reason: "exit 1".into()
            },
            at(2)
        )
        .is_empty()
    );
    let ev = m.apply(traffic(1, 1), at(3));
    assert_eq!(
        kinds(&ev),
        vec![EventKind::ProbeRecovered {
            probe: ProbeKind::Traffic
        }]
    );
    assert!(!m.probes.is_failed(ProbeKind::Traffic));
    // A failed ping is a gap, not a lost ping.
    m.apply(
        Input::Failed {
            probe: ProbeKind::Latency,
            reason: "spawn".into(),
        },
        at(4),
    );
    assert_eq!(m.latency.consecutive_lost, 0);
    assert_eq!(m.latency.stats().lost, 0);
}

#[test]
fn system_resumed_after_wall_clock_jump_and_the_gap_has_no_data() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    m.apply(traffic(1000, 1000), at(1));
    m.apply(traffic(2000, 2000), at(2));
    assert_eq!(m.traffic.current, Some((1000.0, 1000.0)));
    assert!(
        m.apply(Input::Tick, at(3).with_wall(WALL_ORIGIN_MS + 3000 + 9_000))
            .is_empty(),
        "9s jump is below the threshold"
    );
    // Laptop slept 10 minutes.
    let resumed = at(4).with_wall(WALL_ORIGIN_MS + 4000 + 9_000 + 600_000);
    let ev = m.apply(traffic(9_000_000, 2000), resumed);
    assert_eq!(
        kinds(&ev)[0],
        EventKind::SystemResumed {
            gap: Duration::from_secs(600)
        }
    );
    assert_eq!(ev[0].severity(), Severity::Dim);
    assert_eq!(m.traffic.current, None, "no rate across the sleep gap");
    assert_eq!(m.state(), State::Up, "stall clock restarted, not tripped");
    let ev = m.apply(
        traffic(9_001_000, 2000),
        at(5).with_wall(resumed.wall + 1000),
    );
    assert!(ev.is_empty());
    assert_eq!(m.traffic.current, Some((1000.0, 0.0)));
}

#[test]
fn traffic_is_a_gap_while_down_and_rates_resume_with_a_new_master() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    m.apply(traffic(0, 0), at(1));
    m.apply(traffic(500, 100), at(2));
    assert_eq!(m.traffic.current, Some((500.0, 100.0)));
    m.apply(down(), at(3));
    m.apply(Input::Traffic(None), at(4));
    assert_eq!(m.traffic.rx.latest().unwrap().bps, None);
    m.apply(up(2), at(5));
    m.apply(traffic(10, 10), at(6));
    assert_eq!(m.traffic.current, None, "first sample of the new master");
    m.apply(traffic(20, 10), at(7));
    assert_eq!(m.traffic.current, Some((10.0, 0.0)));
}

#[test]
fn clear_events_empties_the_log_but_not_the_state() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    run(&mut m, 1, [lost(), lost(), lost()]);
    assert_eq!(m.events.len(), 2);
    m.clear_events();
    assert!(m.events.is_empty());
    assert_eq!(m.state(), State::Degraded);
}

#[test]
fn full_scenario_up_loss_degraded_recovered_down_loop_up() {
    let mut m = monitor();
    let mut all = Vec::new();
    all.extend(m.apply(tail(""), at(0)));
    all.extend(m.apply(up(1), at(0)));
    all.extend(run(&mut m, 1, [lost(), lost(), lost()]));
    all.extend(run(&mut m, 4, (0..5).map(|_| ping(12.0))));
    all.extend(m.apply(down(), at(20)));
    let mut text = String::from(
        "2026-10-09 16:46:40  master ended after 20s (rc=255) — healthy run, backoff reset\n",
    );
    all.extend(m.apply(tail(&text), at(21)));
    let mut t = 22;
    for n in 0..3u64 {
        text.push_str(&format!(
            "2026-10-09 16:5{n}:00  opening master connection\n"
        ));
        all.extend(m.apply(tail(&text), at(t)));
        t += 75;
        text.push_str(&format!(
            "2026-10-09 16:5{n}:75  master ended after 75s (rc=255) — healthy run, backoff reset\n"
        ));
        all.extend(m.apply(tail(&text), at(t)));
        all.extend(m.apply(down(), at(t)));
        t += 1;
    }
    all.extend(m.apply(up(2), at(t)));
    let k = kinds(&all);
    assert_eq!(
        k,
        vec![
            EventKind::MasterUp { pid: 1 },
            EventKind::Degraded(DegradeReason::Loss { consecutive: 3 }),
            EventKind::Recovered {
                degraded_for: Duration::from_secs(5)
            },
            EventKind::MasterDown {
                was_up: None,
                supervisor: None
            },
            EventKind::ReconnectLoop {
                attempts: 3,
                avg_secs: 75
            },
            EventKind::MasterRestarted { pid: 2 },
            EventKind::ReconnectLoopEnded { attempts: 3 },
        ]
    );
    let stored = m
        .events
        .oldest_first()
        .find(|e| matches!(e.kind, EventKind::MasterDown { .. }))
        .unwrap();
    assert_eq!(
        stored.kind,
        EventKind::MasterDown {
            was_up: None,
            supervisor: Some(MasterExit {
                after_s: 20,
                rc: 255
            })
        }
    );
    assert_eq!(m.state(), State::Up);
    assert_eq!(m.events.newest_first().count(), 7);
}

#[test]
fn traffic_probe_failure_never_reports_a_stall() {
    let mut m = monitor(); // stall limit 35s
    m.apply(up(1), at(0));
    m.apply(traffic(100, 100), at(1));
    // nettop fails for a minute while pings are clean: nobody measured any silence.
    for t in 2..=60 {
        m.apply(ping(10.0), at(t));
        let ev = m.apply(
            Input::Failed {
                probe: ProbeKind::Traffic,
                reason: "exit 1".into(),
            },
            at(t),
        );
        assert!(
            ev.iter()
                .all(|e| matches!(e.kind, EventKind::ProbeFailed { .. })),
            "t={t}: {ev:?}"
        );
    }
    assert_eq!(m.state(), State::Up);
    // Dead-PID rows (no data) are gaps too (the first one also recovers the probe).
    for t in 61..=100 {
        m.apply(ping(10.0), at(t));
        let ev = m.apply(Input::Traffic(None), at(t));
        assert!(
            ev.iter()
                .all(|e| matches!(e.kind, EventKind::ProbeRecovered { .. })),
            "t={t}: {ev:?}"
        );
    }
    assert_eq!(m.state(), State::Up);
    // Measured silence still counts once counters flow again.
    m.apply(traffic(200, 200), at(101));
    for t in 102..=136 {
        m.apply(ping(10.0), at(t));
        m.apply(traffic(200, 200), at(t));
    }
    assert_eq!(m.state(), State::Up, "35s measured silence is not > 35s");
    let ev = m.apply(traffic(200, 200), at(137));
    assert!(
        matches!(
            kinds(&ev)[..],
            [EventKind::Degraded(DegradeReason::Stall { .. })]
        ),
        "{ev:?}"
    );
}

#[test]
fn sustained_slowness_stays_degraded_past_the_baseline_window() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, (0..10).map(|_| ping(10.0)));
    let ev = run(&mut m, 11, (0..5).map(|_| ping(300.0)));
    assert!(
        ev.iter()
            .any(|e| matches!(e.kind, EventKind::Degraded(DegradeReason::Slow { .. }))),
        "{ev:?}"
    );
    // Six minutes of 300ms pings: the UP baseline samples are long outside the window.
    let ev = run(&mut m, 16, (0..360).map(|_| ping(300.0)));
    assert!(ev.is_empty(), "no false recovery: {ev:?}");
    assert_eq!(m.state(), State::Degraded);
    assert_eq!(m.latency.baseline(), Some(10.0), "frozen while not UP");
    let ev = run(&mut m, 376, (0..7).map(|_| ping(10.0)));
    assert!(
        ev.iter()
            .any(|e| matches!(e.kind, EventKind::Recovered { .. })),
        "{ev:?}"
    );
    assert_eq!(m.state(), State::Up);
}

#[test]
fn recovery_needs_ping_replies_not_just_silence() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, (0..10).map(|_| ping(10.0)));
    run(&mut m, 11, (0..5).map(|_| ping(300.0)));
    assert_eq!(m.state(), State::Degraded);
    let ev = run(
        &mut m,
        16,
        (0..20).map(|_| Input::Failed {
            probe: ProbeKind::Latency,
            reason: "spawn".into(),
        }),
    );
    assert!(
        ev.iter()
            .all(|e| matches!(e.kind, EventKind::ProbeFailed { .. })),
        "{ev:?}"
    );
    assert_eq!(m.state(), State::Degraded);
    assert!(run(&mut m, 36, (0..50).map(|_| Input::Tick)).is_empty());
    assert_eq!(m.state(), State::Degraded, "ticks alone never recover");
}

#[test]
fn path_change_detected_across_a_failed_tailscale_sample_and_resets_baseline() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    m.apply(path(TS_WAN), at(0));
    run(&mut m, 1, (0..10).map(|_| ping(10.0)));
    assert!(m.latency.baseline().is_some());
    m.apply(
        Input::Failed {
            probe: ProbeKind::Path,
            reason: "exit 1".into(),
        },
        at(11),
    );
    assert_eq!(m.path.kind(), None, "title shows n/a meanwhile");
    let ev = m.apply(path(TS_DERP), at(12));
    assert!(
        ev.iter()
            .any(|e| matches!(e.kind, EventKind::PathChanged { .. })),
        "{ev:?}"
    );
    assert_eq!(m.latency.baseline(), None, "reset despite the gap");
}

#[test]
fn storm_culprit_persists_through_down_without_repeat_events() {
    let mut m = monitor();
    m.apply(up(22489), at(0));
    let conns = parse_lsof(0, LSOF_POST, "").unwrap();
    let ev = m.apply(Input::Connections(conns.clone()), at(1));
    assert_eq!(ev.len(), 2);
    m.apply(down(), at(2));
    // Culprits still connected while the master is gone: rule (b) is not evaluated.
    let rogues: Vec<TcpConn> = conns.iter().filter(|c| c.pid != 22489).cloned().collect();
    assert!(m.apply(Input::Connections(rogues), at(3)).is_empty());
    assert!(m.storm.active(), "not cleared by the master going away");
    m.apply(up(22489), at(4));
    assert!(
        m.apply(Input::Connections(conns), at(5)).is_empty(),
        "same culprits: once per culprit"
    );
    let only_master: Vec<TcpConn> = parse_lsof(0, LSOF_POST, "")
        .unwrap()
        .into_iter()
        .filter(|c| c.pid == 22489)
        .collect();
    let ev = m.apply(Input::Connections(only_master), at(6));
    assert!(matches!(kinds(&ev)[..], [EventKind::StormCleared { .. }]));
}

#[test]
fn pings_lost_while_down_do_not_degrade_the_new_master() {
    let mut m = monitor_no_stall();
    m.apply(down(), at(0));
    run(&mut m, 1, (0..10).map(|_| lost()));
    assert_eq!(m.latency.consecutive_lost, 10);
    let ev = m.apply(up(1), at(11));
    assert_eq!(kinds(&ev), vec![EventKind::MasterUp { pid: 1 }]);
    assert_eq!(m.state(), State::Up);
    assert_eq!(m.latency.consecutive_lost, 0);
    assert!(m.apply(Input::Tick, at(12)).is_empty());
    assert_eq!(m.state(), State::Up);
}

#[test]
fn supervisor_exit_line_seen_long_before_down_is_not_merged() {
    let mut m = monitor();
    m.apply(tail(""), at(0));
    m.apply(up(1), at(0));
    let line =
        "2026-10-09 16:46:40  master ended after 10s (rc=255) — healthy run, backoff reset\n";
    m.apply(tail(line), at(10));
    m.apply(down(), at(16));
    let ev = m
        .events
        .newest_first()
        .find(|e| matches!(e.kind, EventKind::MasterDown { .. }))
        .unwrap();
    assert!(matches!(
        ev.kind,
        EventKind::MasterDown {
            supervisor: None,
            ..
        }
    ));
}

#[test]
fn attempts_ending_while_the_status_probe_fails_are_inconclusive() {
    let mut m = monitor();
    m.apply(tail(""), at(0));
    m.apply(
        Input::Failed {
            probe: ProbeKind::Status,
            reason: "timeout".into(),
        },
        at(0),
    );
    let mut text = String::new();
    let mut t = 1;
    for n in 0..4u64 {
        text.push_str(&format!(
            "2026-10-09 16:5{n}:00  opening master connection\n"
        ));
        m.apply(tail(&text), at(t));
        t += 75;
        text.push_str(&format!(
            "2026-10-09 16:5{n}:75  master ended after 75s (rc=255) — healthy run, backoff reset\n"
        ));
        let ev = m.apply(tail(&text), at(t));
        assert!(ev.is_empty(), "{ev:?}");
        m.apply(
            Input::Failed {
                probe: ProbeKind::Status,
                reason: "timeout".into(),
            },
            at(t),
        );
        t += 2;
    }
    assert!(
        !m.supervisor.loop_active(),
        "nobody observed those attempts"
    );
    assert_eq!(m.supervisor.streak_attempts, 0);
}

#[test]
fn huge_uptime_and_age_never_panic() {
    let mut m = monitor();
    m.apply(up(1), at(0));
    m.apply(Input::Uptime(Duration::MAX), at(1));
    let ev = m.apply(down(), at(2));
    assert!(matches!(
        kinds(&ev)[..],
        [EventKind::MasterDown {
            was_up: Some(Duration::MAX),
            ..
        }]
    ));
    let procs = vec![ssh_monitor_core::parse::Process {
        pid: 9,
        ppid: 1,
        etime: Duration::MAX,
        command: "ssh fedora".into(),
    }];
    m.apply(Input::Processes(procs), at(3));
    assert_eq!(m.clients.list[0].age_at(at(1000)), Duration::MAX);
}

#[test]
fn system_resume_marks_a_latency_gap_too() {
    let mut m = monitor_no_stall();
    m.apply(up(1), at(0));
    run(&mut m, 1, (0..5).map(|_| ping(10.0)));
    let resumed = at(6).with_wall(WALL_ORIGIN_MS + 6000 + 600_000);
    let ev = m.apply(ping(10.0), resumed);
    assert!(matches!(kinds(&ev)[..], [EventKind::SystemResumed { .. }]));
    let samples: Vec<Option<f64>> = m.latency.samples.iter().map(|s| s.rtt_ms).collect();
    assert_eq!(
        samples[samples.len() - 2],
        None,
        "gap marker before the first post-sleep ping"
    );
    assert_eq!(m.latency.stats().lost, 0, "a gap is not a lost ping");
}

#[test]
fn whole_outage_log_replay_counts_only_attempts_that_never_connected() {
    // Attempts shorter than this are treated as never connected by the replay (we feed DOWN);
    // longer ones get an UP observation in the middle, as a real connection would produce.
    const CONNECTED_IF_LONGER_THAN: u64 = 120;
    let lines: Vec<&str> = SUP_OUTAGE.lines().collect();
    let parsed = parse_supervisor_log(SUP_OUTAGE);
    let t0 = parsed[0].naive_secs;
    let stamp_of = |i: usize| at(u64::try_from(parsed[i].naive_secs - t0).unwrap() * 2 + 10);
    let mut m = monitor();
    m.apply(tail(&format!("{}\n", lines[..6].join("\n"))), stamp_of(5));
    m.apply(down(), stamp_of(5));
    let mut fired = Vec::new();
    let mut pid = 100;
    for i in 6..lines.len() {
        let text = format!("{}\n", lines[i.saturating_sub(49)..=i].join("\n"));
        let stamp = stamp_of(i);
        fired.extend(m.apply(tail(&text), stamp));
        match parsed[i].kind {
            ssh_monitor_core::parse::SupervisorKind::Opening => {
                let ends_long = parsed[i + 1..]
                    .iter()
                    .find_map(|l| match l.kind {
                        ssh_monitor_core::parse::SupervisorKind::Ended { after_s, .. } => {
                            Some(after_s > CONNECTED_IF_LONGER_THAN)
                        }
                        _ => None,
                    })
                    .unwrap_or(false);
                if ends_long {
                    pid += 1;
                    fired.extend(m.apply(up(pid), at(stamp.mono.as_secs() + 1)));
                }
            }
            ssh_monitor_core::parse::SupervisorKind::Ended { .. } => {
                fired.extend(m.apply(down(), at(stamp.mono.as_secs() + 1)));
            }
            _ => {}
        }
    }
    // Expected loops: maximal runs of ≥3 consecutive short attempts after the start line.
    let mut runs = Vec::new();
    let mut streak = 0u32;
    for l in &parsed[6..] {
        if let ssh_monitor_core::parse::SupervisorKind::Ended { after_s, .. } = l.kind {
            if after_s > CONNECTED_IF_LONGER_THAN {
                if streak >= 3 {
                    runs.push((streak, true));
                }
                streak = 0;
            } else {
                streak += 1;
            }
        }
    }
    if streak >= 3 {
        runs.push((streak, false));
    }
    let loops: Vec<&Event> = fired
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReconnectLoop { .. }))
        .collect();
    let ended: Vec<&Event> = fired
        .iter()
        .filter(|e| matches!(e.kind, EventKind::ReconnectLoopEnded { .. }))
        .collect();
    assert_eq!(
        loops.len(),
        runs.len(),
        "one `reconnect loop` per run: {runs:?}"
    );
    assert_eq!(
        ended.len(),
        runs.iter().filter(|(_, ended)| *ended).count(),
        "one `loop ended` per run that a real connection ended"
    );
    for (ev, (attempts, _)) in ended.iter().zip(runs.iter().filter(|(_, e)| *e)) {
        assert_eq!(
            ev.kind,
            EventKind::ReconnectLoopEnded {
                attempts: *attempts
            }
        );
    }
    assert!(
        runs.len() >= 2,
        "the fixture has the 75s loop and the final 45s failures"
    );
    // The log ends inside the `died after 45s — failure N/8` streak (its labels are ignored).
    assert!(m.supervisor.loop_active());
    assert_eq!(m.supervisor.streak_attempts, runs.last().unwrap().0);
    assert_eq!(m.supervisor.streak_avg_secs(), Some(45));
}
