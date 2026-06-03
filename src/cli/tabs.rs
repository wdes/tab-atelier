// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! `tab-atelier tabs [--once]`
//!
//! Connect to the running tab-atelier instance's local HTTP API and
//! render a live table of every tab — name, agent indicator,
//! CPU/watts, uptime, cwd. Mirrors what the desktop's bottom bar
//! shows; intended for the headless deb where there is no bottom
//! bar at all.
//!
//! Discovery rules (in order):
//!   1. `TAB_ATELIER_API_URL` + `TAB_ATELIER_API_TOKEN` env vars —
//!      automatically exported into every PTY by tab-atelier, so
//!      `tabs` Just Works from inside any tab.
//!   2. Token file at `~/.local/state/tab-atelier/api.token` + the
//!      default `http://127.0.0.1:7890` URL. Lets the viewer run
//!      from an SSH shell where the env vars aren't set.
//!
//! Default mode is `--watch`: ratatui-driven live view, refreshing
//! every 500 ms. `--once` prints a single snapshot and exits — handy
//! for scripts and pipes.

use std::io::{self, Write as _};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug)]
struct Endpoint {
    url: String,
    token: String,
}

fn discover_endpoint() -> Result<Endpoint, String> {
    if let (Ok(url), Ok(token)) = (
        std::env::var("TAB_ATELIER_API_URL"),
        std::env::var("TAB_ATELIER_API_TOKEN"),
    ) {
        return Ok(Endpoint { url, token });
    }
    // System-service path first, then per-user, so a stale token left
    // by a direct root invocation can't trump the daemon's live one.
    let candidates = [
        std::path::PathBuf::from("/var/lib/tab-atelier/.local/state/tab-atelier/api.token"),
        crate::platform::state_base_dir().join("tab-atelier").join("api.token"),
    ];
    let mut tried = Vec::new();
    for path in &candidates {
        tried.push(path.display().to_string());
        if let Ok(t) = std::fs::read_to_string(path) {
            let token = t.trim().to_string();
            if !token.is_empty() {
                return Ok(Endpoint {
                    url: "http://127.0.0.1:7890".into(),
                    token,
                });
            }
        }
    }
    Err(format!("no api.token found (tried env vars + {})", tried.join(", ")))
}

#[derive(Debug, Default, Clone)]
struct TabRow {
    index: usize,
    name: String,
    active: bool,
    cwd: Option<String>,
    uptime_secs: f64,
    cpu_percent: f64,
    watts: Option<f64>,
    agent_state: Option<String>,
    agent_kind: Option<String>,
}

#[derive(Debug, Default, Clone)]
struct Snapshot {
    host_watts: Option<f64>,
    host_battery: Option<u8>,
    tabs: Vec<TabRow>,
}

fn fetch(agent: &ureq::Agent, ep: &Endpoint) -> Result<Snapshot, String> {
    let url = format!("{}/tabs", ep.url);
    let mut resp = agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", ep.token))
        .call()
        .map_err(|e| format!("GET /tabs: {e}"))?;
    let body: serde_json::Value = resp.body_mut().read_json().map_err(|e| format!("parse /tabs: {e}"))?;
    let mut snap = Snapshot {
        host_watts: body
            .get("host")
            .and_then(|h| h.get("watts"))
            .and_then(serde_json::Value::as_f64),
        host_battery: body
            .get("host")
            .and_then(|h| h.get("battery_percent"))
            .and_then(serde_json::Value::as_u64)
            .and_then(|v| u8::try_from(v).ok()),
        tabs: vec![],
    };
    if let Some(arr) = body.get("tabs").and_then(serde_json::Value::as_array) {
        for t in arr {
            snap.tabs.push(TabRow {
                index: t
                    .get("index")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
                    .try_into()
                    .unwrap_or(0),
                name: t
                    .get("name")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("?")
                    .to_string(),
                active: t.get("active").and_then(serde_json::Value::as_bool).unwrap_or(false),
                cwd: t.get("cwd").and_then(serde_json::Value::as_str).map(str::to_string),
                uptime_secs: t.get("uptime_secs").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
                cpu_percent: t.get("cpu_percent").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
                watts: t.get("watts").and_then(serde_json::Value::as_f64),
                agent_state: t
                    .get("agent_state")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
                agent_kind: t
                    .get("agent_kind")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string),
            });
        }
    }
    Ok(snap)
}

