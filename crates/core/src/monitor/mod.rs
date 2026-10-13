//! Each detector owns its state and only reads the others (decision A9); the shell formats.

pub mod clients;
pub mod event;
pub mod health;
pub mod known;
pub mod latency;
pub mod path;
pub mod probe_health;
pub mod sleep;
pub mod storm;
pub mod supervisor;
pub mod traffic;

pub use clients::{Client, Clients};
pub use event::{
    ClientRef, DegradeReason, EVENTS_KEPT, Event, EventKind, EventLog, MasterExit, ProbeKind,
    Severity, StormCulprit,
};
pub use health::{Health, MasterInfo, State, Transition};
pub use known::Badge;
pub use latency::{Latency, LatencyStats, RttSample};
pub use path::Path;
pub use probe_health::ProbeHealth;
pub use sleep::Sleep;
pub use storm::Storm;
pub use supervisor::{Breaker, Supervisor};
pub use traffic::{RateSample, Traffic};

use crate::Stamp;
use crate::parse::{
    ByteCounters, MasterStatus, PeerPath, PingResult, Process, SupervisorLine, TcpConn,
};
use std::time::Duration;

/// Resolved once at startup via `ssh -G`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorConfig {
    pub host_alias: String,
    pub hostname: String,
    /// `ServerAliveInterval` in seconds; `None` disables the stall rule.
    pub server_alive_interval: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Input {
    /// Base tick: re-evaluates time-based rules without new data.
    Tick,
    Status(MasterStatus),
    Uptime(Duration),
    /// `None` = nettop had no row for the PID.
    Traffic(Option<ByteCounters>),
    Latency(PingResult),
    Processes(Vec<Process>),
    Connections(Vec<TcpConn>),
    Supervisor(Vec<SupervisorLine>),
    /// `None` = peer absent.
    Path(Option<PeerPath>),
    Failed {
        probe: ProbeKind,
        reason: String,
    },
}

#[derive(Debug, Clone)]
pub struct Monitor {
    pub cfg: MonitorConfig,
    pub health: Health,
    pub traffic: Traffic,
    pub latency: Latency,
    pub storm: Storm,
    pub supervisor: Supervisor,
    pub clients: Clients,
    pub path: Path,
    pub probes: ProbeHealth,
    pub sleep: Sleep,
    pub events: EventLog,
    pub started: Option<Stamp>,
    pub now: Stamp,
}

impl Monitor {
    pub fn new(cfg: MonitorConfig) -> Self {
        Self {
            cfg,
            health: Health::new(),
            traffic: Traffic::new(),
            latency: Latency::new(),
            storm: Storm::default(),
            supervisor: Supervisor::new(),
            clients: Clients::default(),
            path: Path::default(),
            probes: ProbeHealth::default(),
            sleep: Sleep::default(),
            events: EventLog::new(),
            started: None,
            now: Stamp::default(),
        }
    }

    pub fn state(&self) -> State {
        self.health.state
    }

    /// Returns the events it fired, oldest first.
    pub fn apply(&mut self, input: Input, now: Stamp) -> Vec<Event> {
        self.started.get_or_insert(now);
        self.now = now;
        if let Some(gap) = self.sleep.observe(now) {
            self.events.push(now, EventKind::SystemResumed { gap });
            self.traffic.on_resume(now);
            self.latency.on_resume(now, self.health.state);
        }
        match input {
            Input::Tick => {}
            Input::Status(status) => {
                self.probes.ok(ProbeKind::Status, now, &mut self.events);
                match self.health.on_status(&status, now, &mut self.events) {
                    Transition::None => {}
                    Transition::Up | Transition::Restarted => {
                        self.traffic.on_master_changed(now);
                        self.latency.on_master_up();
                        self.supervisor.on_master_up(now, &mut self.events);
                    }
                    Transition::Down { event_id } => {
                        self.traffic.on_master_lost();
                        self.supervisor
                            .on_master_down(event_id, now, &mut self.events);
                    }
                }
            }
            Input::Uptime(d) => {
                self.probes.ok(ProbeKind::Uptime, now, &mut self.events);
                self.health.set_uptime(d, now);
            }
            Input::Traffic(counters) => {
                self.probes.ok(ProbeKind::Traffic, now, &mut self.events);
                if self.health.master.is_some() {
                    self.traffic.on_sample(counters, now);
                } else {
                    self.traffic.on_gap(now);
                }
            }
            Input::Latency(ping) => {
                self.probes.ok(ProbeKind::Latency, now, &mut self.events);
                self.latency
                    .on_ping(ping, now, self.health.state, &mut self.events);
            }
            Input::Processes(procs) => {
                self.probes.ok(ProbeKind::Clients, now, &mut self.events);
                let master = self.health.master_pid();
                self.clients.on_processes(
                    &procs,
                    &self.cfg.host_alias,
                    &self.cfg.hostname,
                    master,
                    now,
                    &mut self.events,
                );
                self.storm.on_processes(&procs, now, &mut self.events);
            }
            Input::Connections(conns) => {
                self.probes.ok(ProbeKind::Rogues, now, &mut self.events);
                let master = self.health.master_pid();
                self.storm
                    .on_connections(&conns, master, now, &mut self.events);
                self.clients.on_connections(&conns, master);
            }
            Input::Supervisor(lines) => {
                self.probes.ok(ProbeKind::Supervisor, now, &mut self.events);
                let status_failed = self.probes.is_failed(ProbeKind::Status);
                self.supervisor.on_tail(
                    &lines,
                    now,
                    self.health.last_up_seen,
                    status_failed,
                    &mut self.events,
                );
            }
            Input::Path(peer) => {
                self.probes.ok(ProbeKind::Path, now, &mut self.events);
                if self.path.on_sample(peer, now, &mut self.events) {
                    self.latency.reset_baseline();
                }
            }
            Input::Failed { probe, reason } => {
                self.probes.failed(probe, &reason, now, &mut self.events);
                match probe {
                    ProbeKind::Traffic => self.traffic.on_gap(now),
                    ProbeKind::Latency => self.latency.on_failed(now, self.health.state),
                    ProbeKind::Supervisor => self.supervisor.on_unavailable(),
                    ProbeKind::Path => self.path.on_failed(),
                    _ => {}
                }
            }
        }
        self.health.evaluate(
            now,
            &self.latency,
            &self.traffic,
            self.cfg.server_alive_interval,
            &mut self.events,
        );
        self.events.take_fresh()
    }

    /// `c` key.
    pub fn clear_events(&mut self) {
        self.events.clear();
    }
}
