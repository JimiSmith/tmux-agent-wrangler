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
per session), `attention/` (turn-finished markers, one file per session that
mirrors its `sessions/` filename), `selection` (shared highlighted row), and
`width` (shared sidebar width). Deleting this directory resets all cross-pane
state.

## Architecture

The core design constraint: **one sidebar pane per window**, not one shared
sidebar pane. Switching windows in tmux would otherwise rearrange layouts.
Each window's sidebar is an independent `sidebar.py` process; they coordinate
only through files in the state dir, so most complexity is about keeping those
independent instances behaving as one.

- **`wrangler.tmux`** — TPM entry point. Binds the toggle key (`@wrangler-key`,
  default `Tab`, bound with prefix) and installs `after-new-window` /
  `after-break-pane` hooks so windows created while the sidebar is on get their
  own sidebar. Also patches `automatic-rename-format` so focusing the sidebar
  pane (command `Python`) does not rename the window.

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
  - **Shared selection**: the highlighted row is written to / read from the
    `selection` file every tick, so all sidebars highlight the same logical
    row and Enter/click on any of them focuses the same target.
  - **Width sync** (`@wrangler-sync-width`, `@wrangler-min-width`): the
    trickiest code. It distinguishes a *user* resize (clamp to the floor,
    publish to the `width` file for other sidebars to adopt) from tmux
    *relayout* width changes caused by panes appearing/disappearing (snap back
    to the published/last width) from *its own* resize requests (tracked via
    `pending_width` so their landing is not re-published as a user resize).
    `relayout_grace` covers the two ticks around a pane-set change.

- **`scripts/agent-hook.sh`** — registers/unregisters an agent session in
  `sessions/` and flags turn completion in `attention/`. Called from the
  agent's own lifecycle hooks as `agent-hook.sh <agent> <start|end|turnFinished>`
  with the hook JSON on stdin (parses both Claude Code snake_case and Copilot
  CLI camelCase). The start record is `pane<TAB>agent<TAB>pid<TAB>cwd`; it walks
  the process ancestry to find the agent's PID so the sidebar can prune the
  entry when the process dies. This PID-pruning exists because not every agent
  fires a reliable `end` event (Copilot CLI fires hooks per prompt-cycle, so its
  `sessionEnd` maps to `turnFinished`, not `end` — see README). `turnFinished`
  writes an `attention/` marker (only for an already-registered session, so
  stray events leave no orphan); the sidebar shows a `●` on that session and
  deletes the marker once its pane is focused. `sidebar.py` prunes any registry
  entry (and its marker) whose pane is gone or whose PID is dead.

## Conventions

- The `@wrangler_sidebar` pane option marks sidebar panes; check it (never the
  pane command) to tell sidebars from real panes.
- `sidebar.py` reads legacy 2-field registry records (old `claude-hook.sh`
  format: `pane<TAB>cwd`) as well as the current 4-field format — preserve that
  backward compatibility when touching the registry format.
- User-facing tmux options are all prefixed `@wrangler-`; document new ones in
  the README's Options section.
