//! In-memory attention signalling: the bell and the desktop notification, fired
//! once per attention event.
//!
//! Every attention event carries a monotonic token. [`Notifier`] holds the
//! newest token it has already signalled per session, so an event fires exactly
//! once even though a session can be placed under several windows at once and
//! each placement is examined on every poll. The escape-building is pure; the
//! tty writes are best-effort and take their targets from the caller.

use indexmap::{IndexMap, IndexSet};

use crate::model::{PaneId, Session, SessionKey, TurnStatus};

/// Per-session record of the newest attention token already signalled. Used to
/// suppress a repeat signal for an event that has already fired.
#[derive(Clone, Debug, Default)]
pub struct Notifier {
    fired: IndexMap<SessionKey, i128>,
}

impl Notifier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `token` as signalled for `session_id` and return whether this call
    /// should raise the signal: true only when `token` is strictly greater than
    /// the newest token already recorded for the session (so the first event for
    /// a session always fires, and a repeated or older token never does). A true
    /// return stores the token; a false return leaves the record unchanged.
    pub fn should_fire(&mut self, session_id: &SessionKey, token: i128) -> bool {
        if let Some(&prev) = self.fired.get(session_id) {
            if token <= prev {
                return false;
            }
        }
        self.fired.insert(session_id.clone(), token);
        true
    }

    /// Forget every session absent from `live`, so the record does not grow
    /// without bound and a session id that later reappears starts fresh. Retains
    /// insertion order of the surviving entries.
    pub fn retain_live(&mut self, live: &IndexSet<SessionKey>) {
        self.fired.retain(|id, _| live.contains(id));
    }
}

/// Which desktop-notification escape [`osc_escape`] builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OscNotify {
    /// `ESC]777;notify;<agent>;<text>BEL`: carries the agent name as the title.
    Osc777,
    /// `ESC]9;<text>BEL`: text only, no title.
    Osc9,
}

/// The message an attention event carries: the window the agent is in and the
/// label the sidebar knows it by, or the window alone when it has no label yet.
///
/// One rule for the wording, because the same two strings are both the body of
/// the escape [`osc_escape`] builds and the description the notification area
/// shows: an event says the same thing wherever it is read. Pure.
pub fn notification_text(window_name: &str, label: &str) -> String {
    if label.is_empty() {
        window_name.to_string()
    } else {
        format!("{window_name} · {label}")
    }
}

/// Build the desktop-notification escape for `mode`. `Osc777` embeds `agent` as
/// the notification title and then `text` as the body; `Osc9` carries `text`
/// alone and ignores `agent`. Both terminate with BEL (`\x07`). Pure.
pub fn osc_escape(mode: OscNotify, agent: &str, text: &str) -> String {
    match mode {
        OscNotify::Osc777 => format!("\x1b]777;notify;{agent};{text}\x07"),
        OscNotify::Osc9 => format!("\x1b]9;{text}\x07"),
    }
}

/// Best-effort raw write of `data` to a tty at `path`, opened write-only. An
/// empty `path` and any I/O error are swallowed and reported as `false`, so a
/// failed signal never interrupts the caller. Returns whether the bytes were
/// written.
pub fn write_tty(path: &str, data: &str) -> bool {
    if path.is_empty() {
        return false;
    }
    use std::io::Write;
    match std::fs::OpenOptions::new().write(true).open(path) {
        Ok(mut f) => f.write_all(data.as_bytes()).is_ok(),
        Err(_) => false,
    }
}

/// Write BEL to a pane's tty so the terminal (and tmux's monitor-bell) reacts.
/// Best-effort; an empty tty is a no-op.
pub fn ring_bell(pane_tty: &str) -> bool {
    write_tty(pane_tty, "\x07")
}

/// Write the notification escape to every client tty. Best-effort per tty; one
/// failing tty does not stop the rest.
pub fn send_notification(client_ttys: &[String], escape: &str) {
    for tty in client_ttys {
        write_tty(tty, escape);
    }
}

