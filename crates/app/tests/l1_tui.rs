//! L1: the whole loop on a paused tokio clock with a fake runner, drawn into a `TestBackend`.

mod common;

use common::{FakeClock, FakeRunner};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::Color;
use ssh_monitor::app::{self, Frontend, Key, Session, View};
use ssh_monitor::cli::TUI_INTERVAL;
use ssh_monitor::config::Host;
use ssh_monitor::csv::CsvLog;
use ssh_monitor::ui::render::render;
use ssh_monitor_core::monitor::{EventKind, Monitor, State};
use ssh_monitor_core::parse::HostConfig;
use ssh_monitor_testkit::{ClientSpec, Phase, Ping, Scenario, reconnect_loop_log};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

#[derive(Clone)]
struct Frame {
    rows: Vec<String>,
    fg: Vec<Vec<Color>>,
}

impl Frame {
    fn text(&self) -> String {
        self.rows.join("\n")
    }

    fn row_of(&self, needle: &str) -> usize {
        self.rows
            .iter()
            .position(|r| r.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not on screen:\n{}", self.text()))
    }

    fn color_of(&self, needle: &str) -> Color {
        let row = self.row_of(needle);
        let col = self.rows[row].find(needle).unwrap();
        let idx = self.rows[row][..col].chars().count();
        self.fg[row][idx]
    }

    fn braille_colors(&self, from: usize, to: usize) -> Vec<Color> {
        (from..to)
            .flat_map(|r| {
                self.rows[r]
                    .chars()
                    .enumerate()
                    .filter(|(_, c)| ('\u{2801}'..='\u{28FF}').contains(c))
                    .map(move |(i, _)| self.fg[r][i])
            })
            .collect()
    }

    fn has_braille(&self) -> bool {
        self.rows
            .iter()
            .any(|r| r.chars().any(|c| ('\u{2801}'..='\u{28FF}').contains(&c)))
    }
}

struct Screen {
    terminal: Terminal<TestBackend>,
    frames: Arc<Mutex<Vec<Frame>>>,
}

impl Screen {
    fn new(w: u16, h: u16) -> (Self, Arc<Mutex<Vec<Frame>>>) {
        let frames = Arc::new(Mutex::new(Vec::new()));
        let screen = Self {
            terminal: Terminal::new(TestBackend::new(w, h)).unwrap(),
            frames: Arc::clone(&frames),
        };
        (screen, frames)
    }
}

impl Frontend for Screen {
    fn draw(&mut self, view: &View<'_>) -> anyhow::Result<()> {
        self.terminal.draw(|f| render(f, view))?;
        let buf = self.terminal.backend().buffer();
        let w = buf.area().width as usize;
        let rows = buf
            .content()
            .chunks(w)
            .map(|row| row.iter().map(|c| c.symbol()).collect::<String>())
            .collect();
        let fg = buf
            .content()
            .chunks(w)
            .map(|row| row.iter().map(|c| c.fg).collect::<Vec<_>>())
            .collect();
        self.frames.lock().unwrap().push(Frame { rows, fg });
        Ok(())
    }
}

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

struct Outcome {
    frames: Vec<Frame>,
    monitor: Monitor,
    elapsed: Duration,
}

impl Outcome {
    fn last(&self) -> &Frame {
        self.frames.last().expect("at least one frame")
    }

