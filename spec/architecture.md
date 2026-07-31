# tmux-agent-wrangler — Rust port architecture (client-server)

## Status

This document is the authority on the **process and coordination architecture**
of the Rust port. It supersedes the architecture and build-order of the WF1
synthesized plan (`spec/specs/_wf1_plan.json`), which faithfully mapped the
*current* file-based, one-sidebar-process-per-window design.

The WF1 **behavioral** specs (`spec/specs/<module>.json`) and the **603 golden
fixtures** (`spec/fixtures/*.json`: color 342, labels 72, assoc 63, rows 126)
remain authoritative for behavior. Only the process/coordination model is
corrected here to the agreed client-server target. Where a WF1 spec describes
file/flock coordination, this document overrides it; where it describes pure
logic (color math, label composition, association algorithm, row building), it
still holds.

## Components

- **daemon** (single, always-on, multi-tenant across tmux servers; see
  Multi-server model). Started at plugin load and detached from the spawning
  server so it outlives it; persists even when the sidebar is toggled off. The
  only process that talks to tmux or reads transcripts. Owns all state in
  memory: the agent-session registry, each
  session's turn state (working/attention), the shared selection, the width
  target, and the attention-dedup set. Runs one global tmux poll (a later phase
  may add control-mode push), computes association/labels/colors/rows once,
  builds a per-window `RowModel`, and pushes it to that window's client. Raises
  the bell and desktop notifications. Persists only the registry, to a snapshot
  file.
- **client** (ratatui + crossterm, one per window sidebar pane). Thin: connects
  to the daemon socket, sends `Hello{window,pane,size}`; receives a `RowModel`
  for its window and paints it; animates the spinner locally; sends input
  (nav/click/resize/focus) back. No tmux calls, no state files, no polling. The
  render path is the existing prototype.
- **hook** (`wrangler hook`, replacing `agent-hook.sh`). Parses the hook JSON on
  stdin, sends one `HookEvent` to the daemon, exits. No file writes. If the
  daemon is not running it starts it (the always-on invariant), then sends.
- **ctl** (`wrangler toggle|focus`, bound by tmux). Sends a command to the
  daemon, which spawns/kills/focuses the per-window sidebar panes.

The "one sidebar pane per window" constraint is physical and unchanged: each
window still needs its own pane running a client. What changes is that the
clients are now thin renderers coordinated by one daemon, instead of N
independent processes each polling tmux and the filesystem.

## Data flow

hooks + tmux events -> daemon (authoritative state) -> diff -> push `RowModel`
-> the affected window's client. Selection and width are daemon state broadcast
to a server's clients. Attention dedup is a single in-memory check. Turn-state
and notifications become push-immediate (sub-100ms) instead of up-to-1s
poll-driven.

## Multi-server model

One daemon serves every tmux server, because the one piece of genuinely global
state, the agent-session registry, has no server affinity: a daemon-hosted
(pane-less) Claude session is located only by title-matching against whatever
panes show it, on whichever server. Keeping that registry, the title-matching,
and the notification dedup in one process avoids the cross-process flock
coordination that per-server daemons would need for exactly that case.

The price is that the daemon is explicitly multi-tenant and never leans on an
ambient `$TMUX`:

- **Server identity is the `$TMUX` socket path** (its first field). Every client
  and hook reports it (`Hello`/`HookEvent`); a pane-less hook reports none and
  its session is title-matched across all connected servers.
- **Every tmux command is issued with `-S <socket>`** for the target server; the
  daemon never runs a bare `tmux`. The daemon is detached (setsid/double-fork) so
  it survives the exit of whichever server first spawned it.
- **All tmux-facing state is keyed by `(server, ...)`.** Pane ids are per-server
  (`%5` in one server is not `%5` in another), so associations, the row model,
  and pushes partition by server socket. The registry stays global; *placement*
  is per-server via pane/title matching.
- **Selection and width are per tmux server:** shared among that server's
  sidebars, independent across servers. This fixes today's latent global-file
  coupling (two servers sharing one cursor/width).
- **toggle / focus / notify target the requesting server:** ctl reports its
  socket; the daemon spawns/kills/focuses panes and writes notifications on that
  server only.
- **Singleton across servers:** the first server to load the plugin spawns the
  daemon; every other server's `tmux-entry` finds the socket already bound and
  just connects. A server exiting (its socket/commands go dead, which the
  fail-soft `run_tmux` reads as empty) drops that server's partition.

## File-state: dropped vs kept

