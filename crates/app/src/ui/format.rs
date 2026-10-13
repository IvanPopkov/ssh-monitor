//! Every user-visible string is made here; core exposes raw state (A3).

use ssh_monitor_core::monitor::{
    Client, ClientRef, DegradeReason, EventKind, MasterExit, ProbeKind, StormCulprit,
};
use ssh_monitor_core::parse::{ClientKind, PathKind};
use std::time::Duration;

/// `12s` · `5m12s` · `1h05m` · `3d02h`.
pub fn duration(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m{:02}s", s / 60, s % 60)
    } else if s < 86_400 {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    } else {
        format!("{}d{:02}h", s / 86_400, (s % 86_400) / 3600)
    }
}

/// Unit picked after rounding: 999.96 kB/s is `1.0 MB/s`, never `1000.0 kB/s`.
pub fn rate(bps: f64) -> String {
    if bps.round() < 1000.0 {
        return format!("{} B/s", bps.round() as u64);
    }
    for (unit, div) in [("kB/s", 1e3), ("MB/s", 1e6)] {
        let v = (bps / div * 10.0).round() / 10.0;
        if v < 1000.0 {
            return format!("{v:.1} {unit}");
        }
    }
    format!("{:.1} GB/s", bps / 1e9)
}

pub fn rate_or_na(bps: Option<f64>) -> String {
    bps.map_or_else(|| "n/a".to_string(), rate)
}

pub fn millis(ms: f64) -> String {
    if ms < 10.0 {
        format!("{ms:.1}ms")
    } else {
        format!("{}ms", ms.round() as u64)
    }
}

pub fn millis_or_na(ms: Option<f64>) -> String {
    ms.map_or_else(|| "n/a".to_string(), millis)
}

/// Howard Hinnant's civil-from-days, shifted by `offset_secs`.
pub fn civil(unix_secs: i64, offset_secs: i32) -> (i64, u32, u32, u32, u32, u32) {
    let t = unix_secs + i64::from(offset_secs);
    let days = t.div_euclid(86_400);
    let secs = t.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m as u32,
        d as u32,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

pub fn clock(wall_ms: u64, offset_secs: i32) -> String {
    let (_, _, _, h, m, s) = civil((wall_ms / 1000) as i64, offset_secs);
    format!("{h:02}:{m:02}:{s:02}")
}

pub fn iso_utc(wall_ms: u64) -> String {
    let (y, mo, d, h, mi, s) = civil((wall_ms / 1000) as i64, 0);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        wall_ms % 1000
    )
}

pub fn probe_label(p: ProbeKind) -> &'static str {
    match p {
        ProbeKind::Status => "ssh -O check",
        ProbeKind::Traffic => "nettop",
        ProbeKind::Uptime => "ps etime",
        ProbeKind::Latency => "ping",
        ProbeKind::Clients => "ps",
        ProbeKind::Rogues => "lsof",
        ProbeKind::Supervisor => "supervisor log",
        ProbeKind::Path => "tailscale",
    }
}

/// SPEC F3: `direct · LAN` / `direct · WAN` / `DERP par` / `n/a`.
pub fn path_title(kind: Option<&PathKind>) -> String {
    match kind {
        Some(PathKind::Direct { lan: true }) => "direct · LAN".into(),
        Some(PathKind::Direct { lan: false }) => "direct · WAN".into(),
        Some(PathKind::Derp { region }) if region.is_empty() => "DERP".into(),
        Some(PathKind::Derp { region }) => format!("DERP {region}"),
        Some(PathKind::PeerRelay { via }) => format!("relay {via}"),
        Some(PathKind::Unknown) | None => "n/a".into(),
    }
}

pub fn path_short(kind: Option<&PathKind>) -> String {
    match kind {
        Some(PathKind::Direct { lan: true }) => "direct-lan".into(),
        Some(PathKind::Direct { lan: false }) => "direct-wan".into(),
        Some(PathKind::Derp { region }) if region.is_empty() => "derp".into(),
        Some(PathKind::Derp { region }) => format!("derp-{region}"),
        Some(PathKind::PeerRelay { via }) => format!("relay-{via}"),
        Some(PathKind::Unknown) | None => "n/a".into(),
    }
}

