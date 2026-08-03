//! The daemon's authoritative state and the logic that turns inbound events into
//! outbound pushes.
//!
//! All mutable state lives here and is driven serially: a hook updates the
//! registry, a client reports input, and a poll rebuilds each server's rows. The
//! side-effecting tmux/ps/tty operations are reached only through the
//! [`TmuxEnv`] seam, so the whole poll pass and every handler are exercised in
//! tests against a fake environment with no live tmux and no real sockets.
//!
//! `ConnId` identifies a socket connection; the socket handles themselves live in
//! the runtime, which maps a returned push back to the connection that must
//! receive it.

use std::collections::HashMap;

use indexmap::{IndexMap, IndexSet};

use crate::daemon::assoc::{fetch_agent_sessions, RegistryRecord, RegistrySession};
use crate::daemon::notify::{
    acknowledge_focused_attention, notification_text, osc_escape, Notifier, OscNotify,
};
use crate::daemon::rows::build_tree;
use crate::labels::{label_mode_from, LabelCache};
use crate::model::{
    notification_ids, NamedColor, NotificationNode, PaneId, Placement, RowContent, RowKey,
    RowModel, RowTree, ServerKey, Session, SessionKey, TurnStatus, ViewMode, Window, WindowId,
};
use crate::proto::{HookAction, InputEvent, ServerMsg};

/// A connected socket, numbered in accept order. The lowest id among a window's
/// sidebars is unused here; ordering matters only as a stable client identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ConnId(pub u64);

/// The side-effecting operations the state logic needs, behind one trait so tests
/// substitute a fake. Reads (`fetch_windows`, `ppid_map`, `option`, `pane_tty`,
/// `client_ttys`) feed the poll; the actions (`focus`, `toggle`, `spawn`,
/// `focus_key`) and `write_tty` are fire-and-forget effects.
pub trait TmuxEnv {
    fn fetch_windows(&self, socket: &str) -> crate::tmux::FetchResult;
    /// The real (non-sidebar) pane ids of one window, for the alone-sidebar check.
    fn window_real_panes(&self, socket: &str, window: &str) -> Vec<PaneId>;
    fn ppid_map(&self) -> HashMap<u32, u32>;
    /// Read a global tmux option's raw value (untrimmed), empty when unset.
    fn option(&self, socket: &str, name: &str) -> String;
    fn focus(&self, socket: &str, window: &str, pane: Option<&str>);
    fn toggle(&self, socket: &str);
    fn focus_key(&self, socket: &str);
    /// The tty path of a pane, for the bell.
    fn pane_tty(&self, socket: &str, pane: &str) -> String;
    /// The ttys of every client attached to the session showing `pane`, for the
    /// desktop notification.
    fn client_ttys(&self, socket: &str, pane: &str) -> Vec<String>;
    /// Best-effort raw write to a tty path (empty path is a no-op).
    fn write_tty(&self, path: &str, data: &str);
}

/// One live registry entry. `record` holds the persisted fields; `server` scopes
/// a paned entry to the tmux server that reported it (a pane-less, daemon-hosted
/// entry has `None` and is title-matched on every server). `status` and
/// `attention_token` are the in-memory turn markers, mutually exclusive: a
/// working event clears the token, an attention event clears the status.
#[derive(Clone, Debug)]
pub struct RegistryEntry {
    pub record: RegistryRecord,
    pub server: Option<ServerKey>,
    pub status: TurnStatus,
    pub attention_token: Option<i128>,
}

/// How many attention events the notification area holds. Older ones are pushed
/// out: the area is the recent past, not a log.
const NOTIFICATION_LIMIT: usize = 3;

/// One attention event held in the notification area, keyed by the session it
/// came from (one entry per session). `pane` is where that session is displayed
/// and `body` how the event reads *now*, both re-read on every poll, so opening
/// the entry lands where the agent currently is rather than where it was when
/// the event fired.
#[derive(Clone, Debug, PartialEq)]
pub struct Notification {
    pub session: SessionKey,
    pub pane: PaneId,
    /// The agent kind, which titles the entry.
    pub agent: String,
    pub body: String,
    pub color: Option<NamedColor>,
}

/// Per-server shared UI state: the highlighted row, the column width sidebars on
/// this server keep in sync, and the notification area's entries (newest first).
/// The selection and width are absent until a client sets them.
#[derive(Clone, Debug, Default)]
pub struct ServerState {
    pub selection: Option<RowKey>,
    pub width: Option<u16>,
    pub notifications: Vec<Notification>,
}

/// A connected sidebar client: which window's sidebar it is, on which server, its
/// last reported width, and the last model pushed to it (so an unchanged rebuild
/// is not re-sent).
#[derive(Clone, Debug)]
pub struct ClientConn {
    pub server: ServerKey,
    pub window: WindowId,
    pub pane: PaneId,
    pub cols: u16,
    pub last_pushed: Option<RowModel>,
}

/// The whole daemon state. The registry is global (no server affinity for a
/// pane-less entry); everything tmux-facing is keyed by server.
#[derive(Debug, Default)]
pub struct State {
    pub registry: IndexMap<crate::model::SessionKey, RegistryEntry>,
    pub servers: IndexMap<ServerKey, ServerState>,
    pub clients: IndexMap<ConnId, ClientConn>,
    pub notifier: Notifier,
    pub labels: LabelCache,
    /// Set when the registry's record set changed (an add or a remove), so the
    /// runtime rewrites the persistence snapshot. Turn-marker flips do not set it:
    /// the snapshot holds only the records, not the in-memory markers.
    pub dirty_registry: bool,
}

/// A value in the option's off-set means off; anything else (including unset)
/// means on. The default-on opt-out options use this.
fn opt_enabled_default_on(value: &str) -> bool {
    !matches!(
        value.trim().to_lowercase().as_str(),
        "off" | "0" | "no" | "false"
    )
}

/// A value in the on-set means on; anything else (including unset) means off. The
/// default-off opt-in options use this.
fn opt_enabled_default_off(value: &str) -> bool {
    matches!(
        value.trim().to_lowercase().as_str(),
        "on" | "1" | "yes" | "true"
    )
}

/// The layout from `@wrangler-sections`, a default-off opt-in: only an on-value
/// selects the sectioned layout.
fn view_mode_from(value: &str) -> ViewMode {
    if opt_enabled_default_off(value) {
        ViewMode::Sections
    } else {
        ViewMode::Unified
    }
}

