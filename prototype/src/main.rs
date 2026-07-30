//! Standalone ratatui prototype of the tmux-agent-wrangler sidebar.
//!
//! No daemon, no tmux, no state files: the window/pane tree and agent sessions
//! are hardcoded mock data. The point is to confirm the look and interaction
//! (tree layout, active markers, ellipsis truncation, agent colors, the
//! spinner/dot/OSC-% indicators, keyboard nav, the selection bar, and
//! mouse/Enter activation) before the real client/daemon are built.
//!
//! Keys: j/k or Up/Down to move, Enter or left-click to "activate" (shown in
//! the footer, since there is no tmux to focus), q to quit.

use std::io::{self, Write};
use std::time::{Duration, Instant};

use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEventKind, MouseButton,
    MouseEventKind,
};
use ratatui::crossterm::event::{DisableFocusChange, EnableFocusChange};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

// Frames of the "busy" spinner. Single-width braille so the pinned indicator
// stays one column wide across every frame.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
// Spinner advances ~16fps on the wall clock, independent of input.
const ANIM_INTERVAL: f64 = 0.0625;

fn spinner_frame(frame: usize) -> char {
    SPINNER[frame % SPINNER.len()]
}

// ---------------------------------------------------------------------------
// Color: the muted agent palette plus the chalk rgb->ansi256 mapping, so an
// agent row lands on the same 256-color index the agent CLI itself would use.
// ---------------------------------------------------------------------------

fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u8 {
    let (r, g, b) = (r as i32, g as i32, b as i32);
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (232 + ((r - 8) as f64 / 247.0 * 24.0).round() as i32) as u8;
    }
    let idx = 16
        + 36 * (r as f64 / 255.0 * 5.0).round() as i32
        + 6 * (g as f64 / 255.0 * 5.0).round() as i32
        + (b as f64 / 255.0 * 5.0).round() as i32;
    idx as u8
}

/// The muted-palette color for a named color, as an indexed terminal color, or
/// `None` for an unknown/absent name.
fn palette_color(name: &str) -> Option<Color> {
    let rgb = match name {
        "red" => (220, 38, 38),
        "blue" => (106, 155, 204),
        "green" => (22, 163, 74),
        "yellow" => (202, 138, 4),
        "purple" => (130, 125, 189),
        "orange" => (217, 119, 87),
        "pink" => (196, 102, 134),
        "cyan" => (8, 145, 178),
        _ => return None,
    };
    Some(Color::Indexed(rgb_to_ansi256(rgb.0, rgb.1, rgb.2)))
}

/// An agent row's color: its assigned /color, or a base cyan default when the
/// session has none.
fn agent_color(name: Option<&str>) -> Color {
    name.and_then(palette_color).unwrap_or(Color::Cyan)
}

/// A window/pane row's own color (in the real thing, from the tmux pane border
/// color): the named color, or `None` to leave it uncolored.
fn border_color(name: Option<&str>) -> Option<Color> {
    name.and_then(palette_color)
}

// ---------------------------------------------------------------------------
// Indicators: the single glyph/percentage pinned to a row's right edge.
// ---------------------------------------------------------------------------

/// The color/state of a progress indicator. `Plain` inherits the row's own
/// color, a generic in-progress signal (e.g. an agent hook reporting
/// "working"); the others are OSC 9;4 progress states.
#[derive(Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum ProgressState {
    Plain,
    Normal,
    Paused,
    Error,
}

impl ProgressState {
    /// The indicator's color, or `None` to inherit the row's own color.
    fn color(self) -> Option<Color> {
        match self {
            ProgressState::Plain => None,
            ProgressState::Normal => Some(Color::Green),
            ProgressState::Paused => Some(Color::Yellow),
            ProgressState::Error => Some(Color::Red),
        }
    }
}

/// The single glyph/percentage pinned to a row's right edge.
///
/// `Progress` covers any in-progress signal (an agent hook's "working" turn
/// state or an app's OSC 9;4 report), drawn as a spinner when indeterminate
/// (`pct` is None) or a percentage when determinate (`pct` is Some).
/// `Attention` is the separate "needs input" dot.
#[derive(Clone, Copy, PartialEq)]
enum Indicator {
    None,
    Attention,
    Progress {
        pct: Option<u8>,
        state: ProgressState,
    },
}