    fn frame_with(&self, needle: &str) -> &Frame {
        self.frames
            .iter()
            .find(|f| f.rows[0].contains(needle))
            .unwrap_or_else(|| panic!("no frame with {needle:?} in the status bar"))
    }
}

async fn run_tui(
    scenario: Scenario,
    size: (u16, u16),
    secs: u64,
    keys: Option<mpsc::Receiver<Key>>,
    csv: Option<CsvLog>,
) -> Outcome {
    run_tui_every(Duration::from_secs(1), scenario, size, secs, keys, csv).await
}

async fn run_tui_every(
    interval: Duration,
    scenario: Scenario,
    (w, h): (u16, u16),
    secs: u64,
    keys: Option<mpsc::Receiver<Key>>,
    csv: Option<CsvLog>,
) -> Outcome {
    let clock = Arc::new(FakeClock::new());
    let runner = Arc::new(FakeRunner::new(scenario, Arc::clone(&clock)));
    let session = Session {
        host: host(),
        runner,
        clock: clock.clone(),
        interval,
        csv,
        utc_offset_secs: 0,
    };
    let (mut screen, frames) = Screen::new(w, h);
    let start = tokio::time::Instant::now();
    let monitor = app::run(session, &mut screen, keys, Some(Duration::from_secs(secs)))
        .await
        .unwrap();
    let frames = frames.lock().unwrap().clone();
    // `SSHMON_DUMP_FRAMES=1 cargo test --test l1_tui -- --nocapture` prints each test's last frame.
    if std::env::var_os("SSHMON_DUMP_FRAMES").is_some()
        && let Some(last) = frames.last()
    {
        println!("\n{}\n", last.text());
    }
    Outcome {
        frames,
        monitor,
        elapsed: start.elapsed(),
    }
}

fn keys_at(schedule: Vec<(u64, Key)>) -> mpsc::Receiver<Key> {
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let mut at = 0;
        for (t, key) in schedule {
            tokio::time::sleep(Duration::from_secs(t - at)).await;
            at = t;
            let _ = tx.send(key).await;
        }
        // Keep the sender alive so the loop does not read "channel closed" as quit.
        tokio::time::sleep(Duration::from_secs(3600)).await;
    });
    rx
}

fn s(t: u64) -> Duration {
    Duration::from_secs(t)
}

/// UP → DEGRADED → recovered → DOWN (3-attempt reconnect loop, open breaker) → new master.
fn outage_scenario() -> Scenario {
    let log1 = reconnect_loop_log(1, 75);
    let log2 = reconnect_loop_log(2, 75);
    let log3 = reconnect_loop_log(3, 75);
    let log4 = format!(
        "{log3}2026-10-09 17:00:00  CIRCUIT OPEN — 8 rapid failures; standing down for 300s\n"
    );
    Scenario::new()
        .then_at(s(10), Phase::default().with_ping(Ping::Lost))
        .then_at(s(20), Phase::default())
        .then_at(s(30), Phase::down().with_autossh(Some(4242)))
        .then_at(
            s(32),
            Phase::down()
                .with_autossh(Some(4242))
                .with_supervisor_log(Some(&log1)),
        )
        .then_at(
            s(34),
            Phase::down()
                .with_autossh(Some(4242))
                .with_supervisor_log(Some(&log2)),
        )
        .then_at(
            s(36),
            Phase::down()
                .with_autossh(Some(4242))
                .with_supervisor_log(Some(&log3)),
        )
        .then_at(
            s(38),
            Phase::down()
                .with_autossh(Some(4242))
                .with_supervisor_log(Some(&log4)),
        )
        .then_at(
            s(44),
            Phase::default()
                .with_master(Some(2000))
                .with_supervisor_log(Some(&log4)),
        )
}

