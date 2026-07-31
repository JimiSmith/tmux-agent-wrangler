//! The per-server tmux command and query layer.
//!
//! Every tmux invocation is targeted at one server by its socket path
//! (`tmux -S <socket> ...`), so panes and windows from different servers are
//! never conflated. [`run_tmux`] is the single choke point: it is fail-soft, so
//! a dead or unreachable server reads as empty output and the callers degrade
//! to "no windows / no panes" rather than erroring.

use std::process::Command;

use indexmap::{IndexMap, IndexSet};

use crate::daemon::assoc::strip_status_prefix;
use crate::model::{Pane, PaneId, Window, WindowId};

/// The `list-windows` format: `window_id`, `window_index`, `window_name`,
/// `window_active`, tab-separated.
const WINDOW_FORMAT: &str =
    "#{window_id}\t#{window_index}\t#{window_name}\t#{window_active}";

/// The session-wide `list-panes -s` format. The fixed enum/number fields
/// (`pane_pb_state`, `pane_pb_progress`, `pane_pid`) precede `pane_current_path`
/// and the free-form `pane_title`, which is kept last so a space or glyph in it
/// cannot shift another field.
const PANE_FORMAT: &str = "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{@wrangler_sidebar}\t#{pane_pb_state}\t#{pane_pb_progress}\t#{pane_pid}\t#{pane_current_path}\t#{pane_title}";

/// The whole window/pane model read from a server in one pass: the ordered
/// window tree plus the per-pane lookup maps other layers need to place agent
/// sessions and enforce the one-sidebar-per-window invariant.
///
/// `windows` holds only real panes, in tmux emission order (the render tree
/// depends on that order). Sidebar panes are excluded from the tree but recorded
/// in `sidebars`. `pane_to_window`, `pane_paths`, `pane_progress`, and
/// `pane_pids` cover every pane including sidebars; `pane_titles` (the
/// glyph-stripped title used to match a pane to a session) covers real panes
/// only.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FetchResult {
    pub windows: Vec<Window>,
    pub pane_to_window: IndexMap<PaneId, WindowId>,
    pub sidebars: IndexSet<PaneId>,
    pub pane_paths: IndexMap<PaneId, String>,
    pub pane_progress: IndexMap<PaneId, (String, Option<u8>)>,
    pub pane_titles: IndexMap<PaneId, String>,
    pub pane_pids: IndexMap<PaneId, u32>,
}

/// Run `tmux -S <socket>` with the given arguments and return its stdout as a
/// lossily-decoded string (raw, untrimmed). Any failure — the binary missing,
/// a spawn error, or a non-zero exit — yields an empty string; the exit status
/// and stderr are discarded. Never panics and never propagates an error, so a
/// dead server simply reads as no output. A literal `";"` argument is passed
/// through verbatim as tmux's own command separator (no shell is involved).
pub fn run_tmux(socket: &str, args: &[&str]) -> String {
    match Command::new("tmux").arg("-S").arg(socket).args(args).output() {
        Ok(out) => String::from_utf8_lossy(&out.stdout).into_owned(),
        Err(_) => String::new(),
    }
}

/// Parse a `pane_pb_progress` field: a run of ASCII digits becomes the
/// percentage, anything else (empty on a server that does not report it) is
/// `None`.
fn parse_pb_progress(field: &str) -> Option<u8> {
    if !field.is_empty() && field.bytes().all(|b| b.is_ascii_digit()) {
        field.parse::<u8>().ok()
    } else {
        None
    }
}