/// The desktop-notification mode from `@wrangler-osc-notify`: `777` (also the
/// meaning of on/1/yes/true), `9`, or `None` when disabled (the default).
fn osc_notify_mode(value: &str) -> Option<OscNotify> {
    match value.trim().to_lowercase().as_str() {
        "777" | "on" | "1" | "yes" | "true" => Some(OscNotify::Osc777),
        "9" => Some(OscNotify::Osc9),
        _ => None,
    }
}

/// The hook-status string a session's turn state presents to the window-tree
/// mirror: `"working"`, `"attention"`, or `""` for idle.
fn status_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Working => "working",
        TurnStatus::Attention => "attention",
        TurnStatus::Idle => "",
    }
}

/// Whether the sidebar of `window` is the focused pane: the window is active and
/// none of its real panes is (so the active pane is the sidebar itself). A window
/// no longer present is not focused.
fn window_focus(window: &WindowId, windows: &[Window]) -> bool {
    match windows.iter().find(|w| &w.id == window) {
        Some(w) => w.active && w.panes.iter().all(|p| !p.active),
        None => false,
    }
}

/// Whether `window` still exists but holds no real panes (only its sidebar). A
/// window that is gone entirely is not this: the sidebar's own pane is gone with
/// it, so the client disconnects on its own.
fn window_has_no_real_panes(window: &WindowId, windows: &[Window]) -> bool {
    windows
        .iter()
        .find(|w| &w.id == window)
        .map(|w| w.panes.is_empty())
        .unwrap_or(false)
}

/// Resolve the shared selection against the rows just built: keep the current id
/// if it still names a row, else default to the active window's row, else the
/// first selectable row, else `None` when nothing is selectable.
///
/// Resolving against the flattened rows is what keeps the selection in the order
/// a sidebar navigates and paints in. The notification entries are part of that
/// order (they are navigable), so a selected entry survives a repoll and one
/// that has been opened or pushed out falls back like any vanished row.
fn resolve_selection(
    current: Option<&RowKey>,
    tree: &RowTree,
    notifications: &[NotificationNode],
) -> Option<RowKey> {
    let rows = tree.flatten();
    let notif_ids = notification_ids(notifications);
    let ids = || rows.iter().filter_map(|r| r.id.as_ref()).chain(&notif_ids);
    let first_id = ids().next();
    first_id?;
    if let Some(k) = current {
        if ids().any(|id| id == k) {
            return Some(k.clone());
        }
    }
    for r in &rows {
        if let (
            Some(id @ RowKey::Window { .. }),
            RowContent::Window {
                placement: Placement::Here,
                ..
            },
        ) = (&r.id, &r.content)
        {
            return Some(id.clone());
        }
    }
    first_id.cloned()
}

impl State {
    pub fn new() -> Self {
        Self::default()
    }

    /// Seed the registry from persisted records. Turn markers are not persisted,
    /// so every loaded entry starts idle; the server scope is unknown on load and
    /// filled in again by the next hook.
    pub fn load_records(&mut self, records: Vec<(crate::model::SessionKey, RegistryRecord)>) {
        for (key, record) in records {
            self.registry.insert(
                key,
                RegistryEntry {
                    record,
                    server: None,
                    status: TurnStatus::Idle,
                    attention_token: None,
                },
            );
        }
    }

    /// The set of every registered session id, for the notifier's liveness pruning.
    pub fn registry_keys(&self) -> IndexSet<crate::model::SessionKey> {
        self.registry.keys().cloned().collect()
    }

    /// Apply a hook event and return the servers whose sidebars should repaint
    /// immediately (only an attention event, so its bell/notification fires within
    /// a tick rather than on the next poll): the entry's own server, or every
    /// server for a pane-less entry. A non-attention event returns nothing and is
    /// reflected on the next scheduled poll.
    #[allow(clippy::too_many_arguments)]
    pub fn on_hook(
        &mut self,
        server: Option<ServerKey>,
        pane: Option<PaneId>,
        agent: String,
        event: HookAction,
        session_id: String,
        cwd: String,
        transcript: String,
        pid: Option<u32>,
        token: i128,
    ) -> Vec<ServerKey> {
        let key = crate::model::SessionKey(format!("{agent}-{session_id}"));

        if event == HookAction::End {
            if self.registry.shift_remove(&key).is_some() {
                self.dirty_registry = true;
            }
            return Vec::new();
        }

        if !self.registry.contains_key(&key) {
            self.registry.insert(
                key.clone(),
                RegistryEntry {
                    record: RegistryRecord {
                        pane: pane.map(|p| p.0).unwrap_or_default(),
                        agent: agent.clone(),
                        pid: pid.map(|p| p.to_string()).unwrap_or_default(),
                        cwd,
                        transcript,
                        session_id: session_id.clone(),
                    },
                    server: server.clone(),
                    status: TurnStatus::Idle,
                    attention_token: None,
                },
            );
            self.dirty_registry = true;
        }

        let entry = self.registry.get_mut(&key).expect("just ensured present");
        let attention = match event {
            HookAction::Working => {
                entry.status = TurnStatus::Working;
                entry.attention_token = None;
                false
            }
            HookAction::NeedsAttention | HookAction::Error => {
                entry.status = TurnStatus::Idle;
                entry.attention_token = Some(token);
                true
            }
            HookAction::Start => false,
            HookAction::End => unreachable!("handled above"),
        };

        if !attention {
            return Vec::new();
        }
        match self.registry.get(&key).and_then(|e| e.server.clone()) {
            Some(s) => vec![s],
            None => self.servers.keys().cloned().collect(),
        }
    }

    /// Register a client and render its server. Returns the initial pushes: the
    /// server's rows for every client (this one included) plus the shared width
    /// for the new client to adopt, if one is set.
    pub fn on_hello<E: TmuxEnv>(
        &mut self,
        env: &E,
        conn: ConnId,
        server: ServerKey,
        window: WindowId,
        pane: PaneId,
        cols: u16,
    ) -> Vec<(ConnId, ServerMsg)> {
        self.servers.entry(server.clone()).or_default();
        self.clients.insert(
            conn,
            ClientConn {
                server: server.clone(),
                window,
                pane,
                cols,
                last_pushed: None,
            },
        );

        let mut pushes = Vec::new();
        let parents = env.ppid_map();
        self.poll_server(env, &server, &parents, &mut pushes);
        if let Some(w) = self.servers.get(&server).and_then(|s| s.width) {
            pushes.push((conn, ServerMsg::Width { cols: w }));
        }
        pushes
    }

