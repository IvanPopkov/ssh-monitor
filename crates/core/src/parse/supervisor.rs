//! "ended" and "died" are the same: the supervisor called 75s connect timeouts healthy (S4).

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SupervisorKind {
    Opening,
    Ended { after_s: u64, rc: i32 },
    Reconnecting { in_s: u64 },
    CircuitOpen { standing_down_s: Option<u64> },
    CircuitHalfOpen,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorLine {
    /// Local time.
    pub timestamp: String,
    /// Naive seconds (timestamp read as UTC); differences between lines are exact.
    pub naive_secs: i64,
    pub kind: SupervisorKind,
    pub text: String,
}

/// `None` for blank lines; lines without the timestamp prefix become `Other`.
pub fn parse_supervisor_line(line: &str) -> Option<SupervisorLine> {
    let line = line.trim_end_matches('\r').trim_end();
    if line.trim().is_empty() {
        return None;
    }
    let (timestamp, naive_secs, text) = match split_timestamp(line) {
        Some((ts, secs, rest)) => (ts.to_string(), secs, rest.trim().to_string()),
        None => (String::new(), 0, line.trim().to_string()),
    };
    let kind = classify(&text);
    Some(SupervisorLine {
        timestamp,
        naive_secs,
        kind,
        text,
    })
}

pub fn parse_supervisor_log(text: &str) -> Vec<SupervisorLine> {
    text.lines().filter_map(parse_supervisor_line).collect()
}

fn classify(text: &str) -> SupervisorKind {
    if text.starts_with("opening master connection") {
        return SupervisorKind::Opening;
    }
    if let Some(rest) = text
        .strip_prefix("master ended after ")
        .or_else(|| text.strip_prefix("master died after "))
    {
        // Both numbers must be present; a mangled line is `Other`, never "after 0s (rc=-1)".
        let after_s = leading_number(rest);
        let rc = rest.split("(rc=").nth(1).and_then(leading_signed);
        return match (after_s, rc) {
            (Some(after_s), Some(rc)) => SupervisorKind::Ended { after_s, rc },
            _ => SupervisorKind::Other,
        };
    }
    if let Some(rest) = text.strip_prefix("reconnecting in ") {
        return match leading_number(rest) {
            Some(in_s) => SupervisorKind::Reconnecting { in_s },
            None => SupervisorKind::Other,
        };
    }
    if text.starts_with("CIRCUIT HALF-OPEN") {
        return SupervisorKind::CircuitHalfOpen;
    }
    if text.starts_with("CIRCUIT OPEN") {
        let standing_down_s = text
            .split("standing down for ")
            .nth(1)
            .and_then(leading_number);
        return SupervisorKind::CircuitOpen { standing_down_s };
    }
    SupervisorKind::Other
}

fn leading_number(s: &str) -> Option<u64> {
    let digits: String = s
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    digits.parse().ok()
}

fn leading_signed(s: &str) -> Option<i32> {
    let s = s.trim_start();
    let n: String = s
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '-')
        .collect();
    n.parse().ok()
}

/// `2026-10-11 03:00:05  rest` → (`2026-10-11 03:00:05`, naive seconds, `rest`).
fn split_timestamp(line: &str) -> Option<(&str, i64, &str)> {
    if line.len() < 19 || !line.is_char_boundary(19) {
        return None;
    }
    let (ts, rest) = line.split_at(19);
    let b = ts.as_bytes();
    let ok = b[4] == b'-' && b[7] == b'-' && b[10] == b' ' && b[13] == b':' && b[16] == b':';
    if !ok {
        return None;
    }
    let num = |a: usize, z: usize| ts[a..z].parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, s) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let secs = days_from_civil(y, mo, d)? * 86_400 + h * 3_600 + mi * 60 + s;
    Some((ts, secs, rest))
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> Option<i64> {
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some(era * 146_097 + doe - 719_468)
}

#[cfg(test)]
mod tests {
    use super::*;

    const OUTAGE: &str = include_str!("../../tests/fixtures/supervisor_outage.log");
    const BREAKER: &str = include_str!("../../tests/fixtures/supervisor_breaker.log");

