//! Hand-rolled: five flags do not need a parser crate.

use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_HOST: &str = "fedora";
pub const MIN_INTERVAL: Duration = Duration::from_millis(500);
pub const MAX_INTERVAL: Duration = Duration::from_secs(3600);
/// TUI default: half the probe load of 1s.
pub const TUI_INTERVAL: Duration = Duration::from_secs(2);
/// `--once` default: its 3s window must fit three pings (the lost-ping rule).
pub const ONCE_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub host: String,
    /// `None` = the mode's default (see [`Args::tick`]).
    pub interval: Option<Duration>,
    pub log: Option<PathBuf>,
    pub once: bool,
    pub record: Option<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            host: DEFAULT_HOST.into(),
            interval: None,
            log: None,
            once: false,
            record: None,
        }
    }
}

impl Args {
    pub fn tick(&self) -> Duration {
        self.interval.unwrap_or(if self.once {
            ONCE_INTERVAL
        } else {
            TUI_INTERVAL
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Parsed {
    Run(Args),
    Help,
    Version,
}

pub const USAGE: &str = "\
ssh-monitor — watch one SSH ControlMaster connection

USAGE: ssh-monitor [OPTIONS]

OPTIONS:
  --host <alias>      ssh host alias to watch (default: fedora)
  --interval <secs>   base tick in seconds, min 0.5 (default: 2, --once: 1)
  --log <path>        append a CSV row per tick to <path>
  --once              sample ~3s, print a JSON snapshot, exit 0/1/2 = UP/DEGRADED/DOWN
  --record <dir>      save every probe command + output as <dir>/tape.jsonl
  -h, --help          this help
  -V, --version       version

KEYS (TUI): q quit · p pause display · c clear events
EXIT: 0 UP · 1 DEGRADED · 2 DOWN · 3 error";

/// Parses everything after the program name.
pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Parsed, String> {
    let mut out = Args::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            match inline.clone().or_else(|| it.next()) {
                Some(v) => Ok(v),
                None => Err(format!("{name} needs a value")),
            }
        };
        match flag.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            "--host" => out.host = value("--host")?,
            "--interval" => {
                let v = value("--interval")?;
                let secs: f64 = v
                    .parse()
                    .map_err(|_| format!("--interval: not a number: {v:?}"))?;
                if !secs.is_finite() || secs <= 0.0 {
                    return Err(format!("--interval: must be positive, got {v}"));
                }
                let d = Duration::try_from_secs_f64(secs)
                    .map_err(|_| format!("--interval: out of range: {v}"))?;
                if d < MIN_INTERVAL {
                    return Err(format!("--interval: minimum is 0.5s, got {v}"));
                }
                if d > MAX_INTERVAL {
                    return Err(format!("--interval: maximum is 3600s, got {v}"));
                }
                out.interval = Some(d);
            }
            "--log" => out.log = Some(PathBuf::from(value("--log")?)),
            "--record" => out.record = Some(PathBuf::from(value("--record")?)),
            "--once" => out.once = true,
            other => return Err(format!("unknown argument {other:?}\n\n{USAGE}")),
        }
    }
    if out.host.is_empty() || out.host.starts_with('-') {
        return Err(format!("--host: invalid alias {:?}", out.host));
    }
    Ok(Parsed::Run(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(v: &[&str]) -> Result<Parsed, String> {
        parse(v.iter().map(|s| (*s).to_string()))
    }

    #[test]
    fn defaults() {
        assert_eq!(p(&[]), Ok(Parsed::Run(Args::default())));
        let Ok(Parsed::Run(a)) = p(&[]) else { panic!() };
        assert_eq!(
            (a.host.as_str(), a.tick(), a.once),
            ("fedora", Duration::from_secs(2), false)
        );
        let Ok(Parsed::Run(a)) = p(&["--once"]) else {
            panic!()
        };
        assert_eq!(
            a.tick(),
            Duration::from_secs(1),
            "--once keeps three pings in its window"
        );
        let Ok(Parsed::Run(a)) = p(&["--once", "--interval", "2"]) else {
            panic!()
        };
        assert_eq!(
            a.tick(),
            Duration::from_secs(2),
            "an explicit --interval wins"
        );
    }

    #[test]
    fn all_flags_in_both_spellings() {
        let Ok(Parsed::Run(a)) = p(&[
            "--host",
            "deb",
            "--interval=0.5",
            "--log",
            "/tmp/x.csv",
            "--once",
            "--record=/tmp/rec",
        ]) else {
            panic!()
        };
        assert_eq!(a.host, "deb");
        assert_eq!(a.tick(), Duration::from_millis(500));
        assert_eq!(a.log, Some(PathBuf::from("/tmp/x.csv")));
        assert!(a.once);
        assert_eq!(a.record, Some(PathBuf::from("/tmp/rec")));
        assert_eq!(p(&["-h"]), Ok(Parsed::Help));
        assert_eq!(p(&["--version"]), Ok(Parsed::Version));
    }

    #[test]
    fn rejects_bad_input() {
        assert!(p(&["--interval", "0.4"]).unwrap_err().contains("minimum"));
        assert!(
            p(&["--interval", "abc"])
                .unwrap_err()
                .contains("not a number")
        );
        assert!(p(&["--interval", "-1"]).unwrap_err().contains("positive"));
        assert!(
            p(&["--interval", "1e30"]).unwrap_err().contains("maximum")
                || p(&["--interval", "1e30"]).unwrap_err().contains("range")
        );
        assert!(p(&["--interval", "1e19"]).is_err());
        assert!(p(&["--interval", "4000"]).unwrap_err().contains("maximum"));
        assert!(p(&["--interval", "3600"]).is_ok());
        assert!(p(&["--host"]).unwrap_err().contains("needs a value"));
        assert!(p(&["--host", "-x"]).unwrap_err().contains("invalid alias"));
        assert!(p(&["--bogus"]).unwrap_err().contains("unknown argument"));
    }
}
