//! Symlinked as every probe command in L2 tests; replays `FAKE_CMD_TAPE` by time since the
//! first invocation (epoch file next to the tape). `exit < 0` entries hang until the app's timeout.

use ssh_monitor_core::tape::read_jsonl;
use ssh_monitor_testkit::select_entry;
use std::io::Write;
use std::process::exit;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let prog = argv
        .first()
        .map(|a| a.rsplit('/').next().unwrap_or(a).to_string())
        .unwrap_or_default();
    let Some(tape_path) = std::env::var_os("FAKE_CMD_TAPE") else {
        eprintln!("fake-cmd ({prog}): FAKE_CMD_TAPE is not set");
        exit(127);
    };
    let text = match std::fs::read_to_string(&tape_path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("fake-cmd ({prog}): cannot read tape: {e}");
            exit(127);
        }
    };
    let entries = match read_jsonl(&text) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("fake-cmd ({prog}): bad tape: {e}");
            exit(127);
        }
    };
    let now_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let epoch_path = format!("{}.epoch", tape_path.to_string_lossy());
    let epoch = match std::fs::read_to_string(&epoch_path)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
    {
        Some(e) => e,
        None => {
            let _ = std::fs::write(&epoch_path, now_unix.to_string());
            now_unix
        }
    };
    let now_ms = now_unix.saturating_sub(epoch);
    let mut key_argv = argv.clone();
    key_argv[0] = prog.clone();
    let Some(entry) = select_entry(&entries, &key_argv, now_ms) else {
        eprintln!(
            "fake-cmd: refused, no tape entry for `{}`",
            key_argv.join(" ")
        );
        exit(127);
    };
    if entry.delay_ms > 0 {
        std::thread::sleep(Duration::from_millis(entry.delay_ms));
    }
    let _ = std::io::stdout().write_all(entry.stdout.as_bytes());
    let _ = std::io::stderr().write_all(entry.stderr.as_bytes());
    let _ = std::io::stdout().flush();
    if entry.exit < 0 {
        std::thread::sleep(Duration::from_secs(3600));
    }
    exit(entry.exit);
}