/// Build the window/pane model from raw `list-windows` and `list-panes -s`
/// output. Pure: no I/O, deterministic, and order-preserving.
///
/// Windows are kept in `list_windows_out` order; each real pane is appended to
/// its window in `list_panes_out` order. A pane is a sidebar when its
/// `@wrangler_sidebar` flag is exactly `"1"`; sidebars are collected in
/// `sidebars` and left out of the window tree. A pane whose window was not in
/// `list_windows_out` is dropped. Field splitting is bounded so a space or glyph
/// in the trailing title (or a tab-free path) cannot corrupt the parse; a line
/// with too few fields is skipped.
pub fn parse_windows(list_windows_out: &str, list_panes_out: &str) -> FetchResult {
    let mut windows: Vec<Window> = Vec::new();
    // Window id -> its index in `windows`, so panes append to the right window
    // while the Vec preserves emission order.
    let mut pos_by_id: IndexMap<String, usize> = IndexMap::new();

    for line in list_windows_out.lines() {
        let parts: Vec<&str> = line.splitn(4, '\t').collect();
        if parts.len() < 4 {
            continue;
        }
        let (id, index, name, active) = (parts[0], parts[1], parts[2], parts[3]);
        pos_by_id.insert(id.to_string(), windows.len());
        windows.push(Window {
            id: WindowId(id.to_string()),
            index: index.to_string(),
            name: name.to_string(),
            active: active == "1",
            color: None,
            panes: Vec::new(),
        });
    }

    let mut result = FetchResult {
        windows,
        ..FetchResult::default()
    };

    for line in list_panes_out.lines() {
        let parts: Vec<&str> = line.splitn(10, '\t').collect();
        if parts.len() < 10 {
            continue;
        }
        let wid = parts[0];
        let pid = parts[1];
        let index = parts[2];
        let active = parts[3];
        let flag = parts[4];
        let pb_state = parts[5];
        let pb_progress = parts[6];
        let pane_pid = parts[7];
        let path = parts[8];
        let title = parts[9];

        let wpos = match pos_by_id.get(wid) {
            Some(&p) => p,
            None => continue,
        };

        let pane_id = PaneId(pid.to_string());
        result
            .pane_to_window
            .insert(pane_id.clone(), WindowId(wid.to_string()));
        result
            .pane_pids
            .insert(pane_id.clone(), pane_pid.parse::<u32>().unwrap_or(0));
        result.pane_paths.insert(pane_id.clone(), path.to_string());
        let progress = parse_pb_progress(pb_progress);
        result
            .pane_progress
            .insert(pane_id.clone(), (pb_state.to_string(), progress));

        if flag == "1" {
            result.sidebars.insert(pane_id);
            continue;
        }

        // The row keeps the raw title (its live status glyph and all); the
        // association map keeps the glyph-stripped form used to match a pane to
        // the session displayed in it.
        result
            .pane_titles
            .insert(pane_id.clone(), strip_status_prefix(title));
        result.windows[wpos].panes.push(Pane {
            id: pane_id,
            index: index.to_string(),
            active: active == "1",
            title: title.to_string(),
            pb_state: pb_state.to_string(),
            pb_progress: progress,
            color: None,
        });
    }

    result
}

/// Query a server and build its full window/pane model.
pub fn fetch_windows(socket: &str) -> FetchResult {
    let windows_out = run_tmux(socket, &["list-windows", "-F", WINDOW_FORMAT]);
    let panes_out = run_tmux(socket, &["list-panes", "-s", "-F", PANE_FORMAT]);
    parse_windows(&windows_out, &panes_out)
}

/// The real (non-sidebar) pane ids of one window, in tmux emission order. A pane
/// counts as a sidebar only when its `@wrangler_sidebar` flag is exactly `"1"`;
/// a flag-less line (no tab) is a real pane.
pub fn window_real_panes(socket: &str, window: &str) -> Vec<PaneId> {
    let out = run_tmux(
        socket,
        &["list-panes", "-t", window, "-F", "#{pane_id}\t#{@wrangler_sidebar}"],
    );
    let mut panes = Vec::new();
    for line in out.lines() {
        let (pid, flag) = line.split_once('\t').unwrap_or((line, ""));
        if flag != "1" {
            panes.push(PaneId(pid.to_string()));
        }
    }
    panes
}

