# tmux-agent-wrangler

A persistent sidebar for tmux. Lists every window in the session with its
panes shown as a tree underneath. Windows are interactive: highlight one and
press Enter, or click it, to focus that window. The sidebar follows you —
it moves itself into whichever window becomes current.

```
 WINDOWS

* 1: vim
   ├─ 1: vim
   └─*2: zsh
  2: server
   └─ 1: node
```

## Requirements

- tmux ≥ 3.1
- python3 (with the standard-library `curses` module — present on macOS and most Linux distros)

## Install

### TPM

```tmux
set -g @plugin 'james/tmux-agent-wrangler'
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

Sessions register in `$XDG_STATE_HOME/tmux-agent-wrangler/sessions` (default
`~/.local/state/...`) via `scripts/agent-hook.sh <agent> <start|end>`. The
start hook records the pane, cwd, and the agent's PID; the sidebar prunes an
entry when its pane disappears or its process exits.

### Claude Code

Register the hooks in `~/.claude/settings.json` (adjust the path):

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "~/Development/adhoc/tmux-agent-wrangler/scripts/agent-hook.sh claude start" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "command": "~/Development/adhoc/tmux-agent-wrangler/scripts/agent-hook.sh claude end" }] }
    ]
  }
}
```

### Copilot CLI

Create `~/.copilot/hooks/wrangler.json` (adjust the path):

```json
{
  "version": 1,
  "hooks": {
    "sessionStart": [
      { "type": "command", "bash": "~/Development/adhoc/tmux-agent-wrangler/scripts/agent-hook.sh copilot start" }
    ]
  }
}
```

Copilot CLI fires its lifecycle hooks per prompt-cycle rather than per session
([copilot-cli#991](https://github.com/github/copilot-cli/issues/991)). Two
consequences:

- a session only appears in the sidebar once its first message is sent
  (`sessionStart` does not fire at launch);
- `sessionEnd` is deliberately not registered, since it would remove the
  entry after every response. Cleanup relies on PID pruning instead.

## Options

```tmux
set -g @wrangler-key 'Tab'   # toggle key (bound with prefix)
set -g @wrangler-width 32      # sidebar width in columns
set -g @wrangler-min-width 24  # sidebar snaps back if squeezed below this
```
