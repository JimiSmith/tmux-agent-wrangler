# Daemon runtime design (the socket server + event loop)

This is the design for the last big integration piece: the daemon's runtime
shell. Everything it depends on (association, labels, notify, rows, tmux, proto)
already exists and is fixture-tested. This document covers only the *runtime*:
concurrency model, state, the event loop, lifecycle, persistence, and the parts
that can only be validated against a live tmux.

It refines, and does not contradict, `spec/architecture.md`.

## Concurrency model: a single-owner core, message-driven

All mutable state lives in **one owner task**. Nothing else touches it. Every
input becomes an `Event` delivered to that task over one `mpsc` channel, and the
task processes events **serially**. There are no locks on daemon state.

This mirrors the Python's single-threaded model exactly, which is what makes the
fixture-tested pure logic (`fetch_agent_sessions`, `build_rows`, `Notifier`)
drop in unchanged: they run inside the core, on one thread of control, in a
deterministic order.

```
        ┌──────────────────────────── tokio runtime ────────────────────────────┐
        │                                                                        │
  UnixListener accept ─┐                                                         │
                       │  spawn per-connection READER task ──┐                   │
  poll interval (1s) ──┤                                     │  Event            │
                       │                                     ▼                   │
                       └──────────────────────────────►  mpsc<Event>  ──►  CORE  │
                                                                            (owns │
                                                                             all  │
                                                                            state)│
                          per-connection WRITER task ◄── mpsc<ServerMsg> ◄──┘     │
                       (one per client; core holds the Sender)                    │
        └────────────────────────────────────────────────────────────────────────┘
```

- **Reader task** (one per accepted connection): reads newline-JSON frames with
  `proto::read_message`, tags each with a `ConnId`, forwards it to the core as
  an `Event`. On EOF/error it sends `Event::Disconnected(ConnId)` and exits.
- **Writer task** (one per *client* connection only): owns the write half and a
  `mpsc::Receiver<ServerMsg>`; the core pushes a `ServerMsg` to render. Hook and
  ctl connections are request/exit and need no writer.
- **Core task**: the `match` over `Event`. Owns `State`. The only place tmux is
  called, transcripts are read, notifications fire, and `RowModel`s are built.