/// Select a window and, best-effort, a real pane within it.
///
/// With `pane` given (non-empty), that pane is the target. Otherwise the
/// window's active pane is inspected: if it is a sidebar, the first real pane
/// becomes the target; if it is already a real pane, no pane is forced and only
/// the window is selected. The final `select-window` and optional `select-pane`
/// run in a single tmux process, separated by a literal `";"` argument, so both
/// take effect together.
pub fn focus(socket: &str, window: &str, pane: Option<&str>) {
    let mut target: Option<String> = pane.filter(|p| !p.is_empty()).map(|p| p.to_string());

    if target.is_none() {
        let line = run_tmux(
            socket,
            &["display-message", "-p", "-t", window, "#{pane_id}\t#{@wrangler_sidebar}"],
        );
        let line = line.trim();
        let (_active, flag) = line.split_once('\t').unwrap_or((line, ""));
        if flag == "1" {
            target = window_real_panes(socket, window)
                .into_iter()
                .next()
                .map(|p| p.0);
        }
    }

    let mut args: Vec<&str> = vec!["select-window", "-t", window];
    if let Some(t) = target.as_deref() {
        args.push(";");
        args.push("select-pane");
        args.push("-t");
        args.push(t);
    }
    let _ = run_tmux(socket, &args);
}

/// Among sidebar panes sharing a window, the one that survives a spawn race: the
/// lowest pane id by numeric (`%`-stripped) value, compared as integers so `%9`
/// beats `%10`. A pane id not of the `%<digits>` form sorts after every numeric
/// id. Empty input yields `None`; on a tie the first in order is returned.
pub fn spawn_race_survivor(sidebars: &[PaneId]) -> Option<&PaneId> {
    sidebars.iter().min_by_key(|p| p.numeric().unwrap_or(u64::MAX))
}

/// Whether any pane in the session has the `@wrangler_sidebar` flag set (the
/// sidebar is toggled on for this server).
fn session_has_sidebar(socket: &str) -> bool {
    run_tmux(socket, &["list-panes", "-s", "-F", "#{@wrangler_sidebar}"])
        .lines()
        .any(|l| l == "1")
}

/// Whether the given window already holds a sidebar pane.
fn window_has_sidebar(socket: &str, window: &str) -> bool {
    run_tmux(socket, &["list-panes", "-t", window, "-F", "#{@wrangler_sidebar}"])
        .lines()
        .any(|l| l == "1")
}

/// The initial sidebar width in columns from `@wrangler-width`, defaulting to 32
/// when unset or non-numeric.
fn spawn_width(socket: &str) -> u32 {
    let value = run_tmux(socket, &["show-option", "-gqv", "@wrangler-width"]);
    let value = value.trim();
    if !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()) {
        value.parse::<u32>().unwrap_or(32)
    } else {
        32
    }
}

/// The shell command a spawned sidebar pane runs: this executable's `client`
/// subcommand. The executable path is single-quoted so a space in it survives
/// tmux handing the string to the shell.
fn sidebar_command() -> String {
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "wrangler".to_string());
    format!("'{}' client", exe.replace('\'', "'\\''"))
}

/// Split a sidebar pane into a window unless it already has one, and tag it.
///
/// `window` defaults to the server's current window. With `if_active`, this is a
/// no-op unless the session already has a sidebar (so the new-window hooks only
/// spawn while toggled on). A window that already holds a sidebar is left
/// untouched. The new pane is a full-height column placed on the left at the
/// configured width, then flagged with the `@wrangler_sidebar` pane option that
/// marks it a sidebar everywhere.
pub fn spawn(socket: &str, if_active: bool, window: Option<&str>) {
    let win = match window {
        Some(w) => w.to_string(),
        None => run_tmux(socket, &["display-message", "-p", "#{window_id}"])
            .trim()
            .to_string(),
    };
    if win.is_empty() {
        return;
    }

    if if_active && !session_has_sidebar(socket) {
        return;
    }
    if window_has_sidebar(socket, &win) {
        return;
    }

    let width = spawn_width(socket).to_string();
    let command = sidebar_command();
    let pane = run_tmux(
        socket,
        &[
            "split-window", "-d", "-f", "-h", "-b", "-l", &width, "-t", &win, "-P", "-F",
            "#{pane_id}", &command,
        ],
    );
    let pane = pane.trim();
    if pane.is_empty() {
        return;
    }
    let _ = run_tmux(
        socket,
        &["set-option", "-p", "-t", pane, "@wrangler_sidebar", "1"],
    );
}