impl Indicator {
    /// (text, optional color) for animation frame `frame`; empty text means no
    /// indicator. A color is set only when the progress state carries one,
    /// otherwise the glyph inherits the row's own color.
    fn resolve(self, frame: usize) -> (String, Option<Color>) {
        match self {
            Indicator::None => (String::new(), None),
            Indicator::Attention => ("●".to_string(), None),
            Indicator::Progress { pct: None, state } => {
                (spinner_frame(frame).to_string(), state.color())
            }
            Indicator::Progress {
                pct: Some(p),
                state,
            } => (format!("{p}%"), state.color()),
        }
    }

    /// Whether this indicator animates (an indeterminate progress spinner).
    fn indeterminate(self) -> bool {
        matches!(self, Indicator::Progress { pct: None, .. })
    }
}

// ---------------------------------------------------------------------------
// Row model: one flattened display line. `key` marks a selectable row.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    Header,
    Blank,
    // Windows and panes can carry their own color; `None` uses the default
    // styling (active window greened, pane dimmed).
    Window { active: bool, color: Option<Color> },
    Pane { color: Option<Color> },
    Agent { color: Color, emphatic: bool },
}

struct Row {
    text: String,
    kind: Kind,
    key: Option<String>,
    indicator: Indicator,
}

impl Row {
    fn base_style(&self) -> Style {
        match self.kind {
            Kind::Header => Style::new().bold().underlined(),
            Kind::Blank => Style::new().dim(),
            Kind::Window { active, color } => {
                let s = Style::new().bold();
                // An explicit window color wins; otherwise the active window is
                // greened as the default cue.
                match color {
                    Some(c) => s.fg(c),
                    None if active => s.fg(Color::Green),
                    None => s,
                }
            }
            // A pane with its own color shows in it; an uncolored pane is dimmed.
            Kind::Pane { color } => match color {
                Some(c) => Style::new().fg(c),
                None => Style::new().dim(),
            },
            Kind::Agent { color, emphatic } => {
                let s = Style::new().fg(color);
                if emphatic {
                    s.bold()
                } else {
                    s
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Mock data.
// ---------------------------------------------------------------------------

struct Pane {
    index: &'static str,
    title: &'static str,
    active: bool,
    color: Option<&'static str>,
    indicator: Indicator,
}

struct Window {
    id: &'static str,
    index: &'static str,
    name: &'static str,
    active: bool,
    color: Option<&'static str>,
    panes: Vec<Pane>,
}

/// One agent session placed under one window. A session displayed in several
/// panes yields several of these, one per pane, so it appears under each
/// window it is visible in.
struct AgentRow {
    session_id: &'static str,
    pane: &'static str,
    window_id: &'static str,
    label: String,
    color: Option<&'static str>,
    indicator: Indicator,
}

struct Agent {
    name: &'static str,
    rows: Vec<AgentRow>,
}

fn mock_windows() -> Vec<Window> {
    vec![
        Window {
            id: "@0",
            index: "0",
            name: "editor",
            active: true,
            color: None,
            panes: vec![
                Pane {
                    index: "0",
                    title: "vim src/daemon/mod.rs",
                    active: true,
                    color: Some("blue"),
                    // The copilot build in this pane reports OSC 9;4 progress.
                    indicator: Indicator::Progress {
                        pct: Some(88),
                        state: ProgressState::Normal,
                    },
                },
                Pane {
                    index: "1",
                    title: "bash",
                    active: false,
                    color: None,
                    indicator: Indicator::None,
                },
            ],
        },
        Window {
            id: "@1",
            index: "1",
            name: "server",
            active: false,
            // A window with its own color (demonstrating per-window color).
            color: Some("purple"),
            panes: vec![
                Pane {
                    index: "0",
                    title: "cargo run --release --features telemetry",
                    active: true,
                    color: Some("red"),
                    indicator: Indicator::None,
                },
                Pane {
                    index: "1",
                    title: "tail -f app.log",
                    active: false,
                    color: None,
                    // The @reviewer teammate needs attention in this pane.
                    indicator: Indicator::Attention,
                },
            ],
        },
        Window {
            id: "@2",
            index: "2",
            name: "notes",
            active: false,
            color: None,
            panes: vec![Pane {
                index: "0",
                title: "claude",
                active: true,
                color: None,
                // The "Rewrite in Rust" session is working (indeterminate).
                indicator: Indicator::Progress {
                    pct: None,
                    state: ProgressState::Plain,
                },
            }],
        },
    ]
}

fn mock_agents() -> Vec<Agent> {
    vec![
        Agent {
            name: "claude",
            rows: vec![
                AgentRow {
                    session_id: "rewrite",
                    pane: "%20",
                    window_id: "@2",
                    label: "Rewrite the sidebar in Rust and ratatui".to_string(),
                    color: Some("green"),
                    indicator: Indicator::Progress {
                        pct: None,
                        state: ProgressState::Plain,
                    },
                },
                AgentRow {
                    session_id: "reviewer",
                    pane: "%11",
                    window_id: "@1",
                    label: "@reviewer - width sync state machine".to_string(),
                    color: Some("purple"),
                    indicator: Indicator::Attention,
                },
                // One session shown in two panes gives two placements, filed
                // under two different windows. Exercises the multi-window case.
                AgentRow {
                    session_id: "dedup",
                    pane: "%01",
                    window_id: "@0",
                    label: "Investigate notify dedup".to_string(),
                    color: None,
                    indicator: Indicator::None,
                },
                AgentRow {
                    session_id: "dedup",
                    pane: "%10",
                    window_id: "@1",
                    label: "Investigate notify dedup".to_string(),
                    color: None,
                    indicator: Indicator::None,
                },
            ],
        },
        Agent {
            name: "copilot",
            rows: vec![AgentRow {
                session_id: "build",
                pane: "%00",
                window_id: "@0",
                label: "build-scripts".to_string(),
                color: None,
                indicator: Indicator::Progress {
                    pct: Some(88),
                    state: ProgressState::Normal,
                },
            }],
        },
    ]
}

// ---------------------------------------------------------------------------
// build_rows: flatten windows + agents into the display rows (a header and a
// blank spacer per section, a tree of windows with their panes/sessions).
// ---------------------------------------------------------------------------

fn build_rows(windows: &[Window], agents: &[Agent]) -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(Row {
        text: " WINDOWS".to_string(),
        kind: Kind::Header,
        key: None,
        indicator: Indicator::None,
    });
    rows.push(blank());

    for w in windows {
        let marker = if w.active { '*' } else { ' ' };
        rows.push(Row {
            text: format!("{marker} {}: {}", w.index, w.name),
            kind: Kind::Window {
                active: w.active,
                color: border_color(w.color),
            },
            key: Some(format!("w:{}", w.id)),
            indicator: Indicator::None,
        });
        let last = w.panes.len().saturating_sub(1);
        for (i, p) in w.panes.iter().enumerate() {
            let branch = if i == last { "└─" } else { "├─" };
            let active = if p.active { '*' } else { ' ' };
            rows.push(Row {
                text: format!("   {branch}{active}{}: {}", p.index, p.title),
                kind: Kind::Pane {
                    color: border_color(p.color),
                },
                key: Some(format!("p:{}:{}", w.id, p.index)),
                indicator: p.indicator,
            });
        }
    }

    for agent in agents {
        if agent.rows.is_empty() {
            continue;
        }
        rows.push(blank());
        rows.push(Row {
            text: format!(" {}", agent.name.to_uppercase()),
            kind: Kind::Header,
            key: None,
            indicator: Indicator::None,
        });
        rows.push(blank());

        for w in windows {
            let group: Vec<&AgentRow> = agent.rows.iter().filter(|s| s.window_id == w.id).collect();
            if group.is_empty() {
                continue;
            }
            let marker = if w.active { '*' } else { ' ' };
            rows.push(Row {
                text: format!("{marker} {}: {}", w.index, w.name),
                kind: Kind::Window {
                    active: w.active,
                    color: border_color(w.color),
                },
                key: Some(format!("wa:{}:{}", agent.name, w.id)),
                indicator: Indicator::None,
            });
            let last = group.len().saturating_sub(1);
            for (i, s) in group.iter().enumerate() {
                let branch = if i == last { "└─" } else { "├─" };
                // An agent row is emphasized (bold) while it needs attention or
                // is actively working (an indeterminate spinner).
                let emphatic =
                    matches!(s.indicator, Indicator::Attention) || s.indicator.indeterminate();
                rows.push(Row {
                    text: format!("   {branch} {}", s.label),
                    kind: Kind::Agent {
                        color: agent_color(s.color),
                        emphatic,
                    },
                    key: Some(format!("a:{}:{}", s.session_id, s.pane)),
                    indicator: s.indicator,
                });
            }
        }
    }

    rows
}

fn blank() -> Row {
    Row {
        text: String::new(),
        kind: Kind::Blank,
        key: None,
        indicator: Indicator::None,
    }
}

// ---------------------------------------------------------------------------
// Rendering.
// ---------------------------------------------------------------------------

/// Fit `text` to exactly `field` columns: ellipsize on overflow, else left-pad
/// so the row fills its width and the reverse-video selection bar stays solid.
/// Counts characters, not display cells.
fn fit(text: &str, field: usize) -> String {
    if field == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count > field {
        if field == 1 {
            return "…".to_string();
        }
        let mut s: String = text.chars().take(field - 1).collect();
        s.push('…');
        s
    } else {
        let mut s = text.to_string();
        s.extend(std::iter::repeat(' ').take(field - count));
        s
    }
}

fn render_line(row: &Row, width: usize, frame: usize, selected: bool) -> Line<'static> {
    let base = row.base_style();
    // Reserve the last column so the indicator never touches the pane edge.
    let field = width.saturating_sub(1);
    let (ind_text, ind_color) = row.indicator.resolve(frame);
    let ind_len = ind_text.chars().count();

    if !ind_text.is_empty() && field >= ind_len + 2 {
        let reserve = ind_len + 1;
        let left = fit(&row.text, field - reserve);
        let mut left_style = base;
        // The indicator carries its own OSC state color, or inherits the row's,
        // and the reverse-video bar runs continuously across both spans.
        let mut ind_style = match ind_color {
            Some(c) => Style::new().fg(c),
            None => base,
        };
        if selected {
            left_style = left_style.add_modifier(Modifier::REVERSED);
            ind_style = ind_style.add_modifier(Modifier::REVERSED);
        }
        Line::from(vec![
            Span::styled(left, left_style),
            Span::styled(format!(" {ind_text}"), ind_style),
        ])
    } else {
        let mut style = base;
        if selected {
            style = style.add_modifier(Modifier::REVERSED);
        }
        Line::from(Span::styled(fit(&row.text, field), style))
    }
}

struct UiState {
    offset: usize,
    selected: String,
    has_focus: bool,
    action: String,
}

fn render(frame_ui: &mut ratatui::Frame, rows: &[Row], state: &mut UiState, frame: usize) {
    let area = frame_ui.area();
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    // Reserve the last line for the prototype footer (help / last action).
    let body_h = height.saturating_sub(1);

    // Scroll so the selected row stays visible.
    let sel_row = rows
        .iter()
        .position(|r| r.key.as_deref() == Some(state.selected.as_str()))
        .unwrap_or(0);
    let mut off = state.offset;
    if sel_row < off {
        off = sel_row;
    } else if body_h > 0 && sel_row >= off + body_h {
        off = sel_row - body_h + 1;
    }
    off = off.min(rows.len().saturating_sub(body_h));
    state.offset = off;

    let mut lines = Vec::new();
    for row in rows.iter().skip(off).take(body_h) {
        let selected = state.has_focus && row.key.as_deref() == Some(state.selected.as_str());
        lines.push(render_line(row, width, frame, selected));
    }

    let body = Rect::new(0, 0, area.width, body_h as u16);
    frame_ui.render_widget(Paragraph::new(Text::from(lines)), body);

    let footer_text = if state.action.is_empty() {
        " j/k move · enter/click select · q quit".to_string()
    } else {
        format!(" → {}", state.action)
    };
    let footer = Rect::new(0, body_h as u16, area.width, 1);
    frame_ui.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(&footer_text, width.saturating_sub(1)),
            Style::new()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::REVERSED),
        ))),
        footer,
    );
}

