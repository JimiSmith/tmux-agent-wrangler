//! The always-on, multi-tenant daemon: a single-owner core that holds all state,
//! fed by one channel from the socket readers and a poll timer.
//!
//! A connection's reader thread forwards each decoded line as an [`Event`]; a
//! timer thread emits [`Event::Poll`] once a second, and each watched server's
//! control-mode listener forwards what tmux pushes. The core loop owns [`State`],
//! the client write handles and the listeners, processes events serially, and
//! writes each resulting push back to its connection. The model-building modules
//! it drives stay pure and testable.

pub mod assoc;
pub mod control;
pub mod notify;
pub mod persist;
pub mod rows;
pub mod state;

use std::io::BufReader;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::process::ExitCode;
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Duration;

use indexmap::IndexMap;

use crate::daemon::control::{ControlEvent, Listener};
use crate::daemon::state::{ConnId, RealTmux, State, TmuxEnv};
use crate::model::ServerKey;
use crate::paths::{daemon_pidfile, daemon_socket, state_dir};
use crate::proto::{read_message, write_message, ClientMsg, CtlMsg, HookMsg, Inbound, ServerMsg};

/// The interval between full tmux polls.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// An input to the core loop. Every socket line, connection lifecycle change, and
/// timer tick reaches the core as one of these.
enum Event {
    Connected {
        conn: ConnId,
        writer: UnixStream,
    },
    Inbound {
        conn: ConnId,
        msg: Inbound,
    },
    Disconnected {
        conn: ConnId,
    },
    Control {
        server: ServerKey,
        event: ControlEvent,
    },
    Poll,
}

/// The result of trying to become the singleton daemon.
enum Bind {
    Listener(UnixListener),
    AlreadyRunning,
    Failed,
}

/// Bind the daemon socket, or determine that another daemon already owns it. An
/// `AddrInUse` error is disambiguated by connecting: a successful connect means a
/// live daemon; a refused one means a stale socket file, which is removed and
/// rebound.
///
/// `replace` (set when started from a freshly built binary) evicts a live
/// incumbent instead of yielding to it: the recorded pid is killed and its socket
/// reclaimed once it exits. An incumbent with no recorded pid (one from before
/// pidfiles) cannot be targeted, so it is left running rather than orphaned by
/// rebinding over it; the next update replaces cleanly, since this daemon records
/// its pid.
fn bind_singleton(path: &Path, replace: bool) -> Bind {
    match UnixListener::bind(path) {
        Ok(l) => Bind::Listener(l),
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                if !replace {
                    return Bind::AlreadyRunning;
                }
                match read_pidfile() {
                    Some(pid) => evict(pid, path),
                    None => return Bind::AlreadyRunning,
                }
            }
            let _ = std::fs::remove_file(path);
            match UnixListener::bind(path) {
                Ok(l) => Bind::Listener(l),
                Err(_) => Bind::Failed,
            }
        }
        Err(_) => Bind::Failed,
    }
}

/// The pid recorded by the running daemon, or `None` if unreadable/unparseable.
fn read_pidfile() -> Option<i32> {
    std::fs::read_to_string(daemon_pidfile())
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Kill the incumbent daemon and wait for it to release the socket so the caller
/// can rebind. Escalates to `SIGKILL` if it does not exit under `SIGTERM`.
fn evict(pid: i32, path: &Path) {
    // Safe: kill only inspects/signals the pid; an invalid or dead pid is a
    // harmless error we ignore.
    unsafe { libc::kill(pid, libc::SIGTERM) };
    if wait_socket_free(path) {
        return;
    }
    unsafe { libc::kill(pid, libc::SIGKILL) };
    wait_socket_free(path);
}

/// Poll until the socket refuses connections (the incumbent is gone), up to two
/// seconds. Returns whether it became free.
fn wait_socket_free(path: &Path) -> bool {
    for _ in 0..100 {
        if UnixStream::connect(path).is_err() {
            return true;
        }
        thread::sleep(Duration::from_millis(20));
    }
    false
}

/// Accept connections forever, assigning each an id in accept order. A write
/// handle is cloned for the core to push through, and a reader thread forwards
/// that connection's lines.
fn spawn_acceptor(listener: UnixListener, tx: Sender<Event>) {
    thread::spawn(move || {
        let mut next: u64 = 0;
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let conn = ConnId(next);
            next += 1;
            let Ok(writer) = stream.try_clone() else {
                continue;
            };
            if tx.send(Event::Connected { conn, writer }).is_err() {
                return;
            }
            let reader_tx = tx.clone();
            thread::spawn(move || reader_loop(conn, stream, reader_tx));
        }
    });
}

/// Forward one connection's decoded lines to the core until end of stream or a
/// decode error, then report the disconnect.
fn reader_loop(conn: ConnId, stream: UnixStream, tx: Sender<Event>) {
    let mut reader = BufReader::new(stream);
    loop {
        match read_message::<_, Inbound>(&mut reader) {
            Ok(Some(msg)) => {
                if tx.send(Event::Inbound { conn, msg }).is_err() {
                    return;
                }
            }
            _ => {
                let _ = tx.send(Event::Disconnected { conn });
                return;
            }
        }
    }
}

