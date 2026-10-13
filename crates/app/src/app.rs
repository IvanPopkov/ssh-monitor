//! The main loop (A5): probe tasks → mpsc → the single owner of the `Monitor`.

use crate::clock::Clock;
use crate::config::Host;
use crate::csv::CsvLog;
use crate::probes::{self, ProbeContext};
use crate::runner::CommandRunner;
use anyhow::Context;
use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{Input, Monitor, MonitorConfig};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::time::MissedTickBehavior;

pub const RENDER_PERIOD: Duration = Duration::from_millis(250);
pub const ONCE_SAMPLE: Duration = Duration::from_secs(3);
/// Grace for in-flight commands after the `--once` window (a lost ping takes ~1s).
pub const DRAIN_MAX: Duration = Duration::from_secs(6);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Quit,
    Pause,
    Clear,
    Redraw,
}

pub struct View<'a> {
    pub monitor: &'a Monitor,
    pub now: Stamp,
    pub paused: bool,
    pub utc_offset_secs: i32,
}

pub trait Frontend {
    fn draw(&mut self, view: &View<'_>) -> anyhow::Result<()>;
}

pub struct Headless;

impl Frontend for Headless {
    fn draw(&mut self, _view: &View<'_>) -> anyhow::Result<()> {
        Ok(())
    }
}

pub struct Session {
    pub host: Host,
    pub runner: Arc<dyn CommandRunner>,
    pub clock: Arc<dyn Clock>,
    pub interval: Duration,
    pub csv: Option<CsvLog>,
    pub utc_offset_secs: i32,
}

impl Session {
    pub fn monitor_config(&self) -> MonitorConfig {
        MonitorConfig {
            host_alias: self.host.alias.clone(),
            hostname: self.host.cfg.hostname.clone(),
            server_alive_interval: self.host.cfg.server_alive_interval,
        }
    }

    fn probe_context(&self) -> ProbeContext {
        ProbeContext {
            alias: self.host.alias.clone(),
            hostname: self.host.cfg.hostname.clone(),
            supervisor_log: self.host.supervisor_log.clone(),
            interval: self.interval,
        }
    }
}

async fn next_key(keys: &mut Option<mpsc::Receiver<Key>>) -> Key {
    match keys {
        Some(rx) => rx.recv().await.unwrap_or(Key::Quit),
        None => std::future::pending().await,
    }
}

async fn deadline(after: Option<Duration>) {
    match after {
        Some(d) => tokio::time::sleep(d).await,
        None => std::future::pending().await,
    }
}

struct Loop {
    monitor: Monitor,
    csv: Option<CsvLog>,
    csv_error: Option<std::io::Error>,
    paused: bool,
    /// Frozen at `p`; redrawn on resize while paused.
    frozen: Option<(Monitor, Stamp)>,
}

impl Loop {
    fn apply(&mut self, input: Input, stamp: Stamp, pid_tx: &watch::Sender<probes::PidState>) {
        let events = self.monitor.apply(input, stamp);
        if self.monitor.health.initialized() {
            pid_tx.send_replace(Some(self.monitor.health.master_pid()));
        }
        if let Some(csv) = self.csv.as_mut() {
            csv.note_events(&events);
        }
    }

    /// A write error ends logging (reported on return) but never stops monitoring.
    fn csv_row(&mut self, now: Stamp) {
        if !self.monitor.health.initialized() {
            return;
        }
        if let Some(csv) = self.csv.as_mut()
            && let Err(e) = csv.write_row(&self.monitor, now)
        {
            self.csv_error = Some(e);
            self.csv = None;
        }
    }

    fn draw(
        &self,
        frontend: &mut dyn Frontend,
        now: Stamp,
        utc_offset_secs: i32,
    ) -> anyhow::Result<()> {
        if self.paused {
            return Ok(());
        }
        self.draw_frozen(frontend, now, utc_offset_secs)
    }

    fn draw_frozen(
        &self,
        frontend: &mut dyn Frontend,
        now: Stamp,
        utc_offset_secs: i32,
    ) -> anyhow::Result<()> {
        let (monitor, now) = match &self.frozen {
            Some((m, at)) if self.paused => (m, *at),
            _ => (&self.monitor, now),
        };
        frontend.draw(&View {
            monitor,
            now,
            paused: self.paused,
            utc_offset_secs,
        })
    }
}

/// On the deadline, stops launching probes and drains in-flight ones so `--once` sees the last ping.
pub async fn run(
    session: Session,
    frontend: &mut dyn Frontend,
    mut keys: Option<mpsc::Receiver<Key>>,
    stop_after: Option<Duration>,
) -> anyhow::Result<Monitor> {
    let monitor_config = session.monitor_config();
    let probe_context = session.probe_context();
    let Session {
        host: _,
        runner,
        clock,
        interval,
        csv,
        utc_offset_secs,
    } = session;
    let mut state = Loop {
        monitor: Monitor::new(monitor_config),
        csv,
        csv_error: None,
        paused: false,
        frozen: None,
    };
    let (tx, mut rx) = mpsc::channel::<(Stamp, Input)>(256);
    let (pid_tx, pid_rx) = watch::channel::<probes::PidState>(None);
    let (stop_tx, stop_rx) = watch::channel(false);
    let handles = probes::spawn_all(
        probe_context,
        Arc::clone(&runner),
        Arc::clone(&clock),
        pid_rx,
        stop_rx,
        tx,
    );
    let mut base = tokio::time::interval(interval);
    base.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut render = tokio::time::interval(RENDER_PERIOD);
    render.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let stop = deadline(stop_after);
    tokio::pin!(stop);
    let mut drain = false;
    let mut draw_error: Option<anyhow::Error> = None;
    loop {
        tokio::select! {
            biased;
            key = next_key(&mut keys) => match key {
                Key::Quit => break,
                Key::Pause => {
                    state.paused = !state.paused;
                    state.frozen = state.paused.then(|| (state.monitor.clone(), clock.now()));
                    // One frozen frame carrying the PAUSED badge; nothing is drawn after it.
                    if state.paused
                        && let Err(e) = state.draw_frozen(frontend, clock.now(), utc_offset_secs)
                    {
                        draw_error = Some(e);
                        break;
                    }
                }
                Key::Clear => state.monitor.clear_events(),
                Key::Redraw => {
                    if let Err(e) = state.draw_frozen(frontend, clock.now(), utc_offset_secs) {
                        draw_error = Some(e);
                        break;
                    }
                }
            },
            Some((stamp, input)) = rx.recv() => state.apply(input, stamp, &pid_tx),
            _ = base.tick() => {
                let now = clock.now();
                state.apply(Input::Tick, now, &pid_tx);
                state.csv_row(now);
            },
            _ = render.tick() => {
                if let Err(e) = state.draw(frontend, clock.now(), utc_offset_secs) {
                    draw_error = Some(e);
                    break;
                }
            },
            () = &mut stop => {
                drain = true;
                break;
            }
        }
    }
    let _ = stop_tx.send(true);
    if drain {
        let collect = async {
            while let Some((stamp, input)) = rx.recv().await {
                state.apply(input, stamp, &pid_tx);
            }
        };
        let _ = tokio::time::timeout(DRAIN_MAX, collect).await;
    }
    for h in handles {
        h.abort();
    }
    if state.csv.as_ref().is_some_and(CsvLog::has_pending) {
        state.csv_row(clock.now());
    }
    if let Some(e) = draw_error {
        return Err(e);
    }
    if let Some(e) = state.csv_error {
        return Err(e).context("writing --log CSV");
    }
    Ok(state.monitor)
}