#[tokio::test(start_paused = true)]
async fn snapshots_at_64x16_90x16_90x29_90x30_and_120x40() {
    let scenario = Scenario::new().starting_with(Phase::default().with_clients(vec![
        ClientSpec::dev_session(2001, 1),
        ClientSpec::vscode(2002, true),
    ]));
    for size in [(64u16, 16u16), (90, 16), (90, 29), (90, 30), (120, 40)] {
        let out = run_tui(scenario.clone(), size, 12, None, None).await;
        let frame = out.last();
        assert_eq!(frame.rows.len(), size.1 as usize, "{size:?}");
        assert!(
            frame
                .rows
                .iter()
                .all(|r| r.chars().count() == size.0 as usize)
        );
        let status = &frame.rows[0];
        assert!(status.contains("● UP"), "{size:?}: {status}");
        assert_eq!(frame.color_of("● UP"), Color::Green);
        assert!(status.contains("⚠ STORM"), "{size:?}: {status}");
        assert_eq!(frame.color_of("⚠ STORM"), Color::Red);
        assert!(status.contains("fedora"), "{status}");
        assert!(status.contains(" up 12s"), "master uptime: {status}");
        assert!(status.contains("for 12s"), "time in state: {status}");
        assert!(status.trim_end().ends_with("00:00:12"), "clock: {status}");
        let text = frame.text();
        for needle in [
            "rx 1.0 kB/s · peak 1.0 kB/s",
            "tx 500 B/s · peak 500 B/s",
            "── latency · direct · LAN",
            "clients (2)",
            "── events",
            "master up (pid 1000)",
            "#1 12s",
            "⚠ vscode 12s",
            "now 10ms",
            "loss 0.0%",
            "max 10ms",
        ] {
            assert!(text.contains(needle), "{size:?} lacks {needle:?}:\n{text}");
        }
        assert_eq!(
            frame.color_of("⚠ vscode"),
            Color::Red,
            "direct client is red"
        );
        assert_eq!(frame.color_of("master up"), Color::Green);
        assert_eq!(
            frame.color_of("clients +#1"),
            Color::DarkGray,
            "dim client event"
        );
        let lat_row = frame.row_of("max 10ms");
        assert!(
            frame.rows[lat_row].contains("ms  "),
            "{}",
            frame.rows[lat_row]
        );
        let lat_title = frame.row_of("── latency");
        if size.1 < 30 {
            assert!(
                !text.contains("── tx "),
                "{size:?}: merged below 30 rows:\n{text}"
            );
            let traffic = frame.row_of("── ↓ rx ");
            assert_eq!(frame.color_of("↓ rx"), Color::Cyan);
            assert_eq!(frame.color_of("↑ tx"), Color::Magenta);
            let chart_rows = lat_title - traffic - 1;
            assert_eq!(chart_rows % 2, 0, "{size:?}: equal halves:\n{text}");
            let zero = traffic + 1 + chart_rows / 2;
            let up = frame.braille_colors(traffic + 1, zero);
            let down = frame.braille_colors(zero, lat_title);
            assert!(
                !up.is_empty() && up.iter().all(|c| *c == Color::Cyan),
                "{size:?}: rx above zero:\n{text}"
            );
            assert!(
                !down.is_empty() && down.iter().all(|c| *c == Color::Magenta),
                "{size:?}: tx below zero:\n{text}"
            );
            assert!(traffic < lat_title && lat_title < frame.row_of("── events"));
            if size.1 == 16 {
                // 2+2 chart rows + the saved tx title = 5 → 2 up, 2 down, the odd row to events.
                assert_eq!(chart_rows, 4, "{text}");
                assert_eq!(frame.rows.len() - frame.row_of("── events"), 6, "{text}");
            }
        } else {
            let rx_row = frame.row_of("── rx ");
            let tx_row = frame.row_of("── tx ");
            let chart = Frame {
                rows: frame.rows[rx_row + 1..tx_row].to_vec(),
                fg: Vec::new(),
            };
            assert!(
                chart.has_braille(),
                "{size:?}: rx chart has Braille columns:\n{text}"
            );
            let order = ["── rx ", "── tx ", "── latency", "── events"].map(|n| frame.row_of(n));
            assert!(order.windows(2).all(|w| w[0] < w[1]), "{order:?}");
            if size == (120, 40) {
                // 25 extra rows split 2:2:1 → rx 10, tx 10, events 5.
                assert_eq!(tx_row - rx_row, 13, "{text}");
                assert_eq!(lat_title - tx_row, 13);
                assert_eq!(frame.rows.len() - frame.row_of("── events"), 9);
            }
        }
    }
}

