# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A tmux plugin (a TPM plugin) that renders a persistent sidebar listing the
session's windows, their panes as a tree, and active AI-agent sessions
(Claude Code, Copilot CLI, ...). It is a single Rust binary, `wrangler`, that
runs in several roles: an always-on daemon holding all state, a thin ratatui
render client per sidebar pane, a hook client the agents invoke, and the
tmux-facing glue commands. They coordinate over one Unix socket. The only
runtime dependency is `tmux ≥ 3.2`.

`wrangler.tmux` (the TPM entry point) keeps the binary at one path,
`$XDG_CACHE_HOME/tmux-agent-wrangler/wrangler-<short-sha>`, whether it was
downloaded or built, so where a binary came from never factors into resolving
it. A commit whose binary is already cached runs it; any other commit obtains
one, preferring the prebuilt release asset for the platform and falling back to
a backgrounded `cargo build --release` whose artifact is copied into the cache.
Keying on the commit is what makes an update resolve to a path that does not
exist yet, so there are no staleness heuristics. Obtaining a new binary runs
`tmux-entry --replace-daemon`, replacing a daemon still running the old code.
An explicit `wrangler` on `PATH` overrides all of this, which is how a developer
runs their own build: a working tree with uncommitted changes still resolves to
the release binary for its commit.

## Running / testing changes

```bash
cargo test           # unit tests plus the golden parity fixtures
cargo build --release
cargo fmt
```

Tests are self-contained: the daemon's tmux access goes through the `TmuxEnv`
trait, so the poll pass and every handler are exercised against a `FakeTmux`
with no live tmux and no real sockets. `spec/fixtures/*.json` are golden
fixtures capturing the behaviour of the original Python implementation; the
parity tests in `color`, `labels`, `daemon::rows`, and `daemon::assoc` assert
against them. They exist to catch drift you did not intend, so a mismatch is a
regression until you can name the change that caused it — when the change is
deliberate, move the goldens with it in the same commit and say so. The
`build_rows` cases assert the drawn line, which is `daemon::rows::build_tree`
composed with `client::render::row_text`: parity is a property of the two
together, so the test deliberately reaches across the layer.

**Never run the side-effecting subcommands against the live tmux server.**
`tmux-entry`, `toggle`, `focus`, and `install-hooks` mutate real state:
they rebind keys, set hooks, start/replace the daemon, and (when
`@wrangler-auto-install-hooks` is on) rewrite the user's `~/.claude/settings.json`
and `~/.copilot/hooks/wrangler.json`. Development happens inside tmux, so `$TMUX`
is set and these do *not* no-op. To exercise a built binary safely, either run it
with `env -u TMUX` (which takes the genuine outside-tmux no-op path) or point it
at a throwaway tmux server on its own socket (`tmux -L test ...`).

To try a change interactively, build it and reload the plugin from your own
tmux (`prefix + I`, or `tmux source-file ~/.tmux.conf`), which re-runs
`wrangler.tmux` and replaces the daemon.

## Architecture

The core design constraint: **one sidebar pane per window**, not one shared
sidebar pane. Switching windows in tmux would otherwise rearrange layouts. Each
window's sidebar is an independent client process. They hold no state of their
own: the daemon owns everything and pushes each client a ready-to-draw row
model, which is what keeps the independent panes behaving as one sidebar.

A second constraint: the daemon is **multi-tenant**. One daemon serves every
tmux server on the machine. The agent registry is global, but a session whose
record carries a pane is scoped to the server that pane lives on, while a
pane-less (daemon-hosted) session is title-matched on every server.

### Roles (`src/main.rs` dispatches on the subcommand)

- **`daemon`** (`src/daemon/`) — the always-on state owner. One process per
  machine, enforced by the socket: `bind_singleton` binds, or connects to
  disambiguate a live incumbent from a stale socket file. `--replace` evicts a
  live incumbent instead of yielding, by reading the pid it recorded in
  `daemon.pid`, signalling it, and waiting for the socket to free (kill, wait,
  and bind happen in the one process, so there is no window where neither
  daemon owns the socket). An incumbent that recorded no pid is left running
  rather than orphaned.

- **`client`** (`src/client/mod.rs`) — the sidebar pane's renderer, spawned as
  `wrangler client` in a split pane. It resolves its server/window/pane once at
  startup, sends `Hello`, then draws whatever `Render` payloads arrive and
  forwards input events. It reconnects if the daemon goes away, so a daemon
  replacement is a blip rather than a dead sidebar.

- **`hook`** (`src/hook.rs`) — invoked by an agent's lifecycle hooks as
  `wrangler hook <agent> <start|end|working|needsAttention|error>` with the hook
  JSON on stdin (it parses both Claude Code snake_case and Copilot CLI
  camelCase). It resolves the reporting server and pane from the environment and
  sends one `HookEvent`. It starts the daemon if none is running.

