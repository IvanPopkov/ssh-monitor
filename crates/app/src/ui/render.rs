//! The frame (SPEC F1–F5, F8): pure ratatui rendering of a `View`.

use crate::app::View;
use crate::ui::format;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Chart, Dataset, GraphType, Paragraph, Sparkline};
use ssh_monitor_core::Stamp;
use ssh_monitor_core::monitor::{Badge, Monitor, RateSample, Severity, State};
use ssh_monitor_core::ring::Ring;
use std::time::Duration;

pub const MIN_COLS: u16 = 64;
pub const MIN_ROWS: u16 = 16;
/// status 1 · rx 1+2 · tx 1+2 · lat|clients 1+3 · events 1+3 = 15, plus one spare row.
const BASE_ROWS: u16 = 15;
const LOG_MIN: f64 = 2.0;
const LOG_MAX: f64 = 8.0;
const HISTORY_SECS: f64 = 300.0;
/// Below this height rx and tx share one mirrored panel.
pub const MERGE_TRAFFIC_BELOW_ROWS: u16 = 30;

/// Halves split the rx + tx chart rows evenly; an odd row goes to events.
pub fn merged_halves(rx_extra: u16, tx_extra: u16) -> (u16, u16) {
    let chart_rows = 2 + rx_extra + 1 + 2 + tx_extra; // both charts + the tx title row it saves
    (chart_rows / 2, chart_rows % 2)
}

/// Extra rows beyond `BASE_ROWS`, split 2:2:1 to rx:tx:events (rounded to nearest).
pub fn split_extra(extra: u16) -> (u16, u16, u16) {
    let share = (u32::from(extra) * 2 + 2) / 5;
    let rx = u16::try_from(share).unwrap_or(0).min(extra);
    let tx = rx.min(extra - rx);
    (rx, tx, extra - rx - tx)
}

pub fn render(frame: &mut Frame, view: &View<'_>) {
    let area = frame.area();
    if area.width < MIN_COLS || area.height < MIN_ROWS {
        let msg = format!(
            "terminal too small: {}×{} (need {MIN_COLS}×{MIN_ROWS})",
            area.width, area.height
        );
        frame.render_widget(
            Paragraph::new(msg).style(Style::default().fg(Color::Red)),
            area,
        );
        return;
    }
    let (rx_extra, tx_extra, ev_extra) = split_extra(area.height.saturating_sub(BASE_ROWS));
    let merged = area.height < MERGE_TRAFFIC_BELOW_ROWS;
    let (half, odd) = merged_halves(rx_extra, tx_extra);
    let traffic = if merged {
        vec![Constraint::Length(1 + 2 * half)]
    } else {
        vec![
            Constraint::Length(1 + 2 + rx_extra),
            Constraint::Length(1 + 2 + tx_extra),
        ]
    };
    let ev_rows = 1 + 3 + ev_extra + if merged { odd } else { 0 };
    let mut constraints = vec![Constraint::Length(1)];
    constraints.extend(traffic);
    constraints.extend([Constraint::Length(1 + 3), Constraint::Length(ev_rows)]);
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    let (rows_mid, rows_ev) = (rows[rows.len() - 2], rows[rows.len() - 1]);
    status_bar(frame, rows[0], view);
    let m = view.monitor;
    let rx_title = RateTitle {
        label: "rx",
        current: m.traffic.current.map(|(rx, _)| rx),
        peak: peak_within(&m.traffic.rx, view.now),
    };
    let tx_title = RateTitle {
        label: "tx",
        current: m.traffic.current.map(|(_, tx)| tx),
        peak: peak_within(&m.traffic.tx, view.now),
    };
    if merged {
        mirrored_chart(frame, rows[1], (&rx_title, &tx_title), m, view.now);
    } else {
        chart(
            frame,
            rows[1],
            &rx_title.full(),
            &m.traffic.rx,
            view.now,
            Color::Cyan,
        );
        chart(
            frame,
            rows[2],
            &tx_title.full(),
            &m.traffic.tx,
            view.now,
            Color::Magenta,
        );
    }
    let mid = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(55),
            Constraint::Length(1),
            Constraint::Min(10),
        ])
        .split(rows_mid);
    latency(frame, mid[0], m);
    clients(frame, mid[2], m, view.now);
    events(frame, rows_ev, m, view.utc_offset_secs);
}

