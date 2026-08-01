//! The sidebar client: a thin ratatui renderer over the daemon socket.
//!
//! It resolves its tmux server/window/pane once at startup, connects to the
//! daemon, and sends a `Hello`. Thereafter it paints whatever [`RowModel`] the
//! daemon pushes, animates the spinner locally on the wall clock, and forwards
//! interaction (an absolute selection id on nav, activate, and terminal focus)
//! back. Which rows exist, what they are named and which is selected are the
//! daemon's decisions; the client flattens the tree, draws every glyph around
//! those names, and resolves the spinner frame at paint time. A row's id is an
//! opaque token here: it is echoed back untouched, never built or inspected.

pub mod render;

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::cursor::{Hide, Show};
use ratatui::crossterm::event::{
    self, DisableFocusChange, DisableMouseCapture, EnableFocusChange, EnableMouseCapture, Event,
    KeyCode, KeyEventKind, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, size as terminal_size, EnterAlternateScreen,
    LeaveAlternateScreen,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use crate::client::render::{base_style, fit_segments, row_segments};
use crate::color::{agent_color_table, claude_dir, read_theme};
use crate::daemon::rows::StateColor;
use crate::model::{NamedColor, PaneId, Row, RowKey, RowModel, ServerKey, WindowId};
use crate::paths::daemon_socket;
use crate::proto::{read_message, write_message, ClientMsg, CtlMsg, InputEvent, ServerMsg};

/// Seconds between spinner frame advances (~16 fps), on the wall clock.
const ANIM_INTERVAL: f64 = 0.0625;
/// The blocking cadence of the input poll, also the redraw ceiling.
const TICK: Duration = Duration::from_millis(16);
/// Connect attempts made after spawning the daemon before giving up.
const CONNECT_RETRIES: u32 = 40;
/// The pause between connect attempts while a freshly spawned daemon comes up.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Name -> ansi color index, resolved once from the user's theme.
pub struct Colors {
    table: IndexMap<&'static str, i16>,
}

impl Colors {
    fn new() -> Self {
        let theme = read_theme(&claude_dir());
        // A modern terminal is assumed 256-color; the ANSI-base fallback is a
        // later refinement.
        let table = agent_color_table(&theme, 256).indices;
        Colors { table }
    }

    fn named(&self, color: NamedColor) -> Color {
        let idx = self.table.get(color.as_str()).copied().unwrap_or(6);
        Color::Indexed(idx as u8)
    }

    /// A row's own color, or `None` to leave it default-styled.
    fn optional(&self, color: Option<NamedColor>) -> Option<Color> {
        color.map(|c| self.named(c))
    }
}

/// The ratatui color an OSC/hook indicator state paints in.
fn state_color(state: StateColor) -> Color {
    match state {
        StateColor::Green => Color::Green,
        StateColor::Yellow => Color::Yellow,
        StateColor::Red => Color::Red,
    }
}

/// The reverse-video bar marking the selected row, applied to every span of it
/// so the bar spans the whole width rather than the colored pieces alone.
///
/// The bar drops the color it covers: under reverse video a foreground color
/// becomes the background, so a colored icon or indicator would paint a block of
/// color across the bar. Reverse video already says which row is selected, and
/// the weight and glyphs survive it.
fn selection_bar(style: Style, selected: bool) -> Style {
    if selected {
        Style { fg: None, ..style }.add_modifier(Modifier::REVERSED)
    } else {
        style
    }
}

/// Render one row to a styled line: its segments fit to the width with the
/// indicator pinned to the right edge, and the reverse-video bar applied when
/// selected.
fn render_line(
    row: &Row,
    colors: &Colors,
    width: usize,
    frame: usize,
    selected: bool,
) -> Line<'static> {
    let indicator = row.indicator;
    let segments = row_segments(&row.content, colors);
    // Reserve the last column so the indicator never touches the pane edge.
    let field = width.saturating_sub(1);
    let (ind_text, ind_color) = indicator.resolve(frame);
    let ind_len = ind_text.chars().count();

    let (left, tail) = if !ind_text.is_empty() && field >= ind_len + 2 {
        let reserve = ind_len + 1;
        // No state color of its own: the indicator inherits the row's style.
        let ind_style = match ind_color {
            Some(c) => Style::new().fg(state_color(c)),
            None => base_style(&row.content, colors),
        };
        (
            fit_segments(segments, field - reserve),
            Some(Span::styled(
                format!(" {ind_text}"),
                selection_bar(ind_style, selected),
            )),
        )
    } else {
        (fit_segments(segments, field), None)
    };

    let spans = left
        .into_iter()
        .map(|s| Span::styled(s.text, selection_bar(s.style, selected)))
        .chain(tail);
    Line::from(spans.collect::<Vec<_>>())
}