#[tokio::test(start_paused = true)]
async fn below_minimum_size_shows_too_small() {
    for size in [(60u16, 15u16), (64, 15), (63, 16)] {
        let out = run_tui(Scenario::new(), size, 3, None, None).await;
        assert!(out.last().text().contains("terminal too small"), "{size:?}");
    }
}

#[tokio::test(start_paused = true)]
async fn down_view_shows_no_master_rogues_and_a_gap_in_the_charts() {
    let scenario = Scenario::new().then_at(
        s(10),
        Phase::down().with_clients(vec![ClientSpec::plain(3001, "", true)]),
    );
    let out = run_tui(scenario, (90, 16), 30, None, None).await;
    let frame = out.last();
    let text = frame.text();
    assert_eq!(out.monitor.state(), State::Down);
    assert!(frame.rows[0].contains("○ DOWN"), "{text}");
    assert_eq!(frame.color_of("○ DOWN"), Color::Red);
    assert!(frame.rows[0].contains("down "), "{text}");
    assert!(frame.rows[0].contains("(was up "), "{text}");
    assert!(text.contains("clients · no master"), "{text}");
    assert!(
        text.contains("⚠ ssh"),
        "rogue ssh listed while DOWN: {text}"
    );
    assert!(text.contains("rx n/a"), "{text}");
    assert!(
        text.contains("now 10ms"),
        "keeps pinging while DOWN: {text}"
    );
    assert_eq!(frame.color_of("master down"), Color::Red);
    let traffic = frame.row_of("── ↓ rx ");
    let chart_rows = &frame.rows[traffic + 1..frame.row_of("── latency")];
    let cols: Vec<usize> = chart_rows
        .iter()
        .flat_map(|r| {
            r.chars()
                .enumerate()
                .filter(|(_, c)| ('\u{2801}'..='\u{28FF}').contains(c))
                .map(|(i, _)| i)
        })
        .collect();
    assert!(!cols.is_empty(), "history from the UP period:\n{text}");
    assert!(
        cols.iter().all(|i| *i < 84),
        "no columns in the newest seconds while DOWN: {cols:?}\n{text}"
    );
}

#[tokio::test(start_paused = true)]
async fn pause_freezes_the_display_while_csv_keeps_writing() {
    let path = std::env::temp_dir().join(format!("sshmon-l1-pause-{}.csv", std::process::id()));
    let _ = std::fs::remove_file(&path);
    let csv = CsvLog::open(&path).unwrap();
    let keys = keys_at(vec![(4, Key::Pause), (6, Key::Redraw)]);
    let out = run_tui(Scenario::new(), (90, 16), 12, Some(keys), Some(csv)).await;
    // Frames stop at the pause (4s at 250ms ≈ 16-17), plus the frozen one and its resize redraw.
    assert!(
        (12..=24).contains(&out.frames.len()),
        "frames after pausing at 4s of 12s: {}",
        out.frames.len()
    );
    let last = out.last();
    assert!(last.rows[0].contains("PAUSED"), "{}", last.text());
    assert!(
        last.rows[0].trim_end().ends_with("00:00:04"),
        "frozen clock even on the resize redraw at 6s: {}",
        last.rows[0]
    );
    let rows = std::fs::read_to_string(&path).unwrap().lines().count();
    assert!(rows >= 10, "CSV kept writing after the pause: {rows} lines");
    assert_eq!(out.monitor.state(), State::Up);
    let _ = std::fs::remove_file(&path);
}

#[tokio::test(start_paused = true)]
async fn clear_key_empties_the_events_panel_only() {
    let out = run_tui(
        Scenario::new(),
        (90, 16),
        6,
        Some(keys_at(vec![(3, Key::Clear)])),
        None,
    )
    .await;
    assert!(out.monitor.events.is_empty(), "on-screen log cleared");
    assert!(
        !out.last().text().contains("master up"),
        "{}",
        out.last().text()
    );
    assert_eq!(out.monitor.state(), State::Up, "state untouched");
    assert!(out.monitor.health.master_pid().is_some());
}

