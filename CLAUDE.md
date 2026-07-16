# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

A tmux plugin (a TPM plugin) that renders a persistent sidebar listing the
session's windows, their panes as a tree, and active AI-agent sessions
(Claude Code, Copilot CLI, ...). Everything is bash glue plus one Python
curses TUI. There is no build step, no dependency manager, and no test suite;
the runtime dependencies are `tmux ≥ 3.1` and `python3` with stdlib `curses`.

## Running / testing changes

There is no test harness. Exercise changes inside a live tmux session:

```bash
# Load the plugin into the running tmux server (re-run after editing wrangler.tmux)
tmux run-shell "$PWD/wrangler.tmux"

# Toggle the sidebar directly without going through the key binding
scripts/toggle.sh

# Spawn a sidebar in the current window only
scripts/spawn.sh
```

The sidebar is `python3 scripts/sidebar.py`; it must run inside a tmux pane
(it reads `$TMUX_PANE`). To see tracebacks, run tmux from a terminal so the
pane's stderr is visible, or temporarily wrap the loop.

State lives under `$XDG_STATE_HOME/tmux-agent-wrangler` (default
`~/.local/state/tmux-agent-wrangler`): `sessions/` (agent registry, one file
per session), `attention/` and `working/` (turn-state markers, one file per
session that mirrors its `sessions/` filename), `notified/` (one flag per
session id, the single-fire dedup for the bell/notification, see
`notify_attention`), `selection` (shared highlighted row), and `width` (shared
sidebar width). Deleting this directory resets all cross-pane state.

## Architecture

The core design constraint: **one sidebar pane per window**, not one shared
sidebar pane. Switching windows in tmux would otherwise rearrange layouts.
Each window's sidebar is an independent `sidebar.py` process; they coordinate
only through files in the state dir, so most complexity is about keeping those
independent instances behaving as one.

- **`wrangler.tmux`** — TPM entry point. Binds the toggle key (`@wrangler-key`,
  default `Tab`, bound with prefix) and the focus key (`@wrangler-focus-key`,
  default `a`, bound with prefix) and installs `after-new-window` /
  `after-break-pane` hooks so windows created while the sidebar is on get their
  own sidebar. Also patches `automatic-rename-format` so focusing the sidebar
  pane (command `Python`) does not rename the window.

- **`scripts/focus.sh`** — bound to the focus key. Selects the current window's
  sidebar pane (found via the `@wrangler_sidebar` option); a no-op if the window
  has no sidebar, so it never spawns one. After selecting, it sends `C-l` to the
  sidebar to force an immediate repaint — the guaranteed path when a terminal or
  config leaves `focus-events` off and the mode-1004 report never arrives.

- **`scripts/toggle.sh`** — the on/off switch. If any sidebar pane exists, kills
  all of them; otherwise clears the shared width and spawns one sidebar per
  window. Sidebar panes are tagged with the pane option `@wrangler_sidebar 1`,
  which is the single source of truth for "is this a sidebar" everywhere.

- **`scripts/spawn.sh`** — splits a left-hand sidebar pane into one window and
  tags it. `--if-active` makes it a no-op unless the session already has
  sidebars (used by the new-window hooks so sidebars only auto-spawn when
  toggled on).