    /// Apply a client input event and return the resulting pushes.
    pub fn on_input<E: TmuxEnv>(
        &mut self,
        env: &E,
        conn: ConnId,
        event: InputEvent,
    ) -> Vec<(ConnId, ServerMsg)> {
        let Some(client) = self.clients.get(&conn) else {
            return Vec::new();
        };
        let server = client.server.clone();
        let mut pushes = Vec::new();

        match event {
            InputEvent::Select { key } => {
                if let Some(s) = self.servers.get_mut(&server) {
                    s.selection = Some(key);
                }
                let parents = env.ppid_map();
                self.poll_server(env, &server, &parents, &mut pushes);
            }
            InputEvent::Activate { key } => {
                self.activate(env, &server, &key);
                if let Some(s) = self.servers.get_mut(&server) {
                    s.selection = Some(key.clone());
                }
                // Opening one notification dismisses it and the entries sharing
                // its pane, because the jump answers all of them at once; the
                // rest are calls from elsewhere and stay. The selection then
                // names a row that is gone, so the repoll below falls it back
                // into the tree — onto the window just jumped to.
                if let RowKey::Notification { session } = &key {
                    self.dismiss_notification(&server, session);
                }
                let parents = env.ppid_map();
                self.poll_server(env, &server, &parents, &mut pushes);
            }
            InputEvent::Resize { cols } => {
                // A sidebar alone in its window is expanding to fill the window,
                // not a user width choice: tell it to exit and do not adopt or
                // relay its width, so the other sidebars are not dragged wide.
                let window = self.clients.get(&conn).map(|c| c.window.clone());
                let alone = window
                    .map(|w| env.window_real_panes(&server.0, &w.0).is_empty())
                    .unwrap_or(false);
                if alone {
                    pushes.push((conn, ServerMsg::Exit));
                } else {
                    if let Some(s) = self.servers.get_mut(&server) {
                        s.width = Some(cols);
                    }
                    if let Some(c) = self.clients.get_mut(&conn) {
                        c.cols = cols;
                    }
                    let others: Vec<ConnId> = self
                        .clients
                        .iter()
                        .filter(|(id, c)| **id != conn && c.server == server)
                        .map(|(id, _)| *id)
                        .collect();
                    for other in others {
                        pushes.push((other, ServerMsg::Width { cols }));
                    }
                }
            }
            // A terminal focus change is a wake only: the selection bar's owner is
            // recomputed from tmux, never taken from the client's focus report.
            InputEvent::FocusGained | InputEvent::FocusLost => {
                let parents = env.ppid_map();
                self.poll_server(env, &server, &parents, &mut pushes);
            }
        }
        pushes
    }

    /// Focus the tmux target a selected row names: a window row selects the
    /// window; a pane, agent or notification row selects that pane within its
    /// window. A notification names its pane through the entry the daemon holds,
    /// which the poll keeps current.
    fn activate<E: TmuxEnv>(&self, env: &E, server: &ServerKey, key: &RowKey) {
        let fetch = env.fetch_windows(&server.0);
        let pane = match key {
            RowKey::Window { window } | RowKey::AgentWindow { window, .. } => {
                env.focus(&server.0, &window.0, None);
                return;
            }
            RowKey::Pane { pane } | RowKey::Agent { pane, .. } => pane.clone(),
            RowKey::Notification { session } => {
                let Some(n) = self
                    .servers
                    .get(server)
                    .and_then(|s| s.notifications.iter().find(|n| &n.session == session))
                else {
                    return;
                };
                n.pane.clone()
            }
        };
        if let Some(window) = fetch.pane_to_window.get(&pane) {
            env.focus(&server.0, &window.0, Some(&pane.0));
        }
    }

    /// Drop a client. When its server has no clients left, forget that server's
    /// shared UI state; the next `Hello` recreates it.
    pub fn on_disconnect(&mut self, conn: ConnId) {
        if let Some(client) = self.clients.shift_remove(&conn) {
            let still_used = self.clients.values().any(|c| c.server == client.server);
            if !still_used {
                self.servers.shift_remove(&client.server);
            }
        }
    }