Dropped (replaced by in-memory daemon state + the socket):

- `attention/`, `working/` (turn markers) -> in-memory per-session status
- `selection` (shared row) -> daemon state, per server, pushed to that server's clients
- `width` (shared width) -> daemon state, per server, pushed to that server's clients
- `notified/` + flock (single-fire dedup) -> in-memory dedup map

Kept:

- The agent-session **registry** -> a single persistence **snapshot** file the
  daemon loads on start and rewrites on change (low frequency: session
  start/end). No legacy/back-compat record formats: only the current record is
  supported. This is the only on-disk state.

## Crate layout

Single binary `wrangler` (edition 2021); tokio + ratatui + crossterm + serde +
serde_json. argv dispatch: `daemon | client | hook | toggle | focus | spawn |
install-hooks | tmux-entry`.

```
src/main.rs        subcommand dispatch (thin)
src/model.rs       shared domain types: Window, Pane, Session, RowModel, RowKey,
                   Indicator, ProgressState. MUST land first (WF1's #1 blocker:
                   the pure band references these types but WF1 never homed them).
src/proto.rs       WIRE protocol (client<->daemon, hook->daemon, ctl->daemon):
                   message enums + newline-delimited JSON framing. Hello,
                   HookEvent, and ctl messages carry the sender's tmux server
                   socket (from $TMUX) so the daemon can scope state and target
                   tmux commands per server. AND the registry snapshot
                   serialize/parse, including the legacy 2-field read. NOT the
                   old marker/selection/width files.
src/color.rs       palette tables + rgb_to_ansi256 + read_theme + theme_palette.
                   Pure; the curses pair allocation moves into client terminal
                   setup as a crossterm equivalent.
src/labels.rs      transcript scan_tail / workspace.yaml parse / agent_label /
                   label_mode, plus the mtime-keyed sticky session-meta caches.
src/tmux.rs        run_tmux choke point (fail-soft: empty string, never
                   propagate), fetch_windows, window_real_panes,
                   strip_status_prefix, and the spawn/toggle/focus commands.
src/daemon/
  mod.rs           socket server, per-client registry, always-on singleton,
                   the event loop, the global poll, push/diff, self-exit rules.
  state.rs         authoritative model assembly; build_rows -> semantic RowModel.
  assoc.rs         ppid_map, process_under, parse_registry_record, the two-pass
                   association (now over the in-memory registry + one snapshot).
  rows.rs          build_rows, indicator_for, semantic row construction.
  notify.rs        in-memory attention dedup, bell, OSC 777/9 escapes, tty write.
  width.rs         width-target reconciliation (target is daemon state).
  persist.rs       registry snapshot load/save.
src/client/mod.rs  ratatui render (from prototype), input, spinner animation,
                   terminal lifecycle, reconnect/degrade if the daemon dies.
src/hook.rs        parse stdin JSON + normalize event -> HookEvent socket message.
src/glue/          tmux-entry (bindings, hooks, automatic-rename guard),
                   install-hooks (manifest-driven, JSON-parity critical),
                   toggle/spawn/focus routed through the daemon.
```

## Module homes and which WF1 spec/fixtures apply

- **color.rs** — WF1 color spec applies; drop the `init_agent_colors` curses pair
  allocation (it becomes crossterm terminal setup in the client). Fixtures
  `color.json` (342) apply: `rgb_to_ansi256`, `theme_palette`, `read_theme`, the
  pair-index lookups.
- **labels.rs** — WF1 labels spec and `labels.json` (72) apply unchanged.
- **assoc.rs** — WF1 assoc spec and `assoc.json` (63) apply (`process_under`,
  `parse_registry_record`, `scan_tail` bits). `fetch_agent_sessions` becomes a
  daemon method over the in-memory registry plus one tmux/ps snapshot instead of
  per-window filesystem reads; the association *algorithm* is unchanged.
- **rows.rs** — WF1 rows spec and `rows.json` (126) apply. `build_rows` now emits
  a `RowModel` pushed over the wire; the client paints it. This matches the
  prototype's split: the daemon sends a semantic `Indicator`, the client resolves
  the spinner frame and `fit` at paint time.
- **proto.rs** — WF1 proto spec applies *partially*. Keep: registry record
  serialize/parse (current format only), session-id/key helpers, `parse_field_int`
  (the ASCII-isdigit gate), and ns-token minting. Drop: marker read/write,
  selection tab-framing, the flock `claim_attention`, `prune_notified`. Add
  (new): the socket message enum and framing.
