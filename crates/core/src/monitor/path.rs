//! LAN ↔ WAN flips the title only (decision S3).

use super::event::{EventKind, EventLog};
use crate::Stamp;
use crate::parse::{PathKind, PeerPath};

#[derive(Debug, Clone, Default)]
pub struct Path {
    /// `None` before the first sample, when the peer is absent, or after a failure (`n/a`).
    pub current: Option<PeerPath>,
    /// Last kind actually observed; survives failed/empty samples so a change is never missed.
    last_kind: Option<PathKind>,
    pub since: Option<Stamp>,
}

impl Path {
    /// Returns `true` when the path kind changed (the RTT baseline must reset).
    pub fn on_sample(&mut self, peer: Option<PeerPath>, now: Stamp, log: &mut EventLog) -> bool {
        let Some(peer) = peer else {
            self.current = None;
            return false;
        };
        let to = peer.kind.clone();
        let changed = self
            .last_kind
            .as_ref()
            .is_some_and(|from| kind_changed(from, &to));
        if changed {
            log.push(
                now,
                EventKind::PathChanged {
                    from: self.last_kind.clone(),
                    to: Some(to.clone()),
                },
            );
        }
        if changed || self.last_kind.is_none() {
            self.since = Some(now);
        }
        self.last_kind = Some(to);
        self.current = Some(peer);
        changed
    }

    pub fn on_failed(&mut self) {
        self.current = None;
    }

    pub fn kind(&self) -> Option<&PathKind> {
        self.current.as_ref().map(|p| &p.kind)
    }
}

/// Only direct ↔ not direct or a DERP region change emits; the rest is title-only (SPEC F5).
fn kind_changed(a: &PathKind, b: &PathKind) -> bool {
    match (a, b) {
        (PathKind::Derp { region: ra }, PathKind::Derp { region: rb }) => ra != rb,
        _ => a.is_direct() != b.is_direct(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_tailscale_status;

    const WAN: &str = include_str!("../../tests/fixtures/tailscale_status_direct_wan.json");
    const LAN: &str = include_str!("../../tests/fixtures/tailscale_status_direct_lan.json");
    const DERP: &str = include_str!("../../tests/fixtures/tailscale_status_derp.json");

    fn peer(json: &str) -> Option<PeerPath> {
        parse_tailscale_status(json, "100.100.1.10").unwrap()
    }

    #[test]
    fn direct_to_derp_and_back_fire_events_lan_wan_does_not() {
        let mut p = Path::default();
        let mut log = EventLog::new();
        assert!(
            !p.on_sample(peer(WAN), Stamp::at_secs(1), &mut log),
            "first sample: no change"
        );
        assert!(
            !p.on_sample(peer(LAN), Stamp::at_secs(2), &mut log),
            "LAN ↔ WAN: title only"
        );
        assert!(log.take_fresh().is_empty());
        assert!(p.on_sample(peer(DERP), Stamp::at_secs(3), &mut log));
        let fresh = log.take_fresh();
        assert_eq!(
            fresh[0].kind,
            EventKind::PathChanged {
                from: Some(PathKind::Direct { lan: true }),
                to: Some(PathKind::Derp {
                    region: "par".into()
                })
            }
        );
        assert_eq!(p.since, Some(Stamp::at_secs(3)));
        assert!(p.on_sample(peer(WAN), Stamp::at_secs(4), &mut log));
        let region_change = DERP.replace(r#""Relay": "par""#, r#""Relay": "fra""#);
        p.on_sample(peer(DERP), Stamp::at_secs(5), &mut log);
        assert!(
            p.on_sample(peer(&region_change), Stamp::at_secs(6), &mut log),
            "DERP region change"
        );
    }

    #[test]
    fn unknown_and_peer_relay_are_not_direct_and_do_not_flap_against_derp() {
        let mut p = Path::default();
        let mut log = EventLog::new();
        let offline = DERP.replace(r#""Online": true"#, r#""Online": false"#);
        let relayed = DERP.replace(r#""PeerRelay": """#, r#""PeerRelay": "100.100.1.30:1""#);
        p.on_sample(peer(DERP), Stamp::at_secs(1), &mut log);
        assert!(
            !p.on_sample(peer(&offline), Stamp::at_secs(2), &mut log),
            "DERP → unknown"
        );
        assert!(
            !p.on_sample(peer(&relayed), Stamp::at_secs(3), &mut log),
            "unknown → peer relay"
        );
        assert!(
            p.on_sample(peer(WAN), Stamp::at_secs(4), &mut log),
            "peer relay → direct"
        );
        assert!(
            p.on_sample(peer(&offline), Stamp::at_secs(5), &mut log),
            "direct → unknown"
        );
        assert_eq!(log.take_fresh().len(), 2);
    }

    #[test]
    fn change_is_detected_across_a_failed_or_empty_sample() {
        let mut p = Path::default();
        let mut log = EventLog::new();
        p.on_sample(peer(WAN), Stamp::at_secs(1), &mut log);
        assert!(!p.on_sample(None, Stamp::at_secs(2), &mut log));
        assert_eq!(p.kind(), None, "title shows n/a");
        p.on_failed();
        assert_eq!(p.current, None);
        assert!(log.take_fresh().is_empty());
        assert!(
            p.on_sample(peer(DERP), Stamp::at_secs(3), &mut log),
            "direct → (gap) → DERP is still a change"
        );
        assert!(matches!(
            log.take_fresh()[0].kind,
            EventKind::PathChanged {
                from: Some(PathKind::Direct { .. }),
                ..
            }
        ));
    }
}