    /// Poll one server: read its windows, associate sessions, signal attention,
    /// build the rows, and append a `Render` for each of its clients whose model
    /// changed. Pruned registry entries are removed. An empty window read (a
    /// transient failure or a momentarily unreachable server) is skipped without
    /// touching state.
    pub fn poll_server<E: TmuxEnv>(
        &mut self,
        env: &E,
        server: &ServerKey,
        parents: &HashMap<u32, u32>,
        pushes: &mut Vec<(ConnId, ServerMsg)>,
    ) {
        let fetch = env.fetch_windows(&server.0);
        if fetch.windows.is_empty() {
            return;
        }

        let hook_on = opt_enabled_default_on(&env.option(&server.0, "@wrangler-hook-progress"));
        let osc_on = opt_enabled_default_off(&env.option(&server.0, "@wrangler-osc-progress"));
        let bell_on = opt_enabled_default_off(&env.option(&server.0, "@wrangler-bell"));
        let notify_mode = osc_notify_mode(&env.option(&server.0, "@wrangler-osc-notify"));
        let notif_on = opt_enabled_default_on(&env.option(&server.0, "@wrangler-notifications"));
        let label_mode = label_mode_from(&env.option(&server.0, "@wrangler-label"));
        let view_mode = view_mode_from(&env.option(&server.0, "@wrangler-sections"));

        // This server's records plus every pane-less one, in sorted-key order so
        // candidate order and title-collision tiebreaks are deterministic.
        let mut slice_keys: Vec<crate::model::SessionKey> = self
            .registry
            .iter()
            .filter(|(_, e)| e.server.as_ref() == Some(server) || e.server.is_none())
            .map(|(k, _)| k.clone())
            .collect();
        slice_keys.sort_by(|a, b| a.0.cmp(&b.0));
        let slice: Vec<RegistrySession> = slice_keys
            .iter()
            .map(|k| {
                let e = &self.registry[k];
                RegistrySession {
                    record: e.record.clone(),
                    status: e.status,
                    attention_token: e.attention_token,
                }
            })
            .collect();

        let all_panes: IndexSet<PaneId> = fetch.pane_to_window.keys().cloned().collect();
        let (sessions, prune) = fetch_agent_sessions(
            &slice,
            &fetch.windows,
            &all_panes,
            &fetch.pane_paths,
            &fetch.pane_pids,
            parents,
            &mut self.labels,
            label_mode,
        );
        for key in &prune {
            if self.registry.shift_remove(key).is_some() {
                self.dirty_registry = true;
            }
        }

        self.signal_attention(
            env,
            server,
            &fetch.windows,
            &sessions,
            bell_on,
            notify_mode,
            notif_on,
        );
        self.refresh_notifications(server, &fetch.windows, &sessions, notif_on);

        // Clear attention for a session whose pane is now the focused one, so the
        // dot goes on the next poll (this poll's rows still show it, matching the
        // marker's one-tick persistence).
        let focused_panes: IndexSet<PaneId> = fetch
            .windows
            .iter()
            .filter(|w| w.active)
            .flat_map(|w| w.panes.iter().filter(|p| p.active).map(|p| p.id.clone()))
            .collect();
        for id in acknowledge_focused_attention(&sessions, &focused_panes) {
            if let Some(e) = self.registry.get_mut(&id) {
                e.attention_token = None;
            }
        }
        self.clear_focused_notifications(server, &focused_panes);

        let pane_status = pane_status_map(&sessions);
        let tree = build_tree(
            &fetch.windows,
            &sessions,
            &fetch.pane_progress,
            &pane_status,
            hook_on,
            osc_on,
            view_mode,
        );

        let notifications = self
            .servers
            .get(server)
            .map(|s| notification_nodes(&s.notifications))
            .unwrap_or_default();
        let current = self.servers.get(server).and_then(|s| s.selection.clone());
        let selection = resolve_selection(current.as_ref(), &tree, &notifications);
        if let Some(s) = self.servers.get_mut(server) {
            s.selection = selection.clone();
        }

        let clients: Vec<ConnId> = self
            .clients
            .iter()
            .filter(|(_, c)| &c.server == server)
            .map(|(id, _)| *id)
            .collect();
        for conn in clients {
            let window = self.clients[&conn].window.clone();
            // A sidebar must never sit alone: when its window has lost every real
            // pane, tell it to quit so its pane closes and tmux closes the window.
            if window_has_no_real_panes(&window, &fetch.windows) {
                pushes.push((conn, ServerMsg::Exit));
                continue;
            }
            let has_focus = window_focus(&window, &fetch.windows);
            let model = RowModel {
                tree: tree.clone(),
                notifications: notifications.clone(),
                selection: selection.clone(),
                has_focus,
            };
            let client = self.clients.get_mut(&conn).expect("id from this server");
            if client.last_pushed.as_ref() != Some(&model) {
                client.last_pushed = Some(model.clone());
                pushes.push((conn, ServerMsg::Render(model)));
            }
        }
    }

    /// Ring the bell, raise the desktop notification and record the notification
    /// area's entry, once per attention event. One representative placement per
    /// session is chosen, preferring the one under the active window (whose name
    /// titles the notification); the dedup is the notifier's per-session token
    /// check, shared by all three sinks so they can never disagree about what
    /// fired.
    #[allow(clippy::too_many_arguments)]
    fn signal_attention<E: TmuxEnv>(
        &mut self,
        env: &E,
        server: &ServerKey,
        windows: &[Window],
        sessions: &[Session],
        bell_on: bool,
        notify_mode: Option<OscNotify>,
        notif_on: bool,
    ) {
        if !bell_on && notify_mode.is_none() && !notif_on {
            return;
        }
        let active_windows: IndexSet<WindowId> = windows
            .iter()
            .filter(|w| w.active)
            .map(|w| w.id.clone())
            .collect();

        // Representative index per session id, preferring an active-window placement.
        let mut rep: IndexMap<crate::model::SessionKey, usize> = IndexMap::new();
        for (i, s) in sessions.iter().enumerate() {
            let has_token = self
                .registry
                .get(&s.id)
                .and_then(|e| e.attention_token)
                .is_some();
            if !has_token {
                continue;
            }
            match rep.get(&s.id) {
                None => {
                    rep.insert(s.id.clone(), i);
                }
                Some(&j) => {
                    if active_windows.contains(&s.window)
                        && !active_windows.contains(&sessions[j].window)
                    {
                        rep.insert(s.id.clone(), i);
                    }
                }
            }
        }

        for (sid, &i) in &rep {
            let Some(token) = self.registry.get(sid).and_then(|e| e.attention_token) else {
                continue;
            };
            if !self.notifier.should_fire(sid, token) {
                continue;
            }
            let s = &sessions[i];
            // The one message this event carries, however it is delivered.
            let text = notification_text(&window_name(windows, &s.window), &s.label);
            if let Some(mode) = notify_mode {
                let escape = osc_escape(mode, &s.agent, &text);
                for tty in env.client_ttys(&server.0, &s.pane.0) {
                    env.write_tty(&tty, &escape);
                }
            }
            if bell_on && !s.pane.0.is_empty() {
                let tty = env.pane_tty(&server.0, &s.pane.0);
                env.write_tty(&tty, "\x07");
            }
            if notif_on {
                self.push_notification(server, s, text);
            }
        }
    }

    /// Record an attention event in this server's notification area: newest
    /// first, one entry per session (a session that fires again moves back to the
    /// top rather than filling the area with itself), and never more than
    /// [`NOTIFICATION_LIMIT`].
    fn push_notification(&mut self, server: &ServerKey, session: &Session, body: String) {
        let Some(state) = self.servers.get_mut(server) else {
            return;
        };
        state.notifications.retain(|n| n.session != session.id);
        state.notifications.insert(
            0,
            Notification {
                session: session.id.clone(),
                pane: session.pane.clone(),
                agent: session.agent.clone(),
                body,
                color: session.color,
            },
        );
        state.notifications.truncate(NOTIFICATION_LIMIT);
    }

