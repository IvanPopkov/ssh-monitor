//! The first `tail -n 50` poll is history (sets breaker state, no events); later polls fire for new lines.

use super::event::{EventKind, EventLog, MasterExit};
use crate::Stamp;
use crate::parse::{SupervisorKind, SupervisorLine};
use std::time::Duration;

/// Supervisor lines within this long of a `master down` are merged into it.
pub const MERGE_WINDOW: Duration = Duration::from_secs(5);
/// Consecutive attempts that never reached UP before `reconnect loop` fires.
pub const LOOP_THRESHOLD: u32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Breaker {
    Closed,
    Open,
    HalfOpen,
}

#[derive(Debug, Clone)]
pub struct Supervisor {
    /// `false` until the log has been read once, and after a read fails (features show `n/a`).
    pub available: bool,
    pub breaker: Breaker,
    pub streak_attempts: u32,
    streak_durations: Vec<u64>,
    loop_event: Option<u64>,
    pub reconnecting: Option<(u64, Stamp)>,
    prev_lines: Vec<String>,
    seen_history: bool,
    first_stamp: Option<Stamp>,
    attempt_start: Option<Stamp>,
    pending_down: Option<(u64, Stamp)>,
    recent_exits: Vec<(Stamp, MasterExit)>,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    pub fn new() -> Self {
        Self {
            available: false,
            breaker: Breaker::Closed,
            streak_attempts: 0,
            streak_durations: Vec::new(),
            loop_event: None,
            reconnecting: None,
            prev_lines: Vec::new(),
            seen_history: false,
            first_stamp: None,
            attempt_start: None,
            pending_down: None,
            recent_exits: Vec::new(),
        }
    }

    pub fn loop_active(&self) -> bool {
        self.loop_event.is_some()
    }

    /// Mean duration of the attempts in the current streak, from the log's `after Ns`.
    pub fn streak_avg_secs(&self) -> Option<u64> {
        if self.streak_durations.is_empty() {
            return None;
        }
        let total = self
            .streak_durations
            .iter()
            .fold(0u64, |a, b| a.saturating_add(*b));
        Some(total / self.streak_durations.len() as u64)
    }

    pub fn on_tail(
        &mut self,
        lines: &[SupervisorLine],
        now: Stamp,
        last_up_seen: Option<Stamp>,
        status_probe_failed: bool,
        log: &mut EventLog,
    ) {
        self.available = true;
        self.first_stamp.get_or_insert(now);
        let raw: Vec<String> = lines
            .iter()
            .map(|l| format!("{} {}", l.timestamp, l.text))
            .collect();
        let start = if self.seen_history {
            find_new_start(&self.prev_lines, &raw)
        } else {
            raw.len()
        };
        if !self.seen_history {
            self.breaker = breaker_from_history(lines);
            self.seen_history = true;
        }
        for line in &lines[start..] {
            self.on_new_line(line, now, last_up_seen, status_probe_failed, log);
        }
        self.prev_lines = raw;
    }

    pub fn on_unavailable(&mut self) {
        self.available = false;
    }

    /// Merges a supervisor exit line seen just before, or waits for one seen just after.
    pub fn on_master_down(&mut self, event_id: u64, now: Stamp, log: &mut EventLog) {
        self.recent_exits
            .retain(|(at, _)| now.mono.saturating_sub(at.mono) <= MERGE_WINDOW);
        if let Some(pos) = self
            .recent_exits
            .iter()
            .position(|(at, _)| now.mono.saturating_sub(at.mono) <= MERGE_WINDOW)
        {
            let (_, exit) = self.recent_exits.remove(pos);
            merge(log, event_id, exit);
            self.pending_down = None;
        } else {
            self.pending_down = Some((event_id, now));
        }
    }

    /// The master was reported running: any loop ends, a half-open breaker closes.
    pub fn on_master_up(&mut self, now: Stamp, log: &mut EventLog) {
        if self.loop_event.take().is_some() {
            log.push(
                now,
                EventKind::ReconnectLoopEnded {
                    attempts: self.streak_attempts,
                },
            );
        }
        self.streak_attempts = 0;
        self.streak_durations.clear();
        if self.breaker == Breaker::HalfOpen {
            self.breaker = Breaker::Closed;
        }
        self.reconnecting = None;
    }