/// A pushed model with its tree already flattened, so the paint loop walks the
/// rows rather than the tree on every frame.
struct View {
    model: RowModel,
    rows: Vec<Row>,
}

impl View {
    fn new(model: RowModel) -> Self {
        let rows = model.tree.flatten();
        View { model, rows }
    }
}

/// Paint the view, scrolling so the selected row stays visible. `offset` is the
/// scroll position, carried between frames.
fn render(
    frame_ui: &mut ratatui::Frame,
    view: &View,
    colors: &Colors,
    frame: usize,
    offset: &mut usize,
) {
    let area = frame_ui.area();
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }

    let selection = view.model.selection.as_ref();
    let sel_row = selection
        .and_then(|k| view.rows.iter().position(|r| r.id.as_ref() == Some(k)))
        .unwrap_or(0);
    let mut off = *offset;
    if sel_row < off {
        off = sel_row;
    } else if height > 0 && sel_row >= off + height {
        off = sel_row - height + 1;
    }
    off = off.min(view.rows.len().saturating_sub(height));
    *offset = off;

    let mut lines = Vec::new();
    for row in view.rows.iter().skip(off).take(height) {
        let selected = view.model.has_focus && row.id.is_some() && row.id.as_ref() == selection;
        lines.push(render_line(row, colors, width, frame, selected));
    }
    frame_ui.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The selectable rows' ids in display order.
fn selectable_ids(view: &View) -> Vec<RowKey> {
    view.rows.iter().filter_map(|r| r.id.clone()).collect()
}

/// The id `delta` steps from the current selection among the selectable rows,
/// clamped to the ends. `None` when there is nothing selectable.
fn neighbor_id(view: &View, delta: isize) -> Option<RowKey> {
    let ids = selectable_ids(view);
    if ids.is_empty() {
        return None;
    }
    let pos = view
        .model
        .selection
        .as_ref()
        .and_then(|k| ids.iter().position(|kk| kk == k))
        .unwrap_or(0) as isize;
    let next = (pos + delta).clamp(0, ids.len() as isize - 1) as usize;
    Some(ids[next].clone())
}

/// The id of the selectable row at display line `line` (accounting for scroll).
fn id_at_line(view: &View, offset: usize, line: usize) -> Option<RowKey> {
    view.rows.get(offset + line).and_then(|r| r.id.clone())
}

/// A message from the reader thread: a decoded server push, or the connection
/// closing.
enum Incoming {
    Msg(ServerMsg),
    Closed,
}

/// A live connection to the daemon: the write half plus the reader thread's
/// channel of incoming pushes.
struct Connection {
    writer: UnixStream,
    rx: Receiver<Incoming>,
}

/// The tmux context a sidebar client reports: its server socket, window, and
/// pane, from the environment (and one tmux query for the window).
struct Context {
    server: ServerKey,
    window: WindowId,
    pane: PaneId,
}

/// Resolve the client's tmux context, or `None` when it is not running inside a
/// tmux pane (nothing to be a sidebar for).
fn resolve_context() -> Option<Context> {
    let tmux = std::env::var("TMUX").ok()?;
    let socket = tmux.split(',').next().unwrap_or("");
    if socket.is_empty() {
        return None;
    }
    let pane = std::env::var("TMUX_PANE").ok().filter(|p| !p.is_empty())?;
    let window = crate::tmux::run_tmux(
        socket,
        &["display-message", "-p", "-t", &pane, "#{window_id}"],
    )
    .trim()
    .to_string();
    if window.is_empty() {
        return None;
    }
    Some(Context {
        server: ServerKey(socket.to_string()),
        window: WindowId(window),
        pane: PaneId(pane),
    })
}

