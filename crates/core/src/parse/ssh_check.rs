//! The answer is on stderr, ends with `\r\n`. No exit code (timeout) is a probe failure, not a down master.

use super::{ParseError, first_line};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MasterStatus {
    Running {
        pid: u32,
    },
    /// `reason`: first stderr line, or `exit N` when ssh said nothing.
    Down {
        reason: String,
    },
}

pub fn parse_ssh_check(exit: i32, stderr: &str) -> Result<MasterStatus, ParseError> {
    if exit < 0 {
        return Err(ParseError::new(
            "ssh -O check",
            "no exit code (timed out or killed)",
        ));
    }
    if let Some(pid) = find_pid(stderr) {
        return Ok(MasterStatus::Running { pid });
    }
    if exit == 0 {
        return Err(ParseError::new(
            "ssh -O check",
            format!("exit 0 without 'Master running': {:?}", first_line(stderr)),
        ));
    }
    // Only a control-socket failure means no master; any other ssh error is a probe failure.
    let reason = first_line(stderr);
    if !reason.contains("Control socket") {
        return Err(ParseError::new(
            "ssh -O check",
            format!(
                "exit {exit}: {}",
                if reason.is_empty() {
                    "no output"
                } else {
                    &reason
                }
            ),
        ));
    }
    Ok(MasterStatus::Down { reason })
}

fn find_pid(text: &str) -> Option<u32> {
    let rest = text.split("Master running (pid=").nth(1)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const UP: &str = include_str!("../../tests/fixtures/ssh_check_up.stderr");
    const UP_MUX: &str = include_str!("../../tests/fixtures/ssh_check_up_mux.stderr");
    const DOWN: &str = include_str!("../../tests/fixtures/ssh_check_down.stderr");

    #[test]
    fn running_with_crlf_stderr() {
        assert!(UP.ends_with("\r\n"), "fixture must keep its CRLF");
        assert_eq!(
            parse_ssh_check(0, UP).unwrap(),
            MasterStatus::Running { pid: 34299 }
        );
    }

    #[test]
    fn running_when_a_pane_won_the_master_race() {
        assert_eq!(
            parse_ssh_check(0, UP_MUX).unwrap(),
            MasterStatus::Running { pid: 22489 }
        );
    }

    #[test]
    fn down_with_reason() {
        let got = parse_ssh_check(255, DOWN).unwrap();
        let MasterStatus::Down { reason } = got else {
            panic!("expected Down, got {got:?}")
        };
        assert_eq!(
            reason,
            "Control socket connect(/var/empty/nope.sock): No such file or directory"
        );
    }

    #[test]
    fn other_ssh_errors_are_probe_failures_not_down() {
        assert!(
            parse_ssh_check(255, "").is_err(),
            "no output is not evidence"
        );
        let err = parse_ssh_check(255, "config line 3: Bad configuration option\r\n").unwrap_err();
        assert!(err.reason.contains("Bad configuration"));
        let refused =
            parse_ssh_check(255, "Control socket connect(/x): Connection refused\r\n").unwrap();
        assert!(matches!(refused, MasterStatus::Down { .. }));
    }

    #[test]
    fn timeout_is_a_probe_failure_not_down() {
        let err = parse_ssh_check(-1, "").unwrap_err();
        assert_eq!(err.probe, "ssh -O check");
        assert!(err.reason.contains("timed out"));
        // Even a late "Master running" line does not rescue a timed-out probe.
        assert!(parse_ssh_check(-1, UP).is_err());
    }

    #[test]
    fn exit_zero_without_pid_is_a_parse_error() {
        let err = parse_ssh_check(0, "something odd\r\n").unwrap_err();
        assert_eq!(err.probe, "ssh -O check");
        assert!(parse_ssh_check(0, "Master running (pid=x)\r\n").is_err());
        assert!(parse_ssh_check(0, "").is_err());
    }
}
