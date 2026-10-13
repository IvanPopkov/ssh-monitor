//! Clients: argv0 is `ssh` and the destination is the host; `-O`/`-G`/`-V`/`-Q` runs never connect.

use super::{ParseError, parse_etime};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    pub etime: Duration,
    /// As printed by ps: quoting lost.
    pub command: String,
}

/// Lenient per line, strict overall: non-empty input where nothing parses is an error.
pub fn parse_ps(stdout: &str) -> Result<Vec<Process>, ParseError> {
    let mut out = Vec::new();
    let mut nonblank = 0;
    for line in stdout.lines() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() {
            continue;
        }
        nonblank += 1;
        if let Some(p) = parse_line(line) {
            out.push(p);
        }
    }
    if nonblank > 0 && out.is_empty() {
        return Err(ParseError::new(
            "ps",
            format!("no process rows in {:?}", super::first_line(stdout)),
        ));
    }
    Ok(out)
}

fn parse_line(line: &str) -> Option<Process> {
    let mut it = line.split_whitespace();
    let pid = it.next()?.parse().ok()?;
    let ppid = it.next()?.parse().ok()?;
    let etime = parse_etime(it.next()?).ok()?;
    let command = it.collect::<Vec<_>>().join(" ");
    if command.is_empty() {
        return None;
    }
    Some(Process {
        pid,
        ppid,
        etime,
        command,
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SshInvocation {
    /// `user@host`, `ssh://user@host:22/` → `host`.
    pub host: String,
    pub user: Option<String>,
    /// The remote command (`--` dropped).
    pub remote_argv: Vec<String>,
    pub flags: Vec<char>,
    pub options: Vec<(char, String)>,
}

impl SshInvocation {
    pub fn has_flag(&self, flag: char) -> bool {
        self.flags.contains(&flag)
    }

    pub fn option(&self, opt: char) -> Option<&str> {
        self.options
            .iter()
            .find(|(c, _)| *c == opt)
            .map(|(_, v)| v.as_str())
    }

    /// ssh exits without ever connecting.
    pub fn never_connects(&self) -> bool {
        self.option('O').is_some()
            || self.has_flag('G')
            || self.has_flag('V')
            || self.option('Q').is_some()
    }
}

/// Everything else is a flag; clusters like `-tt` expand.
const WITH_ARG: &str = "BbcDEeFIiJLlmOoPpQRSWw";

/// `None` when argv0 is not `ssh` (basename compare, so `/usr/bin/ssh` counts; `ssh:` does not).
pub fn parse_ssh_argv(command: &str) -> Option<SshInvocation> {
    let argv: Vec<&str> = command.split_whitespace().collect();
    let argv0 = argv.first()?;
    if argv0.rsplit('/').next() != Some("ssh") {
        return None;
    }
    let mut inv = SshInvocation::default();
    let mut dest: Option<&str> = None;
    let mut i = 1;
    while i < argv.len() {
        let a = argv[i];
        if a == "--" {
            // scp/sftp spawn `ssh … -- host sftp`: after `--` come the destination, then the remote command.
            let rest = &argv[i + 1..];
            let skip = usize::from(dest.is_none());
            if dest.is_none() {
                dest = rest.first().copied();
            }
            inv.remote_argv = rest.iter().skip(skip).map(|s| (*s).to_string()).collect();
            break;
        }
        if a.starts_with('-') && a.len() > 1 {
            let cluster: Vec<char> = a[1..].chars().collect();
            let mut j = 0;
            while j < cluster.len() {
                let c = cluster[j];
                if WITH_ARG.contains(c) {
                    let attached: String = cluster[j + 1..].iter().collect();
                    let value = if attached.is_empty() {
                        i += 1;
                        argv.get(i).map_or(String::new(), |s| (*s).to_string())
                    } else {
                        attached
                    };
                    inv.options.push((c, value));
                    break;
                }
                inv.flags.push(c);
                j += 1;
            }
            i += 1;
            continue;
        }
        if dest.is_none() {
            // ssh re-runs option parsing after the destination, so options may follow it.
            dest = Some(a);
            i += 1;
            continue;
        }
        inv.remote_argv = argv[i..].iter().map(|s| (*s).to_string()).collect();
        break;
    }
    let dest = dest?;
    let (user, host) = split_destination(dest);
    inv.user = user.or_else(|| inv.option('l').map(str::to_string));
    inv.host = host;
    Some(inv)
}

fn split_destination(dest: &str) -> (Option<String>, String) {
    let mut d = dest;
    let url = d.strip_prefix("ssh://");
    if let Some(rest) = url {
        d = rest.trim_end_matches('/');
    }
    let (user, hostport) = match d.rsplit_once('@') {
        Some((u, h)) => (Some(u.to_string()), h),
        None => (None, d),
    };
    let host = if url.is_some() {
        hostport.rsplit_once(':').map_or(hostport, |(h, _)| h)
    } else {
        hostport
    };
    (user, host.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientKind {
    /// `… -- dev-session N` → shown as `#N`.
    DevSession(u32),
    /// VS Code Remote-SSH (parent is the Remote-SSH helper, or its `-T -D <port>` signature).
    VsCode,
    /// Anything else: shown as the remote argv (truncated) or `ssh` when there is none.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SshProcess {
    pub pid: u32,
    pub ppid: u32,
    pub etime: Duration,
    pub invocation: SshInvocation,
    pub kind: ClientKind,
}

/// Master included (the caller removes it by PID). Sorted by PID.
pub fn ssh_processes(procs: &[Process], alias: &str, hostname: &str) -> Vec<SshProcess> {
    let targets = |h: &str| h.eq_ignore_ascii_case(alias) || h.eq_ignore_ascii_case(hostname);
    let mut out: Vec<SshProcess> = procs
        .iter()
        .filter_map(|p| {
            let inv = parse_ssh_argv(&p.command)?;
            if !targets(&inv.host) || inv.never_connects() {
                return None;
            }
            let parent = procs
                .iter()
                .find(|q| q.pid == p.ppid)
                .map(|q| q.command.as_str());
            let kind = classify(&inv, parent);
            Some(SshProcess {
                pid: p.pid,
                ppid: p.ppid,
                etime: p.etime,
                invocation: inv,
                kind,
            })
        })
        .collect();
    out.sort_by_key(|p| p.pid);
    out
}

fn classify(inv: &SshInvocation, parent_command: Option<&str>) -> ClientKind {
    if let [cmd, n, ..] = inv.remote_argv.as_slice()
        && cmd.rsplit('/').next() == Some("dev-session")
        && let Ok(n) = n.parse::<u32>()
    {
        return ClientKind::DevSession(n);
    }
    let parent_is_vscode =
        parent_command.is_some_and(|c| c.contains("remote-ssh") || c.contains("Code Helper"));
    let looks_like_vscode =
        inv.has_flag('T') && inv.option('D').is_some() && inv.remote_argv.is_empty();
    if parent_is_vscode || looks_like_vscode {
        return ClientKind::VsCode;
    }
    ClientKind::Other
}

#[cfg(test)]
mod tests {
    use super::*;

    const NORMAL: &str = include_str!("../../tests/fixtures/ps_axo_normal.txt");
    const POST_OUTAGE: &str = include_str!("../../tests/fixtures/ps_axo_postoutage.txt");

    fn pids(v: &[SshProcess]) -> Vec<u32> {
        v.iter().map(|p| p.pid).collect()
    }

    #[test]
    fn parses_every_row_of_the_normal_fixture() {
        let procs = parse_ps(NORMAL).unwrap();
        assert_eq!(procs.len(), 6);
        let master = procs.iter().find(|p| p.pid == 34299).unwrap();
        assert_eq!(master.ppid, 2775);
        assert_eq!(master.etime, Duration::from_secs(59 * 60 + 40));
        assert!(master.command.starts_with("ssh -N -o ControlMaster=yes"));
    }

    #[test]
    fn normal_fixture_clients_are_two_panes_vscode_and_master() {
        let procs = parse_ps(NORMAL).unwrap();
        let ssh = ssh_processes(&procs, "fedora", "100.100.1.10");
        assert_eq!(pids(&ssh), vec![34299, 34340, 34343, 34345]);
        let kind = |pid: u32| ssh.iter().find(|p| p.pid == pid).unwrap().kind.clone();
        assert_eq!(
            kind(34299),
            ClientKind::Other,
            "the master: caller excludes it by PID"
        );
        assert_eq!(kind(34340), ClientKind::DevSession(1));
        assert_eq!(kind(34345), ClientKind::DevSession(4));
        assert_eq!(
            kind(34343),
            ClientKind::VsCode,
            "parent not in listing: -T -D signature"
        );
        // The zsh rows mention `ssh` everywhere but argv0 is /bin/zsh.
        assert!(ssh.iter().all(|p| p.pid != 42950 && p.pid != 42957));
    }

    #[test]
    fn post_outage_fixture_excludes_supervisor_bash_vscode_helper_agent_and_mux_title() {
        let procs = parse_ps(POST_OUTAGE).unwrap();
        assert_eq!(procs.len(), 20);
        let ssh = ssh_processes(&procs, "fedora", "100.100.1.10");
        assert_eq!(pids(&ssh), vec![22340, 22475, 22508]);
        let kind = |pid: u32| ssh.iter().find(|p| p.pid == pid).unwrap().kind.clone();
        assert_eq!(kind(22340), ClientKind::DevSession(1));
        assert_eq!(
            kind(22475),
            ClientKind::VsCode,
            "parent 22474 is the Remote-SSH helper"
        );
        assert_eq!(
            kind(22508),
            ClientKind::Other,
            "supervisor duplicate master attempt"
        );
        // Excluded: 2775 bash ssh-master, 14698 ssh-agent, 22474 Code Helper (JSON args name the host),
        // 22489 `ssh: … [mux]` (argv0 is `ssh:`), 31327 zsh.
        for pid in [2775, 14698, 22474, 22489, 31327] {
            assert!(!pids(&ssh).contains(&pid), "pid {pid} must not be a client");
        }
    }

    #[test]
    fn mux_master_title_row_still_parses_as_a_process() {
        let procs = parse_ps(POST_OUTAGE).unwrap();
        let mux = procs.iter().find(|p| p.pid == 22489).unwrap();
        assert_eq!(
            mux.command,
            "ssh: /Users/user/.ssh/cm-user@100.100.1.10:22 [mux]"
        );
        assert_eq!(mux.etime, Duration::from_secs(22 * 60 + 27));
        assert_eq!(parse_ssh_argv(&mux.command), None);
    }

    #[test]
    fn argv_parsing_handles_options_after_host_and_double_dash() {
        let inv = parse_ssh_argv("ssh -t fedora -- /home/user/.local/bin/dev-session 1").unwrap();
        assert_eq!(inv.host, "fedora");
        assert!(inv.has_flag('t'));
        assert_eq!(
            inv.remote_argv,
            vec!["/home/user/.local/bin/dev-session", "1"]
        );

        let inv = parse_ssh_argv("ssh fedora -p 2222 -tt uptime -a").unwrap();
        assert_eq!(inv.option('p'), Some("2222"));
        assert_eq!(inv.flags, vec!['t', 't']);
        assert_eq!(inv.remote_argv, vec!["uptime", "-a"]);
    }

    #[test]
    fn argv_parsing_handles_attached_values_and_destinations() {
        let inv = parse_ssh_argv("ssh -oConnectTimeout=5 -D52124 -l bob fedora").unwrap();
        assert_eq!(inv.option('o'), Some("ConnectTimeout=5"));
        assert_eq!(inv.option('D'), Some("52124"));
        assert_eq!(inv.user.as_deref(), Some("bob"));

        let inv = parse_ssh_argv("/usr/bin/ssh alice@100.100.1.10 scp -t /tmp").unwrap();
        assert_eq!(
            (inv.user.as_deref(), inv.host.as_str()),
            (Some("alice"), "100.100.1.10")
        );
        assert_eq!(inv.remote_argv, vec!["scp", "-t", "/tmp"]);

        let inv = parse_ssh_argv("ssh ssh://alice@fedora:22/").unwrap();
        assert_eq!(
            (inv.user.as_deref(), inv.host.as_str()),
            (Some("alice"), "fedora")
        );
    }

    #[test]
    fn scp_and_sftp_put_double_dash_before_the_destination() {
        // Exact argv shapes OpenSSH scp/sftp spawn for their ssh child.
        let scp = "ssh -x -oPermitLocalCommand=no -oClearAllForwardings=yes -oRemoteCommand=none -oRequestTTY=no -oForwardAgent=no -- fedora scp -t /tmp";
        let inv = parse_ssh_argv(scp).unwrap();
        assert_eq!(inv.host, "fedora");
        assert_eq!(inv.remote_argv, vec!["scp", "-t", "/tmp"]);
        assert!(inv.has_flag('x'));

        let sftp = "ssh -oForwardX11=no -oPermitLocalCommand=no -oClearAllForwardings=yes -oForwardAgent=no -oRemoteCommand=none -oRequestTTY=no -l alice -s -- fedora sftp";
        let inv = parse_ssh_argv(sftp).unwrap();
        assert_eq!(
            (inv.user.as_deref(), inv.host.as_str()),
            (Some("alice"), "fedora")
        );
        assert_eq!(inv.remote_argv, vec!["sftp"]);

        let procs = vec![Process {
            pid: 9,
            ppid: 1,
            etime: Duration::ZERO,
            command: scp.into(),
        }];
        let ssh = ssh_processes(&procs, "fedora", "100.100.1.10");
        assert_eq!(ssh.len(), 1);
        assert_eq!(ssh[0].kind, ClientKind::Other);
        assert_eq!(
            parse_ssh_argv("ssh --"),
            None,
            "double dash with nothing after it"
        );
    }

    #[test]
    fn control_and_query_invocations_never_connect() {
        for cmd in [
            "ssh -O check fedora",
            "ssh -G fedora",
            "ssh -V",
            "ssh -Q cipher fedora",
            "ssh -O exit fedora",
        ] {
            let inv = parse_ssh_argv(cmd);
            assert!(inv.is_none_or(|i| i.never_connects()), "{cmd}");
        }
        let procs = vec![Process {
            pid: 1,
            ppid: 0,
            etime: Duration::ZERO,
            command: "ssh -O check fedora".into(),
        }];
        assert!(ssh_processes(&procs, "fedora", "100.100.1.10").is_empty());
    }

    #[test]
    fn not_ssh_or_other_host_is_ignored() {
        assert_eq!(parse_ssh_argv("sshd: user@pts/0"), None);
        assert_eq!(parse_ssh_argv("/usr/bin/ssh-agent -l"), None);
        assert_eq!(parse_ssh_argv("ssh"), None, "no destination");
        let procs = vec![Process {
            pid: 1,
            ppid: 0,
            etime: Duration::ZERO,
            command: "ssh otherhost".into(),
        }];
        assert!(ssh_processes(&procs, "fedora", "100.100.1.10").is_empty());
    }

    #[test]
    fn malformed_listing_is_an_error_but_junk_lines_are_skipped() {
        assert_eq!(parse_ps("").unwrap(), vec![]);
        assert!(parse_ps("garbage\nmore garbage\n").is_err());
        let procs = parse_ps("garbage\n  7     1       00:01 sleep 1\n").unwrap();
        assert_eq!(procs.len(), 1);
        assert_eq!(procs[0].command, "sleep 1");
    }
}
