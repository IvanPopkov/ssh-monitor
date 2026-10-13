use super::ParseError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostConfig {
    /// An IP for Tailscale hosts.
    pub hostname: String,
    pub port: u16,
    pub user: Option<String>,
    /// `None` when ssh reports `controlpath none` (or omits it): nothing to watch.
    pub control_path: Option<String>,
    /// Seconds; `None` when 0 (keepalives disabled) or missing.
    pub server_alive_interval: Option<u32>,
    pub control_master: Option<String>,
}

pub fn parse_ssh_config(stdout: &str) -> Result<HostConfig, ParseError> {
    let mut cfg = HostConfig {
        hostname: String::new(),
        port: 22,
        user: None,
        control_path: None,
        server_alive_interval: None,
        control_master: None,
    };
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = value.trim();
        match key {
            "hostname" => cfg.hostname = value.to_string(),
            "port" => cfg.port = value.parse().map_err(|_| bad("port", value))?,
            "user" => cfg.user = Some(value.to_string()),
            "controlpath" => {
                cfg.control_path =
                    (!value.is_empty() && value != "none").then(|| value.to_string());
            }
            "serveraliveinterval" => {
                let n: u32 = value
                    .parse()
                    .map_err(|_| bad("serveraliveinterval", value))?;
                cfg.server_alive_interval = (n > 0).then_some(n);
            }
            "controlmaster" => cfg.control_master = Some(value.to_string()),
            _ => {}
        }
    }
    if cfg.hostname.is_empty() {
        return Err(ParseError::new("ssh -G", "no hostname line"));
    }
    Ok(cfg)
}

fn bad(key: &str, value: &str) -> ParseError {
    ParseError::new("ssh -G", format!("bad {key} value {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../tests/fixtures/ssh_G.txt");

    #[test]
    fn parses_the_real_fixture() {
        let cfg = parse_ssh_config(FIXTURE).unwrap();
        assert_eq!(cfg.hostname, "100.100.1.10");
        assert_eq!(cfg.port, 22);
        assert_eq!(cfg.user.as_deref(), Some("user"));
        assert_eq!(
            cfg.control_path.as_deref(),
            Some("/Users/user/.ssh/cm-user@100.100.1.10:22")
        );
        assert_eq!(cfg.server_alive_interval, Some(15));
        assert_eq!(cfg.control_master.as_deref(), Some("auto"));
    }

    #[test]
    fn controlpath_none_means_absent() {
        let cfg =
            parse_ssh_config("hostname h\ncontrolpath none\nserveraliveinterval 0\n").unwrap();
        assert_eq!(cfg.control_path, None);
        assert_eq!(cfg.server_alive_interval, None, "0 = keepalives disabled");
        assert_eq!(cfg.port, 22, "default port when the line is missing");
    }

    #[test]
    fn missing_hostname_is_an_error() {
        let err = parse_ssh_config("port 22\n").unwrap_err();
        assert_eq!(err.probe, "ssh -G");
        assert!(parse_ssh_config("").is_err());
    }

    #[test]
    fn bad_numbers_are_errors_not_panics() {
        assert!(parse_ssh_config("hostname h\nport abc\n").is_err());
        assert!(parse_ssh_config("hostname h\nserveraliveinterval x\n").is_err());
    }

    #[test]
    fn tolerates_crlf_and_junk_lines() {
        let cfg = parse_ssh_config(
            "Pseudo-terminal will not be allocated\r\nhostname 10.0.0.1\r\n\r\nx\n",
        )
        .unwrap();
        assert_eq!(cfg.hostname, "10.0.0.1");
    }
}