fn title_rule(frame: &mut Frame, area: Rect, title: &str) {
    let text = format!("── {title} ");
    let fill = "─".repeat((area.width as usize).saturating_sub(text.chars().count()));
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(text, Style::default().add_modifier(Modifier::BOLD)),
            Span::styled(fill, Style::default().fg(Color::DarkGray)),
        ])),
        area,
    );
}

fn bold(color: Color) -> Style {
    Style::default().fg(color).add_modifier(Modifier::BOLD)
}

/// Same rules as the `--once` JSON (SPEC F1).
pub fn badge_span(badge: Badge) -> Span<'static> {
    let (glyph, color) = match badge {
        Badge::Storm => ("⚠", Color::Red),
        Badge::Breaker => ("⚡", Color::Red),
        Badge::Retry => ("↻", Color::Yellow),
    };
    Span::styled(format!("{glyph} {}", badge.name()), bold(color))
}

/// In priority order (first = never dropped).
pub fn status_fields(view: &View<'_>) -> Vec<Span<'static>> {
    let m = view.monitor;
    let now = view.now;
    let state = m.known_state();
    let (light, text, color) = match state {
        Some(State::Up) => ("●", "UP".to_string(), Color::Green),
        Some(State::Degraded) => {
            let why = m
                .health
                .reason
                .as_ref()
                .map(format::degrade_reason_short)
                .unwrap_or("");
            ("◐", format!("DEGRADED ({why})"), Color::Yellow)
        }
        Some(State::Down) => ("○", "DOWN".to_string(), Color::Red),
        None => ("?", "n/a".to_string(), Color::DarkGray),
    };
    let mut fields = vec![Span::styled(format!("{light} {text}"), bold(color))];
    fields.extend(m.badges().into_iter().map(badge_span));
    fields.push(Span::styled(m.cfg.host_alias.clone(), bold(Color::White)));
    let since = now.mono.saturating_sub(m.health.since.mono);
    match (state, &m.health.master) {
        (Some(State::Down), _) => {
            fields.push(Span::raw(format!("down {}", format::duration(since))));
            if let Some(was) = m.health.was_up_for {
                fields.push(Span::raw(format!("(was up {})", format::duration(was))));
            }
        }
        (Some(_), Some(master)) => {
            fields.push(Span::raw(format!(
                "up {}",
                master.uptime_at(now).map_or("n/a".into(), format::duration)
            )));
            fields.push(Span::styled(
                format!("for {}", format::duration(since)),
                Style::default().fg(Color::DarkGray),
            ));
        }
        _ => {}
    }
    if !m.supervisor.available {
        fields.push(Span::styled(
            "log n/a",
            Style::default().fg(Color::DarkGray),
        ));
    }
    fields
}

fn status_bar(frame: &mut Frame, area: Rect, view: &View<'_>) {
    let mut fields = status_fields(view);
    let clock = format::clock(view.now.wall, view.utc_offset_secs);
    let mut right = Vec::new();
    if view.paused {
        right.push(Span::styled(
            " PAUSED ",
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
        right.push(Span::raw(" "));
    }
    right.push(Span::styled(clock, Style::default().fg(Color::DarkGray)));
    let right = Line::from(right);
    let right_w = right.width() as u16;
    let avail = area.width.saturating_sub(right_w + 1) as usize;
    // Drop the lowest-priority fields until the left side fits (the state light always stays).
    let width_of =
        |f: &[Span]| f.iter().map(|s| s.width()).sum::<usize>() + f.len().saturating_sub(1) * 2;
    while fields.len() > 1 && width_of(&fields) > avail {
        fields.pop();
    }
    let mut spans = Vec::new();
    for (i, f) in fields.into_iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw("  "));
        }
        spans.push(f);
    }
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x, area.y, area.width.saturating_sub(right_w + 1), 1),
    );
    if area.width > right_w {
        let right_area = Rect::new(area.x + area.width - right_w, area.y, right_w, 1);
        frame.render_widget(Paragraph::new(right), right_area);
    }
}

