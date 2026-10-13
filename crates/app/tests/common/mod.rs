#![allow(dead_code)]

use ssh_monitor::clock::Clock;
use ssh_monitor::runner::{BoxFuture, CmdOutput, CmdStatus, CommandRunner};
use ssh_monitor_core::Stamp;
use ssh_monitor_core::stamp::WALL_ORIGIN_MS;
use ssh_monitor_core::tape::TapeEntry;
use ssh_monitor_testkit::{Response, Scenario, select_entry};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, Once};
use std::time::Duration;

/// `wall` locked to tokio's paused `Instant`, plus an offset to simulate sleep.
pub struct FakeClock {
    origin: tokio::time::Instant,
    wall_skew_ms: std::sync::atomic::AtomicU64,
}

impl FakeClock {
    pub fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            wall_skew_ms: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Simulates laptop sleep.
    pub fn skew_wall(&self, ms: u64) {
        self.wall_skew_ms
            .fetch_add(ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Default for FakeClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for FakeClock {
    fn now(&self) -> Stamp {
        let mono = self.origin.elapsed();
        let skew = self.wall_skew_ms.load(std::sync::atomic::Ordering::SeqCst);
        Stamp::new(mono, WALL_ORIGIN_MS + mono.as_millis() as u64 + skew)
    }
}

/// Hangs and over-timeout delays become `TimedOut` after sleeping the timeout, like `kill_on_drop`.
async fn deliver(r: Response, timeout: Duration) -> CmdOutput {
    let delay = Duration::from_millis(r.delay_ms);
    if r.exit < 0 || delay >= timeout {
        tokio::time::sleep(timeout).await;
        return CmdOutput {
            status: CmdStatus::TimedOut,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: timeout,
        };
    }
    if !delay.is_zero() {
        tokio::time::sleep(delay).await;
    }
    if r.exit == 127 && r.stdout.is_empty() {
        // How a recording stores "tool missing" (see RecordingRunner).
        if let Some(reason) = r.stderr.strip_prefix("spawn failed: ") {
            return CmdOutput::spawn_failed(reason.trim().to_string());
        }
    }
    CmdOutput {
        status: CmdStatus::Exited(r.exit),
        stdout: r.stdout,
        stderr: r.stderr,
        elapsed: delay,
    }
}

/// Records every invocation with its virtual time, for scheduling assertions.
pub struct FakeRunner {
    pub scenario: Arc<Scenario>,
    pub clock: Arc<FakeClock>,
    pub calls: Mutex<Vec<(Duration, Vec<String>)>>,
}

impl FakeRunner {
    pub fn new(scenario: Scenario, clock: Arc<FakeClock>) -> Self {
        Self {
            scenario: Arc::new(scenario),
            clock,
            calls: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> Vec<(Duration, Vec<String>)> {
        self.calls.lock().unwrap().clone()
    }

    pub fn count(&self, prefix: &[&str]) -> usize {
        self.calls()
            .iter()
            .filter(|(_, a)| {
                prefix.iter().zip(a.iter()).all(|(p, x)| p == x) && a.len() >= prefix.len()
            })
            .count()
    }
}

impl CommandRunner for FakeRunner {
    fn run(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, CmdOutput> {
        Box::pin(async move {
            let now = self.clock.now().mono;
            self.calls.lock().unwrap().push((now, argv.clone()));
            let r = self.scenario.respond(&argv, now);
            deliver(r, timeout).await
        })
    }
}

/// Same selection rule as `fake-cmd` (A7).
pub struct TapeRunner {
    pub entries: Vec<TapeEntry>,
    pub clock: Arc<FakeClock>,
}

impl CommandRunner for TapeRunner {
    fn run(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, CmdOutput> {
        Box::pin(async move {
            let now_ms = self.clock.now().mono.as_millis() as u64;
            let Some(e) = select_entry(&self.entries, &argv, now_ms) else {
                return CmdOutput::spawn_failed(format!("no tape entry for {}", argv.join(" ")));
            };
            let r = Response {
                exit: e.exit,
                stdout: e.stdout.clone(),
                stderr: e.stderr.clone(),
                delay_ms: e.delay_ms,
            };
            deliver(r, timeout).await
        })
    }
}

pub fn bin_dir() -> PathBuf {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let status = Command::new(cargo)
            .args([
                "build",
                "-q",
                "-p",
                "ssh-monitor-testkit",
                "--bin",
                "fake-cmd",
            ])
            .status()
            .expect("cargo build fake-cmd");
        assert!(status.success(), "building fake-cmd failed");
    });
    Path::new(env!("CARGO_BIN_EXE_ssh-monitor"))
        .parent()
        .unwrap()
        .to_path_buf()
}

pub fn ssh_monitor_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_ssh-monitor"))
}

pub fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("sshmon-l2-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub const FAKE_TOOLS: [&str; 7] = ["ssh", "nettop", "ping", "ps", "lsof", "tail", "tailscale"];

/// `HOME` points into the scratch dir so the derived supervisor-log path matches the tape.
pub struct FakeEnv {
    pub dir: PathBuf,
    pub tape: PathBuf,
    pub bin: PathBuf,
}

impl FakeEnv {
    /// From a scenario: 12s worth of entries per second.
    pub fn new(name: &str, scenario: &Scenario, tools: &[&str]) -> Self {
        let dir = scratch(name);
        let scenario = scenario.clone().with_home(dir.to_str().unwrap());
        let entries = scenario.to_tape(Duration::from_secs(12), Duration::from_secs(1));
        Self::build(dir, &entries, tools)
    }

    pub fn from_tape(name: &str, entries: &[TapeEntry], tools: &[&str]) -> Self {
        let dir = scratch(name);
        Self::build(dir, entries, tools)
    }

    fn build(dir: PathBuf, entries: &[TapeEntry], tools: &[&str]) -> Self {
        let tape = dir.join("tape.jsonl");
        std::fs::write(&tape, ssh_monitor_core::tape::write_jsonl(entries)).unwrap();
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let fake = bin_dir().join("fake-cmd");
        assert!(fake.exists(), "fake-cmd not built at {}", fake.display());
        for tool in tools {
            std::os::unix::fs::symlink(&fake, bin.join(tool)).unwrap();
        }
        Self { dir, tape, bin }
    }

    /// Returns (exit code, stdout, stderr).
    pub fn run(&self, args: &[&str]) -> (i32, String, String) {
        // Only the fakes: a tool missing from `bin/` must be missing for the binary too.
        let path = self.bin.display().to_string();
        let out = Command::new(ssh_monitor_bin())
            .args(args)
            .env("PATH", path)
            .env("FAKE_CMD_TAPE", &self.tape)
            .env("HOME", &self.dir)
            .output()
            .expect("run ssh-monitor");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    pub fn run_once(&self, extra: &[&str]) -> (i32, serde_json::Value, String) {
        let mut args = vec!["--once"];
        args.extend_from_slice(extra);
        let (code, stdout, stderr) = self.run(&args);
        let json: serde_json::Value = serde_json::from_str(&stdout)
            .unwrap_or_else(|e| panic!("not JSON ({e}): stdout={stdout:?} stderr={stderr:?}"));
        (code, json, stderr)
    }
}

impl Drop for FakeEnv {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