/// SPEC F4: `#N` · `vscode` · remote argv truncated (`scp`, `rsync`) · `ssh` when bare.
pub fn client_label(kind: &ClientKind, remote_argv: &[String], max: usize) -> String {
    match kind {
        ClientKind::DevSession(n) => format!("#{n}"),
        ClientKind::VsCode => "vscode".into(),
        ClientKind::Other => {
            if remote_argv.is_empty() {
                return "ssh".into();
            }
            let joined = remote_argv.join(" ");
            truncate(&joined, max)
        }
    }
}

pub fn client_line(c: &Client) -> String {
    client_label(&c.kind, &c.remote_argv, 12)
}

fn client_ref_label(c: &ClientRef) -> String {
    client_label(&c.kind, &c.remote_argv, 12)
}

pub fn truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(1);
    let mut out: String = s.chars().take(keep).collect();
    out.push('…');
    out
}

pub fn event_text(kind: &EventKind) -> String {
    match kind {
        EventKind::MasterUp { pid } => format!("master up (pid {pid})"),
        EventKind::MasterRestarted { pid } => format!("master restarted (pid {pid})"),
        EventKind::MasterDown { was_up, supervisor } => match (supervisor, was_up) {
            (Some(MasterExit { after_s, rc }), _) => {
                format!("master down after {after_s}s (rc={rc})")
            }
            (None, Some(up)) => format!("master down (was up {})", duration(*up)),
            (None, None) => "master down".into(),
        },
        EventKind::Degraded(reason) => format!("degraded: {}", degrade_reason(reason)),
        EventKind::Recovered { degraded_for } => {
            format!("recovered after {}", duration(*degraded_for))
        }
        EventKind::RttSpike {
            rtt_ms,
            threshold_ms,
            count,
        } => {
            let times = if *count > 1 {
                format!(" ×{count}")
            } else {
                String::new()
            };
            format!(
                "rtt spike {} (> {}){times}",
                millis(*rtt_ms),
                millis(*threshold_ms)
            )
        }
        EventKind::StormStarted(culprit) => match culprit {
            StormCulprit::Autossh { pid } => format!("storm: autossh present (pid {pid})"),
            StormCulprit::NonMux { pid, command } => {
                format!("storm: non-mux ssh pid {pid} ({})", truncate(command, 20))
            }
        },
        EventKind::StormCleared { lasted } => format!("storm cleared after {}", duration(*lasted)),
        EventKind::SupervisorReconnecting { in_s } => format!("reconnecting in {in_s}s"),
        EventKind::CircuitOpen { standing_down_s } => match standing_down_s {
            Some(s) => format!("CIRCUIT OPEN (standing down {s}s)"),
            None => "CIRCUIT OPEN".into(),
        },
        EventKind::CircuitHalfOpen => "CIRCUIT HALF-OPEN".into(),
        EventKind::ReconnectLoop { attempts, avg_secs } => {
            format!("reconnect loop: {attempts} attempts never connected (~{avg_secs}s each)")
        }
        EventKind::ReconnectLoopEnded { attempts } => {
            format!("loop ended after {attempts} attempts")
        }
        EventKind::PathChanged { from, to } => {
            format!(
                "path changed: {} → {}",
                path_title(from.as_ref()),
                path_title(to.as_ref())
            )
        }
        EventKind::ClientsChanged { added, removed } => {
            let mut parts: Vec<String> = added
                .iter()
                .map(|c| format!("+{}", client_ref_label(c)))
                .collect();
            parts.extend(removed.iter().map(|c| format!("−{}", client_ref_label(c))));
            format!("clients {}", parts.join(" "))
        }
        EventKind::ProbeFailed { probe, reason } => {
            format!(
                "probe failed: {} ({})",
                probe_label(*probe),
                truncate(reason, 40)
            )
        }
        EventKind::ProbeRecovered { probe } => format!("probe recovered: {}", probe_label(*probe)),
        EventKind::SystemResumed { gap } => format!("system resumed after {}", duration(*gap)),
    }
}

pub fn degrade_reason(reason: &DegradeReason) -> String {
    match reason {
        DegradeReason::Loss { consecutive } => format!("{consecutive} lost pings"),
        DegradeReason::Slow {
            median_ms,
            threshold_ms,
        } => {
            format!(
                "slow (median {} > {})",
                millis(*median_ms),
                millis(*threshold_ms)
            )
        }
        DegradeReason::Stall { silent_for } => format!("rx stall {}", duration(*silent_for)),
    }
}

