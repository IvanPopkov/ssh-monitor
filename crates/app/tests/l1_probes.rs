//! L1: SPEC F7 probe cadences and start order (status first) through the real loop.

mod common;

use common::{FakeClock, FakeRunner};
use ssh_monitor::app::{self, Headless, Session};
use ssh_monitor::config::Host;
use ssh_monitor_core::parse::HostConfig;
use ssh_monitor_testkit::{Phase, Scenario};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn host() -> Host {
    Host {
        alias: "fedora".into(),
        cfg: HostConfig {
            hostname: "100.100.1.10".into(),
            port: 22,
            user: Some("user".into()),
            control_path: Some("/tmp/cm.sock".into()),
            server_alive_interval: Some(15),
            control_master: Some("auto".into()),
        },
        supervisor_log: Some(PathBuf::from(
            "/Users/user/.local/state/ssh-master-fedora.log",
        )),
    }
}

async fn run_for(
    scenario: Scenario,
    secs: u64,
) -> (Arc<FakeRunner>, ssh_monitor_core::monitor::Monitor) {
    let clock = Arc::new(FakeClock::new());
    let runner = Arc::new(FakeRunner::new(scenario, Arc::clone(&clock)));
    let session = Session {
        host: host(),
        runner: runner.clone(),
        clock,
        interval: Duration::from_secs(1),
        csv: None,
        utc_offset_secs: 0,
    };
    let monitor = app::run(
        session,
        &mut Headless,
        None,
        Some(Duration::from_secs(secs)),
    )
    .await
    .unwrap();
    (runner, monitor)
}

#[tokio::test(start_paused = true)]
async fn probes_run_at_spec_cadences() {
    let (runner, monitor) = run_for(Scenario::new(), 20).await;
    assert_eq!(monitor.state(), ssh_monitor_core::monitor::State::Up);
    let n = |prefix: &[&str]| runner.count(prefix);
    // 1×: every second from 0 to 20 → 21 (±1 at the deadline edge).
    assert!((20..=22).contains(&n(&["ping"])), "ping {}", n(&["ping"]));
    assert!(
        (20..=22).contains(&n(&["nettop"])),
        "nettop {}",
        n(&["nettop"])
    );
    // 2×: 0, 2, …, 20 → 11.
    for probe in [
        &["ssh", "-O", "check"][..],
        &["ps", "-axo"][..],
        &["lsof"][..],
        &["tail"][..],
        &["ps", "-o", "etime="][..],
    ] {
        let c = n(probe);
        assert!((10..=12).contains(&c), "{probe:?}: {c}");
    }
    // 10×: 0, 10, 20 → 3.
    let ts = n(&["tailscale"]);
    assert!((2..=4).contains(&ts), "tailscale {ts}");
    for (_, argv) in runner.calls() {
        if argv[0] == "ssh" {
            assert_eq!(&argv[1..], ["-O", "check", "fedora"], "{argv:?}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn status_runs_first_and_counters_start_after_the_pid_is_known() {
    let (runner, monitor) = run_for(Scenario::new(), 3).await;
    let calls = runner.calls();
    assert_eq!(
        calls[0].1[..3],
        ["ssh", "-O", "check"],
        "status probe first: {calls:?}"
    );
    let first_status_at = calls[0].0;
    for (at, argv) in &calls[1..] {
        assert!(
            *at >= first_status_at,
            "{argv:?} ran before the status probe"
        );
    }
    assert!(runner.count(&["nettop"]) >= 2);
    assert!(
        monitor.traffic.current.is_some(),
        "rates from the first two nettop samples"
    );
    // The master never shows up as a client for one poll.
    assert!(
        monitor
            .events
            .oldest_first()
            .all(|e| !matches!(&e.kind, ssh_monitor_core::monitor::EventKind::ClientsChanged { added, .. } if !added.is_empty())),
        "no client events for the master itself"
    );
}

#[tokio::test(start_paused = true)]
async fn probes_that_need_a_pid_stay_idle_while_down() {
    let (runner, monitor) = run_for(Scenario::new().starting_with(Phase::down()), 10).await;
    assert_eq!(monitor.state(), ssh_monitor_core::monitor::State::Down);
    assert_eq!(runner.count(&["nettop"]), 0);
    assert_eq!(runner.count(&["ps", "-o", "etime="]), 0);
    assert!(runner.count(&["ping"]) >= 10, "keeps pinging while DOWN");
}
