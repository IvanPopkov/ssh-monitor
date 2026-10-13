//! Lost = exit 2 with no reply line, or no exit code at all (timeout fired).

use super::{ParseError, first_line};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PingResult {
    Reply { rtt_ms: f64 },
    Lost,
}

pub fn parse_ping(exit: i32, stdout: &str) -> Result<PingResult, ParseError> {
    if exit < 0 {
        return Ok(PingResult::Lost);
    }
    for line in stdout.lines() {
        if let Some(rest) = line.split("time=").nth(1) {
            let num: String = rest
                .trim_start()
                .chars()
                .take_while(|c| c.is_ascii_digit() || *c == '.')
                .collect();
            if let Ok(rtt_ms) = num.parse::<f64>() {
                return Ok(PingResult::Reply { rtt_ms });
            }
        }
    }
    if stdout.contains("packets transmitted") {
        return Ok(PingResult::Lost);
    }
    Err(ParseError::new(
        "ping",
        format!("exit {exit}: {}", first_line(stdout)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const OK: &str = include_str!("../../tests/fixtures/ping_ok.txt");
    const LOST: &str = include_str!("../../tests/fixtures/ping_lost.txt");

    #[test]
    fn reply_with_rtt() {
        assert_eq!(
            parse_ping(0, OK).unwrap(),
            PingResult::Reply { rtt_ms: 10.012 }
        );
    }

    #[test]
    fn lost_on_exit_2_with_statistics_only() {
        assert_eq!(parse_ping(2, LOST).unwrap(), PingResult::Lost);
    }

    #[test]
    fn timeout_without_exit_code_is_lost() {
        assert_eq!(
            parse_ping(-1, "PING 100.100.1.10 (100.100.1.10): 56 data bytes\n").unwrap(),
            PingResult::Lost
        );
        assert_eq!(parse_ping(-1, "").unwrap(), PingResult::Lost);
    }

    #[test]
    fn unreachable_host_errors_count_as_lost_when_statistics_exist() {
        let out = "PING 10.9.9.9 (10.9.9.9): 56 data bytes\nping: sendto: No route to host\n\n--- 10.9.9.9 ping statistics ---\n1 packets transmitted, 0 packets received, 100.0% packet loss\n";
        assert_eq!(parse_ping(2, out).unwrap(), PingResult::Lost);
    }

    #[test]
    fn no_output_is_a_probe_failure() {
        let err = parse_ping(68, "").unwrap_err();
        assert_eq!(err.probe, "ping");
        assert!(err.reason.starts_with("exit 68"));
        assert!(parse_ping(64, "ping: cannot resolve nope: Unknown host\n").is_err());
    }

    #[test]
    fn sub_millisecond_and_integer_times() {
        assert_eq!(
            parse_ping(0, "x time=0.5 ms\n").unwrap(),
            PingResult::Reply { rtt_ms: 0.5 }
        );
        assert_eq!(
            parse_ping(0, "x time=12 ms\n").unwrap(),
            PingResult::Reply { rtt_ms: 12.0 }
        );
        assert!(parse_ping(0, "x time=ms\n").is_err());
    }
}
