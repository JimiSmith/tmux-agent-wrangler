//! Filesystem locations for the daemon socket and shared state.

use std::path::PathBuf;

/// The state directory: `$XDG_STATE_HOME/tmux-agent-wrangler`, or
/// `~/.local/state/tmux-agent-wrangler` when `XDG_STATE_HOME` is unset or empty.
pub fn state_dir() -> PathBuf {
    let base = match std::env::var_os("XDG_STATE_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(".local").join("state")
        }
    };
    base.join("tmux-agent-wrangler")
}

/// The Unix socket the daemon listens on and clients/hooks connect to.
pub fn daemon_socket() -> PathBuf {
    state_dir().join("daemon.sock")
}