/// Whether a lower-numbered sidebar already occupies this window (a spawn race),
/// so this client should yield rather than run a duplicate sidebar.
fn lower_sidebar_present(ctx: &Context) -> bool {
    let my_num = ctx.pane.numeric().unwrap_or(u64::MAX);
    let listing = crate::tmux::run_tmux(
        &ctx.server.0,
        &[
            "list-panes",
            "-t",
            &ctx.window.0,
            "-F",
            "#{pane_id} #{@wrangler_sidebar}",
        ],
    );
    listing.lines().any(|line| {
        let mut fields = line.split_whitespace();
        let (Some(id), Some(flag)) = (fields.next(), fields.next()) else {
            return false;
        };
        flag == "1"
            && id != ctx.pane.0
            && PaneId(id.to_string()).numeric().unwrap_or(u64::MAX) < my_num
    })
}

/// Connect to the daemon, spawning it if nothing is listening yet, and send the
/// opening `Hello`. Returns a live [`Connection`], or `None` if the daemon could
/// not be reached.
fn connect(ctx: &Context, cols: u16, rows: u16) -> Option<Connection> {
    let stream = open_stream()?;
    let mut writer = stream.try_clone().ok()?;
    let hello = ClientMsg::Hello {
        server: ctx.server.clone(),
        window: ctx.window.clone(),
        pane: ctx.pane.clone(),
        cols,
        rows,
    };
    if write_message(&mut writer, &hello).is_err() {
        return None;
    }

    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut reader = std::io::BufReader::new(stream);
        loop {
            match read_message::<_, ServerMsg>(&mut reader) {
                Ok(Some(msg)) => {
                    if tx.send(Incoming::Msg(msg)).is_err() {
                        return;
                    }
                }
                _ => {
                    let _ = tx.send(Incoming::Closed);
                    return;
                }
            }
        }
    });
    Some(Connection { writer, rx })
}

/// Open a stream to the daemon socket: try once, and if nothing is listening,
/// spawn the daemon and retry a bounded number of times.
fn open_stream() -> Option<UnixStream> {
    if let Ok(s) = UnixStream::connect(daemon_socket()) {
        return Some(s);
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut command = Command::new(exe);
        command.arg("daemon");
        crate::platform::spawn_detached(command);
    }
    for _ in 0..CONNECT_RETRIES {
        thread::sleep(CONNECT_RETRY_INTERVAL);
        if let Ok(s) = UnixStream::connect(daemon_socket()) {
            return Some(s);
        }
    }
    None
}

/// Send one client message, reporting whether the write succeeded (a failure
/// means the connection died).
fn send(conn: &mut Connection, msg: &ClientMsg) -> bool {
    write_message(&mut conn.writer, msg).is_ok()
}

/// The client entry point: set up the terminal, connect, and run the render/input
/// loop until quit or an unrecoverable disconnect.
pub fn run() -> ExitCode {
    let Some(ctx) = resolve_context() else {
        eprintln!("wrangler client: not inside a tmux pane");
        return ExitCode::from(2);
    };
    if lower_sidebar_present(&ctx) {
        // Another sidebar won the spawn race for this window.
        return ExitCode::SUCCESS;
    }

    let (cols, rows) = terminal_size().unwrap_or((32, 24));
    let Some(mut conn) = connect(&ctx, cols, rows) else {
        eprintln!("wrangler client: could not reach the daemon");
        return ExitCode::from(1);
    };

    let colors = Colors::new();

    let mut stdout = io::stdout();
    if enable_raw_mode().is_err() {
        return ExitCode::from(1);
    }
    let _ = execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableFocusChange,
        Hide
    );
    let backend = CrosstermBackend::new(io::stdout());
    let outcome = Terminal::new(backend)
        .and_then(|mut terminal| event_loop(&mut terminal, &ctx, &colors, &mut conn));

    let _ = disable_raw_mode();
    let _ = execute!(
        io::stdout(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableFocusChange,
        Show
    );
    let _ = io::stdout().flush();

    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => ExitCode::from(1),
    }
}

