# tmux-agent-wrangler

A persistent sidebar for tmux. Lists every window in the session with its
panes shown as a tree underneath. Windows, panes, and agent sessions are
interactive: highlight one and press Enter, or click it, to focus it. Every
window gets its own sidebar pane — switching windows never rearranges a
layout — and the sidebars share their selection, so it feels like one
sidebar that follows you.

```
 WINDOWS

* 1: vim
   ├─ 1: vim
   └─*2: claude
  2: server
   └─ 1: node
  3: agents
   ├─ 1: claude
   └─ 2: copilot

 CLAUDE

* 1: vim
   └─ api-service ●
  3: agents
   └─ frontend ◐

 COPILOT

  3: agents
   └─ docs ◐
```

## Requirements

- tmux ≥ 3.1
- python3 (with the standard-library `curses` module — present on macOS and most Linux distros)

## Install

### TPM

```tmux
set -g @plugin 'JimiSmith/tmux-agent-wrangler'
```

### Manual

```tmux
run-shell /path/to/tmux-agent-wrangler/wrangler.tmux
```

## Usage

- `prefix + Tab` — toggle the sidebar
- `Up`/`Down` or `k`/`j` — move the highlight between windows
- `Enter` — focus the highlighted window
- mouse click on a window line — focus it
- `q` — close the sidebar

## Agent sessions

The sidebar shows a section per agent below the windows (`CLAUDE`, `COPILOT`,
...) listing active sessions running inside the tmux session. Selecting one
focuses its window and pane.

Each session is annotated with its turn state, so you can see at a glance what
your agents are doing:

- `◐` — working: a turn is in progress. Shown from turn start until it ends,
  whether or not you are looking at the pane.
- `●` — attention: the agent finished a turn or raised a notification (e.g. a
  permission prompt) and is waiting on you. The dot clears as soon as you focus
  that session's pane, so it means "wanted your attention while you were not
  looking at it".

The two are mutually exclusive: starting a turn replaces the dot with `◐`,
finishing one replaces `◐` with the dot. The annotations are optional; they
need the turn-state hooks below (Claude Code's `UserPromptSubmit`, `Stop`, and
`Notification`; Copilot CLI's `sessionStart` and `sessionEnd`).

Sessions register in `$XDG_STATE_HOME/tmux-agent-wrangler/sessions` (default
`~/.local/state/...`) via `scripts/agent-hook.sh <agent> <start|end|working|needsAttention>`.
The start hook records the pane, cwd, and the agent's PID; the sidebar prunes
an entry when its pane disappears or its process exits.

The examples below assume the default TPM install path,
`~/.tmux/plugins/tmux-agent-wrangler`. To confirm where TPM put the plugin,
run `tmux show-environment -g TMUX_PLUGIN_MANAGER_PATH` — the plugin lives in
a `tmux-agent-wrangler` directory under that path. If you installed manually,
use the directory you cloned instead.

### Claude Code

Register the hooks in `~/.claude/settings.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude start" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude end" }] }
    ],
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude working" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ],
    "Notification": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ]
  }
}
```

### Copilot CLI

Create `~/.copilot/hooks/wrangler.json`:

```json
{
  "version": 1,
  "hooks": {
    "sessionStart": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot start" },
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "sessionEnd": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ]
  }
}
```

Copilot CLI fires its lifecycle hooks per prompt-cycle rather than per session
([copilot-cli#991](https://github.com/github/copilot-cli/issues/991)). Two
consequences:

- a session only appears in the sidebar once its first message is sent
  (`sessionStart` does not fire at launch);
- `sessionStart` doubles as the turn-start signal, so it maps to both `start`
  (register) and `working` — `start` first, so the session is registered
  before `working` marks it;
- `sessionEnd` is mapped to `needsAttention`, not `end`: firing per
  prompt-cycle makes it the turn-finished signal that lights the dot, whereas
  `end` would remove the session after every response. Session cleanup relies
  on PID pruning instead.

## Options

```tmux
set -g @wrangler-key 'Tab'   # toggle key (bound with prefix)
set -g @wrangler-width 32      # sidebar width in columns
set -g @wrangler-min-width 24  # sidebar snaps back if squeezed below this
set -g @wrangler-sync-width on # resizing one sidebar resizes them all ('off' to disable)
```