- **`scripts/sidebar.py`** — the TUI and all interactive logic. Polls tmux on a
  1s timeout, redraws, and handles keys/mouse. Key responsibilities:
  - **Self-exit conditions** (so tmux can close windows / avoid duplicate
    sidebars): exits if its window has no real panes left, or if a
    lower-numbered sidebar pane also occupies its window (a spawn race).
  - **Focus reporting** (mode 1004): enables `ESC[?1004h` on start and disables
    it on exit. The terminal's `ESC[I` / `ESC[O` focus reports arrive as an
    unrecognised key code (curses assembles them into one code, or they fall
    through as raw bytes); either way they wake the blocking `getch()`, so the
    loop redraws and the focus-only selection highlight appears/clears the
    instant focus changes rather than on the next poll. It deliberately does
    *not* special-case a raw `ESC`: an arrow key arriving right after a focus
    report can fragment into a bare `ESC` + `[` + letter, and swallowing it
    would eat the keypress. tmux only delivers these reports when the user has
    set `focus-events on` (the plugin does not set it); without it the highlight
    falls back to the poll, and `focus.sh`'s `C-l` nudge always covers the focus
    key.
  - **Shared selection**: the highlighted row is written to / read from the
    `selection` file every tick, so all sidebars highlight the same logical
    row and Enter/click on any of them focuses the same target. An agent row's
    selection key is `("a", session_id, pane)` — the pane is part of the key
    because one session can be placed under several windows at once (see agent
    association), and `main`'s nav/activate assume unique keys.
  - **Agent association** (`fetch_agent_sessions`): a session's registry record
    carries the pane captured at hook time, but a daemon-hosted (background)
    session has none — no `TMUX_PANE` when its hook ran, and no process/env link
    back to a pane. Such a session is associated by matching its title
    (`session_meta`) against each pane's live title (`pane_titles` from
    `fetch_windows`, glyph-stripped by `strip_status_prefix`): Claude Code sets
    the pane title to the session title however the session is viewed (`claude
    attach`, `--resume`, or the agents view), so a match means that pane is
    displaying the session. A session is filed under the window of *every* pane
    showing it (recorded-if-local ∪ title-matched), so it can appear under two
    windows; one shown nowhere stays detached under "Agents". Title collisions
    are broken by the recorded pane then the cwd, and left unassigned if still
    ambiguous (better no jump than a wrong one); empty titles never match.
  - **Progress indicators** (`progress_indicator`): a single glyph/percentage
    pinned to each row's right edge, from two independently-toggled sources.
    `@wrangler-hook-progress` (default on) draws the hook turn state (an animated
    spinner while working / `●` attention). `@wrangler-osc-progress` (default
    off) draws an app's OSC 9;4 report as a state-colored percentage, read from
    the `#{pane_pb_state}` / `#{pane_pb_progress}` pane vars in `fetch_windows`
    (empty on a tmux too old to know them, so it degrades to a no-op). OSC wins
    when a pane reports an active state (tmux 3.7 uses `hidden` for none, and
    names OSC state 4 `paused`), else the hook glyph. Both render in
    the window tree (per pane, keyed off `pane_progress` / `pane_status`) and
    the agents section. `draw()` gives the indicator its own color pair (green/
    yellow/red per state) so it stands out from the row's own color. The busy
    glyph (hook `working`, OSC `indeterminate`) is a spinner (`spinner_frame`):
    `main` advances a `frame` counter on a sub-second timer independent of the
    1s data poll — between polls it only re-runs `build_rows` on the cached poll
    data and repaints, and the fast timer engages only while a spinner is on
    screen, so an idle sidebar still just blocks for the poll interval.
  - **Attention signals** (`notify_attention`): the bell (`@wrangler-bell`) and
    the OSC desktop notification (`@wrangler-osc-notify`: `off` | `777`/`on` |
    `9`) are raised here off the poll, not by the hook — reacting to the same
    attention state as the `●` glyph, so a session whose pane is focused (marker
    already cleared by `fetch_agent_sessions`) never fires. This is what lets a
    daemon-hosted session signal at all: the hook has no pane for it, but the
    sidebar resolves one via title matching. Because there is one sidebar per
    window (and a session can sit under several windows), the fire is gated on an
    atomic create of a `notified/<id>` flag, so exactly one sidebar signals per
    episode; the flag is pruned once the attention marker is gone, rearming the
    next. The notification (built as `<window> · <label>`, OSC 777 also carrying
    the agent name as its title) goes to each client tty of this tmux session; the
    bell writes BEL to the matched pane's tty. With the sidebar off, nothing
    signals.
  - **Width sync** (`@wrangler-sync-width`, `@wrangler-min-width`): the
    trickiest code. It distinguishes a *user* resize (clamp to the floor,
    publish to the `width` file for other sidebars to adopt) from tmux
    *relayout* width changes caused by panes appearing/disappearing (snap back
    to the published/last width) from *its own* resize requests (tracked via
    `pending_width` so their landing is not re-published as a user resize).
    `relayout_grace` covers the two ticks around a pane-set change.