/// The client-owned width logic. It clamps a user/tmux resize to the floor,
/// publishes the corrected width for the daemon to relay, and adopts a shared
/// width the daemon pushes, while never re-publishing a resize it requested
/// itself. `width` is the pane's last known width; `pending` is a width the
/// client asked tmux for and is awaiting (so its landing is not mistaken for a
/// fresh user resize).
struct WidthSync {
    width: u16,
    pending: Option<u16>,
    floor: u16,
    sync: bool,
}

impl WidthSync {
    fn new(width: u16, floor: u16, sync: bool) -> Self {
        Self {
            width,
            pending: None,
            floor,
            sync,
        }
    }

    /// A terminal resize left the pane `new_w` wide. Returns `(resize_to,
    /// publish)`: a width to resize the pane to (a min-width correction) and a
    /// width to publish to the daemon. A resize the client itself requested (its
    /// width equals the pending request) is swallowed, returning neither, so an
    /// adopted or self-corrected width never echoes back as a new user resize.
    fn on_terminal_resize(&mut self, new_w: u16) -> (Option<u16>, Option<u16>) {
        let was_ours = self.pending == Some(new_w);
        self.pending = None;
        self.width = new_w;
        if was_ours {
            return (None, None);
        }
        let corrected = new_w.max(self.floor);
        let resize = if corrected != new_w {
            self.pending = Some(corrected);
            Some(corrected)
        } else {
            None
        };
        let publish = self.sync.then_some(corrected);
        (resize, publish)
    }

    /// The daemon relayed a shared width of `cols`. Returns the width to resize
    /// the pane to, or `None` when sync is off, the pane is already at it, or that
    /// resize is already pending.
    fn on_shared_width(&mut self, cols: u16) -> Option<u16> {
        if !self.sync || cols == self.width || self.pending == Some(cols) {
            return None;
        }
        self.pending = Some(cols);
        Some(cols)
    }
}

/// The sidebar width floor and whether cross-sidebar width sync is on, from
/// `@wrangler-min-width` (default 24) and `@wrangler-sync-width` (default on).
fn read_width_options(server: &str) -> (u16, bool) {
    let floor = crate::tmux::run_tmux(server, &["show-option", "-gqv", "@wrangler-min-width"])
        .trim()
        .parse::<u16>()
        .unwrap_or(24);
    let sync_raw = crate::tmux::run_tmux(server, &["show-option", "-gqv", "@wrangler-sync-width"]);
    let sync = !matches!(
        sync_raw.trim().to_lowercase().as_str(),
        "off" | "0" | "no" | "false"
    );
    (floor, sync)
}

/// Resize this pane to `cols` columns (best-effort).
fn resize_pane(server: &str, pane: &str, cols: u16) {
    let _ = crate::tmux::run_tmux(
        server,
        &["resize-pane", "-t", pane, "-x", &cols.to_string()],
    );
}