/// x = seconds ago, height = log10(bps) above the floor. Each rate fills back to the previous
/// entry every `FILL_STEP` so columns are solid at any `--interval`; gaps (`None`) stay empty.
pub fn chart_points(ring: &Ring<RateSample>, now: Stamp) -> Vec<(f64, f64)> {
    const FILL_STEP: f64 = 0.5;
    let mut out = Vec::new();
    let mut prev_ago: Option<f64> = None;
    for s in ring.iter() {
        let ago = now.mono.saturating_sub(s.stamp.mono).as_secs_f64();
        if let Some(bps) = s.bps {
            let y = if bps <= 0.0 {
                LOG_MIN
            } else {
                bps.log10().clamp(LOG_MIN, LOG_MAX)
            } - LOG_MIN;
            let steps = prev_ago.map_or(0.0, |p| ((p - ago) / FILL_STEP - 1e-9).floor().max(0.0));
            for k in (0..=steps as u32).rev() {
                let t = ago + f64::from(k) * FILL_STEP;
                if t <= HISTORY_SECS {
                    out.push((-t, y));
                }
            }
        }
        prev_ago = Some(ago);
    }
    out
}

/// SPEC F2 5-min peak, judged against `now` so an outage without samples still ages it.
pub fn peak_within(ring: &Ring<RateSample>, now: Stamp) -> Option<f64> {
    ring.iter()
        .filter(|s| now.mono.saturating_sub(s.stamp.mono).as_secs_f64() <= HISTORY_SECS)
        .filter_map(|s| s.bps)
        .reduce(f64::max)
}

fn chart(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    ring: &Ring<RateSample>,
    now: Stamp,
    color: Color,
) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    title_rule(frame, parts[0], title);
    let points = chart_points(ring, now);
    let dataset = Dataset::default()
        .marker(Marker::Braille)
        .graph_type(GraphType::Bar)
        .style(Style::default().fg(color))
        .data(&points);
    let widget = Chart::new(vec![dataset])
        .x_axis(Axis::default().bounds([-HISTORY_SECS, 0.0]))
        .y_axis(Axis::default().bounds([0.0, LOG_MAX - LOG_MIN]));
    frame.render_widget(widget, parts[1]);
}

pub struct RateTitle {
    pub label: &'static str,
    pub current: Option<f64>,
    pub peak: Option<f64>,
}

impl RateTitle {
    pub fn full(&self) -> String {
        format!(
            "{} · peak {}",
            self.current_only(),
            format::rate_or_na(self.peak)
        )
    }

    pub fn compact(&self) -> String {
        format!(
            "{} pk {}",
            self.current_only(),
            format::rate_or_na(self.peak)
        )
    }

    pub fn current_only(&self) -> String {
        format!("{} {}", self.label, format::rate_or_na(self.current))
    }
}

/// Full, then compact (fits 64 columns below 1000 GB/s), then the rates alone.
pub fn merged_titles(rx: &RateTitle, tx: &RateTitle, width: u16) -> (String, String, &'static str) {
    let fits = |rx: &str, tx: &str, gap: &str| {
        format!("── ↓ {rx}{gap}↑ {tx} ").chars().count() <= usize::from(width)
    };
    let (full_rx, full_tx) = (rx.full(), tx.full());
    if fits(&full_rx, &full_tx, "   ") {
        return (full_rx, full_tx, "   ");
    }
    let (compact_rx, compact_tx) = (rx.compact(), tx.compact());
    if fits(&compact_rx, &compact_tx, "  ") {
        return (compact_rx, compact_tx, "  ");
    }
    (rx.current_only(), tx.current_only(), "  ")
}