fn fmt_uptime(secs: f64) -> String {
    let s = secs as u64;
    let h = s / 3600;
    let m = (s % 3600) / 60;
    let sec = s % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{sec:02}s")
    } else {
        format!("{sec}s")
    }
}

fn fmt_watts(w: Option<f64>) -> String {
    w.map_or_else(String::new, |v| format!("{v:.2} W"))
}

fn fmt_cpu(p: f64) -> String {
    if p < 0.05 { String::new() } else { format!("{p:.1}%") }
}

/// Single-character LED that maps to the same colour scheme the
/// desktop tab bar uses (cyan thinking / amber waiting / red error
/// / steady grey when only `agent_kind` is set / blank when nothing
/// is attached).
fn led_cell(row: &TabRow) -> Cell<'static> {
    let (glyph, color) = match (row.agent_state.as_deref(), row.agent_kind.is_some()) {
        (Some("thinking"), _) => ("●", Color::Cyan),
        (Some("waiting"), _) => ("●", Color::Yellow),
        (Some("error"), _) => ("●", Color::Red),
        // Unknown / no transient state but a session is attached →
        // steady grey dot, same as the desktop's "session attached"
        // baseline.
        (Some(_) | None, true) => ("●", Color::DarkGray),
        (Some(_) | None, false) => (" ", Color::Reset),
    };
    Cell::from(glyph).style(Style::default().fg(color))
}

fn render_table(snap: &Snapshot) -> Table<'static> {
    let header = Row::new(["#", "●", "Name", "Agent", "CPU", "Watts", "Up", "CWD"])
        .style(Style::default().add_modifier(Modifier::BOLD));

    let rows: Vec<Row> = snap
        .tabs
        .iter()
        .map(|t| {
            let name = if t.active {
                Span::styled(
                    t.name.clone(),
                    Style::default().add_modifier(Modifier::BOLD).fg(Color::White),
                )
            } else {
                Span::raw(t.name.clone())
            };
            let kind = t.agent_kind.clone().unwrap_or_default();
            let state_label = match (t.agent_state.as_deref(), kind.as_str()) {
                (Some(s), k) if !k.is_empty() => format!("{k} · {s}"),
                (Some(s), _) => s.to_string(),
                (None, k) if !k.is_empty() => format!("{k} · idle"),
                _ => String::new(),
            };
            Row::new(vec![
                Cell::from(t.index.to_string()),
                led_cell(t),
                Cell::from(Line::from(name)),
                Cell::from(state_label),
                Cell::from(fmt_cpu(t.cpu_percent)),
                Cell::from(fmt_watts(t.watts)),
                Cell::from(fmt_uptime(t.uptime_secs)),
                Cell::from(t.cwd.clone().unwrap_or_default()),
            ])
        })
        .collect();

    Table::new(
        rows,
        [
            Constraint::Length(3),  // #
            Constraint::Length(2),  // led
            Constraint::Length(18), // name
            Constraint::Length(22), // agent
            Constraint::Length(6),  // cpu
            Constraint::Length(8),  // watts
            Constraint::Length(7),  // uptime
            Constraint::Min(20),    // cwd
        ],
    )
    .header(header)
    .block(Block::default().borders(Borders::ALL).title(" tab-atelier "))
}

fn render_footer(snap: &Snapshot, toast: Option<&str>) -> Line<'static> {
    let mut parts: Vec<Span<'static>> = Vec::new();
    parts.push(Span::raw(format!("{} tab(s)", snap.tabs.len())));
    if let Some(w) = snap.host_watts {
        parts.push(Span::raw(format!(" · host {w:.1} W")));
    }
    if let Some(b) = snap.host_battery {
        parts.push(Span::raw(format!(" · battery {b}%")));
    }
    parts.push(Span::raw(" · "));
    parts.push(Span::styled(
        "n: new tab · q/Ctrl-C: quit",
        Style::default().fg(Color::DarkGray),
    ));
    if let Some(t) = toast {
        parts.push(Span::raw(" · "));
        parts.push(Span::styled(t.to_string(), Style::default().fg(Color::Green)));
    }
    Line::from(parts)
}

fn centered_rect(area: Rect, percent_x: u16, height: u16) -> Rect {
    let w = area.width * percent_x / 100;
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    Rect::new(x, y, w, height.min(area.height))
}