- **`scripts/agent-hook.sh`** — registers/unregisters an agent session in
  `sessions/` and flags its turn state in `working/` and `attention/`. Called
  from the agent's own lifecycle hooks as
  `agent-hook.sh <agent> <start|end|working|needsAttention>` with the hook JSON
  on stdin (parses both Claude Code snake_case and Copilot CLI camelCase). The
  registry record is `pane<TAB>agent<TAB>pid<TAB>cwd<TAB>transcript`; it walks
  the process ancestry to find the agent's PID so the sidebar can prune the
  entry when the
  process dies. This PID-pruning exists because not every agent fires a reliable
  `end` event (Copilot CLI fires hooks per prompt-cycle, so its `sessionEnd`
  maps to `needsAttention`, not `end` — see README). `working` (Claude Code's
  `UserPromptSubmit` plus the resume signals `PostToolUse` / `PostToolUseFailure`
  / `PostToolBatch` / `SubagentStart`; Copilot's per-cycle `sessionStart`) and
  `needsAttention` (Claude Code's `Stop` / `StopFailure` / `PermissionRequest`,
  the `idle_prompt` and `elicitation_dialog` `Notification` types, and a
  `PreToolUse` matcher for the `AskUserQuestion` / `ExitPlanMode` interactive
  tools; Copilot's `sessionEnd`) each write their marker and delete the other's,
  so the two are mutually exclusive. Every event (`start`, `working`,
  `needsAttention`) self-registers the session first via `register_session` if
  its registry record is missing, so a session whose `start` was missed — most
  visibly a resumed Claude Code session, where SessionStart does not re-create
  the entry — reappears the instant any later hook fires. `register_session` is
  a no-op outside tmux (no `TMUX_PANE`), and the marker branches re-check the
  record exists after ensuring registration, so an agent running outside a tmux
  pane still leaves no orphan. The
  sidebar renders an animated spinner for working and `●` for attention, and deletes the
  attention marker once its pane is focused (the working marker persists until
  the turn ends). `sidebar.py` prunes any registry entry (and both markers)
  whose pane is gone or whose PID is dead. The hook does *not* fire the bell or
  the desktop notification: it only writes the attention marker. The sidebar
  reacts to that marker (see `notify_attention` in the `sidebar.py` bullet),
  because it, not the hook, can locate the pane displaying a daemon-hosted
  session.

- **`scripts/session_labels.py`** — `sidebar.py`'s agent-row label logic, kept in
  its own module so the transcript-scanning stays isolated and testable. Holds
  `session_meta` (reads the session title / teammate `@name` / `/color` from the
  transcript tail), `agent_label` (composes the row text from mode/title/agent/dir),
  and `label_mode_from` (the `@wrangler-label` `dir`-else-`name` rule). Stdlib-only
  (json/os), no curses, no `TMUX_PANE`.

- **`scripts/install-hooks.py`** — installs (or `--uninstall`s) the
  `agent-hook.sh` invocations into each agent's config so users need not hand-edit
  them. It renders `scripts/hooks-manifest.json` — the declarative per-agent list
  of `event -> [action]` mappings — wiring the absolute path to this repo's
  `agent-hook.sh`. An event value is either a list of action strings (one hook
  group, no matcher) or a list of `{matcher, actions}` objects (one group each);
  matchers are Claude-only (the `claude` format emits them; the `copilot` format
  flattens the actions and drops the matcher). Two `format`s: `claude` merges non-destructively into the
  shared `~/.claude/settings.json` (replacing only wrangler's own hook groups,
  keyed on the `agent-hook.sh` command, preserving mode and a `.wrangler.bak`
  backup); `copilot` writes the dedicated `~/.copilot/hooks/wrangler.json` it
  owns outright. Idempotent. `wrangler.tmux` runs it on load when
  `@wrangler-auto-install-hooks` is on. Adding an agent event is one line in the
  manifest; a new agent whose config differs needs a new `format` handler.

## Conventions

- The `@wrangler_sidebar` pane option marks sidebar panes; check it (never the
  pane command) to tell sidebars from real panes.
- `sidebar.py` reads legacy 2-field registry records (old `claude-hook.sh`
  format: `pane<TAB>cwd`) as well as the current 4-field format — preserve that
  backward compatibility when touching the registry format.
- User-facing tmux options are all prefixed `@wrangler-`; document new ones in
  the README's Options section.
