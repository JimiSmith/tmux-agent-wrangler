//! The tmux glue: the plugin entry point and the key/hook-bound subcommands.
//!
//! `tmux-entry` runs at plugin load to bind keys, install the new-window hooks
//! and the rename guard, optionally install the agent hooks, and start the
//! daemon. `toggle`, `focus`, and `spawn` are bound to keys or tmux hooks and
//! drive the sidebar panes directly (server-side tmux operations); the daemon is
//! reached only for state and rendering, so the glue works even before it is up.

pub mod install;

use std::process::{Command, ExitCode};

use crate::tmux::run_tmux;

pub use install::shlex_quote;

/// The tmux server socket from `$TMUX` (its first field), or `None` outside tmux.
fn server_socket() -> Option<String> {
    let tmux = std::env::var("TMUX").ok()?;
    let socket = tmux.split(',').next().unwrap_or("");
    if socket.is_empty() {
        None
    } else {
        Some(socket.to_string())
    }
}

/// This executable's path, or `wrangler` if it cannot be resolved.
fn exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "wrangler".to_string())
}

/// A global option's value, trimmed, with a default when unset.
fn option_or(socket: &str, name: &str, default: &str) -> String {
    let value = run_tmux(socket, &["show-option", "-gqv", name]);
    let value = value.trim();
    if value.is_empty() {
        default.to_string()
    } else {
        value.to_string()
    }
}

/// Whether an option's value reads as on (`on`/`1`/`yes`/`true`).
fn option_on(socket: &str, name: &str) -> bool {
    matches!(
        run_tmux(socket, &["show-option", "-gqv", name])
            .trim()
            .to_lowercase()
            .as_str(),
        "on" | "1" | "yes" | "true"
    )
}

/// Start the daemon detached, so it is up regardless of which key first ran.
/// `replace` makes it evict a running daemon (used after a rebuild so the new
/// binary takes over) rather than yielding to it.
fn start_daemon(exe: &str, replace: bool) {
    let mut command = Command::new(exe);
    command.arg("daemon");
    if replace {
        command.arg("--replace");
    }
    crate::platform::spawn_detached(command);
}

/// The plugin entry point: bind the toggle/focus keys to this binary, install the
/// new-window/break-pane spawn hooks and the automatic-rename guard, optionally
/// install the agent hooks, and start the daemon. `replace_daemon` (set when the
/// wrapper just rebuilt the binary) evicts a running daemon so the new build takes
/// over.
pub fn tmux_entry(replace_daemon: bool) -> ExitCode {
    let Some(socket) = server_socket() else {
        eprintln!("wrangler tmux-entry: not inside a tmux server");
        return ExitCode::from(2);
    };
    let exe = exe_path();
    let quoted_exe = shlex_quote(&exe);

    start_daemon(&exe, replace_daemon);

    // Toggle and focus keys, bound with the prefix.
    let key = option_or(&socket, "@wrangler-key", "Tab");
    run_tmux(
        &socket,
        &[
            "bind-key",
            &key,
            "run-shell",
            &format!("{quoted_exe} toggle"),
        ],
    );
    let focus_key = option_or(&socket, "@wrangler-focus-key", "a");
    run_tmux(
        &socket,
        &[
            "bind-key",
            &focus_key,
            "run-shell",
            &format!("{quoted_exe} focus"),
        ],
    );

    // Opt-in agent-hook install on load, backgrounded so load never blocks.
    if option_on(&socket, "@wrangler-auto-install-hooks") {
        run_tmux(
            &socket,
            &["run-shell", "-b", &format!("{quoted_exe} install-hooks")],
        );
    }

    // Windows created while the sidebar is on get their own sidebar pane.
    let spawn_hook = format!("run-shell '{quoted_exe} spawn --if-active'");
    run_tmux(
        &socket,
        &["set-hook", "-g", "after-new-window", &spawn_hook],
    );
    run_tmux(
        &socket,
        &["set-hook", "-g", "after-break-pane", &spawn_hook],
    );
    // Keep the session-window-changed hook unset.
    run_tmux(&socket, &["set-hook", "-gu", "session-window-changed"]);

    // automatic-rename uses the active pane's command, so focusing the sidebar
    // would rename the window. Guard the format so the window keeps its name
    // while the sidebar pane is active.
    let fmt = run_tmux(&socket, &["show-option", "-gv", "automatic-rename-format"]);
    let fmt = fmt.trim_end_matches('\n');
    if !fmt.contains("@wrangler_sidebar") {
        let guarded = format!("#{{?#{{@wrangler_sidebar}},#{{window_name}},{fmt}}}");
        run_tmux(
            &socket,
            &["set-option", "-g", "automatic-rename-format", &guarded],
        );
    }

    ExitCode::SUCCESS
}

/// Toggle the sidebar on this server: kill every sidebar pane, or spawn one per
/// window.
pub fn toggle() -> ExitCode {
    let Some(socket) = server_socket() else {
        return ExitCode::from(2);
    };
    crate::tmux::toggle(&socket);
    ExitCode::SUCCESS
}

/// Give keyboard focus to the current window's sidebar pane, if it has one.
pub fn focus() -> ExitCode {
    let Some(socket) = server_socket() else {
        return ExitCode::from(2);
    };
    crate::tmux::focus_key(&socket);
    ExitCode::SUCCESS
}

/// Spawn a sidebar pane. `args`: an optional `--if-active` (only when the session
/// already has sidebars) and an optional target window id.
pub fn spawn(args: &[String]) -> ExitCode {
    let Some(socket) = server_socket() else {
        return ExitCode::from(2);
    };
    let mut if_active = false;
    let mut window: Option<String> = None;
    for arg in args {
        if arg == "--if-active" {
            if_active = true;
        } else {
            window = Some(arg.clone());
        }
    }
    crate::tmux::spawn(&socket, if_active, window.as_deref());
    ExitCode::SUCCESS
}