/// Arrows show data direction (rx comes down from the host). The zero line sits on a cell
/// boundary so rx and tx colours never share a cell.
fn mirrored_chart(
    frame: &mut Frame,
    area: Rect,
    (rx_title, tx_title): (&RateTitle, &RateTitle),
    m: &Monitor,
    now: Stamp,
) {
    let half = area.height.saturating_sub(1) / 2;
    let (rx_t, tx_t, gap) = merged_titles(rx_title, tx_title, area.width);
    let head = vec![
        Span::styled("── ", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!("↓ {rx_t}"), bold(Color::Cyan)),
        Span::raw(gap),
        Span::styled(format!("↑ {tx_t} "), bold(Color::Magenta)),
    ];
    let used: usize = head.iter().map(|s| s.width()).sum();
    let mut head = head;
    head.push(Span::styled(
        "─".repeat(usize::from(area.width).saturating_sub(used)),
        Style::default().fg(Color::DarkGray),
    ));
    frame.render_widget(
        Paragraph::new(Line::from(head)),
        Rect::new(area.x, area.y, area.width, 1),
    );
    let span = LOG_MAX - LOG_MIN;
    let up = Rect::new(area.x, area.y + 1, area.width, half);
    let down = Rect::new(area.x, area.y + 1 + half, area.width, half);
    let rx = chart_points(&m.traffic.rx, now);
    let tx: Vec<(f64, f64)> = chart_points(&m.traffic.tx, now)
        .into_iter()
        .map(|(x, y)| (x, -y))
        .collect();
    for (rect, points, bounds, color) in [
        (up, &rx, [0.0, span], Color::Cyan),
        (down, &tx, [-span, 0.0], Color::Magenta),
    ] {
        let dataset = Dataset::default()
            .marker(Marker::Braille)
            .graph_type(GraphType::Bar)
            .style(Style::default().fg(color))
            .data(points);
        frame.render_widget(
            Chart::new(vec![dataset])
                .x_axis(Axis::default().bounds([-HISTORY_SECS, 0.0]))
                .y_axis(Axis::default().bounds(bounds)),
            rect,
        );
    }
}

/// Sized to fit 35 columns (the 64×16 minimum).
pub fn latency_lines(m: &Monitor) -> (String, String) {
    let st = m.latency.stats();
    let loss = st
        .loss_pct
        .map_or("n/a".to_string(), |p| format!("{p:.1}%"));
    (
        format!(
            "now {}  jitter {}  loss {loss}",
            format::millis_or_na(st.now_ms),
            format::millis_or_na(st.jitter_ms)
        ),
        format!(
            "min {}  avg {}  max {}",
            format::millis_or_na(st.min_ms),
            format::millis_or_na(st.avg_ms),
            format::millis_or_na(st.max_ms)
        ),
    )
}

fn latency(frame: &mut Frame, area: Rect, m: &Monitor) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .split(area);
    title_rule(
        frame,
        parts[0],
        &format!("latency · {}", format::path_title(m.path.kind())),
    );
    let (line1, line2) = latency_lines(m);
    frame.render_widget(Paragraph::new(line1), parts[1]);
    frame.render_widget(Paragraph::new(line2), parts[2]);
    // Lost pings and gaps are blanks.
    let width = parts[3].width as usize;
    let rtts: Vec<Option<f64>> = m
        .latency
        .samples
        .iter()
        .rev()
        .take(width)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .map(|s| s.rtt_ms)
        .collect();
    let data = sparkline_values(&rtts, parts[3].height);
    frame.render_widget(
        Sparkline::default()
            .data(data)
            .absent_value_symbol(" ")
            .style(Style::default().fg(Color::Blue)),
        parts[3],
    );
}