fn render_new_tab_modal<'a>(input: &'a str, error: Option<&'a str>) -> impl ratatui::widgets::Widget + 'a {
    let title = " New tab — enter cwd (Enter to confirm, Esc to cancel) ";
    let prompt = Line::from(vec![Span::raw("cwd: "), Span::raw(input)]);
    let mut lines = vec![prompt];
    if let Some(e) = error {
        lines.push(Line::from(Span::styled(
            format!("⚠ {e}"),
            Style::default().fg(Color::Red),
        )));
    } else {
        lines.push(Line::from(Span::styled(
            "(blank inherits from active tab)",
            Style::default().fg(Color::DarkGray),
        )));
    }
    Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title))
}

fn post_new_tab(agent: &ureq::Agent, ep: &Endpoint, cwd: Option<&str>) -> Result<(), String> {
    let url = format!("{}/tabs", ep.url);
    let body = cwd.map_or_else(String::new, |c| serde_json::json!({ "cwd": c }).to_string());
    let req = agent
        .post(&url)
        .header("Authorization", &format!("Bearer {}", ep.token))
        .header("Content-Type", "application/json");
    if body.is_empty() {
        req.send_empty().map_err(|e| format!("POST /tabs: {e}"))?;
    } else {
        req.send(&body).map_err(|e| format!("POST /tabs: {e}"))?;
    }
    Ok(())
}

