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
   └─ frontend ⠹

 COPILOT

  3: agents
   └─ docs ⠹
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

- a spinner (`⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`, animated) — working: a turn is in progress. Shown from
  turn start until it ends, whether or not you are looking at the pane.
- `●` — attention: the agent finished a turn or raised a notification (e.g. a
  permission prompt) and is waiting on you. The dot clears as soon as you focus
  that session's pane, so it means "wanted your attention while you were not
  looking at it".

The two are mutually exclusive: starting a turn replaces the dot with the
spinner, finishing one replaces the spinner with the dot. The annotations are
optional; they
come from two families of hooks wired below — `working` on every event that
begins a turn, and `needsAttention` on every event that ends one or hands
control back to you (a stop, an error, an attention-worthy notification, or a
permission prompt). Wire as many of each family as your agent fires; more
coverage just means the state flips sooner.

Sessions register in `$XDG_STATE_HOME/tmux-agent-wrangler/sessions` (default
`~/.local/state/...`) via `scripts/agent-hook.sh <agent> <start|end|working|needsAttention>`.
The start hook records the pane, cwd, and the agent's PID; the sidebar prunes
an entry when its pane disappears or its process exits.

Session names update live. Claude titles come from its transcript; Copilot
titles come from `~/.copilot/session-state/<session-id>/workspace.yaml`
(`name`, falling back to `summary`), including changes made with `/rename`.

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
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot start" }
    ],
    "userPromptSubmitted": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "postToolUse": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "postToolUseFailure": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "subagentStart": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "subagentStop": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working" }
    ],
    "agentStop": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "errorOccurred": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention" }
    ],
    "notification": [
      {
        "type": "command",
        "matcher": "shell_completed|shell_detached_completed|agent_completed|agent_idle",
        "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot working"
      },
      {
        "type": "command",
        "matcher": "permission_prompt|elicitation_dialog",
        "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot needsAttention"
      }
    ],
    "sessionEnd": [
      { "type": "command", "bash": "~/.tmux/plugins/tmux-agent-wrangler/scripts/agent-hook.sh copilot end" }
    ]
  }
}
```

Copilot CLI fires `sessionStart` once when a new or resumed session begins and
`sessionEnd` once when it terminates. They register and unregister the sidebar
row respectively; PID pruning remains a backstop for crashes that skip the end
hook. `userPromptSubmitted`, tool results, and subagent lifecycle events mark
the turn working, so the spinner recovers whenever Copilot continues after an
intermediate interaction. Background shell and agent completion notifications
also mark working because Copilot immediately resumes the main agent to process
them. `agentStop` and `errorOccurred` mark the session as needing attention.

`permissionRequest` is deliberately not an attention signal: it runs before
Copilot's permission rules for every applicable tool call, including calls that
are allowed without prompting the user. The matched `notification` hook instead
marks only actual permission prompts and elicitation dialogs as attention;
background completion and idle notifications are continuation signals instead.
The status hook deliberately does not use `preToolUse`: Copilot treats failures
from command hooks on that event as a denial, so sidebar telemetry must not sit
on the tool-execution critical path.

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
set -g @wrangler-hook-progress on  # spinner/● working/attention indicators from agent hooks ('off' to disable)
set -g @wrangler-osc-progress off  # OSC 9;4 progress % reported by panes ('on' to enable)
set -g @wrangler-osc-notify off    # desktop notification when an agent needs attention: 'off' | '777' (or 'on') | '9'
```

`@wrangler-label name` shows each agent session's own title (Claude Code's
generated session name) and falls back to the working-directory basename when no
title is available yet (a just-started session, or an agent like Copilot CLI
that exposes no title). Set it to `dir` to always show the directory basename.

Agent-teams teammates are labelled `@name - title` (just `@name` until the
teammate has a title) in either mode, so you can tell them from top-level
sessions.

Each agent row is drawn in that session's own assigned color - the one Claude
shows for the session (changed with `/color`), or, for an agent-teams teammate,
its team color - so a row's color matches the session it points at. Sessions
with no assigned color (e.g. Copilot CLI) use a default. The shade is matched to
your Claude theme (read from `~/.claude/settings.json`) and mapped to the same
xterm-256 index Claude itself emits (Claude renders these colors as 256-color
indices, so the row matches exactly rather than approximately); the ANSI themes
and non-256-color terminals fall back to the base terminal colors. Turn state
stays legible through the spinner/`●` glyph.

The sidebar pins a progress indicator to the right edge of each row, from two
independent sources you can toggle separately:

- `@wrangler-hook-progress` (default on) draws the hook-driven turn state:
  an animated spinner while an agent is working, `●` once it wants attention.
  These come from the agent hooks (see [Agent sessions](#agent-sessions)).
- `@wrangler-osc-progress` (default off) draws an app's OSC 9;4 progress report
  as a percentage colored by state (`normal` green, `paused` yellow, `error`
  red, `indeterminate` the spinner; `hidden` shows nothing). It reads tmux's
  `#{pane_pb_progress}` / `#{pane_pb_state}`, so it needs a tmux new enough to
  expose them; on an older tmux enabling it is a harmless no-op.

Both indicators appear in the window tree (per pane) and the agents section.
When both are enabled, OSC wins for any pane actively reporting progress; a pane
with no OSC progress falls back to its spinner/`●` hook glyph.

`@wrangler-osc-notify` (default off) raises a desktop notification the moment an
agent needs attention, and `@wrangler-bell` (default off) rings the terminal bell
at the same point; the two are independent. Set osc-notify to `777` (or `on`) for
an OSC 777 notification (the agent name as the title, `<window> · <label>` as the
body) or `9` for an OSC 9 notification (`<window> · <label>` as the single
message); `off` disables it. Pick the escape your terminal understands: OSC 777
(rxvt-unicode, foot, ...) or OSC 9 (ConEmu, iTerm2, ...); a terminal that does
not understand the chosen one silently ignores it. Either way `<window>` /
`<label>` are the window name and the row label as the sidebar shows them (e.g.
`vim · api-service`), and the escape is sent to the terminal itself rather than
through tmux, so the notification arrives whatever window you are on.

Both signals are raised by the sidebar as it polls, so they need the sidebar
toggled on, and — like the `●` indicator — they fire only when you are not
already looking at that agent's pane (focusing it clears the pending attention
instead).

For the selection highlight to follow focus the moment it changes rather than
on the sidebar's 1s poll, enable tmux's built-in focus reporting yourself:

```tmux
set -g focus-events on
```

The plugin does not set this for you, since it is a server-wide option. Without
it the highlight still updates on the next poll, and focusing via the focus key
(`prefix + a`) is instant regardless.
