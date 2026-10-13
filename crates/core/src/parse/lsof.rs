//! No matches → lsof exits 1 (= no connections). Processes sharing one socket show the same DEVICE.

use super::{ParseError, first_line};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TcpConn {
    pub command: String,
    pub pid: u32,
    pub fd: String,
    /// Kernel socket identity (`0x…`); equal for processes sharing one connection.
    pub device: String,
    pub local: String,
    pub remote: String,
    pub state: Option<String>,
}

pub fn parse_lsof(exit: i32, stdout: &str, stderr: &str) -> Result<Vec<TcpConn>, ParseError> {
    if exit < 0 {
        return Err(ParseError::new(
            "lsof",
            "no exit code (timed out or killed)",
        ));
    }
    let mut out = Vec::new();
    let mut nonblank = 0;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.starts_with("COMMAND") {
            continue;
        }
        nonblank += 1;
        if let Some(conn) = parse_row(line) {
            out.push(conn);
        }
    }
    if nonblank > 0 && out.is_empty() {
        return Err(ParseError::new(
            "lsof",
            format!("no connection rows in {:?}", first_line(stdout)),
        ));
    }
    if out.is_empty()
        && let Some(msg) = complaint(stderr)
    {
        return Err(ParseError::new("lsof", format!("exit {exit}: {msg}")));
    }
    Ok(out)
}

/// First stderr line outside a `lsof: WARNING:` block (warning plus indented continuations).
fn complaint(stderr: &str) -> Option<&str> {
    let mut in_warning = false;
    for line in stderr.lines() {
        if line.starts_with("lsof: WARNING") {
            in_warning = true;
            continue;
        }
        if line.trim().is_empty() || (in_warning && line.starts_with(char::is_whitespace)) {
            continue;
        }
        return Some(line.trim());
    }
    None
}

/// Parsed from the right because COMMAND may contain spaces (`Code Helper`).
fn parse_row(line: &str) -> Option<TcpConn> {
    let mut toks: Vec<&str> = line.split_whitespace().collect();
    let state = match toks.last() {
        Some(t) if t.starts_with('(') => {
            let s = toks
                .pop()?
                .trim_matches(|c| c == '(' || c == ')')
                .to_string();
            Some(s)
        }
        _ => None,
    };
    // … user pid are the last two before: fd type device size/off node name
    if toks.len() < 9 {
        return None;
    }
    let name = toks.pop()?;
    let _node = toks.pop()?;
    let _size = toks.pop()?;
    let device = toks.pop()?.to_string();
    let _type = toks.pop()?;
    let fd = toks.pop()?.to_string();
    let _user = toks.pop()?;
    let pid: u32 = toks.pop()?.parse().ok()?;
    let command = toks.join(" ");
    let (local, remote) = name.split_once("->")?;
    Some(TcpConn {
        command,
        pid,
        fd,
        device,
        local: local.to_string(),
        remote: remote.to_string(),
        state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: &str = include_str!("../../tests/fixtures/lsof_tcp_normal.txt");
    const POST_OUTAGE: &str = include_str!("../../tests/fixtures/lsof_tcp_postoutage.txt");

    #[test]
    fn normal_is_the_master_alone() {
        let conns = parse_lsof(0, NORMAL, "").unwrap();
        assert_eq!(conns.len(), 1);
        let c = &conns[0];
        assert_eq!(
            (c.command.as_str(), c.pid, c.fd.as_str()),
            ("ssh", 34299, "3u")
        );
        assert_eq!(c.device, "0x120ffca401ac839e");
        assert_eq!(c.local, "100.100.1.20:52103");
        assert_eq!(c.remote, "100.100.1.10:22");
        assert_eq!(c.state.as_deref(), Some("ESTABLISHED"));
    }

    #[test]
    fn post_outage_has_four_rows_on_three_sockets() {
        let conns = parse_lsof(0, POST_OUTAGE, "").unwrap();
        assert_eq!(
            conns.iter().map(|c| c.pid).collect::<Vec<_>>(),
            vec![22340, 22475, 22489, 22508]
        );
        let dev = |pid: u32| conns.iter().find(|c| c.pid == pid).unwrap().device.clone();
        assert_eq!(
            dev(22340),
            dev(22489),
            "pane and the [mux] master it spawned share one socket"
        );
        let mut devices: Vec<String> = conns.iter().map(|c| c.device.clone()).collect();
        devices.sort();
        devices.dedup();
        assert_eq!(devices.len(), 3);
    }

    #[test]
    fn exit_1_without_output_means_no_connections() {
        assert_eq!(parse_lsof(1, "", "").unwrap(), vec![]);
        assert_eq!(parse_lsof(1, "\n", "").unwrap(), vec![]);
        let header = "COMMAND PID USER FD TYPE DEVICE SIZE/OFF NODE NAME\n";
        assert_eq!(parse_lsof(0, header, "").unwrap(), vec![]);
        let warn = "lsof: WARNING: can't stat() fuse file system /x\n      Output information may be incomplete.\n";
        assert_eq!(parse_lsof(1, "", warn).unwrap(), vec![]);
    }

    #[test]
    fn complaint_on_stderr_without_rows_is_a_failure() {
        let err = parse_lsof(1, "", "lsof: unknown option -Z\n").unwrap_err();
        assert_eq!(err.probe, "lsof");
        assert!(err.reason.contains("unknown option"));
        assert!(parse_lsof(-1, "", "").is_err(), "timeout");
        // Rows win over stderr noise.
        assert_eq!(parse_lsof(0, NORMAL, "lsof: something\n").unwrap().len(), 1);
    }

    #[test]
    fn command_with_spaces_and_missing_state() {
        let row = "Code\\x20He 123 user 5u IPv6 0xabc 0t0 TCP [::1]:5->[::2]:22\n";
        let c = &parse_lsof(0, row, "").unwrap()[0];
        assert_eq!((c.pid, c.state.as_deref()), (123, None));
        let row = "Code Helper 123 user 5u IPv4 0xabc 0t0 TCP 1.2.3.4:5->5.6.7.8:22 (SYN_SENT)\n";
        let c = &parse_lsof(0, row, "").unwrap()[0];
        assert_eq!(
            (c.command.as_str(), c.state.as_deref()),
            ("Code Helper", Some("SYN_SENT"))
        );
    }

    #[test]
    fn garbage_on_stdout_is_an_error() {
        assert_eq!(
            parse_lsof(0, "lsof: unknown option\n", "")
                .unwrap_err()
                .probe,
            "lsof"
        );
    }
}
