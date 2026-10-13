//! Commands resolve via `PATH` only, so L2 tests and `scripts/live.sh` can shim them (A4).

use ssh_monitor_core::tape::{TapeEntry, entry_to_line};
use std::fs::File;
use std::future::Future;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CmdStatus {
    Exited(i32),
    TimedOut,
    SpawnFailed(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct CmdOutput {
    pub status: CmdStatus,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

impl CmdOutput {
    /// Negative when there is none (timeout / spawn failure).
    pub fn exit_code(&self) -> i32 {
        match &self.status {
            CmdStatus::Exited(code) => *code,
            CmdStatus::TimedOut => -1,
            CmdStatus::SpawnFailed(_) => -2,
        }
    }

    pub fn spawn_failed(reason: String) -> Self {
        Self {
            status: CmdStatus::SpawnFailed(reason),
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::ZERO,
        }
    }
}

pub trait CommandRunner: Send + Sync {
    fn run(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, CmdOutput>;
}

/// A timed-out child is killed when its future is dropped (kill_on_drop).
pub struct TokioRunner;

impl CommandRunner for TokioRunner {
    fn run(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, CmdOutput> {
        Box::pin(async move {
            let start = tokio::time::Instant::now();
            let Some((program, args)) = argv.split_first() else {
                return CmdOutput::spawn_failed("empty command".into());
            };
            let mut cmd = tokio::process::Command::new(program);
            cmd.args(args)
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .kill_on_drop(true);
            let child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => return CmdOutput::spawn_failed(format!("{program}: {e}")),
            };
            match tokio::time::timeout(timeout, child.wait_with_output()).await {
                Ok(Ok(out)) => CmdOutput {
                    status: CmdStatus::Exited(out.status.code().unwrap_or(-1)),
                    stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
                    stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
                    elapsed: start.elapsed(),
                },
                Ok(Err(e)) => CmdOutput::spawn_failed(format!("{program}: {e}")),
                Err(_) => CmdOutput {
                    status: CmdStatus::TimedOut,
                    stdout: String::new(),
                    stderr: String::new(),
                    elapsed: start.elapsed(),
                },
            }
        })
    }
}

/// `--record <dir>`: appends every command and its output to `<dir>/tape.jsonl`.
pub struct RecordingRunner {
    inner: Arc<dyn CommandRunner>,
    file: Mutex<BufWriter<File>>,
    start: tokio::time::Instant,
}

impl RecordingRunner {
    pub fn new(inner: Arc<dyn CommandRunner>, dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        let file = File::options()
            .create(true)
            .append(true)
            .open(dir.join("tape.jsonl"))?;
        Ok(Self {
            inner,
            file: Mutex::new(BufWriter::new(file)),
            start: tokio::time::Instant::now(),
        })
    }
}

impl CommandRunner for RecordingRunner {
    fn run(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, CmdOutput> {
        Box::pin(async move {
            let at_ms = self.start.elapsed().as_millis() as u64;
            let out = self.inner.run(argv.clone(), timeout).await;
            // A spawn failure replays as a missing tool (exit 127), never as a hang (exit < 0).
            let (exit, stderr) = match &out.status {
                CmdStatus::SpawnFailed(reason) => (127, format!("spawn failed: {reason}\n")),
                _ => (out.exit_code(), out.stderr.clone()),
            };
            let entry = TapeEntry {
                at_ms,
                cmd: argv,
                exit,
                stdout: out.stdout.clone(),
                stderr,
                delay_ms: out.elapsed.as_millis() as u64,
            };
            if let Ok(mut f) = self.file.lock() {
                let _ = writeln!(f, "{}", entry_to_line(&entry));
                let _ = f.flush();
            }
            out
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| (*s).to_string()).collect()
    }

    #[tokio::test]
    async fn runs_a_command_and_captures_streams_and_exit_code() {
        let out = TokioRunner
            .run(
                argv(&["sh", "-c", "echo out; echo err >&2; exit 3"]),
                Duration::from_secs(5),
            )
            .await;
        assert_eq!(out.status, CmdStatus::Exited(3));
        assert_eq!(out.stdout, "out\n");
        assert_eq!(out.stderr, "err\n");
        assert_eq!(out.exit_code(), 3);
    }

    #[tokio::test]
    async fn missing_tool_is_spawn_failed() {
        let out = TokioRunner
            .run(
                argv(&["definitely-not-a-real-tool-xyz"]),
                Duration::from_secs(1),
            )
            .await;
        assert!(matches!(out.status, CmdStatus::SpawnFailed(_)));
        assert_eq!(out.exit_code(), -2);
    }

    #[tokio::test]
    async fn timeout_kills_the_child() {
        let start = std::time::Instant::now();
        let out = TokioRunner
            .run(argv(&["sleep", "31.4159"]), Duration::from_millis(200))
            .await;
        assert_eq!(out.status, CmdStatus::TimedOut);
        assert_eq!(out.exit_code(), -1);
        assert!(start.elapsed() < Duration::from_secs(5));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let gone = std::process::Command::new("pgrep")
            .args(["-f", "sleep 31.4159"])
            .output()
            .unwrap();
        assert!(
            !gone.status.success(),
            "orphan: {}",
            String::from_utf8_lossy(&gone.stdout)
        );
    }

    #[tokio::test]
    async fn recording_runner_writes_a_replayable_tape() {
        let dir = std::env::temp_dir().join(format!("sshmon-rec-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rec = RecordingRunner::new(Arc::new(TokioRunner), &dir).unwrap();
        let out = rec
            .run(argv(&["sh", "-c", "printf hi"]), Duration::from_secs(5))
            .await;
        assert_eq!(out.stdout, "hi");
        drop(rec);
        let text = std::fs::read_to_string(dir.join("tape.jsonl")).unwrap();
        let tape = ssh_monitor_core::tape::read_jsonl(&text).unwrap();
        assert_eq!(tape.len(), 1);
        assert_eq!(tape[0].cmd, argv(&["sh", "-c", "printf hi"]));
        assert_eq!((tape[0].exit, tape[0].stdout.as_str()), (0, "hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
