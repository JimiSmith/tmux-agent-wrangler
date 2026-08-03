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

use crate::client::render::notification_body_field;
use crate::client::render::{base_style, fit_segments, row_segments};
use crate::color::{agent_color_table, claude_dir, read_theme};
use crate::daemon::rows::StateColor;
use crate::model::{
    notification_ids, Indicator, NamedColor, NotificationNode, PaneId, Row, RowContent, RowKey,
    RowModel, ServerKey, WindowId,
};
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
/// rows rather than the tree on every frame. The notification area is not
/// flattened here: an entry's height depends on the width its description wraps
/// to, so its rows are built by the paint. What is fixed is the order of its
/// entries, which is what navigation runs on.
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

/// The heading drawn above the notification area.
const NOTIFICATIONS_HEADING: &str = "notifications";

/// How the last frame was laid out: the rows given to the tree, and the
/// notification area exactly as it was drawn. The paint records it so a click
/// resolves against the lines it landed on rather than a re-derived layout.
#[derive(Clone, Debug, Default)]
struct Layout {
    tree_height: usize,
    notif: Vec<Row>,
}

/// Wrap `text` to `field` columns, breaking on spaces and hard-splitting a word
/// too long to fit one. Never returns an empty run, so an entry always draws at
/// least one description line.
fn wrap(text: &str, field: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let mut word = word;
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() <= field {
            line.push(' ');
            line.push_str(word);
            continue;
        }
        if !line.is_empty() {
            lines.push(std::mem::take(&mut line));
        }
        // A word wider than the field is cut across as many lines as it takes.
        while word.chars().count() > field {
            let head: String = word.chars().take(field).collect();
            let cut = head.len();
            lines.push(head);
            word = &word[cut..];
        }
        line.push_str(word);
    }
    lines.push(line);
    lines
}

/// The notification area's rows for a pane `width` columns wide, given at most
/// `cap` of them: the heading, then each entry as its title over its wrapped
/// description.
///
/// An entry is laid in only if it fits whole, so the area never shows a title
/// over a cut-off description; the newest go in first, so a short area is the
/// oldest entries that give way. Nothing at all comes back when the cap leaves
/// no room for an entry beside the heading — a heading over nothing costs a row
/// and says less than the tree it displaced.
fn notification_lines(nodes: &[NotificationNode], width: usize, cap: usize) -> Vec<Row> {
    if nodes.is_empty() || cap < 2 {
        return Vec::new();
    }
    let mut rows = vec![plain_row(RowContent::Header {
        text: NOTIFICATIONS_HEADING.to_string(),
    })];
    for node in nodes {
        let mut entry = vec![Row {
            content: RowContent::NotificationTitle {
                title: node.title.clone(),
                color: node.color,
            },
            id: Some(node.id.clone()),
            indicator: Indicator::None,
        }];
        entry.extend(
            wrap(&node.body, notification_body_field(width))
                .into_iter()
                .map(|text| Row {
                    content: RowContent::NotificationBody { text },
                    // Every line of an entry answers to the entry, so a click
                    // anywhere in it opens the same thing.
                    id: Some(node.id.clone()),
                    indicator: Indicator::None,
                }),
        );
        if rows.len() + entry.len() > cap {
            break;
        }
        rows.extend(entry);
    }
    if rows.len() < 2 {
        return Vec::new();
    }
    rows
}

/// A row that is drawn but not selectable: the area's heading.
fn plain_row(content: RowContent) -> Row {
    Row {
        content,
        id: None,
        indicator: Indicator::None,
    }
}