/// The sidebar on/off switch for one server. If any sidebar pane exists, kill
/// every one of them (errors swallowed, since a pane may die mid-loop) and stop.
/// Otherwise turn on: spawn one sidebar per window (each spawn is a no-op if that
/// window already has one).
pub fn toggle(socket: &str) {
    let listing = run_tmux(socket, &["list-panes", "-s", "-F", "#{pane_id} #{@wrangler_sidebar}"]);
    let sidebars: Vec<String> = listing
        .lines()
        .filter_map(|l| {
            let mut fields = l.split_whitespace();
            let id = fields.next()?;
            let flag = fields.next().unwrap_or("");
            (flag == "1").then(|| id.to_string())
        })
        .collect();

    if !sidebars.is_empty() {
        for pane in &sidebars {
            let _ = run_tmux(socket, &["kill-pane", "-t", pane]);
        }
        return;
    }

    let windows = run_tmux(socket, &["list-windows", "-F", "#{window_id}"]);
    for win in windows.lines() {
        let win = win.trim();
        if !win.is_empty() {
            spawn(socket, false, Some(win));
        }
    }
}

/// Give keyboard focus to the current window's sidebar pane, if it has one.
///
/// Scoped to the current window (no `-s`), the first pane whose
/// `@wrangler_sidebar` flag is exactly `"1"` is selected, then a `C-l` keystroke
/// is sent to it so it repaints at once. A window with no sidebar pane is a
/// no-op; nothing is ever spawned.
pub fn focus_key(socket: &str) {
    let listing = run_tmux(socket, &["list-panes", "-F", "#{pane_id} #{@wrangler_sidebar}"]);
    let pane = listing.lines().find_map(|l| {
        let mut fields = l.split_whitespace();
        let id = fields.next()?;
        let flag = fields.next().unwrap_or("");
        (flag == "1").then(|| id.to_string())
    });
    if let Some(pane) = pane {
        let _ = run_tmux(socket, &["select-pane", "-t", &pane]);
        let _ = run_tmux(socket, &["send-keys", "-t", &pane, "C-l"]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ten tab-separated pane fields in the fetch order.
    #[allow(clippy::too_many_arguments)]
    fn pane_line(
        wid: &str,
        pid: &str,
        index: &str,
        active: &str,
        flag: &str,
        pb_state: &str,
        pb_progress: &str,
        pane_pid: &str,
        path: &str,
        title: &str,
    ) -> String {
        format!(
            "{wid}\t{pid}\t{index}\t{active}\t{flag}\t{pb_state}\t{pb_progress}\t{pane_pid}\t{path}\t{title}"
        )
    }

    fn sample() -> FetchResult {
        let windows_out = "@1\t0\teditor\t1\n@2\t1\tlogs\t0\n";
        let panes_out = [
            // Real, active, OSC-progress pane with a glyph-prefixed title.
            pane_line("@1", "%1", "0", "1", "", "normal", "42", "1001", "/home/a", "⠋ Working"),
            // Sidebar pane (flag set): excluded from the tree, recorded as a sidebar.
            pane_line("@1", "%2", "1", "0", "1", "", "", "1002", "/home/a", "sidebar"),
            // Real, inactive, empty progress vars.
            pane_line("@1", "%3", "2", "0", "", "", "", "1003", "/home/a", "shell"),
            // Real pane in the second window; a "hidden" state with no percentage.
            pane_line("@2", "%4", "0", "1", "", "hidden", "", "2001", "/var/log", "tail"),
            // Pane in a window that list-windows did not return: dropped.
            pane_line("@9", "%9", "0", "1", "", "", "", "9000", "/x", "ghost"),
        ]
        .join("\n");
        parse_windows(windows_out, &panes_out)
    }

    #[test]
    fn windows_kept_in_emission_order() {
        let r = sample();
        let ids: Vec<&str> = r.windows.iter().map(|w| w.id.0.as_str()).collect();
        assert_eq!(ids, ["@1", "@2"]);
        assert_eq!(r.windows[0].name, "editor");
        assert!(r.windows[0].active);
        assert!(!r.windows[1].active);
    }

    #[test]
    fn real_panes_only_in_tree_in_order() {
        let r = sample();
        let w1: Vec<&str> = r.windows[0].panes.iter().map(|p| p.id.0.as_str()).collect();
        // %2 is a sidebar, so the tree holds only %1 then %3.
        assert_eq!(w1, ["%1", "%3"]);
        let w2: Vec<&str> = r.windows[1].panes.iter().map(|p| p.id.0.as_str()).collect();
        assert_eq!(w2, ["%4"]);
    }

    #[test]
    fn pane_fields_parsed() {
        let r = sample();
        let p1 = &r.windows[0].panes[0];
        assert_eq!(p1.index, "0");
        assert!(p1.active);
        // The tree keeps the raw glyph-bearing title.
        assert_eq!(p1.title, "⠋ Working");
        assert_eq!(p1.pb_state, "normal");
        assert_eq!(p1.pb_progress, Some(42));

        let p3 = &r.windows[0].panes[1];
        assert!(!p3.active);
        assert_eq!(p3.pb_state, "");
        assert_eq!(p3.pb_progress, None);

        let p4 = &r.windows[1].panes[0];
        assert_eq!(p4.pb_state, "hidden");
        assert_eq!(p4.pb_progress, None);
    }

    #[test]
    fn sidebar_recorded_but_not_a_pane() {
        let r = sample();
        assert!(r.sidebars.contains(&PaneId("%2".into())));
        assert!(!r.sidebars.contains(&PaneId("%1".into())));
        // The sidebar's progress and window mapping are still recorded.
        assert_eq!(
            r.pane_progress.get(&PaneId("%2".into())),
            Some(&(String::new(), None))
        );
        assert_eq!(
            r.pane_to_window.get(&PaneId("%2".into())),
            Some(&WindowId("@1".into()))
        );
        // But a sidebar contributes no association title.
        assert!(!r.pane_titles.contains_key(&PaneId("%2".into())));
    }

    #[test]
    fn association_titles_are_glyph_stripped() {
        let r = sample();
        assert_eq!(r.pane_titles.get(&PaneId("%1".into())).unwrap(), "Working");
        assert_eq!(r.pane_titles.get(&PaneId("%3".into())).unwrap(), "shell");
    }

    #[test]
    fn lookup_maps_cover_panes_and_skip_unknown_window() {
        let r = sample();
        assert_eq!(r.pane_pids.get(&PaneId("%1".into())), Some(&1001));
        assert_eq!(
            r.pane_paths.get(&PaneId("%1".into())).map(String::as_str),
            Some("/home/a")
        );
        assert_eq!(
            r.pane_to_window.get(&PaneId("%4".into())),
            Some(&WindowId("@2".into()))
        );
        // The pane in the unlisted window @9 is dropped entirely.
        assert!(!r.pane_to_window.contains_key(&PaneId("%9".into())));
        assert!(!r.pane_pids.contains_key(&PaneId("%9".into())));
    }

    #[test]
    fn malformed_and_empty_lines_are_skipped() {
        // First window line is well-formed; the second has only three fields
        // (no active flag) and the blank line has none, so both are skipped.
        let windows_out = "@1\t0\tgood\t1\n@2\t1\tmissing-active\n\n";
        // The pane line has under ten fields and is skipped.
        let panes_out = "@1\t%1\ttoo\tfew\n";
        let r = parse_windows(windows_out, panes_out);
        assert_eq!(r.windows.len(), 1);
        assert_eq!(r.windows[0].name, "good");
        assert!(r.windows[0].active);
        assert!(r.windows[0].panes.is_empty());
    }

    #[test]
    fn spawn_race_survivor_is_numeric_not_lexical() {
        let panes = [PaneId("%10".into()), PaneId("%9".into()), PaneId("%2".into())];
        assert_eq!(spawn_race_survivor(&panes), Some(&PaneId("%2".into())));

        // Lexically "%10" < "%9", but numerically 9 < 10 wins.
        let two = [PaneId("%10".into()), PaneId("%9".into())];
        assert_eq!(spawn_race_survivor(&two), Some(&PaneId("%9".into())));

        assert_eq!(spawn_race_survivor(&[]), None);

        // A non-numeric id sorts after every numeric one.
        let mixed = [PaneId("%bogus".into()), PaneId("%5".into())];
        assert_eq!(spawn_race_survivor(&mixed), Some(&PaneId("%5".into())));
    }
}
