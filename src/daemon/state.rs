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
use crate::daemon::notify::{acknowledge_focused_attention, osc_escape, Notifier, OscNotify};
use crate::daemon::rows::build_rows;
use crate::labels::{label_mode_from, LabelCache};
use crate::model::{
    PaneId, Row, RowKey, RowKind, RowModel, ServerKey, Session, TurnStatus, Window, WindowId,
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

/// Per-server shared UI state: the highlighted row and the column width sidebars
/// on this server keep in sync. Both are absent until a client sets them.
#[derive(Clone, Debug, Default)]
pub struct ServerState {
    pub selection: Option<RowKey>,
    pub width: Option<u16>,
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

/// Resolve the shared selection against the rows just built: keep the current key
/// if it still names a row, else default to the active window's row, else the
/// first selectable row, else `None` when nothing is selectable.
fn resolve_selection(current: Option<&RowKey>, rows: &[Row]) -> Option<RowKey> {
    let first_key = rows.iter().find_map(|r| r.key.as_ref());
    first_key?;
    if let Some(k) = current {
        if rows.iter().any(|r| r.key.as_ref() == Some(k)) {
            return Some(k.clone());
        }
    }
    for r in rows {
        if let (Some(key @ RowKey::Window { .. }), RowKind::Window { active: true, .. }) =
            (&r.key, &r.kind)
        {
            return Some(key.clone());
        }
    }
    first_key.cloned()
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
                if let Some(s) = self.servers.get_mut(&server) {
                    s.selection = Some(key.clone());
                }
                self.activate(env, &server, &key);
                let parents = env.ppid_map();
                self.poll_server(env, &server, &parents, &mut pushes);
            }
            InputEvent::Resize { cols } => {
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
    /// window; a pane or agent row selects that pane within its window.
    fn activate<E: TmuxEnv>(&self, env: &E, server: &ServerKey, key: &RowKey) {
        let fetch = env.fetch_windows(&server.0);
        match key {
            RowKey::Window { window } | RowKey::AgentWindow { window, .. } => {
                env.focus(&server.0, &window.0, None);
            }
            RowKey::Pane { pane } | RowKey::Agent { pane, .. } => {
                if let Some(window) = fetch.pane_to_window.get(pane) {
                    env.focus(&server.0, &window.0, Some(&pane.0));
                }
            }
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
        let label_mode = label_mode_from(&env.option(&server.0, "@wrangler-label"));

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

        self.signal_attention(env, server, &fetch.windows, &sessions, bell_on, notify_mode);

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

        let pane_status = pane_status_map(&sessions);
        let rows = build_rows(
            &fetch.windows,
            &sessions,
            &fetch.pane_progress,
            &pane_status,
            hook_on,
            osc_on,
        );

        let current = self.servers.get(server).and_then(|s| s.selection.clone());
        let selection = resolve_selection(current.as_ref(), &rows);
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
            let has_focus = window_focus(&self.clients[&conn].window, &fetch.windows);
            let model = RowModel {
                rows: rows.clone(),
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

    /// Ring the bell and raise the desktop notification once per attention event.
    /// One representative placement per session is chosen, preferring the one
    /// under the active window (whose name titles the notification); the dedup is
    /// the notifier's per-session token check.
    fn signal_attention<E: TmuxEnv>(
        &mut self,
        env: &E,
        server: &ServerKey,
        windows: &[Window],
        sessions: &[Session],
        bell_on: bool,
        notify_mode: Option<OscNotify>,
    ) {
        if !bell_on && notify_mode.is_none() {
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
            if let Some(mode) = notify_mode {
                let win_name = windows
                    .iter()
                    .find(|w| w.id == s.window)
                    .map(|w| w.name.clone())
                    .unwrap_or_default();
                let text = if s.label.is_empty() {
                    win_name
                } else {
                    format!("{win_name} · {}", s.label)
                };
                let escape = osc_escape(mode, &s.agent, &text);
                for tty in env.client_ttys(&server.0, &s.pane.0) {
                    env.write_tty(&tty, &escape);
                }
            }
            if bell_on && !s.pane.0.is_empty() {
                let tty = env.pane_tty(&server.0, &s.pane.0);
                env.write_tty(&tty, "\x07");
            }
        }
    }
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
    use crate::proto::{HookAction, InputEvent};
    use crate::tmux::{parse_windows, FetchResult};
    use std::cell::RefCell;

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

    /// Register a claude session occupying `%1` via its recorded pid descending
    /// from the pane's process (pid 2), so it is placed there.
    fn register_occupying(state: &mut State, event: HookAction, token: i128) -> Vec<ServerKey> {
        let self_pid = std::process::id();
        state.on_hook(
            Some(ServerKey("/s".into())),
            Some(PaneId("%1".into())),
            "claude".into(),
            event,
            "abc".into(),
            "/home/x".into(),
            String::new(),
            Some(self_pid),
            token,
        )
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
                .rows
                .iter()
                .any(|r| matches!(r.kind, RowKind::Agent { .. })),
            "an agent row is present"
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
    fn disconnect_drops_server_state_when_last_client_leaves() {
        let env = env_for(true);
        let mut state = State::new();
        hello_client(&mut state, &env, 0);
        assert!(state.servers.contains_key(&ServerKey("/s".into())));
        state.on_disconnect(ConnId(0));
        assert!(!state.servers.contains_key(&ServerKey("/s".into())));
    }
}