/// Every reply is at least `▁` so a fast one never looks like a lost ping's blank.
pub fn sparkline_values(rtts: &[Option<f64>], rows: u16) -> Vec<Option<u64>> {
    let ms: Vec<Option<u64>> = rtts
        .iter()
        .map(|r| r.map(|ms| ms.max(1.0).round() as u64))
        .collect();
    let max = ms.iter().flatten().copied().max().unwrap_or(1);
    let floor = max.div_ceil(8 * u64::from(rows.max(1)));
    ms.into_iter().map(|v| v.map(|v| v.max(floor))).collect()
}

/// Direct (`⚠`) clients first so an overflow never hides them.
pub fn client_cells(m: &Monitor, now: Stamp) -> Vec<(String, bool)> {
    let mut cells: Vec<(String, bool)> = m
        .clients
        .list
        .iter()
        .map(|c| {
            let label = format!(
                "{} {}",
                format::client_line(c),
                format::duration(c.age_at(now))
            );
            (label, c.direct)
        })
        .collect();
    cells.sort_by_key(|(_, direct)| !direct);
    cells
}

/// Column-major, stopping with `+k more` when the width runs out.
pub fn layout_cells(
    cells: &[(String, bool)],
    rows: usize,
    width: usize,
) -> (Vec<Vec<(String, bool)>>, usize) {
    let mut lines: Vec<Vec<(String, bool)>> = vec![Vec::new(); rows.max(1)];
    if cells.is_empty() || rows == 0 || width == 0 {
        return (lines, 0);
    }
    let cell_w =
        |(label, direct): &(String, bool)| label.chars().count() + usize::from(*direct) * 2;
    let mut used = 0usize;
    let mut shown = 0usize;
    for col in cells.chunks(rows) {
        let col_w = col.iter().map(cell_w).max().unwrap_or(0) + 2;
        let remaining = cells.len() - shown - col.len();
        let more_w = if remaining > 0 {
            format!("+{remaining} more").len()
        } else {
            0
        };
        if used + col_w + more_w > width {
            break;
        }
        for (row, cell) in col.iter().enumerate() {
            let (label, direct) = cell;
            let text = if *direct {
                format!("⚠ {label}")
            } else {
                label.clone()
            };
            lines[row].push((format!("{text:<col_w$}"), *direct));
        }
        used += col_w;
        shown += col.len();
    }
    if shown < cells.len() {
        let row = shown % rows.max(1);
        lines[row].push((format!("+{} more", cells.len() - shown), false));
    }
    (lines, shown)
}