/// Paint the view, scrolling the tree so the selected row stays visible and
/// pinning the notification area to the foot of the pane. `offset` is the tree's
/// scroll position and `place` the height split, both carried between frames —
/// `place` so a click can be resolved against the frame it hit.
fn render(
    frame_ui: &mut ratatui::Frame,
    view: &View,
    colors: &Colors,
    frame: usize,
    offset: &mut usize,
    place: &mut Layout,
) {
    let area = frame_ui.area();
    let width = area.width as usize;
    let height = area.height as usize;
    if width == 0 || height == 0 {
        return;
    }
    // A quarter of the pane is the area's ceiling; the tree keeps the rest.
    place.notif = notification_lines(&view.model.notifications, width, height / 4);
    place.tree_height = height - place.notif.len();
    let tree_height = place.tree_height;

    let selection = view.model.selection.as_ref();
    let mut off = *offset;
    // A selected notification is always on screen, so only a selection in the
    // tree moves the tree's scroll.
    if let Some(sel_row) =
        selection.and_then(|k| view.rows.iter().position(|r| r.id.as_ref() == Some(k)))
    {
        if sel_row < off {
            off = sel_row;
        } else if tree_height > 0 && sel_row >= off + tree_height {
            off = sel_row - tree_height + 1;
        }
    }
    off = off.min(view.rows.len().saturating_sub(tree_height));
    *offset = off;

    let selected =
        |row: &Row| view.model.has_focus && row.id.is_some() && row.id.as_ref() == selection;
    let mut lines = Vec::new();
    for row in view.rows.iter().skip(off).take(tree_height) {
        lines.push(render_line(row, colors, width, frame, selected(row)));
    }
    // A tree shorter than its region is padded out, so the notification area
    // stays at the foot of the pane rather than riding up under the last window.
    lines.resize(tree_height, Line::default());
    for row in &place.notif {
        lines.push(render_line(row, colors, width, frame, selected(row)));
    }
    frame_ui.render_widget(Paragraph::new(Text::from(lines)), area);
}