- **notify.rs** — the *when to signal*, dedup semantics, and bell/OSC escape
  building from the WF1 notify spec apply, but the cross-process flock dedup
  collapses to one in-memory map, because there is now a single daemon. The tty
  and OSC writes are unchanged.
- **width.rs** — the WF1 width state-machine logic applies, but the shared width
  is daemon state, not a file. The user-resize vs relayout vs own-request
  distinction stays client-side (the client owns its real size) and is reported
  to the daemon, which holds and broadcasts the target.
- **hook.rs** — the WF1 hook spec's payload parsing, event normalization,
  session-id sanitize, and recoverable-Copilot rule apply, but the *output* is a
  socket `HookEvent`, not registry/marker file writes. The ps ancestry walk that
  finds the agent pid moves to the daemon, which owns the registry.
- **glue** — the WF1 glue/install-hooks spec applies unchanged (install-hooks
  JSON parity, tmux bindings/hooks/rename guard). toggle/focus/spawn now route
  through the daemon.

## Parity risks

Carry (architecture-independent, from the WF1 risk list):

- Python `isdigit`/`round`/`splitlines`/`isalnum` semantics; the
  `(202,138,4) -> 178` gold-rounding case; a full 0..255 `rgb_to_ansi256` sweep.
- `run_tmux` must fail soft (return stdout-or-empty, never propagate); same
  best-effort swallow on every tty write, resize-pane, and notification.
- Numeric pane-id comparison (strip the leading `%`, compare as integers); pane
  ids stay opaque strings everywhere else.
- Deterministic iteration order: sorted registry order drives candidate/row order
  and title-collision tiebreak; tmux emission order drives the pane tree; use
  ordered `Vec`/`IndexMap`, `HashSet` only where order is irrelevant.
- Installer JSON parity: `ensure_ascii` escaping, 2-space indent, exactly one
  trailing newline, insertion-ordered keys, `shlex.quote` byte-for-byte,
  per-format file modes and backup, all-or-nothing wrangler-group replacement.
- Sticky label caches (mtime-keyed, keep-previous-on-empty); Claude float
  `getmtime` vs Copilot `st_mtime_ns` kept as distinct types compared exactly.
- Byte-exact registry record framing (strip only trailing `\n`, exactly five
  fields with genuine empty fields preserved, any other field count rejected)
  for the snapshot.
- curses -> crossterm traps: the COLORS>=256 palette gate, default-color
  background, immediate button-down (mouseinterval 0), mode-1004. Note the
  deliberate divergence: crossterm `FocusGained`/`FocusLost` is a wake/redraw
  trigger only and must not set `has_focus`; `has_focus` is a tmux fact (the
  active pane of the active window). The prototype currently sets it from the
  crossterm event; the real client must not.

Void under client-server (no longer relevant):

- flock `LOCK_EX` single-fire dedup (was cross-process) -> in-memory map.
- selection/width/marker/notified file framing and their races.
- per-window redraw gating by deep-equality over a cached 7-tuple: the daemon
  diffs and pushes; the client repaints on push plus the local spinner tick.

New (client-server, become WF4 audit targets):

- Socket protocol framing and versioning; client reconnect/degradation when the
  daemon dies.
- Always-on daemon singleton (bind-or-connect, socket/pidfile guard, no double
  start).
- Push/diff correctness: a window's `RowModel` is pushed only when it changed.
- Registry snapshot persistence: survive a daemon restart; still prune records
  whose pid is dead or whose pane is gone.
- Multi-server tenancy: state keyed by `(server, ...)`, every tmux command run
  with `-S <socket>`, no ambient `$TMUX`; a detached daemon that survives its
  spawning server; per-server partition teardown when a server exits; pane-id
  collisions across servers never conflated (key by `(server, pane)`).

## Build order

- **WF2 (pure, parallel, fixture-tested):** `model`, `proto` (record-parse core
  only), `color`, `labels`, `rows`, `assoc`. All architecture-independent; their
  fixtures apply directly.
- **WF3 (integration, sequential, build-gated):** `proto` (socket layer) ->
  `tmux` -> `daemon` (state + assoc + rows + notify + width + persist wired into
  the loop and the socket server) -> `client` (the prototype, now over the
  socket) -> `hook` -> `glue`.
- **WF4 (audit, parallel):** run all fixture tests + prototype `--snapshot`
  goldens; a completeness critic against every WF1 edge case; and the new
  client-server checks (reconnect, singleton, push/diff, persistence).