    /// Bring this server's notification area back in line with the sessions just
    /// placed: an entry whose session is displayed nowhere is dropped, and a
    /// surviving one re-reads the pane, message and color of the placement it
    /// names so opening it lands where the agent is now and it reads as the
    /// session reads now. Turning the area off empties it, so switching it back
    /// on does not resurrect stale entries.
    fn refresh_notifications(
        &mut self,
        server: &ServerKey,
        windows: &[Window],
        sessions: &[Session],
        on: bool,
    ) {
        let Some(state) = self.servers.get_mut(server) else {
            return;
        };
        if !on {
            state.notifications.clear();
            return;
        }
        state
            .notifications
            .retain_mut(|n| match sessions.iter().find(|s| s.id == n.session) {
                Some(s) => {
                    n.pane = s.pane.clone();
                    n.agent = s.agent.clone();
                    n.body = notification_text(&window_name(windows, &s.window), &s.label);
                    n.color = s.color;
                    true
                }
                None => false,
            });
    }

    /// Drop the notification area's entry for `session` and every entry naming
    /// the same pane: opening one focuses that pane, which answers every call
    /// coming from it just as focusing the pane by any other route would. An
    /// entry for another pane is a call you have not been to yet, so it stays.
    fn dismiss_notification(&mut self, server: &ServerKey, session: &SessionKey) {
        let Some(state) = self.servers.get_mut(server) else {
            return;
        };
        let Some(pane) = state
            .notifications
            .iter()
            .find(|n| &n.session == session)
            .map(|n| n.pane.clone())
        else {
            return;
        };
        state.notifications.retain(|n| n.pane != pane);
    }

    /// Drop the notification area's entries for every pane in `focused`: you are
    /// looking at the agent, so its call has been answered. This is what clears
    /// the `●` too, so the two go together, and an event raised by a pane you are
    /// already sitting in never reaches the area at all.
    fn clear_focused_notifications(&mut self, server: &ServerKey, focused: &IndexSet<PaneId>) {
        if let Some(state) = self.servers.get_mut(server) {
            state.notifications.retain(|n| !focused.contains(&n.pane));
        }
    }
}

/// The name of the window `id` names, empty when that window is gone.
fn window_name(windows: &[Window], id: &WindowId) -> String {
    windows
        .iter()
        .find(|w| &w.id == id)
        .map(|w| w.name.clone())
        .unwrap_or_default()
}

/// The notification area's entries as the client draws them: the row key it
/// echoes back, and the label and color naming the agent that raised each.
fn notification_nodes(notifications: &[Notification]) -> Vec<NotificationNode> {
    notifications
        .iter()
        .map(|n| NotificationNode {
            id: RowKey::Notification {
                session: n.session.clone(),
            },
            title: n.agent.clone(),
            body: n.body.clone(),
            color: n.color,
        })
        .collect()
}

/// Mirror each session's turn state onto the pane it occupies, so the window-tree
/// pane draws the agent's glyph. Attention wins over working; idle contributes
/// nothing.
fn pane_status_map(sessions: &[Session]) -> IndexMap<PaneId, String> {
    let mut map: IndexMap<PaneId, String> = IndexMap::new();
    for s in sessions {
        if s.pane.0.is_empty() {
            continue;
        }
        let promote = s.status == TurnStatus::Attention
            || (s.status == TurnStatus::Working
                && map.get(&s.pane).map(|c| c != "attention").unwrap_or(true));
        if promote {
            map.insert(s.pane.clone(), status_str(s.status).to_string());
        }
    }
    map
}

/// The production [`TmuxEnv`], issuing real tmux/ps/tty operations.
pub struct RealTmux;