#[tokio::test(start_paused = true)]
async fn quit_key_exits_cleanly_before_the_deadline() {
    let out = run_tui(
        Scenario::new(),
        (90, 16),
        600,
        Some(keys_at(vec![(2, Key::Quit)])),
        None,
    )
    .await;
    assert!(out.elapsed < Duration::from_secs(10), "{:?}", out.elapsed);
    assert!(!out.frames.is_empty());
}

#[tokio::test(start_paused = true)]
async fn client_overflow_keeps_direct_clients_visible() {
    let mut clients: Vec<ClientSpec> = (1..=6)
        .map(|n| ClientSpec::dev_session(2000 + n, n))
        .collect();
    clients.push(ClientSpec::vscode(2010, true));
    clients.push(ClientSpec::plain(2011, "", true));
    let scenario = Scenario::new().starting_with(Phase::default().with_clients(clients));
    let out = run_tui(scenario, (64, 16), 3700, None, None).await;
    let frame = out.last();
    let text = frame.text();
    assert!(text.contains("clients (8)"), "{text}");
    assert!(
        text.contains("⚠ vscode 1h01m"),
        "direct clients are never folded away:\n{text}"
    );
    assert!(text.contains("⚠ ssh 1h01m"), "{text}");
    assert!(text.contains(" more"), "+k more on overflow:\n{text}");
    assert_eq!(frame.color_of("⚠ vscode"), Color::Red);
    let spark_row = frame.row_of("max 10ms") + 1;
    assert!(
        frame.rows[spark_row].contains("  "),
        "{}",
        frame.rows[spark_row]
    );
}

#[tokio::test(start_paused = true)]
async fn status_bar_keeps_paused_and_clock_when_narrow() {
    let keys = keys_at(vec![(41, Key::Pause)]);
    let out = run_tui(outage_scenario(), (64, 16), 42, Some(keys), None).await;
    let status = &out.last().rows[0];
    assert!(status.contains("○ DOWN"), "{status}");
    assert!(status.contains("⚠ STORM"), "autossh: {status}");
    assert!(status.contains("BREAKER"), "{status}");
    assert!(status.contains("↻ RETRY"), "{status}");
    assert!(status.contains("PAUSED"), "{status}");
    assert!(
        status.trim_end().ends_with("00:00:41"),
        "clock survives: {status}"
    );
    assert_eq!(status.chars().count(), 64);
    assert_eq!(out.last().color_of("↻ RETRY"), Color::Yellow);
    assert_eq!(out.last().color_of("BREAKER"), Color::Red);
}

