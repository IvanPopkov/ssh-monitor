//! JSONL tape shared by `--record`, testkit and `fake-cmd`, so recordings replay unchanged (A7).

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TapeEntry {
    /// Monotonic ms since the start of the recording.
    pub at_ms: u64,
    /// argv; `cmd[0]` is the program name as resolved via PATH.
    pub cmd: Vec<String>,
    /// Exit code; negative when the process produced none (killed, timed out).
    pub exit: i32,
    pub stdout: String,
    pub stderr: String,
    /// How long the command took (or should take when replayed).
    #[serde(default)]
    pub delay_ms: u64,
}

impl TapeEntry {
    pub fn new(at_ms: u64, cmd: &[&str], exit: i32, stdout: &str, stderr: &str) -> Self {
        Self {
            at_ms,
            cmd: cmd.iter().map(|s| (*s).to_string()).collect(),
            exit,
            stdout: stdout.to_string(),
            stderr: stderr.to_string(),
            delay_ms: 0,
        }
    }

    pub fn with_delay(mut self, delay_ms: u64) -> Self {
        self.delay_ms = delay_ms;
        self
    }

    pub fn program(&self) -> &str {
        self.cmd.first().map_or("", String::as_str)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TapeError {
    /// 1-based line number in the JSONL text.
    pub line: usize,
    pub reason: String,
}

impl fmt::Display for TapeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "tape line {}: {}", self.line, self.reason)
    }
}

impl std::error::Error for TapeError {}

pub fn entry_to_line(entry: &TapeEntry) -> String {
    serde_json::to_string(entry).unwrap_or_default()
}

pub fn line_to_entry(line: &str) -> Result<TapeEntry, String> {
    serde_json::from_str(line).map_err(|e| e.to_string())
}

pub fn write_jsonl(entries: &[TapeEntry]) -> String {
    let mut out = String::new();
    for e in entries {
        out.push_str(&entry_to_line(e));
        out.push('\n');
    }
    out
}

/// Blank lines are skipped; the first malformed line is an error.
pub fn read_jsonl(text: &str) -> Result<Vec<TapeEntry>, TapeError> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry = line_to_entry(line).map_err(|reason| TapeError {
            line: i + 1,
            reason,
        })?;
        out.push(entry);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<TapeEntry> {
        vec![
            TapeEntry::new(
                0,
                &["ssh", "-O", "check", "fedora"],
                0,
                "",
                "Master running (pid=48001)\r\n",
            )
            .with_delay(4),
            TapeEntry::new(
                1000,
                &["ping", "-c", "1", "-t", "1", "100.100.1.10"],
                2,
                "PING…\n100.0% packet loss\n",
                "",
            ),
            TapeEntry::new(1500, &["nettop", "-p", "48001"], -1, "", "").with_delay(5000),
        ]
    }

    #[test]
    fn jsonl_round_trip_is_lossless() {
        let text = write_jsonl(&sample());
        assert_eq!(text.lines().count(), 3, "one line per entry");
        assert!(text.ends_with('\n'));
        assert_eq!(read_jsonl(&text).unwrap(), sample());
    }

    #[test]
    fn matches_architecture_example_line() {
        let line = r#"{"at_ms":0,"cmd":["ssh","-O","check","fedora"],"exit":0,"stdout":"","stderr":"Master running (pid=48001)\r\n","delay_ms":4}"#;
        let e = line_to_entry(line).unwrap();
        assert_eq!(e, sample()[0]);
        assert_eq!(entry_to_line(&e), line);
    }

    #[test]
    fn delay_is_optional_when_reading() {
        let e =
            line_to_entry(r#"{"at_ms":1,"cmd":["ps"],"exit":0,"stdout":"x","stderr":""}"#).unwrap();
        assert_eq!(e.delay_ms, 0);
        assert_eq!(e.program(), "ps");
    }

    #[test]
    fn skips_blank_lines_and_reports_bad_line_number() {
        let text = format!("\n{}\n\n   \nnot json\n", entry_to_line(&sample()[1]));
        let err = read_jsonl(&text).unwrap_err();
        assert_eq!(err.line, 5);
        assert!(err.to_string().starts_with("tape line 5: "));
        assert_eq!(read_jsonl("\n\n").unwrap(), Vec::<TapeEntry>::new());
    }
}
