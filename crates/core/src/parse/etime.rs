//! `ps -o etime=`: `mm:ss`, `hh:mm:ss` or `dd-hh:mm:ss`.

use super::ParseError;
use std::time::Duration;

pub const MAX_DAYS: u64 = 36_525;

pub fn parse_etime(text: &str) -> Result<Duration, ParseError> {
    let s = text.trim();
    let bad = || ParseError::new("ps etime", format!("unrecognised elapsed time {s:?}"));
    if s.is_empty() {
        return Err(bad());
    }
    let (days, clock) = match s.split_once('-') {
        Some((d, c)) => (d.parse::<u64>().map_err(|_| bad())?, c),
        None => (0, s),
    };
    // A process older than 100 years is not a process; it is garbage that would overflow later.
    if days > MAX_DAYS {
        return Err(bad());
    }
    let parts = clock
        .split(':')
        .map(|p| p.parse::<u64>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| bad())?;
    let (h, m, sec) = match parts[..] {
        [m, sec] => (0, m, sec),
        [h, m, sec] => (h, m, sec),
        _ => return Err(bad()),
    };
    // ps never prints 60+ seconds/minutes, or 24+ hours once a day field exists.
    if sec >= 60 || m >= 60 || (days > 0 && h >= 24) {
        return Err(bad());
    }
    let total = days
        .checked_mul(86_400)
        .and_then(|d| d.checked_add(h.checked_mul(3_600)?))
        .and_then(|t| t.checked_add(m * 60 + sec))
        .ok_or_else(bad)?;
    Ok(Duration::from_secs(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mm_ss() {
        let d = parse_etime(include_str!("../../tests/fixtures/etime_mmss.txt")).unwrap();
        assert_eq!(d, Duration::from_secs(58 * 60 + 58));
    }

    #[test]
    fn hh_mm_ss() {
        let d = parse_etime(include_str!("../../tests/fixtures/etime_hhmmss.txt")).unwrap();
        assert_eq!(d, Duration::from_secs(4 * 3600 + 45 * 60 + 36));
    }

    #[test]
    fn dd_hh_mm_ss() {
        let d = parse_etime(include_str!("../../tests/fixtures/etime_ddhhmmss.txt")).unwrap();
        assert_eq!(d, Duration::from_secs(3 * 86_400 + 55 * 60 + 13));
    }

    #[test]
    fn tolerates_ps_padding() {
        assert_eq!(parse_etime("   00:07\n").unwrap(), Duration::from_secs(7));
    }

    #[test]
    fn garbage_is_an_error() {
        for bad in [
            "",
            "ps: process id too large: 99999999",
            "1:2:3:4",
            "x:1",
            "3-",
            "--",
            "12",
            "99:99",
            "00:60",
            "1-24:00:00",
        ] {
            assert!(parse_etime(bad).is_err(), "{bad:?} should fail");
        }
    }

    #[test]
    fn huge_values_are_errors_not_overflow_panics() {
        assert!(parse_etime("999999999999999-00:00:00").is_err());
        assert!(parse_etime("999999999999999999:00").is_err());
        assert!(parse_etime("18446744073709551615-23:59:59").is_err());
        assert!(
            parse_etime("213503982334601-00:00:00").is_err(),
            "fits u64 seconds, not a Duration sum"
        );
        assert!(parse_etime("36526-00:00:00").is_err());
        assert!(parse_etime("36525-23:59:59").is_ok());
        // Hours without a day field are unbounded in ps output (e.g. 100:00:00) and fit in u64.
        assert_eq!(
            parse_etime("100:00:00").unwrap(),
            Duration::from_secs(360_000)
        );
    }
}