/// Emit a poll tick every interval until the core stops receiving.
fn spawn_poller(tx: Sender<Event>) {
    thread::spawn(move || loop {
        thread::sleep(POLL_INTERVAL);
        if tx.send(Event::Poll).is_err() {
            return;
        }
    });
}

/// Keep one control-mode listener per watched server.
///
/// A server is watched for exactly as long as it has sidebar clients, which is
/// the same lifetime as its [`State::servers`] entry: the sidebar being on is
/// what makes tmux's notifications worth acting on, so nothing is attached while
/// it is off. Dropping a listener stops it.
fn sync_listeners(
    listeners: &mut IndexMap<ServerKey, Listener>,
    state: &State,
    tx: &Sender<Event>,
) {
    listeners.retain(|server, _| state.servers.contains_key(server));
    for server in state.servers.keys() {
        if listeners.contains_key(server) {
            continue;
        }
        let tx = tx.clone();
        let emit_server = server.clone();
        let listener = control::listen(server.clone(), move |event| {
            tx.send(Event::Control {
                server: emit_server.clone(),
                event,
            })
            .is_ok()
        });
        listeners.insert(server.clone(), listener);
    }
}

/// The single-owner core: own the state, the write handles and the control-mode
/// listeners, and process events serially, writing each push to its connection.
fn core_loop(rx: mpsc::Receiver<Event>, tx: Sender<Event>) {
    let env = RealTmux;
    let mut state = State::new();
    state.load_records(persist::load());
    let mut writers: IndexMap<ConnId, UnixStream> = IndexMap::new();
    let mut listeners: IndexMap<ServerKey, Listener> = IndexMap::new();

    while let Ok(ev) = rx.recv() {
        let mut pushes: Vec<(ConnId, ServerMsg)> = Vec::new();
        match ev {
            Event::Connected { conn, writer } => {
                writers.insert(conn, writer);
            }
            Event::Disconnected { conn } => {
                writers.shift_remove(&conn);
                state.on_disconnect(conn);
            }
            Event::Control { server, event } => {
                if state.on_control(&env, &server, &event) {
                    let parents = env.ppid_map();
                    state.poll_server(&env, &server, &parents, &mut pushes);
                }
            }
            Event::Poll => {
                let parents = env.ppid_map();
                let servers: Vec<ServerKey> = state.servers.keys().cloned().collect();
                for server in servers {
                    state.poll_server(&env, &server, &parents, &mut pushes);
                }
                let keys = state.registry_keys();
                state.notifier.retain_live(&keys);
                if state.dirty_registry {
                    persist::save(&state.registry);
                    state.dirty_registry = false;
                }
            }
            Event::Inbound { conn, msg } => match msg {
                Inbound::Client(ClientMsg::Hello {
                    server,
                    window,
                    pane,
                    cols,
                    ..
                }) => {
                    pushes = state.on_hello(&env, conn, server, window, pane, cols);
                }
                Inbound::Client(ClientMsg::Input { event }) => {
                    pushes = state.on_input(&env, conn, event);
                }
                Inbound::Client(ClientMsg::Bye) => {
                    writers.shift_remove(&conn);
                    state.on_disconnect(conn);
                }
                Inbound::Hook(HookMsg::HookEvent {
                    server,
                    pane,
                    agent,
                    event,
                    session_id,
                    cwd,
                    transcript,
                    pid,
                    token,
                    ..
                }) => {
                    let servers = state.on_hook(
                        server, pane, agent, event, session_id, cwd, transcript, pid, token,
                    );
                    if !servers.is_empty() {
                        let parents = env.ppid_map();
                        for server in servers {
                            state.poll_server(&env, &server, &parents, &mut pushes);
                        }
                    }
                }
                Inbound::Ctl(CtlMsg::Toggle { server }) => env.toggle(&server.0),
                Inbound::Ctl(CtlMsg::Focus { server, .. }) => env.focus_key(&server.0),
            },
        }

        for (conn, msg) in pushes {
            if let Some(writer) = writers.get_mut(&conn) {
                if write_message(writer, &msg).is_err() {
                    writers.shift_remove(&conn);
                }
            }
        }

        sync_listeners(&mut listeners, &state, &tx);
    }
}

/// The daemon entry point: become the singleton, then serve until killed. Exits 0
/// when another daemon already owns the socket (the always-on invariant is
/// already met). `replace` evicts a live incumbent rather than yielding to it,
/// so a rebuilt binary takes over from the old one.
pub fn run(replace: bool) -> ExitCode {
    let socket = daemon_socket();
    let _ = std::fs::create_dir_all(state_dir());

    let listener = match bind_singleton(&socket, replace) {
        Bind::Listener(l) => l,
        Bind::AlreadyRunning => return ExitCode::SUCCESS,
        Bind::Failed => return ExitCode::from(1),
    };

    // Record our pid so a later `daemon --replace` can find and evict us.
    let _ = std::fs::write(daemon_pidfile(), std::process::id().to_string());

    let (tx, rx) = mpsc::channel::<Event>();
    spawn_acceptor(listener, tx.clone());
    spawn_poller(tx.clone());
    core_loop(rx, tx);

    let _ = std::fs::remove_file(&socket);
    let _ = std::fs::remove_file(daemon_pidfile());
    ExitCode::SUCCESS
}