/// The render/input loop. Paints the latest model each tick, drains daemon
/// pushes, and forwards input. A dropped connection triggers a reconnect; if that
/// fails, the loop ends.
fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ctx: &Context,
    colors: &Colors,
    conn: &mut Connection,
) -> io::Result<()> {
    let start = Instant::now();
    let mut offset = 0usize;
    let mut view: Option<View> = None;

    let (floor, sync) = read_width_options(&ctx.server.0);
    let (init_cols, _) = terminal_size().unwrap_or((32, 24));
    let mut width = WidthSync::new(init_cols, floor, sync);

    loop {
        let frame = (start.elapsed().as_secs_f64() / ANIM_INTERVAL) as usize;
        if let Some(v) = &view {
            terminal.draw(|f| render(f, v, colors, frame, &mut offset))?;
        } else {
            terminal.draw(|f| {
                let area = f.area();
                f.render_widget(Paragraph::new(""), area);
            })?;
        }

        // Drain daemon pushes; a close triggers a reconnect.
        loop {
            match conn.rx.try_recv() {
                Ok(Incoming::Msg(ServerMsg::Render(m))) => view = Some(View::new(m)),
                Ok(Incoming::Msg(ServerMsg::Width { cols })) => {
                    if let Some(w) = width.on_shared_width(cols) {
                        resize_pane(&ctx.server.0, &ctx.pane.0, w);
                    }
                }
                // The window has no real panes left; quit so the pane closes.
                Ok(Incoming::Msg(ServerMsg::Exit)) => return Ok(()),
                Ok(Incoming::Closed) | Err(TryRecvError::Disconnected) => {
                    let (cols, rows) = terminal_size().unwrap_or((32, 24));
                    match connect(ctx, cols, rows) {
                        Some(fresh) => *conn = fresh,
                        None => return Ok(()),
                    }
                }
                Err(TryRecvError::Empty) => break,
            }
        }

        if !event::poll(TICK)? {
            continue;
        }
        match event::read()? {
            Event::Key(k) if k.kind == KeyEventKind::Press => match k.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => {
                    // Close every sidebar on this server, not just this one, so
                    // toggling off is symmetric with toggling on.
                    let _ = write_message(
                        &mut conn.writer,
                        &CtlMsg::Toggle {
                            server: ctx.server.clone(),
                        },
                    );
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    if let Some(key) = view.as_ref().and_then(|v| neighbor_id(v, -1)) {
                        send(
                            conn,
                            &ClientMsg::Input {
                                event: InputEvent::Select { key },
                            },
                        );
                    }
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if let Some(key) = view.as_ref().and_then(|v| neighbor_id(v, 1)) {
                        send(
                            conn,
                            &ClientMsg::Input {
                                event: InputEvent::Select { key },
                            },
                        );
                    }
                }
                KeyCode::Enter => {
                    if let Some(key) = view.as_ref().and_then(|v| v.model.selection.clone()) {
                        send(
                            conn,
                            &ClientMsg::Input {
                                event: InputEvent::Activate { key },
                            },
                        );
                    }
                }
                _ => {}
            },
            Event::Mouse(m) => {
                if let MouseEventKind::Down(MouseButton::Left) = m.kind {
                    if let Some(v) = &view {
                        if let Some(key) = id_at_line(v, offset, m.row as usize) {
                            send(
                                conn,
                                &ClientMsg::Input {
                                    event: InputEvent::Activate { key },
                                },
                            );
                        }
                    }
                }
            }
            Event::FocusGained => {
                send(
                    conn,
                    &ClientMsg::Input {
                        event: InputEvent::FocusGained,
                    },
                );
            }
            Event::FocusLost => {
                send(
                    conn,
                    &ClientMsg::Input {
                        event: InputEvent::FocusLost,
                    },
                );
            }
            Event::Resize(new_w, _) => {
                let (resize, publish) = width.on_terminal_resize(new_w);
                if let Some(w) = resize {
                    resize_pane(&ctx.server.0, &ctx.pane.0, w);
                }
                if let Some(cols) = publish {
                    send(
                        conn,
                        &ClientMsg::Input {
                            event: InputEvent::Resize { cols },
                        },
                    );
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{
        Branch, Child, Indicator, ProgressState, RowContent, RowTree, Section, SessionKey,
        WindowNode,
    };
    use ratatui::backend::TestBackend;

    fn sample_view() -> View {
        View::new(RowModel {
            tree: RowTree {
                sections: vec![Section {
                    heading: Some("windows".into()),
                    windows: vec![WindowNode {
                        id: RowKey::Window {
                            window: WindowId("@0".into()),
                        },
                        index: "0".into(),
                        name: "main".into(),
                        active: true,
                        color: None,
                        children: vec![Child::Agent {
                            id: RowKey::Agent {
                                session: SessionKey("claude-abc".into()),
                                pane: PaneId("%1".into()),
                            },
                            index: "0".into(),
                            label: "claude - repo".into(),
                            active: false,
                            color: Some(NamedColor::Green),
                            indicator: Indicator::Attention,
                        }],
                    }],
                }],
            },
            selection: Some(RowKey::Window {
                window: WindowId("@0".into()),
            }),
            has_focus: true,
        })
    }

    #[test]
    fn renders_model_to_a_buffer() {
        let colors = Colors::new();
        let view = sample_view();
        let backend = TestBackend::new(24, 6);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut offset = 0;
        terminal
            .draw(|f| render(f, &view, &colors, 0, &mut offset))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let mut text = String::new();
        for y in 0..6 {
            for x in 0..24 {
                text.push_str(buffer[(x, y)].symbol());
            }
            text.push('\n');
        }
        // The header, the window row, and the attention dot all painted.
        assert!(text.contains("WINDOWS"), "header present:\n{text}");
        assert!(text.contains("0: main"), "window row present:\n{text}");
        assert!(text.contains('●'), "attention dot present:\n{text}");
    }

    #[test]
    fn the_selection_bar_reverses_a_row_and_drops_its_color() {
        let colors = Colors::new();
        // A colored icon and a state-colored indicator: both would paint a block
        // of color across the bar if their color survived the reverse.
        let row = Row {
            content: RowContent::Agent {
                index: "0".into(),
                label: "claude - repo".into(),
                branch: Branch::Last,
                here: false,
                color: Some(NamedColor::Green),
            },
            id: None,
            indicator: Indicator::Progress {
                pct: Some(50),
                state: ProgressState::Error,
            },
        };

        let plain = render_line(&row, &colors, 30, 0, false);
        assert!(
            plain.spans.iter().any(|s| s.style.fg.is_some()),
            "unselected, the icon and indicator keep their colors"
        );

        let selected = render_line(&row, &colors, 30, 0, true);
        for span in &selected.spans {
            assert_eq!(span.style.fg, None, "no color survives the bar: {span:?}");
            assert!(
                span.style.add_modifier.contains(Modifier::REVERSED),
                "the bar spans the whole row: {span:?}"
            );
        }
    }

    #[test]
    fn neighbor_id_moves_and_clamps() {
        let view = sample_view();
        // From the window row, down moves to the agent row.
        let down = neighbor_id(&view, 1).unwrap();
        assert_eq!(
            down,
            RowKey::Agent {
                session: SessionKey("claude-abc".into()),
                pane: PaneId("%1".into()),
            }
        );
        // Up from the first selectable clamps to itself.
        let up = neighbor_id(&view, -1).unwrap();
        assert_eq!(
            up,
            RowKey::Window {
                window: WindowId("@0".into())
            }
        );
    }

    #[test]
    fn user_resize_above_floor_publishes_without_correcting() {
        let mut w = WidthSync::new(40, 24, true);
        // A user drag to 30 is above the floor: no correction, publish 30.
        assert_eq!(w.on_terminal_resize(30), (None, Some(30)));
        assert_eq!(w.pending, None);
    }

    #[test]
    fn user_resize_below_floor_clamps_and_publishes_the_floor() {
        let mut w = WidthSync::new(40, 24, true);
        // A drag to 10 is clamped up to 24, which is both the resize and the
        // published width.
        assert_eq!(w.on_terminal_resize(10), (Some(24), Some(24)));
        assert_eq!(w.pending, Some(24));
        // The clamp's own resize event lands at 24 and is swallowed (no echo).
        assert_eq!(w.on_terminal_resize(24), (None, None));
        assert_eq!(w.pending, None);
    }

    #[test]
    fn adopted_shared_width_does_not_echo() {
        let mut w = WidthSync::new(40, 24, true);
        // A follower adopts the shared width 30, then its resize event lands.
        assert_eq!(w.on_shared_width(30), Some(30));
        assert_eq!(w.on_terminal_resize(30), (None, None));
        // Already at 30: a repeat push is a no-op.
        assert_eq!(w.on_shared_width(30), None);
    }

    #[test]
    fn sync_off_never_publishes_or_adopts() {
        let mut w = WidthSync::new(40, 24, false);
        // A user drag above the floor still applies no correction and, with sync
        // off, publishes nothing.
        assert_eq!(w.on_terminal_resize(30), (None, None));
        // A relayout below the floor still clamps locally, but publishes nothing.
        assert_eq!(w.on_terminal_resize(10), (Some(24), None));
        // A relayed width is ignored entirely.
        assert_eq!(w.on_shared_width(50), None);
    }

    #[test]
    fn id_at_line_indexes_selectable_rows() {
        let view = sample_view();
        // Line 0 is the heading (no id); line 1 is its blank; line 2 is the
        // window row.
        assert_eq!(id_at_line(&view, 0, 0), None);
        assert_eq!(id_at_line(&view, 0, 1), None);
        assert_eq!(
            id_at_line(&view, 0, 2),
            Some(RowKey::Window {
                window: WindowId("@0".into())
            })
        );
    }
}