fn print_once(snap: &Snapshot) {
    println!(
        "{:<3} {:<3} {:<20} {:<26} {:>6} {:>8} {:>7} CWD",
        "#", "S", "Name", "Agent", "CPU", "Watts", "Up"
    );
    for t in &snap.tabs {
        let led = match (t.agent_state.as_deref(), t.agent_kind.is_some()) {
            (Some("thinking"), _) => "T",
            (Some("waiting"), _) => "W",
            (Some("error"), _) => "E",
            (None, true) => "·",
            _ => " ",
        };
        let kind = t.agent_kind.clone().unwrap_or_default();
        let state_label = match (t.agent_state.as_deref(), kind.as_str()) {
            (Some(s), k) if !k.is_empty() => format!("{k} · {s}"),
            (Some(s), _) => s.to_string(),
            (None, k) if !k.is_empty() => format!("{k} · idle"),
            _ => String::new(),
        };
        let active = if t.active { "*" } else { " " };
        println!(
            "{:>2}{active} {led:<3} {:<20} {:<26} {:>6} {:>8} {:>7} {}",
            t.index,
            truncate(&t.name, 20),
            truncate(&state_label, 26),
            fmt_cpu(t.cpu_percent),
            fmt_watts(t.watts),
            fmt_uptime(t.uptime_secs),
            t.cwd.as_deref().unwrap_or("")
        );
    }
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(n.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

enum Mode {
    Table,
    NewTab { input: String, error: Option<String> },
}

#[allow(clippy::too_many_lines)]
fn run_watch(ep: &Endpoint) -> Result<(), String> {
    enable_raw_mode().map_err(|e| format!("raw mode: {e}"))?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen).map_err(|e| format!("alt screen: {e}"))?;
    let backend = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend).map_err(|e| format!("ratatui init: {e}"))?;

    let agent = ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(2)))
        .build()
        .new_agent();

    let mut snapshot = Snapshot::default();
    let mut last_error: Option<String> = None;
    let mut next_poll = std::time::Instant::now();
    let mut mode = Mode::Table;
    let mut toast: Option<(String, std::time::Instant)> = None;

    let cleanup = |term: &mut Terminal<CrosstermBackend<io::Stdout>>| {
        let _ = disable_raw_mode();
        let _ = execute!(term.backend_mut(), LeaveAlternateScreen);
        let _ = term.show_cursor();
    };

    loop {
        if std::time::Instant::now() >= next_poll && matches!(mode, Mode::Table) {
            match fetch(&agent, ep) {
                Ok(s) => {
                    snapshot = s;
                    last_error = None;
                }
                Err(e) => {
                    last_error = Some(e);
                }
            }
            next_poll = std::time::Instant::now() + POLL_INTERVAL;
        }
        // Expire toast after 3 s.
        if let Some((_, when)) = &toast
            && when.elapsed() > Duration::from_secs(3)
        {
            toast = None;
        }

        let snap_for_draw = snapshot.clone();
        let err_for_draw = last_error.clone();
        let toast_for_draw = toast.as_ref().map(|(m, _)| m.clone());
        let mode_ref = &mode;
        term.draw(|f| {
            let area = f.area();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(area);
            f.render_widget(render_table(&snap_for_draw), chunks[0]);
            let footer = err_for_draw.as_ref().map_or_else(
                || render_footer(&snap_for_draw, toast_for_draw.as_deref()),
                |e| Line::from(Span::styled(format!("⚠ {e}"), Style::default().fg(Color::Red))),
            );
            f.render_widget(footer, chunks[1]);

            if let Mode::NewTab { input, error } = mode_ref {
                let r = centered_rect(area, 60, 5);
                f.render_widget(Clear, r);
                f.render_widget(render_new_tab_modal(input, error.as_deref()), r);
            }
        })
        .map_err(|e| format!("draw: {e}"))?;

        // Block briefly waiting for a keystroke; the timeout doubles
        // as our 'next frame' tick so a `q`/Ctrl-C wakes us within
        // ~50 ms regardless of poll cadence.
        let wait_for = next_poll
            .saturating_duration_since(std::time::Instant::now())
            .min(Duration::from_millis(50));
        if event::poll(wait_for).map_err(|e| format!("event poll: {e}"))?
            && let Event::Key(k) = event::read().map_err(|e| format!("event read: {e}"))?
            && k.kind == KeyEventKind::Press
        {
            match &mut mode {
                Mode::Table => {
                    let quit = matches!(k.code, KeyCode::Char('q') | KeyCode::Esc)
                        || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL));
                    if quit {
                        cleanup(&mut term);
                        return Ok(());
                    }
                    if matches!(k.code, KeyCode::Char('n')) {
                        // Pre-fill the input with the currently
                        // active tab's cwd so "open another shell
                        // here" is the one-keystroke path.
                        let default = snapshot
                            .tabs
                            .iter()
                            .find(|t| t.active)
                            .and_then(|t| t.cwd.clone())
                            .unwrap_or_default();
                        mode = Mode::NewTab {
                            input: default,
                            error: None,
                        };
                    }
                }
                Mode::NewTab { input, error } => match k.code {
                    KeyCode::Esc => {
                        mode = Mode::Table;
                    }
                    KeyCode::Enter => {
                        let trimmed = input.trim();
                        let cwd = if trimmed.is_empty() { None } else { Some(trimmed) };
                        // Validate locally so we can show the error in the
                        // modal instead of waiting for the server to ignore
                        // a bad path silently (the API falls back to the
                        // inherit cwd when the hint isn't a directory).
                        if let Some(c) = cwd
                            && !std::path::Path::new(c).is_dir()
                        {
                            *error = Some(format!("not a directory: {c}"));
                        } else {
                            match post_new_tab(&agent, ep, cwd) {
                                Ok(()) => {
                                    toast = Some((
                                        format!(
                                            "opened new tab{}",
                                            cwd.map_or_else(String::new, |c| format!(" in {c}"))
                                        ),
                                        std::time::Instant::now(),
                                    ));
                                    mode = Mode::Table;
                                    // Refresh sooner than the next regular tick.
                                    next_poll = std::time::Instant::now();
                                }
                                Err(e) => {
                                    *error = Some(e);
                                }
                            }
                        }
                    }
                    KeyCode::Backspace => {
                        input.pop();
                        *error = None;
                    }
                    KeyCode::Char(c) => {
                        if k.modifiers.contains(KeyModifiers::CONTROL) && c == 'u' {
                            input.clear();
                        } else {
                            input.push(c);
                        }
                        *error = None;
                    }
                    _ => {}
                },
            }
        }

        if crate::SHUTDOWN_REQUESTED.load(std::sync::atomic::Ordering::SeqCst) {
            cleanup(&mut term);
            return Ok(());
        }
    }
}

#[must_use]
pub fn run(args: &[String]) -> i32 {
    let once = args.iter().any(|a| a == "--once");
    let ep = match discover_endpoint() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("tab-atelier tabs: {e}");
            return 1;
        }
    };

    if once {
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .new_agent();
        match fetch(&agent, &ep) {
            Ok(s) => {
                print_once(&s);
                let _ = io::stdout().flush();
                0
            }
            Err(e) => {
                eprintln!("tab-atelier tabs: {e}");
                1
            }
        }
    } else {
        match run_watch(&ep) {
            Ok(()) => 0,
            Err(e) => {
                eprintln!("tab-atelier tabs: {e}");
                1
            }
        }
    }
}