- **the glue** (`src/glue/mod.rs`) — `tmux-entry` runs at plugin load: it binds
  the toggle/focus keys, patches `automatic-rename-format` so focusing a sidebar
  pane does not rename the window, optionally installs the agent hooks, and
  starts the daemon. It also unsets the global tmux hooks this plugin owns
  (`after-new-window`, `after-break-pane`, `session-window-changed`), since one
  left set by an older release would spawn a second sidebar into a window the
  daemon has already given one. `toggle` and `focus` are bound to keys and drive
  the sidebar panes through server-side tmux operations, so both work even
  before the daemon is up.

### The daemon core

`src/daemon/mod.rs` runs a **single-owner core loop**: all state lives in one
`State`, fed by one mpsc channel and processed serially. Each connection gets a
reader thread that forwards decoded lines as an `Event`; a timer thread emits
`Event::Poll` every second; each watched server's control-mode listener forwards
an `Event::Control`. Nothing else touches the state, so there are no
locks around it. This is std threads and `std::sync::mpsc`, not an async
runtime; the concurrency model is the same either way.

- **`state.rs`** — the authoritative state and every handler (`on_hello`,
  `on_input`, `on_hook`, `on_disconnect`, `poll_server`). All tmux, `ps`, and
  tty access goes through the **`TmuxEnv` trait**, which is the seam that makes
  the whole thing testable; `RealTmux` implements it over `src/tmux.rs`.
  Per-server state (`ServerState`) holds the shared selection and width.