    fn on_new_line(
        &mut self,
        line: &SupervisorLine,
        now: Stamp,
        last_up_seen: Option<Stamp>,
        status_probe_failed: bool,
        log: &mut EventLog,
    ) {
        match line.kind {
            SupervisorKind::Opening => {
                self.attempt_start = Some(now);
                self.reconnecting = None;
            }
            SupervisorKind::Ended { after_s, rc } => {
                let exit = MasterExit { after_s, rc };
                match self.pending_down.take() {
                    Some((id, at)) if now.mono.saturating_sub(at.mono) <= MERGE_WINDOW => {
                        merge(log, id, exit)
                    }
                    _ => {
                        self.recent_exits
                            .retain(|(at, _)| now.mono.saturating_sub(at.mono) <= MERGE_WINDOW);
                        self.recent_exits.push((now, exit));
                    }
                }
                let window_start = self
                    .attempt_start
                    .take()
                    .or(self.first_stamp)
                    .unwrap_or(now);
                let reached_up =
                    last_up_seen.is_some_and(|u| u.mono >= window_start.mono && u.mono <= now.mono);
                if status_probe_failed && !reached_up {
                    // Nobody was watching: neither a connected nor a failed attempt.
                } else if reached_up {
                    self.streak_attempts = 0;
                    self.streak_durations.clear();
                } else {
                    self.streak_attempts += 1;
                    self.streak_durations.push(after_s);
                    self.update_loop(now, log);
                }
            }
            SupervisorKind::Reconnecting { in_s } => {
                self.reconnecting = Some((in_s, now));
                log.push(now, EventKind::SupervisorReconnecting { in_s });
            }
            SupervisorKind::CircuitOpen { standing_down_s } => {
                self.breaker = Breaker::Open;
                log.push(now, EventKind::CircuitOpen { standing_down_s });
            }
            SupervisorKind::CircuitHalfOpen => {
                self.breaker = Breaker::HalfOpen;
                log.push(now, EventKind::CircuitHalfOpen);
            }
            SupervisorKind::Other => {}
        }
    }

    fn update_loop(&mut self, now: Stamp, log: &mut EventLog) {
        let attempts = self.streak_attempts;
        let avg_secs = self.streak_avg_secs().unwrap_or(0);
        match self.loop_event {
            Some(id) if attempts > LOOP_THRESHOLD => {
                log.amend(id, |k| {
                    if let EventKind::ReconnectLoop {
                        attempts: a,
                        avg_secs: s,
                    } = k
                    {
                        *a = attempts;
                        *s = avg_secs;
                    }
                });
            }
            None if attempts >= LOOP_THRESHOLD => {
                self.loop_event =
                    Some(log.push(now, EventKind::ReconnectLoop { attempts, avg_secs }));
            }
            _ => {}
        }
    }
}

fn merge(log: &mut EventLog, event_id: u64, exit: MasterExit) {
    log.amend(event_id, |k| {
        if let EventKind::MasterDown { supervisor, .. } = k {
            *supervisor = Some(exit);
        }
    });
}

/// The last breaker line wins; half-open stays until a master is observed running.
fn breaker_from_history(lines: &[SupervisorLine]) -> Breaker {
    lines
        .iter()
        .rev()
        .find_map(|l| match l.kind {
            SupervisorKind::CircuitOpen { .. } => Some(Breaker::Open),
            SupervisorKind::CircuitHalfOpen => Some(Breaker::HalfOpen),
            _ => None,
        })
        .unwrap_or(Breaker::Closed)
}