/// Which focused sessions' attention to clear this pass. A session is cleared
/// when its status is [`TurnStatus::Attention`] and its placement pane is in
/// `focused`; the caller applies the clear (dropping the attention marker and
/// downgrading the row). Each session id is returned at most once even when it
/// is placed under several focused panes, in first-seen order. The caller runs
/// the signalling pass ([`Notifier::should_fire`]) before this, so an event is
/// always signalled before focus clears it.
pub fn acknowledge_focused_attention(
    sessions: &[Session],
    focused: &IndexSet<PaneId>,
) -> Vec<SessionKey> {
    let mut cleared: IndexSet<SessionKey> = IndexSet::new();
    for s in sessions {
        if s.status == TurnStatus::Attention && focused.contains(&s.pane) {
            cleared.insert(s.id.clone());
        }
    }
    cleared.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::WindowId;

    fn key(s: &str) -> SessionKey {
        SessionKey(s.to_string())
    }

    #[test]
    fn should_fire_first_event_fires() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
    }

    #[test]
    fn should_fire_same_token_does_not_refire() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
        assert!(!n.should_fire(&key("claude-a"), 100));
    }

    #[test]
    fn should_fire_older_token_does_not_fire() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
        assert!(!n.should_fire(&key("claude-a"), 50));
    }

    #[test]
    fn should_fire_newer_token_fires() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
        assert!(n.should_fire(&key("claude-a"), 101));
    }

    #[test]
    fn should_fire_is_per_session() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
        // A different session's first event fires regardless of another
        // session's recorded token.
        assert!(n.should_fire(&key("claude-b"), 1));
    }

    #[test]
    fn retain_live_forgets_absent_sessions_and_resets_them() {
        let mut n = Notifier::new();
        assert!(n.should_fire(&key("claude-a"), 100));
        let live: IndexSet<SessionKey> = IndexSet::new();
        n.retain_live(&live);
        // Forgotten, so the same token fires again as a first event.
        assert!(n.should_fire(&key("claude-a"), 100));
    }

    #[test]
    fn notification_text_joins_the_window_and_label() {
        assert_eq!(notification_text("win", "label"), "win · label");
    }

    #[test]
    fn notification_text_of_an_unlabelled_session_is_the_window_alone() {
        assert_eq!(notification_text("win", ""), "win");
    }

    #[test]
    fn osc_escape_777_is_byte_exact() {
        assert_eq!(
            osc_escape(OscNotify::Osc777, "claude", "win · label"),
            "\x1b]777;notify;claude;win · label\x07",
        );
    }

    #[test]
    fn osc_escape_9_is_byte_exact_and_omits_agent() {
        assert_eq!(
            osc_escape(OscNotify::Osc9, "claude", "win · label"),
            "\x1b]9;win · label\x07",
        );
    }

    fn session(id: &str, pane: &str, status: TurnStatus) -> Session {
        Session {
            id: SessionKey(id.to_string()),
            agent: "claude".to_string(),
            pane: PaneId(pane.to_string()),
            window: WindowId("@1".to_string()),
            label: "label".to_string(),
            color: None,
            status,
        }
    }

    fn panes(ids: &[&str]) -> IndexSet<PaneId> {
        ids.iter().map(|p| PaneId(p.to_string())).collect()
    }

    #[test]
    fn acknowledge_clears_focused_attention_only() {
        let sessions = vec![
            session("claude-a", "%1", TurnStatus::Attention),
            session("claude-b", "%2", TurnStatus::Attention),
            session("claude-c", "%3", TurnStatus::Working),
        ];
        let cleared = acknowledge_focused_attention(&sessions, &panes(&["%1", "%3"]));
        // %1 is focused and in attention; %2 is not focused; %3 is focused but
        // working, not in attention.
        assert_eq!(cleared, vec![key("claude-a")]);
    }

    #[test]
    fn acknowledge_dedups_a_session_placed_under_several_focused_panes() {
        let sessions = vec![
            session("claude-a", "%1", TurnStatus::Attention),
            session("claude-a", "%2", TurnStatus::Attention),
        ];
        let cleared = acknowledge_focused_attention(&sessions, &panes(&["%1", "%2"]));
        assert_eq!(cleared, vec![key("claude-a")]);
    }
}
