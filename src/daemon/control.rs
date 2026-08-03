//! The control-mode listener: one `tmux -C` client per watched server, turning
//! what tmux pushes down it into core-loop events.
//!
//! A control client is attached for exactly as long as a server has sidebar
//! clients, so nothing is watched while the sidebar is off. The attach carries
//! two client flags: `no-output`, without which every byte written by every pane
//! in the attached session arrives as a `%output` line, and `ignore-size`, so a
//! client that renders nothing never takes part in sizing the user's windows.
//!
//! The child's stdin is a pipe this process holds open and never writes to.
//! `tmux -C` exits when its stdin closes, which is what reaps the client if the
//! daemon dies without running any cleanup (a `daemon --replace` kills the
//! incumbent outright).
//!
//! Parsing is pure and separated from the process handling: [`parse_line`] maps
//! one wire line to the [`ControlEvent`] the daemon acts on, and everything else,
//! command replies included, is dropped by the reader before it can reach the
//! core.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::{ServerKey, WindowId};

/// The delay before the first reattach, doubled up to [`RETRY_MAX`] while
/// attaching keeps failing immediately.
const RETRY_MIN: Duration = Duration::from_millis(250);
/// The ceiling on the reattach delay.
const RETRY_MAX: Duration = Duration::from_secs(4);
/// How long an attach must last to count as having worked, resetting the delay.
const RETRY_RESET: Duration = Duration::from_secs(2);

/// Something tmux reported down the control-mode stream that the daemon acts on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlEvent {
    /// A window was created on the watched server.
    WindowAdd(WindowId),
}

/// Map one control-mode line to the event it carries, or `None` for the lines the
/// daemon does not act on.
///
/// `%window-add` and `%unlinked-window-add` are the same event seen from either
/// side of the attached session: a window created in the session this client is
/// attached to, or in any other session on the same server. Both name the new
/// window, and the sidebar spans the whole server, so both are one event here.
pub fn parse_line(line: &str) -> Option<ControlEvent> {
    let (kind, rest) = line.split_once(' ')?;
    match kind {
        "%window-add" | "%unlinked-window-add" => {
            let id = rest.split_whitespace().next()?;
            id.starts_with('@')
                .then(|| ControlEvent::WindowAdd(WindowId(id.to_string())))
        }
        _ => None,
    }
}

/// A running control-mode listener. Dropping it stops the listener: the flag
/// tells the thread not to reattach, and killing the client closes the stream
/// the reader is blocked on.
pub struct Listener {
    stop: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut slot) = self.child.lock() {
            if let Some(child) = slot.as_mut() {
                let _ = child.kill();
            }
        }
    }
}

/// Attach a control-mode client to `server` and report what it sees through
/// `emit` until the returned [`Listener`] is dropped.
///
/// `emit` runs on the listener's own thread and returning `false` from it stops
/// the listener, which is how a closed core channel ends the thread. The client
/// is reattached whenever it exits (the server died, its session was destroyed,
/// or it had no session to attach to yet), so a listener outlives any one tmux
/// client.
pub fn listen<F>(server: ServerKey, emit: F) -> Listener
where
    F: Fn(ControlEvent) -> bool + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    let child: Arc<Mutex<Option<Child>>> = Arc::new(Mutex::new(None));
    let thread_stop = Arc::clone(&stop);
    let thread_child = Arc::clone(&child);

    thread::spawn(move || {
        let mut retry = RETRY_MIN;
        while !thread_stop.load(Ordering::SeqCst) {
            let started = Instant::now();
            if !run_client(&server, &thread_stop, &thread_child, &emit) {
                return;
            }
            if thread_stop.load(Ordering::SeqCst) {
                return;
            }
            if started.elapsed() >= RETRY_RESET {
                retry = RETRY_MIN;
            }
            thread::sleep(retry);
            retry = (retry * 2).min(RETRY_MAX);
        }
    });

    Listener { stop, child }
}

/// Run one control-mode client to completion, emitting what it reports. Returns
/// whether the caller should carry on (`false` once `emit` has asked to stop).
///
/// The child's stdin pipe is held for the client's whole life and dropped with
/// the handle, so the client exits with the daemon.
fn run_client<F>(
    server: &ServerKey,
    stop: &AtomicBool,
    slot: &Mutex<Option<Child>>,
    emit: &F,
) -> bool
where
    F: Fn(ControlEvent) -> bool,
{
    let spawned = Command::new("tmux")
        .arg("-S")
        .arg(&server.0)
        .args(["-C", "attach", "-f", "no-output,ignore-size"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = spawned else {
        return true;
    };
    let Some(stdout) = child.stdout.take() else {
        let _ = child.kill();
        return true;
    };

    match slot.lock() {
        Ok(mut held) => *held = Some(child),
        Err(_) => return false,
    }

    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    let mut carry_on = true;
    while !stop.load(Ordering::SeqCst) {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                if let Some(event) = parse_line(line.trim_end()) {
                    if !emit(event) {
                        carry_on = false;
                        break;
                    }
                }
            }
        }
    }

    if let Ok(mut held) = slot.lock() {
        if let Some(mut child) = held.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
    carry_on
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_add_lines_yield_the_new_window() {
        assert_eq!(
            parse_line("%window-add @3"),
            Some(ControlEvent::WindowAdd(WindowId("@3".into())))
        );
        // A window created in a session this client is not attached to.
        assert_eq!(
            parse_line("%unlinked-window-add @2"),
            Some(ControlEvent::WindowAdd(WindowId("@2".into())))
        );
    }

    #[test]
    fn other_lines_are_ignored() {
        for line in [
            "%begin 1785788229 802 0",
            "%end 1785788229 802 0",
            "%session-changed $0 A",
            "%layout-change @0 ac9d,200x50,0,0,0 ac9d,200x50,0,0,0 *",
            "%window-renamed @1 sleep",
            "%unlinked-window-close @1",
            "%window-close @1",
            "%sessions-changed",
            "%exit",
            "",
        ] {
            assert_eq!(parse_line(line), None, "line: {line:?}");
        }
    }

    #[test]
    fn a_malformed_window_id_is_not_an_event() {
        // The id field must be a window id; anything else is a line shape this
        // does not understand rather than a window to spawn into.
        assert_eq!(parse_line("%window-add %3"), None);
        assert_eq!(parse_line("%window-add"), None);
        assert_eq!(parse_line("%window-add "), None);
    }
}
