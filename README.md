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
- `prefix + a` — focus this window's sidebar (no-op if the sidebar is off)
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
come from two families of hooks wired below — `working` on every event that
begins a turn, and `needsAttention` on every event that ends one or hands
control back to you (a stop, an error, a notification, or a permission
prompt). Wire as many of each family as your agent fires; more coverage just
means the state flips sooner.

Sessions register in `$XDG_STATE_HOME/tmux-agent-wrangler/sessions` (default
`~/.local/state/...`) via `scripts/agent-hook.sh <agent> <start|end|working|needsAttention>`.
The start hook records the pane, cwd, and the agent's PID; the sidebar prunes
an entry when its pane disappears or its process exits.

### Automatic install

Rather than editing the config files by hand (below), run the installer, which
wires the hooks for both agents using this plugin's own path:

```sh
scripts/install-hooks.py            # both agents
scripts/install-hooks.py claude     # or one: claude | copilot
scripts/install-hooks.py --uninstall
```

It merges into Claude Code's shared `~/.claude/settings.json` without touching
your other hooks (backing it up to `settings.json.wrangler.bak` first) and
writes Copilot's dedicated `~/.copilot/hooks/wrangler.json`. It is idempotent,
so re-running is safe. The hook set it installs lives in
`scripts/hooks-manifest.json`. To run it automatically on plugin load, set
`@wrangler-auto-install-hooks on` (see Options).

To wire the hooks manually instead, use the per-agent blocks below.

The examples below assume the default TPM install path,
`~/.tmux/plugins/tmux-agent-wrangler`. To confirm where TPM put the plugin,
run `tmux show-environment -g TMUX_PLUGIN_MANAGER_PATH` — the plugin lives in
a `tmux-agent-wrangler` directory under that path. If you installed manually,
use the directory you cloned instead.

### Claude Code

Register the hooks in `~/.claude/settings.json`. `working` is wired on every
event that (re)starts activity — a fresh prompt, any tool result, a subagent
spawn — so the indicator recovers whenever Claude resumes on its own.
`needsAttention` is wired on every event that hands control back to you,
including a `PreToolUse` matcher for the interactive `AskUserQuestion` /
`ExitPlanMode` tools (which fire no notification of their own) and the
attention-worthy `Notification` types:

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
    "PostToolUse": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude working" }] }
    ],
    "PostToolUseFailure": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude working" }] }
    ],
    "PostToolBatch": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude working" }] }
    ],
    "SubagentStart": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude working" }] }
    ],
    "PreToolUse": [
      { "matcher": "AskUserQuestion|ExitPlanMode", "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ],
    "Stop": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ],
    "StopFailure": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ],
    "PermissionRequest": [
      { "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
    ],
    "Notification": [
      { "matcher": "idle_prompt", "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] },
      { "matcher": "elicitation_dialog", "hooks": [{ "type": "command", "command": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh claude needsAttention" }] }
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
    "userPromptSubmitted": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "agentStop": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "errorOccurred": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "notification": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "permissionRequest": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "sessionEnd": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ]
  }
}
```

Copilot CLI fires its lifecycle hooks per prompt-cycle rather than per session
([copilot-cli#991](https://github.com/github/copilot-cli/issues/991)). So:

- a session only appears in the sidebar once its first message is sent
  (`sessionStart` does not fire at launch);
- `sessionStart` doubles as the turn-start signal, so it maps to both `start`
  (register) and `working` — `start` first, so the session is registered
  before `working` marks it. `userPromptSubmitted` also maps to `working`;
- the turn-ending events (`agentStop`, `errorOccurred`, `notification`,
  `permissionRequest`, `sessionEnd`) all map to `needsAttention`, never `end`:
  firing per prompt-cycle, an `end` would remove the session after every
  response. No `end` is wired at all, so session cleanup relies on PID pruning
  instead.

## Options

```tmux
set -g @wrangler-key 'Tab'   # toggle key (bound with prefix)
set -g @wrangler-focus-key 'a' # focus this window's sidebar (bound with prefix)
set -g @wrangler-width 32      # sidebar width in columns
set -g @wrangler-min-width 24  # sidebar snaps back if squeezed below this
set -g @wrangler-sync-width on # resizing one sidebar resizes them all ('off' to disable)
set -g @wrangler-auto-install-hooks off # install agent hooks on plugin load ('on' to enable)
set -g @wrangler-bell off      # ring the terminal bell when an agent needs attention ('on' to enable)
set -g @wrangler-label name    # agent row label: 'name' (session title, default) | 'dir' (working-dir basename)
```

`@wrangler-label name` shows each agent session's own title (Claude Code's
generated session name) and falls back to the working-directory basename when no
title is available yet (a just-started session, or an agent like Copilot CLI
that exposes no title). Set it to `dir` to always show the directory basename.

Agent-teams teammates are labelled `@name - title` (just `@name` until the
teammate has a title) in either mode, so you can tell them from top-level
sessions.

For the selection highlight to follow focus the moment it changes rather than
on the sidebar's 1s poll, enable tmux's built-in focus reporting yourself:

```tmux
set -g focus-events on
```

The plugin does not set this for you, since it is a server-wide option. Without
it the highlight still updates on the next poll, and focusing via the focus key
(`prefix + a`) is instant regardless.
