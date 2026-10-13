//! Peer found by IP, never name (node `deb`, alias `fedora`); `Relay` alone is only the home region.

use super::ParseError;
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathKind {
    Direct {
        lan: bool,
    },
    /// Relayed through DERP; `region` may be empty if tailscale did not report one.
    Derp {
        region: String,
    },
    /// Relayed through another tailnet node (Tailscale peer relays), not DERP.
    PeerRelay {
        via: String,
    },
    /// Peer offline or inactive: no path to report.
    Unknown,
}

impl PathKind {
    pub fn is_direct(&self) -> bool {
        matches!(self, PathKind::Direct { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerPath {
    pub hostname: String,
    pub online: bool,
    pub relay: String,
    pub cur_addr: Option<String>,
    pub peer_relay: Option<String>,
    pub kind: PathKind,
}

/// `Ok(None)` when no peer has `peer_ip` among its Tailscale IPs.
pub fn parse_tailscale_status(json: &str, peer_ip: &str) -> Result<Option<PeerPath>, ParseError> {
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| ParseError::new("tailscale", format!("invalid JSON: {e}")))?;
    let peers = match v.get("Peer") {
        Some(serde_json::Value::Object(m)) => m,
        Some(serde_json::Value::Null) | None => return Ok(None),
        Some(_) => return Err(ParseError::new("tailscale", "Peer is not an object")),
    };
    for peer in peers.values() {
        let ips = peer.get("TailscaleIPs").and_then(|a| a.as_array());
        let matches = ips.is_some_and(|a| a.iter().any(|ip| ip.as_str() == Some(peer_ip)));
        if !matches {
            continue;
        }
        let str_field = |k: &str| {
            peer.get(k)
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string()
        };
        let non_empty = |s: String| (!s.is_empty()).then_some(s);
        let relay = str_field("Relay");
        let cur_addr = non_empty(str_field("CurAddr"));
        let peer_relay = non_empty(str_field("PeerRelay"));
        let online = peer
            .get("Online")
            .and_then(|o| o.as_bool())
            .unwrap_or(false);
        let kind = match (&cur_addr, &peer_relay) {
            _ if !online => PathKind::Unknown,
            // SPEC F3: direct ⇔ CurAddr non-empty, whatever PeerRelay says.
            (Some(addr), _) => PathKind::Direct {
                lan: is_private_ip(addr),
            },
            (None, Some(via)) => PathKind::PeerRelay { via: via.clone() },
            (None, None) => PathKind::Derp {
                region: relay.clone(),
            },
        };
        return Ok(Some(PeerPath {
            hostname: str_field("HostName"),
            online,
            relay,
            cur_addr,
            peer_relay,
            kind,
        }));
    }
    Ok(None)
}

pub fn is_private_ip(addr: &str) -> bool {
    let host = addr
        .strip_prefix('[')
        .and_then(|r| r.split(']').next())
        .unwrap_or_else(|| {
            // IPv4 with port, or bare IPv4/IPv6.
            if addr.matches(':').count() == 1 {
                addr.split(':').next().unwrap_or(addr)
            } else {
                addr
            }
        });
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => ip.is_private() || ip.is_link_local() || ip.is_loopback(),
        Ok(IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unique_local() || ip.is_unicast_link_local()
        }
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const WAN: &str = include_str!("../../tests/fixtures/tailscale_status_direct_wan.json");
    const LAN: &str = include_str!("../../tests/fixtures/tailscale_status_direct_lan.json");
    const DERP: &str = include_str!("../../tests/fixtures/tailscale_status_derp.json");
    const PEER_IP: &str = "100.100.1.10";

    #[test]
    fn direct_wan() {
        let p = parse_tailscale_status(WAN, PEER_IP).unwrap().unwrap();
        assert_eq!(p.kind, PathKind::Direct { lan: false });
        assert!(p.kind.is_direct());
        assert_eq!(p.cur_addr.as_deref(), Some("203.0.113.46:38205"));
        assert_eq!(
            (p.hostname.as_str(), p.relay.as_str(), p.online),
            ("deb", "par", true)
        );
        assert_eq!(p.peer_relay, None);
    }

    #[test]
    fn direct_lan() {
        let p = parse_tailscale_status(LAN, PEER_IP).unwrap().unwrap();
        assert_eq!(p.kind, PathKind::Direct { lan: true });
    }

    #[test]
    fn derp_when_cur_addr_is_empty_even_though_relay_is_set() {
        let p = parse_tailscale_status(DERP, PEER_IP).unwrap().unwrap();
        assert_eq!(
            p.kind,
            PathKind::Derp {
                region: "par".into()
            }
        );
        assert!(!p.kind.is_direct());
        assert_eq!(p.cur_addr, None);
    }

    #[test]
    fn offline_peer_has_no_path_and_peer_relay_is_not_derp() {
        let offline = DERP.replace(
            r#""Online": true,
      "Active": true"#,
            r#""Online": false,
      "Active": false"#,
        );
        assert_ne!(offline, DERP, "fixture edit must apply");
        let p = parse_tailscale_status(&offline, PEER_IP).unwrap().unwrap();
        assert_eq!((p.online, p.kind), (false, PathKind::Unknown));

        let relayed = DERP.replace(r#""PeerRelay": """#, r#""PeerRelay": "100.100.1.30:40000""#);
        assert_ne!(relayed, DERP);
        let both = WAN.replace(r#""PeerRelay": """#, r#""PeerRelay": "100.100.1.30:40000""#);
        let p = parse_tailscale_status(&both, PEER_IP).unwrap().unwrap();
        assert!(p.kind.is_direct(), "CurAddr wins over PeerRelay (SPEC F3)");
        let p = parse_tailscale_status(&relayed, PEER_IP).unwrap().unwrap();
        assert_eq!(
            p.kind,
            PathKind::PeerRelay {
                via: "100.100.1.30:40000".into()
            }
        );
        assert!(!p.kind.is_direct());
    }

    #[test]
    fn peer_is_matched_by_ip_not_name() {
        assert_eq!(parse_tailscale_status(WAN, "100.100.1.99").unwrap(), None);
        assert!(
            parse_tailscale_status(WAN, "fd7a:115c:a1e0::d139:476f")
                .unwrap()
                .is_some()
        );
        assert_eq!(
            parse_tailscale_status(r#"{"Peer":null}"#, PEER_IP).unwrap(),
            None
        );
        assert_eq!(parse_tailscale_status(r#"{}"#, PEER_IP).unwrap(), None);
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert_eq!(
            parse_tailscale_status("{", PEER_IP).unwrap_err().probe,
            "tailscale"
        );
        assert!(parse_tailscale_status(r#"{"Peer":[]}"#, PEER_IP).is_err());
        let odd = r#"{"Peer":{"k":{"TailscaleIPs":"nope","CurAddr":5}}}"#;
        assert_eq!(parse_tailscale_status(odd, PEER_IP).unwrap(), None);
        let odd = r#"{"Peer":{"k":{"TailscaleIPs":["100.100.1.10"],"CurAddr":5,"Online":"yes"}}}"#;
        let p = parse_tailscale_status(odd, PEER_IP).unwrap().unwrap();
        assert_eq!(
            p.kind,
            PathKind::Unknown,
            "unreadable Online counts as offline"
        );
    }

    #[test]
    fn private_ranges() {
        for a in [
            "192.168.1.192:41641",
            "10.0.0.1:1",
            "172.16.5.5:2",
            "169.254.1.1:3",
            "127.0.0.1",
            "[fd00::1]:5",
            "fe80::1",
        ] {
            assert!(is_private_ip(a), "{a} should be private");
        }
        for a in [
            "203.0.113.46:38205",
            "8.8.8.8",
            "[2001:db8::1]:41641",
            "100.64.0.1:1",
            "garbage",
            "",
        ] {
            assert!(!is_private_ip(a), "{a} should be public/unknown");
        }
    }
}
