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

## Claude Code sessions

The sidebar shows a `CLAUDE` section below the windows listing active Claude
Code sessions running inside the tmux session. Selecting one focuses its
window and pane.

Register the hooks in `~/.claude/settings.json` (adjust the path):

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "~/Development/adhoc/tmux-agent-wrangler/scripts/claude-hook.sh start" }] }
    ],
    "SessionEnd": [
      { "hooks": [{ "type": "command", "command": "~/Development/adhoc/tmux-agent-wrangler/scripts/claude-hook.sh end" }] }
    ]
  }
}
```

Sessions register in `$XDG_STATE_HOME/tmux-agent-wrangler/sessions` (default
`~/.local/state/...`). Entries are removed on SessionEnd and pruned by the
sidebar when their pane disappears; a session that dies without firing
SessionEnd (e.g. a crash) lingers until its pane closes.

## Options

```tmux
set -g @wrangler-key 'Tab'   # toggle key (bound with prefix)
set -g @wrangler-width 32      # sidebar width in columns
set -g @wrangler-min-width 24  # sidebar snaps back if squeezed below this
```