    #[test]
    fn outage_log_parses_every_line_into_a_known_kind() {
        let lines = parse_supervisor_log(OUTAGE);
        assert_eq!(lines.len(), 399);
        assert!(
            lines.iter().all(|l| l.kind != SupervisorKind::Other),
            "no unknown shapes"
        );
        let count = |f: fn(&SupervisorKind) -> bool| lines.iter().filter(|l| f(&l.kind)).count();
        assert_eq!(count(|k| matches!(k, SupervisorKind::Opening)), 133);
        assert_eq!(
            count(|k| matches!(k, SupervisorKind::Reconnecting { .. })),
            133
        );
        assert_eq!(
            count(|k| matches!(k, SupervisorKind::Ended { .. })),
            133,
            "129 ended + 4 died"
        );
    }

    #[test]
    fn healthy_and_failure_labels_are_ignored() {
        let healthy = parse_supervisor_line(
            "2026-10-09 16:47:56  master ended after 75s (rc=255) — healthy run, backoff reset",
        )
        .unwrap();
        let failure = parse_supervisor_line(
            "2026-10-11 01:35:23  master died after 45s (rc=255) — failure 1/8",
        )
        .unwrap();
        assert_eq!(
            healthy.kind,
            SupervisorKind::Ended {
                after_s: 75,
                rc: 255
            }
        );
        assert_eq!(
            failure.kind,
            SupervisorKind::Ended {
                after_s: 45,
                rc: 255
            }
        );
        assert_eq!(healthy.timestamp, "2026-10-09 16:47:56");
    }

    #[test]
    fn first_outage_line_is_the_long_healthy_run() {
        let lines = parse_supervisor_log(OUTAGE);
        assert_eq!(
            lines[3].kind,
            SupervisorKind::Ended {
                after_s: 5736,
                rc: 255
            }
        );
        assert_eq!(lines[4].kind, SupervisorKind::Reconnecting { in_s: 1 });
        assert_eq!(lines[5].kind, SupervisorKind::Opening);
        assert_eq!(lines[4].naive_secs - lines[3].naive_secs, 0);
        assert_eq!(lines[5].naive_secs - lines[4].naive_secs, 1);
    }

    #[test]
    fn breaker_log_has_open_and_half_open() {
        let lines = parse_supervisor_log(BREAKER);
        assert_eq!(lines.len(), 8);
        assert_eq!(
            lines[5].kind,
            SupervisorKind::CircuitOpen {
                standing_down_s: Some(300)
            }
        );
        assert_eq!(lines[6].kind, SupervisorKind::CircuitHalfOpen);
        assert_eq!(lines[6].naive_secs - lines[5].naive_secs, 300);
        assert_eq!(
            lines[1].kind,
            SupervisorKind::Ended {
                after_s: 5,
                rc: 255
            }
        );
        assert_eq!(lines[2].kind, SupervisorKind::Reconnecting { in_s: 61 });
    }

    #[test]
    fn naive_seconds_match_a_known_epoch_value() {
        // 2026-10-11 03:00:00 UTC = 1791687600
        let l = parse_supervisor_line("2026-10-11 03:00:00  opening master connection").unwrap();
        assert_eq!(l.naive_secs, 1_791_687_600);
        assert_eq!(days_from_civil(1970, 1, 1), Some(0));
        assert_eq!(days_from_civil(2000, 3, 1), Some(11_017));
        assert_eq!(days_from_civil(2000, 13, 1), None);
    }

    #[test]
    fn odd_lines_never_panic() {
        assert_eq!(parse_supervisor_line(""), None);
        assert_eq!(parse_supervisor_line("   \r\n"), None);
        let l = parse_supervisor_line("not a timestamp line").unwrap();
        assert_eq!((l.kind, l.timestamp.as_str()), (SupervisorKind::Other, ""));
        let l = parse_supervisor_line("2026-10-11 03:00:00  master ended after s (rc=) —").unwrap();
        assert_eq!(
            l.kind,
            SupervisorKind::Other,
            "mangled numbers are not an Ended"
        );
        let l = parse_supervisor_line("2026-10-11 03:00:00  master died after 5s").unwrap();
        assert_eq!(l.kind, SupervisorKind::Other, "rc missing");
        let l = parse_supervisor_line("2026-10-11 03:00:00  reconnecting in soon").unwrap();
        assert_eq!(l.kind, SupervisorKind::Other);
        let l = parse_supervisor_line("2026-10-11 03:00:00  CIRCUIT OPEN").unwrap();
        assert_eq!(
            l.kind,
            SupervisorKind::CircuitOpen {
                standing_down_s: None
            }
        );
        let l = parse_supervisor_line("2026-99-99 03:00:00  x").unwrap();
        assert_eq!((l.timestamp.as_str(), l.naive_secs), ("", 0));
        assert!(parse_supervisor_line("é é é é é é é é é é é").is_some());
    }
}
