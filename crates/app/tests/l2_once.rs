//! L2: the real binary with `fake-cmd` as every tool on PATH.

mod common;

use common::{FAKE_TOOLS, FakeEnv};
use ssh_monitor_core::tape::read_jsonl;
use ssh_monitor_testkit::{ClientSpec, Phase, Ping, Scenario};

fn within(v: &serde_json::Value, expected: f64, tolerance: f64) -> bool {
    v.as_f64()
        .is_some_and(|x| (x - expected).abs() <= expected * tolerance)
}

#[test]
fn l2_once_up_prints_json_and_exits_0() {
    let env = FakeEnv::new("up", &Scenario::new(), &FAKE_TOOLS);
    let (code, json, stderr) = env.run_once(&[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(json["state"], "UP");
    assert_eq!(json["exit_code"], 0);
    assert_eq!(json["pid"], 1000);
    assert_eq!(json["host"], "fedora");
    assert_eq!(json["hostname"], "100.100.1.10");
    // The scenario grows counters at 1000 B/s in, 500 B/s out.
    assert!(
        within(&json["rx_bps"], 1000.0, 0.2),
        "rx_bps: {}",
        json["rx_bps"]
    );
    assert!(
        within(&json["tx_bps"], 500.0, 0.2),
        "tx_bps: {}",
        json["tx_bps"]
    );
    assert_eq!(json["rtt_ms"], 10.0);
    assert_eq!(json["loss"], 0.0);
    assert_eq!(json["path"], "direct-lan");
    assert_eq!(json["storm"], false);
    assert_eq!(json["badges"], serde_json::json!([]));
    assert_eq!(json["probes_failed"], serde_json::json!([]));
    assert_eq!(json["events"][0], "master up (pid 1000)");
    assert!(json["master_uptime_s"].is_number());
    assert_eq!(json["supervisor_log"], true, "tail answered: log readable");
    assert!(json["sampled_s"].as_u64().unwrap() >= 2);
}

#[test]
fn l2_once_degraded_on_lost_pings_exits_1() {
    // Lost pings take ~1s each, so the third lands near 3s and `--once` must wait for it.
    let scenario = Scenario::new().starting_with(Phase::default().with_ping(Ping::Lost));
    let env = FakeEnv::new("degraded", &scenario, &FAKE_TOOLS);
    let (code, json, stderr) = env.run_once(&[]);
    assert_eq!(code, 1, "stderr: {stderr}\n{json}");
    assert_eq!(json["state"], "DEGRADED");
    assert_eq!(json["loss"], 100.0);
    assert!(json["rtt_ms"].is_null());
    assert!(json["rtt"]["lost"].as_u64().unwrap() >= 3, "{json}");
    let reason = json["degraded_reason"].as_str().unwrap();
    assert!(reason.contains("lost pings"), "{reason}");
    let events = json["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.as_str().unwrap().starts_with("degraded: 3 lost pings")),
        "{events:?}"
    );
}

#[test]
fn l2_once_down_exits_2() {
    let scenario = Scenario::new().starting_with(Phase::down());
    let env = FakeEnv::new("down", &scenario, &FAKE_TOOLS);
    let (code, json, stderr) = env.run_once(&[]);
    assert_eq!(code, 2, "stderr: {stderr}");
    assert_eq!(json["state"], "DOWN");
    assert!(json["pid"].is_null());
    assert!(json["rx_bps"].is_null(), "no master: no counters, not zero");
    assert_eq!(json["rtt_ms"], 10.0, "keeps pinging while DOWN");
    assert_eq!(json["events"][0], "master down");
}

#[test]
fn l2_once_without_status_answer_is_not_down_exits_3() {
    // `ssh -O check` hangs: the state is unknown, never DOWN by default.
    let scenario = Scenario::new().starting_with(Phase::default().with_status_hang(true));
    let env = FakeEnv::new("hang", &scenario, &FAKE_TOOLS);
    let csv = env.dir.join("log.csv");
    let (code, json, stderr) = env.run_once(&["--log", csv.to_str().unwrap()]);
    assert_eq!(code, 3, "stderr: {stderr}\n{json}");
    assert_eq!(json["state"], "n/a");
    assert!(json["pid"].is_null());
    let failed = json["probes_failed"].as_array().unwrap();
    assert!(
        failed.iter().any(|p| p == "ssh -O check"),
        "status timeout must surface inside the window: {json}"
    );
    let text = std::fs::read_to_string(&csv).unwrap();
    assert!(
        !text.contains(",DOWN,"),
        "no made-up DOWN rows before the first status answer: {text}"
    );
}

#[test]
fn l2_once_storm_from_non_mux_connection() {
    let scenario = Scenario::new().starting_with(Phase::default().with_clients(vec![
        ClientSpec::dev_session(2001, 1),
        ClientSpec::vscode(2002, true),
    ]));
    let env = FakeEnv::new("storm", &scenario, &FAKE_TOOLS);
    let (code, json, stderr) = env.run_once(&[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(json["state"], "UP");
    assert_eq!(json["storm"], true);
    assert_eq!(json["badges"], serde_json::json!(["STORM"]));
    let events = json["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.as_str().unwrap() == "storm: non-mux ssh pid 2002 (ssh)"),
        "{events:?}"
    );
    let clients = json["clients"].as_array().unwrap();
    assert_eq!(clients.len(), 2);
    assert_eq!(clients[0]["label"], "#1");
    assert_eq!(clients[0]["direct"], false);
    assert_eq!(clients[1]["label"], "vscode");
    assert_eq!(clients[1]["direct"], true);
    assert_eq!(json["ssh_procs"], 3);
}

#[test]
fn l2_log_writes_the_spec_csv() {
    let env = FakeEnv::new("csv", &Scenario::new(), &FAKE_TOOLS);
    let csv = env.dir.join("log.csv");
    let (code, _json, stderr) = env.run_once(&["--log", csv.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let text = std::fs::read_to_string(&csv).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(
        lines[0],
        "ts,state,pid,rx_bps,tx_bps,rtt_ms,loss,path,clients,ssh_procs,events"
    );
    assert!(
        lines.len() >= 3,
        "a header and at least two rows in ~3s: {text}"
    );
    for row in &lines[1..] {
        let cols: Vec<&str> = row.splitn(11, ',').collect();
        assert_eq!(cols.len(), 11, "{row}");
        assert!(cols[0].ends_with('Z'), "ISO-8601 UTC timestamp: {row}");
        assert_eq!(cols[1], "UP", "{row}");
        assert_eq!(cols[2], "1000", "{row}");
    }
    let settled = lines[1..]
        .iter()
        .map(|row| row.splitn(11, ',').collect::<Vec<_>>())
        .find(|cols| !cols[3].is_empty())
        .unwrap_or_else(|| panic!("no row with rates: {text}"));
    let rx: f64 = settled[3].parse().unwrap();
    let tx: f64 = settled[4].parse().unwrap();
    assert!((800.0..=1200.0).contains(&rx), "rx_bps {rx}");
    assert!((400.0..=600.0).contains(&tx), "tx_bps {tx}");
    assert_eq!(
        &settled[5..10],
        ["10.0", "0.0", "direct-lan", "0", "1"],
        "{settled:?}"
    );
    assert!(
        text.contains("master up (pid 1000)"),
        "events column: {text}"
    );
}

#[test]
fn l2_missing_tool_is_na_plus_probe_failed_without_crash() {
    let tools: Vec<&str> = FAKE_TOOLS
        .iter()
        .copied()
        .filter(|t| *t != "nettop")
        .collect();
    let env = FakeEnv::new("missing", &Scenario::new(), &tools);
    let (code, json, stderr) = env.run_once(&[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(json["state"], "UP");
    assert!(json["rx_bps"].is_null(), "n/a, not zero: {json}");
    assert_eq!(json["probes_failed"], serde_json::json!(["nettop"]));
    let events = json["events"].as_array().unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.as_str().unwrap().starts_with("probe failed: nettop")),
        "{events:?}"
    );
}

#[test]
fn l2_record_writes_a_tape_that_replays_to_the_same_result() {
    let env = FakeEnv::new("record", &Scenario::new(), &FAKE_TOOLS);
    let rec = env.dir.join("rec");
    let (code, json, stderr) = env.run_once(&["--record", rec.to_str().unwrap()]);
    assert_eq!(code, 0, "stderr: {stderr}");
    let text = std::fs::read_to_string(rec.join("tape.jsonl")).unwrap();
    let tape = read_jsonl(&text).unwrap();
    assert!(tape.len() > 10, "{} entries", tape.len());
    assert_eq!(tape[0].cmd, ["ssh", "-G", "fedora"]);
    assert!(
        tape.iter()
            .any(|e| e.cmd == ["ssh", "-O", "check", "fedora"] && e.exit == 0)
    );
    assert!(
        tape.iter()
            .any(|e| e.cmd[0] == "nettop" && e.stdout.contains("ssh.1000,"))
    );
    for e in tape.iter().filter(|e| e.cmd[0] == "ssh") {
        assert!(
            e.cmd == ["ssh", "-G", "fedora"] || e.cmd == ["ssh", "-O", "check", "fedora"],
            "unexpected ssh form: {:?}",
            e.cmd
        );
    }
    assert!(tape.iter().all(|e| e.exit >= 0), "no hangs recorded");
    let replay = FakeEnv::from_tape("replay", &tape, &FAKE_TOOLS);
    let (code, again, stderr) = replay.run_once(&[]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(again["state"], json["state"]);
    assert_eq!(again["pid"], json["pid"]);
    assert_eq!(again["path"], json["path"]);
    assert_eq!(again["events"][0], "master up (pid 1000)");
}

#[test]
fn l2_no_control_path_is_an_error_exit_3() {
    let scenario = Scenario::new().with_control_path(None);
    let env = FakeEnv::new("nocp", &scenario, &FAKE_TOOLS);
    let (code, stdout, stderr) = env.run(&["--once"]);
    assert_eq!(code, 3, "stdout: {stdout}");
    assert!(stderr.contains("ControlPath"), "{stderr}");
}

#[test]
fn l2_cli_help_version_and_bad_args() {
    let env = FakeEnv::new("cli", &Scenario::new(), &FAKE_TOOLS);
    let (code, stdout, _) = env.run(&["--help"]);
    assert_eq!(code, 0);
    assert!(stdout.contains("--once"));
    let (code, stdout, _) = env.run(&["--version"]);
    assert_eq!(code, 0);
    assert!(stdout.starts_with("ssh-monitor "));
    let (code, _, stderr) = env.run(&["--interval", "0.1"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("minimum"));
    let (code, _, stderr) = env.run(&["--what"]);
    assert_eq!(code, 3);
    assert!(stderr.contains("unknown argument"));
}

#[test]
fn l2_once_with_half_second_interval() {
    let env = FakeEnv::new("fast", &Scenario::new(), &FAKE_TOOLS);
    let (code, json, stderr) = env.run_once(&["--interval", "0.5"]);
    assert_eq!(code, 0, "stderr: {stderr}");
    assert_eq!(json["state"], "UP");
    assert!(json["rtt"]["replies"].as_u64().unwrap() >= 4, "{json}");
    assert!(json["sampled_s"].as_u64().unwrap() >= 2);
}
