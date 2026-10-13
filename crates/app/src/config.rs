//! Host facts from `ssh -G <alias>` (the only ssh form besides `-O check`), resolved once.

use crate::runner::{CmdStatus, CommandRunner};
use anyhow::{Context, bail};
use ssh_monitor_core::parse::{HostConfig, parse_ssh_config};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Host {
    pub alias: String,
    pub cfg: HostConfig,
    pub supervisor_log: Option<PathBuf>,
}

pub async fn resolve(runner: &dyn CommandRunner, alias: &str) -> anyhow::Result<Host> {
    let argv = vec!["ssh".to_string(), "-G".to_string(), alias.to_string()];
    let out = runner.run(argv, Duration::from_secs(10)).await;
    match &out.status {
        CmdStatus::SpawnFailed(reason) => bail!("cannot run ssh: {reason}"),
        CmdStatus::TimedOut => bail!("`ssh -G {alias}` timed out"),
        CmdStatus::Exited(0) => {}
        CmdStatus::Exited(code) => {
            let msg = out.stderr.lines().next().unwrap_or("").trim();
            bail!("`ssh -G {alias}` failed (exit {code}): {msg}");
        }
    }
    // stderr may carry "Pseudo-terminal will not be allocated…": ignored by design.
    let cfg = parse_ssh_config(&out.stdout).context("unreadable `ssh -G` output")?;
    if cfg.control_path.is_none() {
        bail!(
            "host {alias} has no ControlPath: nothing to monitor (set ControlPath/ControlMaster in ~/.ssh/config)"
        );
    }
    Ok(Host {
        alias: alias.to_string(),
        cfg,
        supervisor_log: supervisor_log_path(alias),
    })
}

/// Optional source (SPEC): missing → `n/a`.
pub fn supervisor_log_path(alias: &str) -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join(".local")
            .join("state")
            .join(format!("ssh-master-{alias}.log")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supervisor_log_lives_under_home() {
        let p = supervisor_log_path("fedora").unwrap();
        assert!(p.ends_with(".local/state/ssh-master-fedora.log"));
    }
}