#[tokio::test(start_paused = true)]
async fn scenario_timeline_up_loss_degraded_recovered_down_loop_up() {
    let out = run_tui(outage_scenario(), (120, 40), 50, None, None).await;
    let kinds: Vec<&EventKind> = out.monitor.events.oldest_first().map(|e| &e.kind).collect();
    let mut it = kinds.iter();
    let mut expect = |pred: fn(&EventKind) -> bool, what: &str| {
        assert!(
            it.any(|k| pred(k)),
            "missing {what} in order; events were:\n{}",
            kinds
                .iter()
                .map(|k| format!("{k:?}"))
                .collect::<Vec<_>>()
                .join("\n")
        );
    };
    expect(
        |k| matches!(k, EventKind::MasterUp { pid: 1000 }),
        "master up",
    );
    expect(|k| matches!(k, EventKind::Degraded(_)), "degraded");
    expect(|k| matches!(k, EventKind::Recovered { .. }), "recovered");
    expect(|k| matches!(k, EventKind::MasterDown { .. }), "master down");
    expect(
        |k| matches!(k, EventKind::StormStarted(_)),
        "storm (autossh)",
    );
    expect(
        |k| matches!(k, EventKind::ReconnectLoop { attempts: 3, .. }),
        "reconnect loop",
    );
    expect(
        |k| matches!(k, EventKind::CircuitOpen { .. }),
        "circuit open",
    );
    expect(
        |k| matches!(k, EventKind::MasterRestarted { pid: 2000 }),
        "master restarted",
    );
    expect(
        |k| matches!(k, EventKind::ReconnectLoopEnded { attempts: 3 }),
        "loop ended",
    );
    assert_eq!(out.monitor.state(), State::Up);
    let degraded = out.frame_with("◐ DEGRADED (loss)");
    assert_eq!(degraded.color_of("◐ DEGRADED"), Color::Yellow);
    assert!(
        degraded.text().contains("degraded: 3 lost pings"),
        "{}",
        degraded.text()
    );
    let spark = degraded.row_of("max 10ms") + 1;
    assert!(
        degraded.rows[spark].trim_end().ends_with(' ') || degraded.rows[spark].contains("█ "),
        "lost pings are blanks: {:?}",
        degraded.rows[spark]
    );
    let down = out.frame_with("↻ RETRY");
    assert!(down.rows[0].contains("○ DOWN"), "{}", down.rows[0]);
    assert!(down.rows[0].contains("(was up "), "{}", down.rows[0]);
    assert!(down.text().contains("clients · no master"));
    let last = out.last();
    let text = last.text();
    let order = [
        "loop ended after 3 attempts",
        "master restarted (pid 2000)",
        "CIRCUIT OPEN",
        "reconnect loop: 3 attempts never connected (~75s each)",
    ]
    .map(|n| last.row_of(n));
    assert!(
        order.windows(2).all(|w| w[0] < w[1]),
        "newest first: {order:?}\n{text}"
    );
    assert_eq!(last.color_of("loop ended"), Color::Green);
    assert_eq!(last.color_of("master restarted"), Color::Yellow);
    assert_eq!(last.color_of("CIRCUIT OPEN"), Color::Red);
    assert_eq!(last.color_of("reconnecting in 2s"), Color::DarkGray);
    assert!(last.rows[0].contains("● UP"), "{text}");
    assert!(
        last.rows[0].contains("BREAKER"),
        "breaker stays open until half-open: {}",
        last.rows[0]
    );
}

#[tokio::test(start_paused = true)]
async fn missing_supervisor_log_and_failing_probes_show_na() {
    let scenario = Scenario::new().starting_with(
        Phase::default()
            .with_supervisor_log(None)
            .with_nettop_ok(false),
    );
    let out = run_tui(scenario, (90, 16), 6, None, None).await;
    let frame = out.last();
    let text = frame.text();
    assert!(frame.rows[0].contains("log n/a"), "{}", frame.rows[0]);
    assert!(text.contains("rx n/a · peak n/a"), "{text}");
    assert!(text.contains("probe failed: nettop"), "{text}");
    assert!(text.contains("probe failed: supervisor log"), "{text}");
    let scenario = Scenario::new().then_at(s(5), Phase::default().with_status_hang(true));
    let out = run_tui(scenario, (90, 16), 14, None, None).await;
    assert!(
        out.last().rows[0].contains("? n/a"),
        "{}",
        out.last().rows[0]
    );
    assert!(out.frame_with("● UP").rows[0].contains("fedora"));
}

fn braille_cols(row: &str) -> Vec<usize> {
    row.chars()
        .enumerate()
        .filter(|(_, c)| ('\u{2801}'..='\u{28FF}').contains(c))
        .map(|(i, _)| i)
        .collect()
}