// ---------------------------------------------------------------------------
// Input + describe (what activation "would" do, since there is no tmux).
// ---------------------------------------------------------------------------

fn describe(row: &Row) -> String {
    match row.kind {
        Kind::Window { .. } => format!("focus {}", row.text.trim()),
        Kind::Pane { .. } => format!("focus pane {}", row.text.trim()),
        Kind::Agent { .. } => format!("focus {}", row.text.trim()),
        _ => String::new(),
    }
}

fn main() -> io::Result<()> {
    let windows = mock_windows();
    let agents = mock_agents();
    let rows = build_rows(&windows, &agents);

    // Selectable keys in display order, for nav.
    let keys: Vec<String> = rows.iter().filter_map(|r| r.key.clone()).collect();
    // Default selection: the active window's row.
    let initial = rows
        .iter()
        .find(|r| matches!(r.kind, Kind::Window { active: true, .. }))
        .and_then(|r| r.key.clone())
        .or_else(|| keys.first().cloned())
        .unwrap_or_default();

    let mut state = UiState {
        offset: 0,
        selected: initial,
        has_focus: true,
        action: String::new(),
    };

    // `--snapshot [WxH]` renders one frame to an off-screen buffer and prints it
    // as plain text, then exits. No terminal is taken over: a static preview for
    // review, and a deterministic render check the later phases can assert on.
    let args: Vec<String> = std::env::args().collect();
    if let Some(pos) = args.iter().position(|a| a == "--snapshot") {
        let (cols, rows_h) = args
            .get(pos + 1)
            .and_then(|s| s.split_once('x'))
            .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
            .unwrap_or((40, 24));
        print_snapshot(cols, rows_h, &rows, &mut state);
        return Ok(());
    }

    // Terminal setup: alternate screen + raw mode + mouse + focus reporting.
    let mut stdout = io::stdout();
    enable_raw_mode()?;
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange,
        Hide
    )?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;

    let result = run(&mut terminal, &rows, &keys, &mut state);

    disable_raw_mode()?;
    execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange,
        Show
    )?;
    io::stdout().flush()?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    rows: &[Row],
    keys: &[String],
    state: &mut UiState,
) -> io::Result<()> {
    let start = Instant::now();
    let tick = Duration::from_millis(16);

    loop {
        let frame = (start.elapsed().as_secs_f64() / ANIM_INTERVAL) as usize;
        terminal.draw(|f| render(f, rows, state, frame))?;

        if !event::poll(tick)? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => move_selection(keys, state, -1),
                KeyCode::Down | KeyCode::Char('j') => move_selection(keys, state, 1),
                KeyCode::Enter => activate_selected(rows, state),
                _ => {}
            },
            Event::Mouse(m) => {
                if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                    let idx = m.row as usize + state.offset;
                    if let Some(row) = rows.get(idx) {
                        if let Some(key) = &row.key {
                            state.selected = key.clone();
                            state.action = describe(row);
                        }
                    }
                }
            }
            Event::FocusGained => state.has_focus = true,
            Event::FocusLost => state.has_focus = false,
            _ => {}
        }
    }
}