- **`control.rs`** — the control-mode listener, one `tmux -C attach` client per
  watched server, which is what gives a window created while the sidebar is on
  its own sidebar pane. A server is watched for exactly as long as it has
  sidebar clients, the same lifetime as its `ServerState`; the core loop owns the
  listeners beside the client write handles and syncs them against
  `state.servers` after every event. Being watched is a per-server fact and the
  sidebar is toggled per session, so `on_control` asks whether the new window's
  session has one before spawning. The attach carries `no-output` (or every
  byte written by every pane in the attached session arrives as an `%output`
  line) and `ignore-size` (or a client that renders nothing takes part in sizing
  the user's windows). The child's stdin is a pipe the daemon holds and never
  writes to: `tmux -C` exits when its stdin closes, which is what reaps the
  client when the daemon is killed outright. `%window-add` and
  `%unlinked-window-add` are the same event from either side of the attached
  session, so both mean one new window on this server. Parsing is pure
  (`parse_line`) and everything else, command replies included, is dropped by
  the reader before it can reach the core.

- **`assoc.rs`** — which panes and windows are displaying a session. A record's
  recorded pane counts as a placement only when the agent actually occupies it
  (the recorded pid descends from the pane's `#{pane_pid}`, checked against a
  `ps` pid->ppid snapshot). A process launched from a tmux shell into a
  detached/GUI host inherits `TMUX_PANE` without living in that pane, and pane
  ids are reused, so an inherited id must not pin a session to whatever now
  holds it. Ancestry rather than the controlling tty keeps this working on
  macOS as well as Linux. A daemon-hosted session records no pane at all and is
  matched instead by its title against each pane's live title: Claude Code sets
  the pane title to the session title however the session is viewed, so a match
  means that pane is displaying it. A session is filed under the window of
  *every* pane showing it, so it can appear under two windows; one shown in no
  pane is dropped entirely. Title collisions are broken by the recorded pane
  then the cwd, and left unassigned if still ambiguous (better no jump than a
  wrong one). Legacy 2-field registry records have no pid to verify and are
  trusted for back-compat: preserve that when touching the record format.

- **`rows.rs`** — `build_tree` builds the `RowTree` the daemon sends: blocks of
  windows, each with pane and agent children, every node carrying its progress
  indicator, its color and the row id the client echoes back. **It formats
  nothing.** A node's only text is the literal name of the thing — a window's
  name, a pane's title, an agent's label — and the gutter, kind icon, branches
  and index prefix are the client's to compose. A pane hosting an agent contributes that
  agent in place of itself; hosting two contributes two children.
  `ViewMode` selects the grouping only: unified (the default) is one unheaded
  block, and `@wrangler-sections` opts into the window tree followed by a block
  per agent, where an agent's pane appears in both. Rows are drawn identically
  either way, so flipping the option regroups without restyling — and an agent's
  `RowKey::Agent` is the same in both, so activation is unchanged and a
  mid-flight flip falls back through `resolve_selection`. Two independently
  toggled indicator sources: `@wrangler-hook-progress` (default on) draws the
  hook turn state (an animated spinner while working, `●` for attention) and
  `@wrangler-osc-progress` (default off) draws a pane's OSC 9;4 report as a
  state-colored percentage, read from `#{pane_pb_state}` / `#{pane_pb_progress}`
  (empty on a tmux too old to know them, so it degrades to a no-op). OSC wins
  when a pane reports an active state, else the hook glyph.

- **`RowTree::flatten` (`src/model.rs`)** — linearises the tree into display
  rows, deriving the two things that follow from a node's *position*: its
  `Branch`, and its `Placement` — `Here` (the active pane of the active window,
  or that window itself), `Focused` (elsewhere in that window) or `Unfocused`
  (under a window you are not in), which is the one value the client reads both
  the gutter and the row's intensity off. Both ends run it — the daemon to
  resolve the selection, the client to paint — so nav order and paint order are
  the same order by construction. The notification area rides in the `RowModel`
  beside the tree rather than inside it: it is a second region, pinned to the
  foot of the pane and never scrolled, and an entry's *height* depends on the
  width its description wraps to, so only the paint can flatten it. What both
  ends share is `notification_ids` — one id per entry, however many lines it is
  drawn on — so `resolve_selection` and the client's nav run tree-then-area end
  to end in the same order.

- **`client/render.rs`** — the only place a glyph is chosen. `parts` splits a row
  into the tree it hangs off (the `▌` gutter, the `├─`/`└─` branches, the
  index prefix, the heading's spacing and case), the kind icon (`` pane,
  `󱙺` agent, Nerd Font glyphs one column wide) and the name it labels, which
  the icon sits beside rather than out at the margin. `row_text` concatenates
  that split (the daemon's parity tests assert the line it yields) and
  `row_segments` styles it, which is the split's reason to exist: **a child's color goes on its icon and nothing
  else**, because a list of full-width colored rows is unreadable and the icon
  alone ties a row to its thing. Only a window row, having no icon, colors its
  whole line. `base_style` therefore carries intensity (where you are) but no
  child color; `fit_segments` fits the run to the pane width, emptying segments
  from the right and padding the last one so the selection bar spans the width.
  Intensity is the one channel for placement, read straight off it: bold for the
  row you are on, dim for every row of a window you are not in — icons and
  inherited indicators included, so an unfocused window recedes as a block and
  the current one is findable at a glance — and plain for the rest of the window
  you are in.
  The bar (`selection_bar` in `client/mod.rs`) drops the color and the dimming of
  everything it covers: under reverse video both land on what is now the
  background, so a colored icon would paint a block of color across the selected
  row and a dimmed one would wash it out — and since pointing at another window
  is what the sidebar is for, the selected row is usually a dimmed one.
  Turn state is left entirely to the right-edge indicator, which inherits
  `base_style` when it has no state color of its own. A notification entry is two
  contents: a title row drawn as an agent row stripped of the tree (same icon
  column carrying the same color, no gutter, branch or index, because it hangs
  off no window) over dimmed description rows indented to the title's text —
  dimmed and not lightened, because bold is the "where you are" channel and is
  not available to say "detail".
  `notification_lines` (`client/mod.rs`) builds those rows and so decides the
  split between the two regions: it wraps each description to
  `notification_body_field`, admits an entry only if it fits *whole* (never a
  title over a cut-off message), caps the area at a quarter of the pane, and
  yields nothing when that leaves no room for one entry beside the heading. Every
  line of an entry carries the entry's id, so a click anywhere in it opens the
  same thing. The paint records the rows it drew so a click resolves against the
  frame it landed on, and pads a short tree out so the area stays at the foot of
  the pane.

- **`notify.rs`** — the bell (`@wrangler-bell`) and the desktop notification
  (`@wrangler-osc-notify`). Raised by the daemon off the poll, not by the hook,
  because the daemon is what can locate the pane displaying a daemon-hosted
  session. Each attention marker carries a monotonic event token and the
  notifier records the latest token per session, so an event signals exactly
  once. Pane focus does not gate either signal: it says nothing about whether
  the terminal is visible. The `●` still clears when the pane is focused.

- **the notification area** (`@wrangler-notifications`, default on) — a third
  sink on that same attention event, in `state.rs`: `signal_attention` fires the
  bell, the escape and the area's entry off one `should_fire`, so the three can
  never disagree about what fired, and the area fills whether or not the other
  two are enabled. An entry carries the same two strings the escape does — the
  agent as its title, `notification_text` as its message — so what popped up and
  what is listed say the same thing. `ServerState` holds the entries (newest
  first, one per session, `NOTIFICATION_LIMIT` of them) beside the selection, so
  a server's sidebars show and dismiss one area. Every poll re-reads each entry's
  pane and message from the placements and drops one that is displayed nowhere:
  an entry is a live pointer, not a log line, and opening it lands where the
  agent is now. Its key is a `RowKey::Notification` rather than the agent row's
  key, which is what lets `activate` tell "opened the notification" (focus, then
  drop every entry naming the pane just jumped to, the opened one included) from
  "selected that agent" (focus alone). Entries for other panes survive: the jump
  answers only the calls coming from where it landed. The dismissed selection
  then falls back through `resolve_selection` onto the window just jumped to.
  A focused pane's entries are dropped in the same poll pass that acknowledges
  its `●`, so the two clear together — which also means an event raised by the
  pane you are already in is cleared in the poll that recorded it, and never
  appears.

- **`persist.rs`** — the registry snapshot, one file per session under
  `sessions/` in the state dir (`$XDG_STATE_HOME/tmux-agent-wrangler`, default
  `~/.local/state/...`). This is the daemon's only on-disk state; turn markers,
  selection, and width live in memory. The socket (`daemon.sock`) and pidfile
  (`daemon.pid`) sit alongside it.

### The wire protocol (`src/proto.rs`)

Newline-delimited JSON. Inbound messages arrive as an untagged `Inbound`
envelope resolved by disjoint `type` tags into `Client`, `Hook`, or `Ctl`;
outbound the daemon pushes `Render` (the row tree), `Width`, and `Exit`.
Selection is carried as an **absolute** row id, never a relative movement, so
clients cannot drift out of sync. That id is opaque to the client, which only
echoes back the one riding on the row it acted on; the daemon mints it and is
the only side that reads its variants.

### Width sync (`@wrangler-sync-width`, `@wrangler-min-width`, `@wrangler-max-width`)

Owned by the client, and shaped around the fact that only one sidebar is
resized at a time in practice. A *user* resize (tmux resized the pane) is
clamped to the `WidthBounds` (whose ceiling is raised to its floor on
construction, so a max below the min is simply the min) and published to the
daemon, which relays a
`Width` message to the other clients on that server; they adopt it. A resize the
client asked for itself is swallowed via a recorded `pending` width, so an
adopted width never echoes back as a new user resize. The result is one "lead"
client with the others following.

### Sidebar lifecycle

`toggle` spawns one sidebar per window and kills every sidebar pane on the
server; between those two, a window created while the sidebar is on gets its
sidebar from the daemon's control-mode listener, however that window came into
being (tmux reports `break-pane` and a new session's first window as the same
window-add). Whether it gets one is asked of the window's own *session*: the
listener spans the server, but the tree a sidebar draws is the server's current
session, so a pane put into a session the sidebar was never turned on for would
show rows that are never about it.

A sidebar must never be alone in a window (it would expand to full width and
keep an empty window alive). The daemon pushes `ServerMsg::Exit` when a client's
window has no real panes left: immediately on the resize that the closing pane
triggers, and again from the poll as a backstop. That resize is deliberately
*not* relayed as a width update, or the closing sidebar would drag every other
sidebar wide before it exits. Clients also self-exit on a spawn race, when a
lower-numbered sidebar pane occupies the same window.

The ~1s worst case on the poll backstop is inherent to polling: the listener
reports the window that appeared, not the layout change that emptied one.

### Hook installation (`src/glue/install.rs`)

Installs (or `--uninstall`s) the `wrangler hook` invocations into each agent's
config so users need not hand-edit them. It renders
`scripts/hooks-manifest.json` — the declarative per-agent `event -> [action]`
map, embedded at compile time with `include_str!`. An event value is either a
list of action strings (one hook group, no matcher) or a list of
`{matcher, actions}` objects (one group each). Two formats: `claude` merges
non-destructively into the shared `~/.claude/settings.json` (replacing only
wrangler's own hook groups, keyed on the hook command, preserving a
`.wrangler.bak` backup); `copilot` writes the dedicated
`~/.copilot/hooks/wrangler.json` it owns outright. A command written by an older
release (the legacy `agent-hook.sh` script) is recognised so it is upgraded
rather than duplicated. Idempotent. Adding an agent event is one line in the
manifest; a new agent whose config differs needs a new format handler.

### Release binaries

`.github/workflows/build-binaries.yml` builds on every push to a tracked branch
(TPM updates a plugin by pulling it, so each commit needs matching binaries) and
publishes a prerelease **named the commit's 7-character short SHA** carrying
`wrangler-linux-x64` (musl, static) and `wrangler-macos-arm64`. `wrangler.tmux`
derives that same short SHA from its checkout to fetch the right assets, and the
owner/repo from the `origin` remote so a fork fetches its own releases. Keep the
asset names and the short-SHA release name in step with the wrapper.

## Conventions

- The `@wrangler_sidebar` pane option marks sidebar panes; check it (never the
  pane command) to tell sidebars from real panes.
- User-facing tmux options are all prefixed `@wrangler-`; document new ones in
  the README's Options section.
- Prefer types that make invalid states unrepresentable over runtime checks: the
  domain vocabulary in `src/model.rs` (`ServerKey`, `SessionKey`, `RowKey`, ...)
  exists so the daemon, client, and protocol cannot confuse one id for another.
