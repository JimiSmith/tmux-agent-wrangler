//! Shared domain vocabulary for the daemon, client, and wire protocol.
//!
//! These types are the common language the pure-logic modules (color, labels,
//! rows, assoc) and the integration layer speak. The daemon builds a semantic
//! [`RowModel`] and the client paints it: the daemon decides *what* each row is
//! and its [`Indicator`] state, while the client resolves the spinner frame and
//! the terminal colors at paint time.

use serde::{Deserialize, Serialize};

/// A tmux server, identified by its socket path (the first field of `$TMUX`).
/// All tmux-facing state is partitioned by this, so panes/windows from
/// different servers are never conflated (pane ids are per-server).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerKey(pub String);

/// A tmux window id, e.g. `"@3"`. Scoped to one server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

/// A tmux pane id, e.g. `"%5"`. Scoped to one server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub String);

impl PaneId {
    /// The integer after the leading `%`, used only for the spawn-race tiebreak
    /// (a numeric compare, never a lexical one). `None` if it is not the
    /// `%<digits>` form.
    pub fn numeric(&self) -> Option<u64> {
        self.0.strip_prefix('%').and_then(|d| d.parse().ok())
    }
}

/// An agent session's registry key: the `<agent>-<session_id>` file-name key
/// the registry is stored under, and the identity used in row keys and the
/// notification dedup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(pub String);

/// One of the eight color names an agent can be assigned (Claude's `/color`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedColor {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}

/// The color/state of a progress indicator. `Plain` inherits the row's own
/// color (a generic in-progress signal); the others are OSC 9;4 progress states
/// and carry an explicit color the client applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    Plain,
    Normal,
    Paused,
    Error,
}

/// The semantic indicator pinned to a row's right edge. The daemon emits this;
/// the client resolves an indeterminate `Progress` to the current spinner frame
/// at paint time, and a determinate one to a percentage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Indicator {
    None,
    /// A turn finished or a notification fired: the static "needs input" dot.
    Attention,
    /// Progress: indeterminate (`pct` None -> spinner) or determinate
    /// (`pct` Some -> percentage), colored by `state`.
    Progress { pct: Option<u8>, state: ProgressState },
}

/// An agent session's hook turn state. `Working`/`Attention` are mutually
/// exclusive; the row is emphasized (bold) in either.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    Idle,
    Working,
    Attention,
}

/// A pane within a window, as the daemon reads it from tmux.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub index: String,
    pub active: bool,
    /// The pane title (the agent session title Claude sets on it, before the
    /// status-glyph strip that matches it to a session).
    pub title: String,
    /// tmux `pane_pb_state` (OSC 9;4), e.g. `"normal"`/`"paused"`/`"error"`/
    /// `"indeterminate"`/`"hidden"`; empty on a tmux too old to report it.
    pub pb_state: String,
    /// tmux `pane_pb_progress` percentage, if any.
    pub pb_progress: Option<u8>,
    /// Optional per-pane color (from the tmux pane border color).
    pub color: Option<NamedColor>,
}

/// A tmux window and its panes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub index: String,
    pub name: String,
    pub active: bool,
    pub color: Option<NamedColor>,
    pub panes: Vec<Pane>,
}

/// An agent session placed under one window/pane. A session displayed in
/// several panes yields several `Session` placements, one per pane, so it
/// appears under each window it is visible in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionKey,
    /// The agent kind: `"claude"`, `"copilot"`, ...
    pub agent: String,
    pub pane: PaneId,
    pub window: WindowId,
    pub label: String,
    /// The session's assigned color, or `None` for the default agent color.
    pub color: Option<NamedColor>,
    pub status: TurnStatus,
}

/// The stable key of a selectable row. The selection is shared across a
/// server's sidebars, so the key must be identical wherever a logical row is
/// drawn. An agent row's key includes the pane because one session can appear
/// under several windows at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowKey {
    Window { window: WindowId },
    AgentWindow { agent: String, window: WindowId },
    Pane { pane: PaneId },
    Agent { session: SessionKey, pane: PaneId },
}

/// What a display row is, for styling. Windows and panes may carry their own
/// color; an agent row is `emphatic` (bold) while working or needing attention.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowKind {
    Header,
    Blank,
    Window { active: bool, color: Option<NamedColor> },
    Pane { color: Option<NamedColor> },
    Agent { color: Option<NamedColor>, emphatic: bool },
}

/// One flattened display row the client paints. `key` marks a selectable row.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub text: String,
    pub kind: RowKind,
    pub key: Option<RowKey>,
    pub indicator: Indicator,
}

/// The per-window render payload the daemon pushes to a client: the rows to
/// paint, the shared selection, and whether this sidebar currently has focus
/// (the active pane of the active window) so only it shows the selection bar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowModel {
    pub rows: Vec<Row>,
    pub selection: Option<RowKey>,
    pub has_focus: bool,
}