/// Render one frame at a fixed size into an off-screen buffer and print the
/// glyphs as plain text (no color/reverse), a static preview of the layout.
fn print_snapshot(cols: u16, rows_h: u16, rows: &[Row], state: &mut UiState) {
    let backend = ratatui::backend::TestBackend::new(cols, rows_h);
    let mut terminal = Terminal::new(backend).expect("test backend");
    terminal
        .draw(|f| render(f, rows, state, 0))
        .expect("draw snapshot");
    let buffer = terminal.backend().buffer();
    for y in 0..rows_h {
        let mut line = String::new();
        for x in 0..cols {
            line.push_str(buffer[(x, y)].symbol());
        }
        println!("{}", line.trim_end());
    }
}

fn move_selection(keys: &[String], state: &mut UiState, delta: isize) {
    if keys.is_empty() {
        return;
    }
    let pos = keys.iter().position(|k| k == &state.selected).unwrap_or(0) as isize;
    let next = (pos + delta).clamp(0, keys.len() as isize - 1) as usize;
    state.selected = keys[next].clone();
    state.action.clear();
}

fn activate_selected(rows: &[Row], state: &mut UiState) {
    if let Some(row) = rows
        .iter()
        .find(|r| r.key.as_deref() == Some(state.selected.as_str()))
    {
        state.action = describe(row);
    }
}