impl TmuxEnv for RealTmux {
    fn fetch_windows(&self, socket: &str) -> crate::tmux::FetchResult {
        crate::tmux::fetch_windows(socket)
    }
    fn window_real_panes(&self, socket: &str, window: &str) -> Vec<PaneId> {
        crate::tmux::window_real_panes(socket, window)
    }
    fn ppid_map(&self) -> HashMap<u32, u32> {
        crate::daemon::assoc::ppid_map()
    }
    fn option(&self, socket: &str, name: &str) -> String {
        crate::tmux::run_tmux(socket, &["show-option", "-gqv", name])
    }
    fn focus(&self, socket: &str, window: &str, pane: Option<&str>) {
        crate::tmux::focus(socket, window, pane);
    }
    fn toggle(&self, socket: &str) {
        crate::tmux::toggle(socket);
    }
    fn focus_key(&self, socket: &str) {
        crate::tmux::focus_key(socket);
    }
    fn pane_tty(&self, socket: &str, pane: &str) -> String {
        crate::tmux::run_tmux(
            socket,
            &["display-message", "-p", "-t", pane, "#{pane_tty}"],
        )
        .trim()
        .to_string()
    }
    fn client_ttys(&self, socket: &str, pane: &str) -> Vec<String> {
        let session = crate::tmux::run_tmux(
            socket,
            &["display-message", "-p", "-t", pane, "#{session_name}"],
        );
        let session = session.trim();
        if session.is_empty() {
            return Vec::new();
        }
        crate::tmux::run_tmux(
            socket,
            &["list-clients", "-t", session, "-F", "#{client_tty}"],
        )
        .lines()
        .filter(|t| !t.trim().is_empty())
        .map(|t| t.to_string())
        .collect()
    }
    fn write_tty(&self, path: &str, data: &str) {
        crate::daemon::notify::write_tty(path, data);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::render::row_text;
    use crate::proto::{HookAction, InputEvent};
    use crate::tmux::{parse_windows, FetchResult};
    use std::cell::RefCell;

    /// The lines a pushed tree draws. The layout assertions read as the sidebar
    /// looks, which is what makes a grouping change visible in them.
    fn drawn(tree: &RowTree) -> Vec<String> {
        tree.flatten()
            .iter()
            .map(|r| row_text(&r.content))
            .collect()
    }

    /// A fake environment: canned window/option/ancestry reads, and recorded
    /// effects. `pane_tty` and `client_ttys` are synthesized from their inputs so
    /// a test can assert which tty a signal reached.
    #[derive(Default)]
    struct FakeTmux {
        fetch: HashMap<String, FetchResult>,
        options: HashMap<(String, String), String>,
        parents: HashMap<u32, u32>,
        writes: RefCell<Vec<(String, String)>>,
        focuses: RefCell<Vec<(String, Option<String>)>>,
    }

    impl FakeTmux {
        fn with_windows(mut self, socket: &str, windows_out: &str, panes_out: &str) -> Self {
            self.fetch
                .insert(socket.to_string(), parse_windows(windows_out, panes_out));
            self
        }
        fn with_option(mut self, socket: &str, name: &str, value: &str) -> Self {
            self.options
                .insert((socket.to_string(), name.to_string()), value.to_string());
            self
        }
        fn with_parent(mut self, pid: u32, ppid: u32) -> Self {
            self.parents.insert(pid, ppid);
            self
        }
    }

    impl TmuxEnv for FakeTmux {
        fn fetch_windows(&self, socket: &str) -> FetchResult {
            self.fetch.get(socket).cloned().unwrap_or_default()
        }
        fn window_real_panes(&self, socket: &str, window: &str) -> Vec<PaneId> {
            self.fetch
                .get(socket)
                .and_then(|f| f.windows.iter().find(|w| w.id.0 == window))
                .map(|w| w.panes.iter().map(|p| p.id.clone()).collect())
                .unwrap_or_default()
        }
        fn ppid_map(&self) -> HashMap<u32, u32> {
            self.parents.clone()
        }
        fn option(&self, socket: &str, name: &str) -> String {
            self.options
                .get(&(socket.to_string(), name.to_string()))
                .cloned()
                .unwrap_or_default()
        }
        fn focus(&self, _socket: &str, window: &str, pane: Option<&str>) {
            self.focuses
                .borrow_mut()
                .push((window.to_string(), pane.map(str::to_string)));
        }
        fn toggle(&self, _socket: &str) {}
        fn focus_key(&self, _socket: &str) {}
        fn pane_tty(&self, _socket: &str, pane: &str) -> String {
            format!("/tty/pane/{pane}")
        }
        fn client_ttys(&self, _socket: &str, _pane: &str) -> Vec<String> {
            vec!["/tty/client".to_string()]
        }
        fn write_tty(&self, path: &str, data: &str) {
            self.writes
                .borrow_mut()
                .push((path.to_string(), data.to_string()));
        }
    }

    fn key(s: &str) -> crate::model::SessionKey {
        crate::model::SessionKey(s.to_string())
    }

    /// A one-window server whose real pane `%1` is titled `MySession` and whose
    /// top-level process is pid 2. `active` sets whether `%1` (not the sidebar) is
    /// the active pane.
    fn one_window(active: bool) -> (&'static str, String, String) {
        let a = if active { "1" } else { "0" };
        let windows = "@1\t0\tmain\t1".to_string();
        let panes = format!("@1\t%1\t0\t{a}\t\t\t\t2\t/home/x\tMySession");
        ("/s", windows, panes)
    }

    /// Register claude session `session_id` occupying `%1` via its recorded pid
    /// descending from the pane's process (pid 2), so it is placed there. A pane
    /// can host several, which is how a test raises events for more than one.
    fn register_session(
        state: &mut State,
        session_id: &str,
        event: HookAction,
        token: i128,
    ) -> Vec<ServerKey> {
        let self_pid = std::process::id();
        state.on_hook(
            Some(ServerKey("/s".into())),
            Some(PaneId("%1".into())),
            "claude".into(),
            event,
            session_id.into(),
            "/home/x".into(),
            String::new(),
            Some(self_pid),
            token,
        )
    }

    fn register_occupying(state: &mut State, event: HookAction, token: i128) -> Vec<ServerKey> {
        register_session(state, "abc", event, token)
    }

    /// Run one poll of the sample server, discarding the pushes.
    fn poll(state: &mut State, env: &FakeTmux) {
        let parents = env.ppid_map();
        let mut pushes = Vec::new();
        state.poll_server(env, &ServerKey("/s".into()), &parents, &mut pushes);
    }

    /// The session keys held in the sample server's notification area, in the
    /// order they are drawn.
    fn notified(state: &State) -> Vec<crate::model::SessionKey> {
        state.servers[&ServerKey("/s".into())]
            .notifications
            .iter()
            .map(|n| n.session.clone())
            .collect()
    }

    fn hello_client(state: &mut State, env: &FakeTmux, conn: u64) -> Vec<(ConnId, ServerMsg)> {
        state.on_hello(
            env,
            ConnId(conn),
            ServerKey("/s".into()),
            WindowId("@1".into()),
            PaneId("%9".into()),
            30,
        )
    }

    fn env_for(active: bool) -> FakeTmux {
        let (socket, windows, panes) = one_window(active);
        FakeTmux::default()
            .with_windows(socket, &windows, &panes)
            .with_parent(std::process::id(), 2)
    }

    fn render_of(pushes: &[(ConnId, ServerMsg)], conn: u64) -> Option<RowModel> {
        pushes.iter().find_map(|(c, m)| match m {
            ServerMsg::Render(model) if *c == ConnId(conn) => Some(model.clone()),
            _ => None,
        })
    }

    #[test]
    fn hook_registers_and_poll_places_session() {
        let env = env_for(true);
        let mut state = State::new();
        register_occupying(&mut state, HookAction::Start, 0);
        assert!(state.registry.contains_key(&key("claude-abc")));

        let pushes = hello_client(&mut state, &env, 0);
        let model = render_of(&pushes, 0).expect("client gets an initial render");
        assert!(
            model
                .tree
                .flatten()
                .iter()
                .any(|r| matches!(r.content, RowContent::Agent { .. })),
            "an agent row is present"
        );
    }

    #[test]
    fn default_layout_is_unified() {
        let env = env_for(true);
        let mut state = State::new();
        register_occupying(&mut state, HookAction::Start, 0);
        let model = render_of(&hello_client(&mut state, &env, 0), 0).unwrap();

        // One window list: the agent's pane is drawn as the agent, and there is
        // no heading or repeated agent block.
        let rows = model.tree.flatten();
        // The label, not the pane title: this record carries no title, so the
        // name-mode label falls back to the cwd basename.
        assert_eq!(
            drawn(&model.tree),
            vec!["▌ 0: main", "▌ └─ 0: \u{f167a}  x"]
        );
        assert!(matches!(rows[1].content, RowContent::Agent { .. }));
    }

    #[test]
    fn sections_option_groups_the_rows_under_headings() {
        let env = env_for(true).with_option("/s", "@wrangler-sections", "on");
        let mut state = State::new();
        register_occupying(&mut state, HookAction::Start, 0);
        let model = render_of(&hello_client(&mut state, &env, 0), 0).unwrap();

        assert_eq!(
            drawn(&model.tree),
            vec![
                " WINDOWS",
                "",
                "▌ 0: main",
                "▌ └─ 0: \u{f489}  MySession",
                "",
                " CLAUDE",
                "",
                "▌ 0: main",
                "▌ └─ 0: \u{f167a}  x",
            ]
        );
    }

    #[test]
    fn push_diff_suppresses_an_unchanged_repoll() {
        let env = env_for(true);
        let mut state = State::new();
        register_occupying(&mut state, HookAction::Start, 0);
        hello_client(&mut state, &env, 0);

        // A second poll with no change pushes nothing.
        let mut pushes = Vec::new();
        let parents = env.ppid_map();
        state.poll_server(&env, &ServerKey("/s".into()), &parents, &mut pushes);
        assert!(pushes.is_empty());
    }

    #[test]
    fn selection_defaults_to_the_active_window_row() {
        let env = env_for(true);
        let mut state = State::new();
        let pushes = hello_client(&mut state, &env, 0);
        let model = render_of(&pushes, 0).unwrap();
        assert_eq!(
            model.selection,
            Some(RowKey::Window {
                window: WindowId("@1".into())
            })
        );
    }

    #[test]
    fn has_focus_only_when_the_sidebar_is_the_active_pane() {
        // %1 active -> the sidebar is not focused.
        let mut state = State::new();
        let env = env_for(true);
        let model = render_of(&hello_client(&mut state, &env, 0), 0).unwrap();
        assert!(!model.has_focus);

        // %1 inactive (with the window active) -> the active pane is the sidebar.
        let mut state = State::new();
        let env = env_for(false);
        let model = render_of(&hello_client(&mut state, &env, 0), 0).unwrap();
        assert!(model.has_focus);
    }

    #[test]
    fn attention_event_bells_once_per_token() {
        let env = env_for(true).with_option("/s", "@wrangler-bell", "on");
        let mut state = State::new();
        let servers = register_occupying(&mut state, HookAction::NeedsAttention, 100);
        assert_eq!(servers, vec![ServerKey("/s".into())]);
        hello_client(&mut state, &env, 0);

        // The hello poll rang the bell to %1's tty exactly once.
        let bells: Vec<_> = env
            .writes
            .borrow()
            .iter()
            .filter(|(_, d)| d == "\x07")
            .cloned()
            .collect();
        assert_eq!(
            bells,
            vec![("/tty/pane/%1".to_string(), "\x07".to_string())]
        );

        // A repoll with the same token does not re-ring.
        let mut pushes = Vec::new();
        let parents = env.ppid_map();
        state.poll_server(&env, &ServerKey("/s".into()), &parents, &mut pushes);
        let bell_count = env
            .writes
            .borrow()
            .iter()
            .filter(|(_, d)| d == "\x07")
            .count();
        assert_eq!(bell_count, 1);
    }

    #[test]
    fn an_attention_event_reaches_the_notification_area() {
        // No bell and no osc-notify option: the area is driven by the event
        // itself, not by the desktop notification being switched on. The agent's
        // pane is not the focused one, or the event would be answered on
        // arrival.
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &env);

        assert_eq!(notified(&state), vec![key("claude-abc")]);
        let model = state.clients[&ConnId(0)].last_pushed.clone().unwrap();
        assert_eq!(
            model.notifications[0].id,
            RowKey::Notification {
                session: key("claude-abc")
            }
        );
        // The agent titles the entry and the window and label describe it, which
        // is what the desktop notification would have said.
        assert_eq!(model.notifications[0].title, "claude");
        assert_eq!(model.notifications[0].body, "main · x");
    }