pub fn degrade_reason_short(reason: &DegradeReason) -> &'static str {
    match reason {
        DegradeReason::Loss { .. } => "loss",
        DegradeReason::Slow { .. } => "slow",
        DegradeReason::Stall { .. } => "stall",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(12)), "12s");
        assert_eq!(duration(Duration::from_secs(312)), "5m12s");
        assert_eq!(duration(Duration::from_secs(3900)), "1h05m");
        assert_eq!(duration(Duration::from_secs(3 * 86_400 + 7200)), "3d02h");
        assert_eq!(duration(Duration::ZERO), "0s");
    }

    #[test]
    fn rates_and_millis() {
        assert_eq!(rate(0.0), "0 B/s");
        assert_eq!(rate(999.4), "999 B/s");
        assert_eq!(rate(12_345.0), "12.3 kB/s");
        assert_eq!(rate(1_500_000.0), "1.5 MB/s");
        assert_eq!(rate(2.5e9), "2.5 GB/s");
        // Rounding never yields four integer digits (the merged title's 64-column budget).
        assert_eq!(rate(999.6), "1.0 kB/s");
        assert_eq!(rate(999_960.0), "1.0 MB/s");
        assert_eq!(rate(999_960_000.0), "1.0 GB/s");
        assert_eq!(rate(999_940.0), "999.9 kB/s");
        assert_eq!(rate_or_na(None), "n/a");
        assert_eq!(millis(0.5), "0.5ms");
        assert_eq!(millis(12.4), "12ms");
        assert_eq!(millis(1234.6), "1235ms");
        assert_eq!(millis_or_na(None), "n/a");
    }

    #[test]
    fn civil_dates_and_clock() {
        // 2026-10-11T03:00:00Z = 1791687600
        assert_eq!(civil(1_791_687_600, 0), (2026, 10, 11, 3, 0, 0));
        assert_eq!(
            civil(1_791_687_600, 7200),
            (2026, 10, 11, 5, 0, 0),
            "+02:00"
        );
        assert_eq!(civil(0, 0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(951_782_400, 0), (2000, 2, 29, 0, 0, 0), "leap day");
        assert_eq!(civil(0, -3600), (1969, 12, 31, 23, 0, 0), "negative wrap");
        assert_eq!(clock(1_791_687_600_000 + 61_000, 0), "03:01:01");
        assert_eq!(iso_utc(1_791_687_600_123), "2026-10-11T03:00:00.123Z");
    }

    #[test]
    fn path_titles() {
        assert_eq!(
            path_title(Some(&PathKind::Direct { lan: true })),
            "direct · LAN"
        );
        assert_eq!(
            path_title(Some(&PathKind::Direct { lan: false })),
            "direct · WAN"
        );
        assert_eq!(
            path_title(Some(&PathKind::Derp {
                region: "par".into()
            })),
            "DERP par"
        );
        assert_eq!(
            path_title(Some(&PathKind::Derp {
                region: String::new()
            })),
            "DERP"
        );
        assert_eq!(
            path_title(Some(&PathKind::PeerRelay { via: "x:1".into() })),
            "relay x:1"
        );
        assert_eq!(path_title(Some(&PathKind::Unknown)), "n/a");
        assert_eq!(path_title(None), "n/a");
        assert_eq!(
            path_short(Some(&PathKind::Derp {
                region: "par".into()
            })),
            "derp-par"
        );
    }

    #[test]
    fn client_labels() {
        let argv = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        assert_eq!(client_label(&ClientKind::DevSession(4), &[], 12), "#4");
        assert_eq!(
            client_label(&ClientKind::VsCode, &argv(&["x"]), 12),
            "vscode"
        );
        assert_eq!(client_label(&ClientKind::Other, &[], 12), "ssh");
        assert_eq!(
            client_label(&ClientKind::Other, &argv(&["scp", "-t", "/tmp"]), 12),
            "scp -t /tmp"
        );
        assert_eq!(
            client_label(
                &ClientKind::Other,
                &argv(&["rsync", "--server", "-vlogDtpr"]),
                12
            ),
            "rsync --ser…"
        );
        assert_eq!(truncate("héllo", 3), "hé…");
        assert_eq!(truncate("hi", 3), "hi");
    }

    #[test]
    fn event_texts_follow_spec_wording() {
        use ssh_monitor_core::monitor::ProbeKind;
        let t = event_text;
        assert_eq!(t(&EventKind::MasterUp { pid: 7 }), "master up (pid 7)");
        assert_eq!(
            t(&EventKind::MasterRestarted { pid: 7 }),
            "master restarted (pid 7)"
        );
        assert_eq!(
            t(&EventKind::MasterDown {
                was_up: Some(Duration::from_secs(5736)),
                supervisor: Some(MasterExit {
                    after_s: 5736,
                    rc: 255
                })
            }),
            "master down after 5736s (rc=255)"
        );
        assert_eq!(
            t(&EventKind::MasterDown {
                was_up: Some(Duration::from_secs(90)),
                supervisor: None
            }),
            "master down (was up 1m30s)"
        );
        assert_eq!(
            t(&EventKind::MasterDown {
                was_up: None,
                supervisor: None
            }),
            "master down"
        );
        assert_eq!(
            t(&EventKind::Degraded(DegradeReason::Loss { consecutive: 3 })),
            "degraded: 3 lost pings"
        );
        assert_eq!(
            t(&EventKind::Degraded(DegradeReason::Slow {
                median_ms: 200.0,
                threshold_ms: 110.0
            })),
            "degraded: slow (median 200ms > 110ms)"
        );
        assert_eq!(
            t(&EventKind::Degraded(DegradeReason::Stall {
                silent_for: Duration::from_secs(36)
            })),
            "degraded: rx stall 36s"
        );
        assert_eq!(
            t(&EventKind::Recovered {
                degraded_for: Duration::from_secs(12)
            }),
            "recovered after 12s"
        );
        assert_eq!(
            t(&EventKind::RttSpike {
                rtt_ms: 300.0,
                threshold_ms: 110.0,
                count: 1
            }),
            "rtt spike 300ms (> 110ms)"
        );
        assert_eq!(
            t(&EventKind::RttSpike {
                rtt_ms: 300.0,
                threshold_ms: 110.0,
                count: 4
            }),
            "rtt spike 300ms (> 110ms) ×4"
        );
        assert_eq!(
            t(&EventKind::StormStarted(StormCulprit::Autossh { pid: 5 })),
            "storm: autossh present (pid 5)"
        );
        assert_eq!(
            t(&EventKind::StormStarted(StormCulprit::NonMux {
                pid: 5,
                command: "ssh".into()
            })),
            "storm: non-mux ssh pid 5 (ssh)"
        );
        assert_eq!(
            t(&EventKind::StormCleared {
                lasted: Duration::from_secs(8)
            }),
            "storm cleared after 8s"
        );
        assert_eq!(
            t(&EventKind::SupervisorReconnecting { in_s: 2 }),
            "reconnecting in 2s"
        );
        assert_eq!(
            t(&EventKind::CircuitOpen {
                standing_down_s: Some(300)
            }),
            "CIRCUIT OPEN (standing down 300s)"
        );
        assert_eq!(
            t(&EventKind::CircuitOpen {
                standing_down_s: None
            }),
            "CIRCUIT OPEN"
        );
        assert_eq!(t(&EventKind::CircuitHalfOpen), "CIRCUIT HALF-OPEN");
        assert_eq!(
            t(&EventKind::ReconnectLoop {
                attempts: 3,
                avg_secs: 75
            }),
            "reconnect loop: 3 attempts never connected (~75s each)"
        );
        assert_eq!(
            t(&EventKind::ReconnectLoopEnded { attempts: 3 }),
            "loop ended after 3 attempts"
        );
        assert_eq!(
            t(&EventKind::PathChanged {
                from: Some(PathKind::Direct { lan: false }),
                to: Some(PathKind::Derp {
                    region: "par".into()
                })
            }),
            "path changed: direct · WAN → DERP par"
        );
        let added = vec![
            ClientRef {
                pid: 1,
                kind: ClientKind::DevSession(1),
                remote_argv: vec![],
            },
            ClientRef {
                pid: 2,
                kind: ClientKind::VsCode,
                remote_argv: vec![],
            },
        ];
        let removed = vec![ClientRef {
            pid: 3,
            kind: ClientKind::DevSession(4),
            remote_argv: vec![],
        }];
        assert_eq!(
            t(&EventKind::ClientsChanged { added, removed }),
            "clients +#1 +vscode −#4"
        );
        assert_eq!(
            t(&EventKind::ProbeFailed {
                probe: ProbeKind::Traffic,
                reason: "exit 1".into()
            }),
            "probe failed: nettop (exit 1)"
        );
        assert_eq!(
            t(&EventKind::ProbeRecovered {
                probe: ProbeKind::Path
            }),
            "probe recovered: tailscale"
        );
        assert_eq!(
            t(&EventKind::SystemResumed {
                gap: Duration::from_secs(602)
            }),
            "system resumed after 10m02s"
        );
    }
}