fn clients(frame: &mut Frame, area: Rect, m: &Monitor, now: Stamp) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    let down = m.known_state() == Some(State::Down);
    let known = m.known_clients();
    let title = match known {
        None => "clients · n/a".to_string(),
        Some(_) if down => "clients · no master".to_string(),
        Some(c) => format!("clients ({})", c.list.len()),
    };
    title_rule(frame, parts[0], &title);
    let body = parts[1];
    // A failed probe leaves the last list in the model; it is not drawn as if current.
    let cells = known.map_or_else(Vec::new, |_| client_cells(m, now));
    if cells.is_empty() {
        let text = if known.is_none() {
            "n/a"
        } else if down {
            "no master"
        } else {
            "none"
        };
        frame.render_widget(
            Paragraph::new(Span::styled(text, Style::default().fg(Color::DarkGray))),
            body,
        );
        return;
    }
    let (rows, _) = layout_cells(&cells, body.height as usize, body.width as usize);
    let lines: Vec<Line> = rows
        .into_iter()
        .map(|cells| {
            Line::from(
                cells
                    .into_iter()
                    .map(|(text, direct)| {
                        let style = if direct {
                            Style::default().fg(Color::Red)
                        } else if text.starts_with('+') {
                            Style::default().fg(Color::DarkGray)
                        } else {
                            Style::default()
                        };
                        Span::styled(text, style)
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), body);
}

pub fn severity_color(s: Severity) -> Color {
    match s {
        Severity::Green => Color::Green,
        Severity::Yellow => Color::Yellow,
        Severity::Red => Color::Red,
        Severity::Dim => Color::DarkGray,
    }
}

fn events(frame: &mut Frame, area: Rect, m: &Monitor, utc_offset_secs: i32) {
    let parts = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(1), Constraint::Min(1)])
        .split(area);
    title_rule(frame, parts[0], "events");
    let lines: Vec<Line> = m
        .events
        .newest_first()
        .take(parts[1].height as usize)
        .map(|e| {
            Line::from(vec![
                Span::styled(
                    format!("{} ", format::clock(e.at.wall, utc_offset_secs)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format::event_text(&e.kind),
                    Style::default().fg(severity_color(e.severity())),
                ),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), parts[1]);
}

pub fn history_window() -> Duration {
    Duration::from_secs_f64(HISTORY_SECS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ssh_monitor_core::monitor::Traffic;

    #[test]
    fn chart_points_are_log_heights_above_the_floor_and_skip_gaps() {
        let mut t = Traffic::new();
        for (secs, bps) in [
            (0, None),
            (1, Some(0.0)),
            (2, Some(10_000.0)),
            (3, Some(1e12)),
        ] {
            t.rx.push(RateSample {
                stamp: Stamp::at_secs(secs),
                bps,
            });
        }
        let pts = chart_points(&t.rx, Stamp::at_secs(3));
        assert_eq!(
            pts,
            vec![
                (-2.5, 0.0),
                (-2.0, 0.0),
                (-1.5, 2.0),
                (-1.0, 2.0),
                (-0.5, 6.0),
                (0.0, 6.0)
            ],
            "gap entry has no column · idle on the baseline · 10 kB/s two decades up · clamped \
             at 100 MB/s · each rate fills back to the tick it was diffed against"
        );
        let mut t = Traffic::new();
        for (secs, bps) in [
            (0, Some(1000.0)),
            (2, Some(1000.0)),
            (10, Some(1000.0)),
            (11, None),
        ] {
            t.rx.push(RateSample {
                stamp: Stamp::at_secs(secs),
                bps,
            });
        }
        let xs: Vec<f64> = chart_points(&t.rx, Stamp::at_secs(12))
            .into_iter()
            .map(|(x, _)| x)
            .collect();
        let solid: Vec<f64> = (4..=24).rev().map(|k| -f64::from(k) * 0.5).collect();
        assert_eq!(
            xs, solid,
            "−12 … −2 every 0.5 s, nothing after the gap at −1"
        );
        assert_eq!(history_window(), Duration::from_secs(300));
    }

    #[test]
    fn sparkline_keeps_fast_replies_visible_next_to_slow_ones() {
        let rtts = [Some(3.0), Some(30.0), None, Some(0.2), Some(3.0)];
        let v = sparkline_values(&rtts, 1);
        assert_eq!(
            v,
            vec![Some(4), Some(30), None, Some(4), Some(4)],
            "30/8 rounds up to 4: every reply is at least ▁, only the lost ping is absent"
        );
        assert_eq!(
            sparkline_values(&rtts, 2),
            vec![Some(3), Some(30), None, Some(2), Some(3)],
            "two rows: the floor is 30/16"
        );
        assert_eq!(sparkline_values(&[None, None], 1), vec![None, None]);
    }

    #[test]
    fn merged_titles_keep_peaks_at_64_columns() {
        let title = |label, current, peak| RateTitle {
            label,
            current: Some(current),
            peak: Some(peak),
        };
        let (rx, tx) = (
            title("rx", 123_400.0, 999_900.0),
            title("tx", 456_700.0, 888_800.0),
        );
        let (r, t, gap) = merged_titles(&rx, &tx, 90);
        assert_eq!(
            (r.as_str(), t.as_str(), gap),
            (
                "rx 123.4 kB/s · peak 999.9 kB/s",
                "tx 456.7 kB/s · peak 888.8 kB/s",
                "   "
            )
        );
        let (r, t, gap) = merged_titles(&rx, &tx, 64);
        assert_eq!(
            (r.as_str(), t.as_str(), gap),
            (
                "rx 123.4 kB/s pk 999.9 kB/s",
                "tx 456.7 kB/s pk 888.8 kB/s",
                "  "
            ),
            "widest rates still fit the minimum width with their peaks"
        );
        let width = "── ↓ ".chars().count() + r.chars().count() + 2 + 2 + t.chars().count() + 1;
        assert_eq!(width, 64);
        let (r, t, _) = merged_titles(
            &title("rx", 999_960.0, 999_949.0),
            &title("tx", 123_456.0, 999_999.0),
            64,
        );
        assert!(r.contains(" pk ") && t.contains(" pk "), "{r} / {t}");
        let (r, t, _) = merged_titles(&rx, &tx, 40);
        assert_eq!((r.as_str(), t.as_str()), ("rx 123.4 kB/s", "tx 456.7 kB/s"));
        let na = RateTitle {
            label: "rx",
            current: None,
            peak: None,
        };
        assert_eq!(na.full(), "rx n/a · peak n/a");
    }

    #[test]
    fn peak_is_over_the_last_five_minutes_only() {
        let mut t = Traffic::new();
        t.rx.push(RateSample {
            stamp: Stamp::at_secs(0),
            bps: Some(5e6),
        });
        t.rx.push(RateSample {
            stamp: Stamp::at_secs(100),
            bps: Some(1e3),
        });
        assert_eq!(peak_within(&t.rx, Stamp::at_secs(200)), Some(5e6));
        assert_eq!(
            peak_within(&t.rx, Stamp::at_secs(350)),
            Some(1e3),
            "the 5 MB/s sample aged out during a quiet stretch"
        );
        assert_eq!(peak_within(&t.rx, Stamp::at_secs(700)), None);
    }

    #[test]
    fn extra_rows_split_two_two_one_nearest() {
        assert_eq!(split_extra(0), (0, 0, 0));
        assert_eq!(split_extra(1), (0, 0, 1));
        assert_eq!(split_extra(2), (1, 1, 0));
        assert_eq!(split_extra(5), (2, 2, 1));
        assert_eq!(split_extra(9), (4, 4, 1));
        assert_eq!(split_extra(25), (10, 10, 5));
        // Merged panel: 2+2 chart rows + the saved tx title = 5 → halves of 2, 1 row to events.
        assert_eq!(merged_halves(0, 0), (2, 1));
        assert_eq!(merged_halves(1, 1), (3, 1));
        assert_eq!(merged_halves(5, 5), (7, 1));
        assert_eq!(merged_halves(5, 4), (7, 0));
    }

    #[test]
    fn cells_flow_into_columns_with_overflow_marker() {
        let cells: Vec<(String, bool)> = (1..=7).map(|i| (format!("#{i} 12s"), false)).collect();
        let (lines, shown) = layout_cells(&cells, 3, 23);
        assert_eq!(
            shown, 6,
            "two 8-wide columns plus the 7-wide marker fit in 23 columns, the 7th cell does not"
        );
        assert_eq!(
            lines[0].len(),
            3,
            "two cells plus the +1 more marker on the first row"
        );
        assert_eq!(lines[0][2].0, "+1 more");
        let (lines, shown) = layout_cells(&cells, 3, 80);
        assert_eq!(shown, 7);
        assert!(
            lines
                .iter()
                .all(|l| l.iter().all(|(t, _)| !t.starts_with('+')))
        );
        let (_, shown) = layout_cells(&[], 3, 80);
        assert_eq!(shown, 0);
    }
}