/// First line of `cur` not in `prev`, anchored on `prev`'s last line plus matching predecessors.
fn find_new_start(prev: &[String], cur: &[String]) -> usize {
    let Some(last) = prev.last() else {
        return 0;
    };
    for j in (0..cur.len()).rev() {
        if cur[j] != *last {
            continue;
        }
        let mut k = 1;
        let mut ok = true;
        while k <= j && k < prev.len() {
            if cur[j - k] != prev[prev.len() - 1 - k] {
                ok = false;
                break;
            }
            k += 1;
        }
        if ok {
            return j + 1;
        }
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse_supervisor_log;

    fn tail(text: &str) -> Vec<SupervisorLine> {
        parse_supervisor_log(text)
    }

    #[test]
    fn first_poll_is_history_and_sets_breaker() {
        let mut s = Supervisor::new();
        let mut log = EventLog::new();
        let breaker = include_str!("../../tests/fixtures/supervisor_breaker.log");
        s.on_tail(&tail(breaker), Stamp::at_secs(1), None, false, &mut log);
        assert!(log.take_fresh().is_empty(), "history never fires events");
        assert_eq!(
            s.breaker,
            Breaker::HalfOpen,
            "last breaker line is HALF-OPEN"
        );
        assert!(s.available);
        s.on_master_up(Stamp::at_secs(2), &mut log);
        assert_eq!(s.breaker, Breaker::Closed);

        let open_only = &breaker[..breaker.find("CIRCUIT HALF-OPEN").unwrap()];
        let mut s = Supervisor::new();
        s.on_tail(&tail(open_only), Stamp::at_secs(1), None, false, &mut log);
        assert_eq!(s.breaker, Breaker::Open);
    }

    #[test]
    fn new_lines_fire_events_and_history_lines_do_not_repeat() {
        let mut s = Supervisor::new();
        let mut log = EventLog::new();
        let base = "2026-10-11 03:00:00  opening master connection\n";
        s.on_tail(&tail(base), Stamp::at_secs(1), None, false, &mut log);
        let grown = format!(
            "{base}2026-10-11 03:00:05  master died after 5s (rc=255) — failure 7/8\n2026-10-11 03:00:05  reconnecting in 61s\n"
        );
        s.on_tail(
            &tail(&grown),
            Stamp::at_secs(3),
            Some(Stamp::at_secs(2)),
            false,
            &mut log,
        );
        let fresh = log.take_fresh();
        assert_eq!(fresh.len(), 1);
        assert_eq!(
            fresh[0].kind,
            EventKind::SupervisorReconnecting { in_s: 61 }
        );
        assert_eq!(s.reconnecting, Some((61, Stamp::at_secs(3))));
        s.on_tail(&tail(&grown), Stamp::at_secs(5), None, false, &mut log);
        assert!(log.take_fresh().is_empty(), "same tail: nothing new");
        let more = format!(
            "{grown}2026-10-11 03:01:11  CIRCUIT OPEN — 8 rapid failures; standing down for 300s\n2026-10-11 03:06:11  CIRCUIT HALF-OPEN — retrying\n"
        );
        s.on_tail(&tail(&more), Stamp::at_secs(7), None, false, &mut log);
        let kinds: Vec<EventKind> = log.take_fresh().into_iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            vec![
                EventKind::CircuitOpen {
                    standing_down_s: Some(300)
                },
                EventKind::CircuitHalfOpen
            ]
        );
        assert_eq!(s.breaker, Breaker::HalfOpen);
    }

    #[test]
    fn find_new_start_handles_sliding_window_and_rotation() {
        let v = |s: &[&str]| s.iter().map(|x| (*x).to_string()).collect::<Vec<_>>();
        assert_eq!(
            find_new_start(&v(&["a", "b", "c"]), &v(&["b", "c", "d", "e"])),
            2
        );
        assert_eq!(
            find_new_start(&v(&["a", "b", "c"]), &v(&["a", "b", "c"])),
            3
        );
        assert_eq!(find_new_start(&v(&[]), &v(&["x"])), 0);
        assert_eq!(
            find_new_start(&v(&["a", "b"]), &v(&["x", "y"])),
            0,
            "rotated: everything is new"
        );
        // Repeated line text: the anchor must also match the predecessor.
        assert_eq!(
            find_new_start(&v(&["o", "r", "o"]), &v(&["r", "o", "r", "o", "r"])),
            4
        );
        assert_eq!(
            find_new_start(&v(&["p", "r", "o"]), &v(&["r", "o", "p", "r", "o"])),
            5
        );
    }

    #[test]
    fn merge_exit_into_master_down_before_or_after_within_five_seconds() {
        let mut s = Supervisor::new();
        let mut log = EventLog::new();
        s.on_tail(&tail(""), Stamp::at_secs(0), None, false, &mut log);
        let id = log.push(
            Stamp::at_secs(10),
            EventKind::MasterDown {
                was_up: Some(Duration::from_secs(5736)),
                supervisor: None,
            },
        );
        s.on_master_down(id, Stamp::at_secs(10), &mut log);
        let t1 =
            "2026-10-09 16:46:40  master ended after 5736s (rc=255) — healthy run, backoff reset\n";
        s.on_tail(
            &tail(t1),
            Stamp::at_secs(13),
            Some(Stamp::at_secs(9)),
            false,
            &mut log,
        );
        assert_eq!(
            log.newest_first().find(|e| e.id == id).unwrap().kind,
            EventKind::MasterDown {
                was_up: Some(Duration::from_secs(5736)),
                supervisor: Some(MasterExit {
                    after_s: 5736,
                    rc: 255
                })
            }
        );
        let t2 = format!(
            "{t1}2026-10-09 16:47:56  master ended after 75s (rc=255) — healthy run, backoff reset\n"
        );
        s.on_tail(
            &tail(&t2),
            Stamp::at_secs(20),
            Some(Stamp::at_secs(19)),
            false,
            &mut log,
        );
        let id2 = log.push(
            Stamp::at_secs(22),
            EventKind::MasterDown {
                was_up: None,
                supervisor: None,
            },
        );
        s.on_master_down(id2, Stamp::at_secs(22), &mut log);
        assert!(matches!(
            log.newest_first().find(|e| e.id == id2).unwrap().kind,
            EventKind::MasterDown {
                supervisor: Some(MasterExit {
                    after_s: 75,
                    rc: 255
                }),
                ..
            }
        ));
        let id3 = log.push(
            Stamp::at_secs(30),
            EventKind::MasterDown {
                was_up: None,
                supervisor: None,
            },
        );
        s.on_master_down(id3, Stamp::at_secs(30), &mut log);
        let t3 = format!(
            "{t2}2026-10-09 16:49:13  master ended after 75s (rc=255) — healthy run, backoff reset\n"
        );
        s.on_tail(
            &tail(&t3),
            Stamp::at_secs(36),
            Some(Stamp::at_secs(29)),
            false,
            &mut log,
        );
        assert!(matches!(
            log.newest_first().find(|e| e.id == id3).unwrap().kind,
            EventKind::MasterDown {
                supervisor: None,
                ..
            }
        ));
    }

    #[test]
    fn reconnect_loop_fires_at_three_never_up_attempts_and_ends_on_up() {
        let mut s = Supervisor::new();
        let mut log = EventLog::new();
        s.on_tail(&tail(""), Stamp::at_secs(0), None, false, &mut log);
        let mut text = String::new();
        let mut t = 1;
        for n in 0..4u64 {
            text.push_str(&format!(
                "2026-10-09 16:4{n}:00  opening master connection\n"
            ));
            s.on_tail(&tail(&text), Stamp::at_secs(t), None, false, &mut log);
            t += 75;
            text.push_str(&format!("2026-10-09 16:4{n}:75  master ended after 7{n}s (rc=255) — healthy run, backoff reset\n"));
            s.on_tail(&tail(&text), Stamp::at_secs(t), None, false, &mut log);
            t += 2;
        }
        let fresh = log.take_fresh();
        assert_eq!(fresh.len(), 1, "loop event fires once, then grows in place");
        assert!(matches!(
            fresh[0].kind,
            EventKind::ReconnectLoop { attempts: 3, .. }
        ));
        assert_eq!(
            log.newest_first().next().unwrap().kind,
            EventKind::ReconnectLoop {
                attempts: 4,
                avg_secs: (70 + 71 + 72 + 73) / 4
            }
        );
        assert!(s.loop_active());
        s.on_master_up(Stamp::at_secs(t), &mut log);
        assert_eq!(
            log.take_fresh()[0].kind,
            EventKind::ReconnectLoopEnded { attempts: 4 }
        );
        assert!(!s.loop_active());
        assert_eq!(s.streak_attempts, 0);
    }

    #[test]
    fn an_attempt_that_reached_up_breaks_the_streak() {
        let mut s = Supervisor::new();
        let mut log = EventLog::new();
        s.on_tail(&tail(""), Stamp::at_secs(0), None, false, &mut log);
        let l1 = "2026-10-09 16:40:00  opening master connection\n";
        s.on_tail(&tail(l1), Stamp::at_secs(1), None, false, &mut log);
        let l2 = format!("{l1}2026-10-09 16:41:00  master ended after 60s (rc=255) — x\n");
        s.on_tail(&tail(&l2), Stamp::at_secs(62), None, false, &mut log);
        assert_eq!(s.streak_attempts, 1);
        let l3 = format!("{l2}2026-10-09 16:41:02  opening master connection\n");
        s.on_tail(&tail(&l3), Stamp::at_secs(64), None, false, &mut log);
        let l4 = format!("{l3}2026-10-09 16:42:00  master ended after 58s (rc=255) — x\n");
        s.on_tail(
            &tail(&l4),
            Stamp::at_secs(122),
            Some(Stamp::at_secs(100)),
            false,
            &mut log,
        );
        assert_eq!(s.streak_attempts, 0, "UP was seen during the attempt");
    }
}