    #[test]
    fn the_area_holds_the_three_newest_events_newest_first() {
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        for (i, session) in ["a", "b", "c", "d"].iter().enumerate() {
            register_session(&mut state, session, HookAction::NeedsAttention, i as i128);
            poll(&mut state, &env);
        }
        assert_eq!(
            notified(&state),
            vec![key("claude-d"), key("claude-c"), key("claude-b")],
            "the oldest event is pushed out"
        );
    }

    #[test]
    fn a_session_that_fires_again_moves_up_instead_of_repeating() {
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_session(&mut state, "a", HookAction::NeedsAttention, 1);
        poll(&mut state, &env);
        register_session(&mut state, "b", HookAction::NeedsAttention, 2);
        poll(&mut state, &env);
        register_session(&mut state, "a", HookAction::NeedsAttention, 3);
        poll(&mut state, &env);

        assert_eq!(notified(&state), vec![key("claude-a"), key("claude-b")]);
    }

    #[test]
    fn opening_a_notification_focuses_its_pane_and_clears_that_pane() {
        // Both sessions are in %1, so jumping there answers both calls.
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        register_session(&mut state, "sib", HookAction::NeedsAttention, 101);
        poll(&mut state, &env);
        assert_eq!(notified(&state).len(), 2);

        state.on_input(
            &env,
            ConnId(0),
            InputEvent::Activate {
                key: RowKey::Notification {
                    session: key("claude-abc"),
                },
            },
        );
        assert_eq!(
            *env.focuses.borrow(),
            vec![("@1".to_string(), Some("%1".to_string()))]
        );
        assert!(
            notified(&state).is_empty(),
            "both entries name the pane just jumped to"
        );
        // The selection named a row that is gone, so it falls back into the tree.
        assert_eq!(
            state.servers[&ServerKey("/s".into())].selection,
            Some(RowKey::Window {
                window: WindowId("@1".into())
            })
        );
    }

    #[test]
    fn opening_a_notification_leaves_the_entries_for_other_panes() {
        // Two unfocused panes, each with an agent calling: opening one answers
        // that pane only, and the other pane's call is still waiting.
        let panes = [
            "@1\t%1\t0\t0\t\t\t\t2\t/home/x\tMySession",
            "@1\t%2\t1\t0\t\t\t\t3\t/home/y\tOther",
        ]
        .join("\n");
        // The second agent needs a pid that is really running, or it is pruned as
        // dead before it can be placed; the fake ps map hangs it off %2's pane
        // process so it occupies that pane.
        let mut agent = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let env = FakeTmux::default()
            .with_windows("/s", "@1\t0\tmain\t1", &panes)
            .with_parent(std::process::id(), 2)
            .with_parent(agent.id(), 3);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        state.on_hook(
            Some(ServerKey("/s".into())),
            Some(PaneId("%2".into())),
            "claude".into(),
            HookAction::NeedsAttention,
            "def".into(),
            "/home/y".into(),
            String::new(),
            Some(agent.id()),
            101,
        );
        poll(&mut state, &env);
        assert_eq!(notified(&state), vec![key("claude-def"), key("claude-abc")]);

        state.on_input(
            &env,
            ConnId(0),
            InputEvent::Activate {
                key: RowKey::Notification {
                    session: key("claude-abc"),
                },
            },
        );
        assert_eq!(notified(&state), vec![key("claude-def")]);

        agent.kill().expect("kill sleep");
        agent.wait().expect("reap sleep");
    }