Why not `Arc<Mutex<State>>` shared across connection tasks? Because the poll
loop and every connection would contend on one lock around almost all work, and
the ordering guarantees the parity fixtures assume (sorted registry drives row
order; one poll's association is a single snapshot) are exactly what a serial
core gives for free. The actor model is less code and no deadlock surface.

## The `Event` enum (the core's whole input)

```
enum Event {
    Connected   { conn: ConnId, kind: ConnKind, writer: Option<Sender<ServerMsg>> },
    Client      { conn: ConnId, msg: ClientMsg },   // Hello / Input / Bye
    Hook        (HookMsg),                            // HookEvent
    Ctl         { conn: ConnId, msg: CtlMsg },        // Toggle / Focus
    Disconnected(ConnId),
    Poll,                                             // 1s interval tick
}
```

`ConnKind` is learned from the first frame (a `Hello` is a client, a `HookEvent`
a hook, a `CtlMsg` ctl) — or the connection can be typed by which decoder its
reader uses. Simpler: the reader reads a single generic `Inbound` envelope. See
"Framing" below.

## State (owned by the core, in memory)

```
struct State {
    servers:  IndexMap<ServerKey, ServerState>,   // per tmux server
    registry: IndexMap<SessionKey, RegistrySession>, // GLOBAL: no server affinity
    notifier: Notifier,                            // in-memory attention dedup
    labels:   LabelCache,                          // mtime-keyed sticky session meta
    clients:  IndexMap<ConnId, ClientConn>,        // connected sidebars
    dirty_registry: bool,                          // needs a snapshot rewrite
}

struct ServerState {
    selection: Option<RowKey>,   // shared cursor for this server's sidebars
    width:     Option<u16>,      // shared width target for this server
    last_windows: Vec<Window>,   // last poll snapshot (for change detection)
}

struct ClientConn {
    server: ServerKey,
    window: WindowId,
    pane:   PaneId,
    cols:   u16,
    writer: Sender<ServerMsg>,
    last_pushed: Option<RowModel>,  // push/diff: only send on change
}
```

`RegistrySession` (already in `assoc.rs`) carries the record, the turn status,
and the attention token. The registry is the only thing persisted.

## Handling each event

- **Hook(HookEvent)** — the turn-state driver, replacing every `agent-hook.sh`
  file write:
  - `start` / any event with a missing record → self-register (record from the
    event's pane/agent/pid/cwd/transcript). `end` → remove the session.
  - `working` → status Working. `needsAttention`/`error`(non-recoverable) →
    status Attention + store the event token. `error`(recoverable) → stays
    Working. (This is `hook::normalize_event`, already written.)
  - Mark `dirty_registry` on any add/remove.
  - Then rebuild + push (an attention event must reach the screen sub-100ms, not
    on the next poll). The rebuild is the same path Poll runs.
- **Client Hello** — record the `ClientConn`; adopt the server's width if set,
  else this client's `cols` seeds it; build this window's `RowModel` and push.
- **Client Input** —
  - `Select{key}` → set `servers[server].selection = Some(key)`; push to *all*
    that server's clients (shared cursor).
  - `Activate{key}` → set selection, then resolve the key to a tmux target and
    `select-window`/`select-pane -t` on that server (via `tmux::focus`); push.
  - `Resize{cols}` → width state machine (user resize vs relayout vs own
    request); update `servers[server].width`; push to the server's clients.
  - `FocusGained`/`FocusLost` → wake only; **never** sets `has_focus`
    (`has_focus` is a tmux fact computed at build time from the active
    pane/window). Recompute + push.
- **Ctl Toggle{server}** — if the server has sidebar panes, kill them all; else
  clear width and spawn one per window (via `tmux::toggle`/`spawn`).
- **Ctl Focus{server,window}** — `tmux::focus` that window's sidebar pane, then
  the `C-l` repaint nudge.
- **Poll** — the heartbeat, once per second, per known server:
  1. `tmux::fetch_windows(socket)` → `Vec<Window>` (fail-soft empty ⇒ server is
     gone ⇒ drop its partition and its clients).
  2. `fetch_agent_sessions(registry, windows, ps_snapshot, labels)` →
     `(Vec<Session>, dropped_keys)`; prune dead-pid / pane-gone records
     (`pid_alive`), mark `dirty_registry`.
  3. `notifier` pass: for each attention placement, `should_fire(token)` → bell
     + OSC to the resolved ttys; then `acknowledge_focused_attention` clears a
     focused one. `notifier.retain_live(live_ids)`.
  4. Per window: `build_rows(...)` → `RowModel{rows, selection, has_focus}`.
  5. **Push/diff**: send `Render` to a client only if its `RowModel != last_pushed`.
  6. If `dirty_registry`, rewrite the snapshot; clear the flag.
- **Disconnected(conn)** — drop the `ClientConn`. A server with no clients still
  polls (the daemon is always-on); a *server* is dropped only when tmux reports
  it gone.

The spinner is **not** a daemon concern: `build_rows` emits a semantic
`Indicator::Progress{pct:None}` and the client resolves the frame locally, so the
daemon does not tick faster than 1s and does not push for animation.

## Framing on a shared inbound channel

Each connection's first frame decides its kind. Cleanest with the existing
typed enums: read the raw JSON line once, then try to decode as `ClientMsg`,
`HookMsg`, `CtlMsg` in turn — but the tags overlap only if we are careless.
Concrete plan: a single `Inbound` untagged-by-us envelope is avoided; instead
the reader is generic over `M: DeserializeOwned` and the **connecting binary
declares its kind in the first line** via a one-byte-cheap discriminant already
present: `ClientMsg`/`HookMsg`/`CtlMsg` each have a distinct `type` set
(`hello|input|bye` vs `hook_event` vs `toggle|focus`). So one `enum Inbound {
Client(ClientMsg), Hook(HookMsg), Ctl(CtlMsg) }` with `#[serde(untagged)]` over
those three decodes any inbound line unambiguously (disjoint `type` values).
The reader decodes `Inbound` and forwards. Simple and keeps proto typed.

## Lifecycle: singleton + detach

- **Bind-or-connect singleton.** `wrangler daemon` tries to `bind()` the socket
  at `paths::daemon_socket()`. Success ⇒ it is the daemon. `EADDRINUSE` ⇒ a live
  daemon already owns it ⇒ exit 0. A *stale* socket (file exists, `connect()`
  refused) ⇒ unlink and rebind. This is the whole "no double start" guard; no
  pidfile needed.
- **Detach.** The hook/ctl/tmux-entry that first needs a daemon spawns
  `wrangler daemon` **detached**: `setsid` + a double-fork (or
  `Command::new(...).setsid()` via a pre-exec) with stdio to `/dev/null`, so it
  outlives the tmux server that launched it and never holds a pane. Started at
  plugin load *and* lazily by the hook (the always-on invariant): whoever finds
  the socket unbound starts it.
- **Shutdown.** No explicit stop. The daemon runs until killed. On any exit it
  best-effort unlinks its socket. It keeps running with zero clients and zero
  servers (a fresh login re-attaches).

## Persistence (the only on-disk state)

- One **snapshot** file (path under `state_dir()`): the global registry, current
  5-field records, one per line — the exact `serialize_registry_record` framing,
  so a human `cat` and the parity fixtures both hold. Written on change
  (`dirty_registry`), which is low frequency (session start/end/prune).
- Loaded once on start: parse each line with `parse_registry_record` (current
  format only; a malformed line is dropped, never fatal). Then the first poll
  reconciles it against live tmux + ps and prunes anything dead.
- Selection/width are **not** persisted (they are per-server ephemeral UI state).

## Self-exit / pruning parity

The Python sidebar self-exited when its window lost real panes or a
lower-numbered sidebar raced it. In the client-server model those move:

- **Duplicate-sidebar race** → the *client* still self-exits if a lower-numbered
  sidebar pane shares its window (kept client-side; it owns its pane identity).
- **Window emptied** → tmux closes the window; the client's pane goes with it and
  its reader hits EOF ⇒ `Disconnected`. No daemon action beyond dropping it.
- **Dead agent pid / pane gone** → the daemon prunes the registry record in the
  Poll pass (`pid_alive`, pane-absent), replacing the Python sidebar's prune.

## New deps to add to Cargo.toml

```
tokio      = { version = "1", features = ["rt-multi-thread","macros","net","time","sync","io-util","signal"] }
```
(ratatui + crossterm are added in the client stage, not this one.) The core is
happy on a current-thread runtime too; multi-thread costs nothing here and keeps
the writer tasks off the accept path.

## What this stage delivers / how it is checked

- `src/daemon/mod.rs` — the runtime: `run()`, accept loop, reader/writer tasks,
  the core `match`, singleton bind, detach spawn helper.
- `src/daemon/state.rs` — `State`, `ServerState`, `ClientConn`, the event
  handlers (thin wrappers over the existing pure fns), the poll pass.
- `src/daemon/width.rs` — the resize state machine (client reports real size;
  daemon holds/broadcasts the target).
- `src/daemon/persist.rs` — snapshot load/save (record framing reused).
- Unit-testable without a socket: the poll pass and every `Event` handler are
  pure functions of `(State, Event, tmux-snapshot) -> (State, outbound pushes)`,
  so the core is tested by feeding events and asserting the resulting pushes and
  registry — no live tmux, no real sockets. Socket framing is already tested in
  `proto`. Live-tmux validation (singleton, detach, real toggle/focus, real
  notifications) is the hands-on check after client + glue land.

## Open decisions (need sign-off before coding)

1. **Poll transport.** Start with the 1s `fetch_windows` poll (parity with
   today). tmux control-mode push is a later optimization, explicitly out of
   scope for this stage. — proceed?
2. **tmux-snapshot injection for testability.** Make the core take the poll's
   tmux/ps data through a small trait (or a fn pointer) so tests feed canned
   snapshots and no test shells out. Adds one seam; worth it. — proceed?
3. **Detach mechanism.** `setsid` via a `pre_exec` closure (unix-only, we are
   unix-only) rather than a hand-rolled double-fork. Simpler, same effect. — ok?
```