/// The selectable rows' ids in display order: the tree, then the notification
/// area beneath it, so navigation runs off the end of one into the other. An
/// entry appears once however many lines it is drawn on.
fn selectable_ids(view: &View) -> Vec<RowKey> {
    view.rows
        .iter()
        .filter_map(|r| r.id.clone())
        .chain(notification_ids(&view.model.notifications))
        .collect()
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

/// The id of the selectable row at display line `line` of the frame `place`
/// describes: a line in the tree region indexes the scrolled tree, one below it
/// indexes the notification area, which does not scroll.
fn id_at_line(view: &View, offset: usize, place: &Layout, line: usize) -> Option<RowKey> {
    let row = if line < place.tree_height {
        view.rows.get(offset + line)
    } else {
        place.notif.get(line - place.tree_height)
    };
    row.and_then(|r| r.id.clone())
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

/// The width range a sidebar is held within. The ceiling is raised to the floor
/// on construction, so the range can never be empty and `clamp` is total: a
/// `@wrangler-max-width` set below the minimum yields the minimum rather than a
/// contradiction to resolve at every resize.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WidthBounds {
    floor: u16,
    ceiling: u16,
}

impl WidthBounds {
    fn new(floor: u16, ceiling: u16) -> Self {
        Self {
            floor,
            ceiling: ceiling.max(floor),
        }
    }

    fn clamp(self, cols: u16) -> u16 {
        cols.clamp(self.floor, self.ceiling)
    }
}

/// The client-owned width logic. It clamps a user/tmux resize to the bounds,
/// publishes the corrected width for the daemon to relay, and adopts a shared
/// width the daemon pushes, while never re-publishing a resize it requested
/// itself. `width` is the pane's last known width; `pending` is a width the
/// client asked tmux for and is awaiting (so its landing is not mistaken for a
/// fresh user resize).
struct WidthSync {
    width: u16,
    pending: Option<u16>,
    bounds: WidthBounds,
    sync: bool,
}

impl WidthSync {
    fn new(width: u16, bounds: WidthBounds, sync: bool) -> Self {
        Self {
            width,
            pending: None,
            bounds,
            sync,
        }
    }

    /// A terminal resize left the pane `new_w` wide. Returns `(resize_to,
    /// publish)`: a width to resize the pane to (a bounds correction) and a
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
        let corrected = self.bounds.clamp(new_w);
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

/// The sidebar width bounds and whether cross-sidebar width sync is on, from
/// `@wrangler-min-width` (default 24), `@wrangler-max-width` (default unbounded)
/// and `@wrangler-sync-width` (default on).
fn read_width_options(server: &str) -> (WidthBounds, bool) {
    let floor = width_option(server, "@wrangler-min-width").unwrap_or(24);
    let ceiling = width_option(server, "@wrangler-max-width").unwrap_or(u16::MAX);
    let sync_raw = crate::tmux::run_tmux(server, &["show-option", "-gqv", "@wrangler-sync-width"]);
    let sync = !matches!(
        sync_raw.trim().to_lowercase().as_str(),
        "off" | "0" | "no" | "false"
    );
    (WidthBounds::new(floor, ceiling), sync)
}

/// A column count from the tmux option `name`, or `None` when it is unset or not
/// a number.
fn width_option(server: &str, name: &str) -> Option<u16> {
    crate::tmux::run_tmux(server, &["show-option", "-gqv", name])
        .trim()
        .parse::<u16>()
        .ok()
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
    let mut place = Layout::default();
    let mut view: Option<View> = None;

    let (bounds, sync) = read_width_options(&ctx.server.0);
    let (init_cols, _) = terminal_size().unwrap_or((32, 24));
    let mut width = WidthSync::new(init_cols, bounds, sync);

    loop {
        let frame = (start.elapsed().as_secs_f64() / ANIM_INTERVAL) as usize;
        if let Some(v) = &view {
            terminal.draw(|f| render(f, v, colors, frame, &mut offset, &mut place))?;
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
                        if let Some(key) = id_at_line(v, offset, &place, m.row as usize) {
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
        Branch, Child, Indicator, NotificationNode, ProgressState, RowContent, RowTree, Section,
        SessionKey, WindowNode,
    };
    use ratatui::backend::TestBackend;

    fn sample_view() -> View {
        view_with(Vec::new())
    }

    /// The sample view with `notifications` in the area beneath its tree.
    fn view_with(notifications: Vec<NotificationNode>) -> View {
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
            notifications,
            selection: Some(RowKey::Window {
                window: WindowId("@0".into()),
            }),
            has_focus: true,
        })
    }

    fn notification(session: &str, body: &str) -> NotificationNode {
        NotificationNode {
            id: RowKey::Notification {
                session: SessionKey(session.into()),
            },
            title: "claude".into(),
            body: body.into(),
            color: None,
        }
    }

    /// The kind and text of each row, which is what a layout assertion is about.
    fn shape(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r.content {
                RowContent::Header { text } => format!("header:{text}"),
                RowContent::NotificationTitle { title, .. } => format!("title:{title}"),
                RowContent::NotificationBody { text } => format!("body:{text}"),
                other => format!("{other:?}"),
            })
            .collect()
    }

    /// The `height` lines a view paints into a pane `width` columns wide.
    fn painted(view: &View, width: u16, height: u16) -> Vec<String> {
        let colors = Colors::new();
        let backend = TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).unwrap();
        let mut offset = 0;
        let mut place = Layout::default();
        terminal
            .draw(|f| render(f, view, &colors, 0, &mut offset, &mut place))
            .unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
            .collect()
    }

    #[test]
    fn renders_model_to_a_buffer() {
        let text = painted(&sample_view(), 24, 6).join("\n");
        // The header, the window row, and the attention dot all painted.
        assert!(text.contains("WINDOWS"), "header present:\n{text}");
        assert!(text.contains("0: main"), "window row present:\n{text}");
        assert!(text.contains('●'), "attention dot present:\n{text}");
    }

    #[test]
    fn the_notification_area_sits_at_the_foot_of_the_pane() {
        // Two one-line entries plus their heading are 5 rows, inside a quarter of
        // 24: the area is drawn whole, flush with the bottom and below a tree
        // that does not reach it.
        let view = view_with(vec![
            notification("claude-a", "vim · newest"),
            notification("claude-b", "server · older"),
        ]);
        let lines = painted(&view, 24, 24);
        assert!(lines[19].contains("NOTIFICATIONS"), "{lines:#?}");
        assert!(lines[20].contains("claude"), "{lines:#?}");
        assert!(lines[21].contains("vim · newest"), "{lines:#?}");
        assert!(lines[23].contains("server · older"), "{lines:#?}");
    }

    #[test]
    fn an_entry_is_its_title_over_its_description() {
        assert_eq!(
            shape(&notification_lines(
                &[notification("claude-a", "vim · api")],
                24,
                8
            )),
            vec!["header:notifications", "title:claude", "body:vim · api"]
        );
    }

    #[test]
    fn a_description_too_wide_for_the_pane_wraps_rather_than_truncating() {
        let rows = notification_lines(
            &[notification("claude-a", "vim · api-service-gateway")],
            24,
            8,
        );
        assert_eq!(
            shape(&rows),
            vec![
                "header:notifications",
                "title:claude",
                "body:vim ·",
                "body:api-service-gateway",
            ]
        );
        assert!(
            rows[2].id == rows[3].id && rows[3].id == rows[1].id,
            "every line of an entry answers to the entry"
        );
    }

    #[test]
    fn the_area_takes_only_entries_that_fit_whole() {
        let nodes = [
            notification("claude-a", "newest"),
            notification("claude-b", "older"),
        ];
        // Four rows hold the heading and both entries.
        assert_eq!(notification_lines(&nodes, 24, 5).len(), 5);
        // Four leave the second entry a title with no room for its description,
        // so it is left out whole rather than cut.
        assert_eq!(
            shape(&notification_lines(&nodes, 24, 4)),
            vec!["header:notifications", "title:claude", "body:newest"]
        );
        // Two cannot hold one entry, so the area is dropped and the tree keeps
        // the rows.
        assert!(notification_lines(&nodes, 24, 2).is_empty());
        assert!(notification_lines(&nodes, 24, 1).is_empty());
        assert!(notification_lines(&[], 24, 20).is_empty());
    }

    #[test]
    fn wrapping_breaks_on_spaces_and_splits_a_word_too_long_to_fit() {
        assert_eq!(wrap("a b c", 5), vec!["a b c"]);
        assert_eq!(wrap("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        assert_eq!(wrap("aaaaaaaa", 3), vec!["aaa", "aaa", "aa"]);
        // Multi-byte characters are counted, not their bytes.
        assert_eq!(wrap("··· ···", 3), vec!["···", "···"]);
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
    fn navigation_runs_off_the_tree_into_the_notification_area() {
        let view = view_with(vec![notification("claude-a", "one")]);
        // The tree's last row is the agent; one more step reaches the entry
        // beneath it, and a further step clamps there.
        assert_eq!(
            neighbor_id(&view, 2),
            Some(RowKey::Notification {
                session: SessionKey("claude-a".into())
            })
        );
        assert_eq!(neighbor_id(&view, 3), neighbor_id(&view, 2));
    }

    /// The default bounds: a floor of 24 and no ceiling.
    fn bounds() -> WidthBounds {
        WidthBounds::new(24, u16::MAX)
    }

    #[test]
    fn user_resize_within_bounds_publishes_without_correcting() {
        let mut w = WidthSync::new(40, bounds(), true);
        // A user drag to 30 is inside the bounds: no correction, publish 30.
        assert_eq!(w.on_terminal_resize(30), (None, Some(30)));
        assert_eq!(w.pending, None);
    }

    #[test]
    fn user_resize_below_floor_clamps_and_publishes_the_floor() {
        let mut w = WidthSync::new(40, bounds(), true);
        // A drag to 10 is clamped up to 24, which is both the resize and the
        // published width.
        assert_eq!(w.on_terminal_resize(10), (Some(24), Some(24)));
        assert_eq!(w.pending, Some(24));
        // The clamp's own resize event lands at 24 and is swallowed (no echo).
        assert_eq!(w.on_terminal_resize(24), (None, None));
        assert_eq!(w.pending, None);
    }

    #[test]
    fn user_resize_above_ceiling_clamps_and_publishes_the_ceiling() {
        let mut w = WidthSync::new(40, WidthBounds::new(24, 48), true);
        // A drag to 60 is clamped down to 48, which is both the resize and the
        // published width.
        assert_eq!(w.on_terminal_resize(60), (Some(48), Some(48)));
        assert_eq!(w.pending, Some(48));
        // The clamp's own resize event lands at 48 and is swallowed (no echo).
        assert_eq!(w.on_terminal_resize(48), (None, None));
        assert_eq!(w.pending, None);
    }

    #[test]
    fn a_ceiling_below_the_floor_pins_the_width_to_the_floor() {
        let mut w = WidthSync::new(40, WidthBounds::new(24, 10), true);
        assert_eq!(w.on_terminal_resize(60), (Some(24), Some(24)));
        assert_eq!(w.on_terminal_resize(24), (None, None));
        assert_eq!(w.on_terminal_resize(5), (Some(24), Some(24)));
    }

    #[test]
    fn adopted_shared_width_does_not_echo() {
        let mut w = WidthSync::new(40, bounds(), true);
        // A follower adopts the shared width 30, then its resize event lands.
        assert_eq!(w.on_shared_width(30), Some(30));
        assert_eq!(w.on_terminal_resize(30), (None, None));
        // Already at 30: a repeat push is a no-op.
        assert_eq!(w.on_shared_width(30), None);
    }

    #[test]
    fn sync_off_never_publishes_or_adopts() {
        let mut w = WidthSync::new(40, bounds(), false);
        // A user drag inside the bounds still applies no correction and, with
        // sync off, publishes nothing.
        assert_eq!(w.on_terminal_resize(30), (None, None));
        // A relayout below the floor still clamps locally, but publishes nothing.
        assert_eq!(w.on_terminal_resize(10), (Some(24), None));
        // A relayed width is ignored entirely.
        assert_eq!(w.on_shared_width(50), None);
    }

    /// The layout of a `height`-line pane 24 columns wide holding `view`.
    fn placed(view: &View, height: usize) -> Layout {
        let notif = notification_lines(&view.model.notifications, 24, height / 4);
        Layout {
            tree_height: height - notif.len(),
            notif,
        }
    }

    #[test]
    fn id_at_line_indexes_selectable_rows() {
        let view = sample_view();
        let place = placed(&view, 16);
        // Line 0 is the heading (no id); line 1 is its blank; line 2 is the
        // window row.
        assert_eq!(id_at_line(&view, 0, &place, 0), None);
        assert_eq!(id_at_line(&view, 0, &place, 1), None);
        assert_eq!(
            id_at_line(&view, 0, &place, 2),
            Some(RowKey::Window {
                window: WindowId("@0".into())
            })
        );
    }

    #[test]
    fn a_click_anywhere_in_an_entry_opens_it() {
        let view = view_with(vec![notification("claude-a", "vim · api")]);
        // A 16-line pane: the heading, the title and the description take the
        // last three lines, whatever the tree above them is doing.
        let place = placed(&view, 16);
        assert_eq!(place.tree_height, 13);
        assert_eq!(id_at_line(&view, 0, &place, 13), None, "the heading");
        let entry = Some(RowKey::Notification {
            session: SessionKey("claude-a".into()),
        });
        assert_eq!(id_at_line(&view, 0, &place, 14), entry, "its title");
        assert_eq!(id_at_line(&view, 0, &place, 15), entry, "its description");
        // The padding between a short tree and the area names no row.
        assert_eq!(id_at_line(&view, 0, &place, 12), None);
    }
}