    #[test]
    fn an_entry_stays_while_its_pane_is_unfocused() {
        // Nothing but opening it or a newer event displaces an entry: polls on
        // their own leave it alone.
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &env);
        poll(&mut state, &env);

        assert_eq!(notified(&state), vec![key("claude-abc")]);
    }

    #[test]
    fn focusing_a_pane_clears_its_entries() {
        let unfocused = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &unfocused, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &unfocused);
        assert_eq!(notified(&state), vec![key("claude-abc")]);

        // The same server with %1 now the focused pane: you are looking at the
        // agent, so its call is answered — the `●` and the entry go together.
        let focused = env_for(true);
        poll(&mut state, &focused);
        assert_eq!(state.registry[&key("claude-abc")].attention_token, None);
        assert!(notified(&state).is_empty());
    }

    #[test]
    fn an_event_from_the_pane_you_are_in_never_reaches_the_area() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &env);

        assert!(
            notified(&state).is_empty(),
            "there is nothing to tell you about the pane you are sitting in"
        );
    }

    #[test]
    fn a_notification_whose_session_is_gone_is_dropped() {
        let env = env_for(false);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &env);
        assert_eq!(notified(&state), vec![key("claude-abc")]);

        register_occupying(&mut state, HookAction::End, 0);
        poll(&mut state, &env);
        assert!(
            notified(&state).is_empty(),
            "an entry that could no longer be opened is not kept"
        );
    }

    #[test]
    fn the_area_is_empty_while_the_option_is_off() {
        let env = env_for(false).with_option("/s", "@wrangler-notifications", "off");
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        register_occupying(&mut state, HookAction::NeedsAttention, 100);
        poll(&mut state, &env);

        assert!(notified(&state).is_empty());
        let model = state.clients[&ConnId(0)].last_pushed.clone().unwrap();
        assert!(model.notifications.is_empty());
    }

    #[test]
    fn end_event_removes_the_registry_entry() {
        let mut state = State::new();
        register_occupying(&mut state, HookAction::Start, 0);
        assert!(state.registry.contains_key(&key("claude-abc")));
        register_occupying(&mut state, HookAction::End, 0);
        assert!(!state.registry.contains_key(&key("claude-abc")));
        assert!(state.dirty_registry);
    }

    #[test]
    fn resize_relays_shared_width_to_other_clients_only() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        hello_client(&mut state, &env, 1);

        let pushes = state.on_input(&env, ConnId(0), InputEvent::Resize { cols: 44 });
        // Only conn 1 is told to adopt the width; the resizing client is not.
        assert_eq!(pushes, vec![(ConnId(1), ServerMsg::Width { cols: 44 })]);
        assert_eq!(state.servers[&ServerKey("/s".into())].width, Some(44));
    }

    #[test]
    fn resize_from_an_alone_sidebar_exits_and_does_not_relay() {
        // A window with a real pane (%1) plus a second window's client sharing the
        // server. When the sidebar of an emptied window reports a resize, it must
        // be told to exit and its width must not reach the other client.
        let socket = "/s";
        let windows = "@1\t0\tmain\t1\n@2\t1\tside\t0".to_string();
        // @1 has a real pane; @2 has none (its sidebar is alone).
        let panes = "@1\t%1\t0\t1\t\t\t\t2\t/home/x\tt".to_string();
        let env = FakeTmux::default().with_windows(socket, &windows, &panes);
        let mut state = State::new();
        // conn 0 is the healthy window @1; conn 1 is the emptied window @2.
        state.on_hello(
            &env,
            ConnId(0),
            ServerKey("/s".into()),
            WindowId("@1".into()),
            PaneId("%8".into()),
            30,
        );
        state.on_hello(
            &env,
            ConnId(1),
            ServerKey("/s".into()),
            WindowId("@2".into()),
            PaneId("%9".into()),
            30,
        );

        let pushes = state.on_input(&env, ConnId(1), InputEvent::Resize { cols: 200 });
        assert_eq!(pushes, vec![(ConnId(1), ServerMsg::Exit)]);
        // No width was adopted, so the healthy client is never dragged wide.
        assert_eq!(state.servers[&ServerKey("/s".into())].width, None);
        assert!(!pushes
            .iter()
            .any(|(_, m)| matches!(m, ServerMsg::Width { .. })));
    }

    #[test]
    fn new_client_adopts_the_servers_width() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        state.on_input(&env, ConnId(0), InputEvent::Resize { cols: 44 });

        let pushes = hello_client(&mut state, &env, 2);
        assert!(pushes
            .iter()
            .any(|(c, m)| *c == ConnId(2) && *m == ServerMsg::Width { cols: 44 }));
    }

    #[test]
    fn activate_focuses_the_windows_pane() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        state.on_input(
            &env,
            ConnId(0),
            InputEvent::Activate {
                key: RowKey::Pane {
                    pane: PaneId("%1".into()),
                },
            },
        );
        assert_eq!(
            *env.focuses.borrow(),
            vec![("@1".to_string(), Some("%1".to_string()))]
        );
    }

    #[test]
    fn sidebar_alone_in_its_window_is_told_to_exit() {
        // A window that lists no real panes (only the sidebar) yields an Exit for
        // its client instead of a Render.
        let socket = "/s";
        let windows = "@1\t0\tmain\t1".to_string();
        // No pane lines: window @1 has no real panes.
        let env = FakeTmux::default().with_windows(socket, &windows, "");
        let mut state = State::new();
        let pushes = hello_client(&mut state, &env, 0);
        assert_eq!(pushes.first().map(|(_, m)| m), Some(&ServerMsg::Exit));
        assert!(
            !pushes
                .iter()
                .any(|(_, m)| matches!(m, ServerMsg::Render(_))),
            "an alone sidebar gets no render"
        );
    }

    #[test]
    fn disconnect_drops_server_state_when_last_client_leaves() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        assert!(state.servers.contains_key(&ServerKey("/s".into())));
        state.on_disconnect(ConnId(0));
        assert!(!state.servers.contains_key(&ServerKey("/s".into())));
    }
}
