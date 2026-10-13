//! `--log <path>`: append one CSV row per base tick (SPEC F7).

use crate::ui::format;
use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{Event, Monitor};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;

pub const HEADER: &str = "ts,state,pid,rx_bps,tx_bps,rtt_ms,loss,path,clients,ssh_procs,events";

/// `events` are those fired since the previous row, joined by ` | `.
pub fn row(m: &Monitor, now: Stamp, events: &[String]) -> String {
    let (rx, tx) = match m.traffic.current {
        Some((rx, tx)) => (
            format!("{}", rx.round() as u64),
            format!("{}", tx.round() as u64),
        ),
        None => (String::new(), String::new()),
    };
    let stats = m.latency.stats();
    let rtt = stats.now_ms.map_or(String::new(), |ms| format!("{ms:.1}"));
    let loss = stats.loss_pct.map_or(String::new(), |p| format!("{p:.1}"));
    let pid = m
        .known_master_pid()
        .map_or(String::new(), |p| p.to_string());
    let cols = [
        format::iso_utc(now.wall),
        crate::snapshot::state_name(m.known_state()).to_string(),
        pid,
        rx,
        tx,
        rtt,
        loss,
        format::path_short(m.path.kind()),
        m.known_clients()
            .map_or(String::new(), |c| c.list.len().to_string()),
        m.known_ssh_procs().map_or(String::new(), |n| n.to_string()),
        escape(&events.join(" | ")),
    ];
    cols.join(",")
}

/// RFC 4180 quoting for the free-text column.
pub fn escape(field: &str) -> String {
    if field.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

pub struct CsvLog {
    out: BufWriter<File>,
    pending: Vec<String>,
}

impl CsvLog {
    /// Writes the header when the file is new or empty.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = File::options().create(true).append(true).open(path)?;
        let empty = file.metadata().map(|m| m.len() == 0).unwrap_or(true);
        let mut out = BufWriter::new(file);
        if empty {
            writeln!(out, "{HEADER}")?;
            out.flush()?;
        }
        Ok(Self {
            out,
            pending: Vec::new(),
        })
    }

    pub fn note_events(&mut self, events: &[Event]) {
        self.pending
            .extend(events.iter().map(|e| format::event_text(&e.kind)));
    }

    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn write_row(&mut self, m: &Monitor, now: Stamp) -> io::Result<()> {
        let events = std::mem::take(&mut self.pending);
        writeln!(self.out, "{}", row(m, now, &events))?;
        self.out.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_monitor_core::monitor::{Input, MonitorConfig};
    use ssh_monitor_core::parse::{ByteCounters, MasterStatus, PingResult};

    fn monitor() -> Monitor {
        Monitor::new(MonitorConfig {
            host_alias: "fedora".into(),
            hostname: "100.100.1.10".into(),
            server_alive_interval: Some(15),
        })
    }

    #[test]
    fn header_has_eleven_columns_in_spec_order() {
        assert_eq!(HEADER.split(',').count(), 11);
        assert!(HEADER.starts_with("ts,state,pid,"));
        assert!(HEADER.ends_with(",events"));
    }

    #[test]
    fn row_reflects_monitor_state_and_blanks_unknowns() {
        let mut m = monitor();
        let r = row(&m, Stamp::at_secs(0), &[]);
        let cols: Vec<&str> = r.split(',').collect();
        assert_eq!(cols.len(), 11);
        assert_eq!(cols[0], "2026-01-01T00:00:00.000Z");
        assert_eq!(
            &cols[1..8],
            ["n/a", "", "", "", "", "", "n/a"],
            "no status yet"
        );
        assert_eq!(
            &cols[8..],
            ["", "", ""],
            "ps/lsof not answered yet: blank, not 0"
        );

        m.apply(
            Input::Status(MasterStatus::Running { pid: 7 }),
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
                bytes_in: 1500,
                bytes_out: 500,
            })),
            Stamp::at_secs(2),
        );
        m.apply(
            Input::Latency(PingResult::Reply { rtt_ms: 12.34 }),
            Stamp::at_secs(2),
        );
        m.apply(Input::Latency(PingResult::Lost), Stamp::at_secs(3));
        m.apply(
            Input::Latency(PingResult::Reply { rtt_ms: 10.0 }),
            Stamp::at_secs(4),
        );
        let r = row(
            &m,
            Stamp::at_secs(4),
            &["master up (pid 7)".into(), "a, b".into()],
        );
        let cols: Vec<&str> = r.splitn(11, ',').collect();
        assert_eq!(&cols[1..7], ["UP", "7", "1500", "500", "10.0", "33.3"]);
        assert_eq!(cols[10], "\"master up (pid 7) | a, b\"");
        // `ps` failing: the client columns are blank, not the last count.
        m.apply(
            Input::Failed {
                probe: ssh_monitor_core::monitor::ProbeKind::Clients,
                reason: "exit 1".into(),
            },
            Stamp::at_secs(5),
        );
        let r = row(&m, Stamp::at_secs(5), &[]);
        assert_eq!(&r.split(',').collect::<Vec<_>>()[8..], ["", "", ""]);
        // `ssh -O check` failing: state n/a and no stale pid.
        m.apply(
            Input::Failed {
                probe: ssh_monitor_core::monitor::ProbeKind::Status,
                reason: "timed out".into(),
            },
            Stamp::at_secs(6),
        );
        let r = row(&m, Stamp::at_secs(6), &[]);
        assert_eq!(&r.split(',').collect::<Vec<_>>()[1..3], ["n/a", ""]);
    }

    #[test]
    fn escaping_follows_rfc_4180() {
        assert_eq!(escape("plain"), "plain");
        assert_eq!(escape("a,b"), "\"a,b\"");
        assert_eq!(escape("say \"hi\""), "\"say \"\"hi\"\"\"");
    }

    #[test]
    fn file_gets_header_once_and_rows_with_pending_events() {
        let path = std::env::temp_dir().join(format!("sshmon-csv-{}.csv", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let m = monitor();
        {
            let mut log = CsvLog::open(&path).unwrap();
            log.note_events(&[Event {
                id: 1,
                at: Stamp::at_secs(1),
                kind: ssh_monitor_core::monitor::EventKind::MasterUp { pid: 1 },
            }]);
            log.write_row(&m, Stamp::at_secs(1)).unwrap();
            log.write_row(&m, Stamp::at_secs(2)).unwrap();
        }
        {
            let mut log = CsvLog::open(&path).unwrap();
            log.write_row(&m, Stamp::at_secs(3)).unwrap();
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 4, "one header, three rows");
        assert_eq!(lines[0], HEADER);
        assert!(lines[1].ends_with(",master up (pid 1)"));
        assert!(
            lines[2].ends_with(",,,"),
            "events column empty once flushed (clients/ssh_procs blank: ps never answered)"
        );
        let _ = std::fs::remove_file(&path);
    }
}
