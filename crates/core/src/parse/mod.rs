//! Parsers are total: malformed input yields a [`ParseError`], never a panic.

pub mod etime;
pub mod lsof;
pub mod nettop;
pub mod ping;
pub mod ps;
pub mod ssh_check;
pub mod ssh_config;
pub mod supervisor;
pub mod tailscale;

pub use etime::parse_etime;
pub use lsof::{TcpConn, parse_lsof};
pub use nettop::{ByteCounters, parse_nettop};
pub use ping::{PingResult, parse_ping};
pub use ps::{
    ClientKind, Process, SshInvocation, SshProcess, parse_ps, parse_ssh_argv, ssh_processes,
};
pub use ssh_check::{MasterStatus, parse_ssh_check};
pub use ssh_config::{HostConfig, parse_ssh_config};
pub use supervisor::{SupervisorKind, SupervisorLine, parse_supervisor_line, parse_supervisor_log};
pub use tailscale::{PathKind, PeerPath, is_private_ip, parse_tailscale_status};

use std::fmt;

/// Becomes a `probe failed` event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub probe: &'static str,
    pub reason: String,
}

impl ParseError {
    pub fn new(probe: &'static str, reason: impl Into<String>) -> Self {
        Self {
            probe,
            reason: reason.into(),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.probe, self.reason)
    }
}

impl std::error::Error for ParseError {}

/// First non-empty line, trimmed and capped.
pub(crate) fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let mut s: String = line.chars().take(120).collect();
    if line.chars().count() > 120 {
        s.push('…');
    }
    s
}