#[tokio::test(start_paused = true)]
async fn default_interval_draws_solid_columns_from_the_baseline() {
    // The real TUI default (2 s), steady 10 kB/s down / 1 kB/s up at 90×30 (separate charts).
    let scenario = Scenario::new().starting_with(Phase::default().with_rates(10_000, 1_000));
    let out = run_tui_every(TUI_INTERVAL, scenario, (90, 30), 120, None, None).await;
    let frame = out.last();
    let text = frame.text();
    let (rx, tx) = (frame.row_of("── rx "), frame.row_of("── tx "));
    // 10 kB/s is 2 of 6 decades: the lowest rx rows are full cells (scattered points would not be) …
    let bottom = &frame.rows[tx - 1];
    let full: Vec<usize> = bottom
        .chars()
        .enumerate()
        .filter(|(_, c)| *c == '\u{28FF}')
        .map(|(i, _)| i)
        .collect();
    assert!(
        full.len() >= 25,
        "≈ 110 s of columns at 300 s / 90 cols:\n{text}"
    );
    // … with no holes up to now (x = 0 is the last cell's left dot column, so it is half full) …
    assert_eq!(
        full,
        (full[0]..=88).collect::<Vec<_>>(),
        "solid from the first measured tick to now:\n{text}"
    );
    assert_eq!(bottom.chars().nth(89), Some('⡇'), "{text}");
    // … and the top stays empty (the height is the rate).
    assert!(braille_cols(&frame.rows[rx + 1]).is_empty(), "{text}");
}

#[tokio::test(start_paused = true)]
async fn sparkline_blanks_only_lost_pings() {
    // 3 ms on LAN with a 30 ms blip and one lost ping: the fast replies must stay visible.
    let mut scenario = Scenario::new().starting_with(Phase::default().with_ping(Ping::Reply(3.0)));
    for (t, ping) in [
        (3, Ping::Reply(30.0)),
        (4, Ping::Reply(3.0)),
        (6, Ping::Lost),
        (7, Ping::Reply(3.0)),
    ] {
        scenario = scenario.then_at(s(t), Phase::default().with_ping(ping));
    }
    let out = run_tui(scenario, (90, 16), 12, None, None).await;
    let frame = out.last();
    let samples = out.monitor.latency.samples.iter().count();
    let lost = out
        .monitor
        .latency
        .samples
        .iter()
        .filter(|s| s.rtt_ms.is_none())
        .count();
    assert!(samples >= 8 && lost >= 1, "{samples} samples, {lost} lost");
    let spark: Vec<char> = frame.rows[frame.row_of("max 30ms") + 1]
        .chars()
        .take(samples)
        .collect();
    assert_eq!(
        spark.iter().filter(|c| **c == ' ').count(),
        lost,
        "only lost pings are blank: {spark:?}"
    );
    assert!(spark.contains(&'▁') && spark.contains(&'█'), "{spark:?}");
}

/// One dev-session client; `ps` starts failing at 5 s.
fn ps_fails_at_5s() -> Scenario {
    let one_client = || Phase::default().with_clients(vec![ClientSpec::dev_session(2001, 1)]);
    Scenario::new()
        .starting_with(one_client())
        .then_at(s(5), one_client().with_ps_ok(false))
}

#[tokio::test(start_paused = true)]
async fn failed_ps_hides_the_client_list() {
    let scenario = ps_fails_at_5s();
    let panel = |f: &Frame| -> String {
        let top = f.row_of("── clients");
        let left = f.rows[top][..f.rows[top].find("── clients").unwrap()]
            .chars()
            .count();
        f.rows[top + 1..f.row_of("── events")]
            .iter()
            .map(|r| r.chars().skip(left).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let out = run_tui(scenario, (90, 16), 4, None, None).await;
    assert!(panel(out.last()).contains("#1 "), "{}", out.last().text());
    let scenario = ps_fails_at_5s();
    let out = run_tui(scenario, (90, 16), 8, None, None).await;
    let text = out.last().text();
    assert!(text.contains("clients · n/a"), "{text}");
    assert_eq!(
        panel(out.last()).trim(),
        "n/a",
        "the stale list is not drawn:\n{text}"
    );
}
