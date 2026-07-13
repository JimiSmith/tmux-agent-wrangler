#!/usr/bin/env bash
# Generic agent session hook: registers/unregisters a session in the wrangler
# registry so the sidebar can list it, and flags its turn state so the sidebar
# can annotate it: `working` while a turn is in progress, `needsAttention` once
# it finishes a turn or raises a notification. The two states are mutually
# exclusive; each clears the other.
# Usage: agent-hook.sh <agent> <start|end|working|needsAttention>
# Reads the hook JSON payload on stdin (Claude Code snake_case or Copilot CLI
# camelCase). Exits silently outside tmux.
set -euo pipefail

STATE="${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agent-wrangler"
REGISTRY="$STATE/sessions"
ATTENTION="$STATE/attention"
WORKING="$STATE/working"

agent="${1:?agent name required}"
event="${2:-start}"
input="$(cat || true)"

parsed="$(printf '%s' "$input" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(d.get("session_id") or d.get("sessionId") or "")
print(d.get("cwd", ""))
print(d.get("transcript_path") or d.get("transcriptPath") or "")
' 2>/dev/null || true)"

session_id="$(printf '%s' "$parsed" | sed -n 1p)"
session_id="${session_id//\//_}"
[ -z "$session_id" ] && exit 0

# Ring the terminal bell in the session's pane, gated on @wrangler-bell (off by
# default). Writing BEL to the pane's tty makes tmux apply its own bell handling
# (audible bell plus the monitor-bell window flag when you are in another
# window), so audible-vs-visual stays with the user's tmux/terminal config. The
# pane is field 1 of the registry record. Best-effort: never fail the hook over
# a bell.
ring_bell() {
  case "$(tmux show-option -gqv @wrangler-bell 2>/dev/null)" in
    on|1|yes|true) ;;
    *) return 0 ;;
  esac
  local pane tty
  pane="$(cut -f1 "$REGISTRY/$agent-$session_id" 2>/dev/null)" || return 0
  [ -n "$pane" ] || return 0
  tty="$(tmux display-message -p -t "$pane" '#{pane_tty}' 2>/dev/null)" || return 0
  [ -n "$tty" ] && printf '\a' > "$tty" 2>/dev/null || true
}

if [ "$event" = "end" ]; then
  rm -f "$REGISTRY/$agent-$session_id" "$ATTENTION/$agent-$session_id" "$WORKING/$agent-$session_id"
  exit 0
fi

# Turn state changes. Only mark sessions we already track, so an event from an
# agent running outside tmux (never registered) leaves no orphan marker.
#
# working: a turn started; clears any pending attention.
if [ "$event" = "working" ]; then
  [ -f "$REGISTRY/$agent-$session_id" ] || exit 0
  mkdir -p "$WORKING"
  : > "$WORKING/$agent-$session_id"
  rm -f "$ATTENTION/$agent-$session_id"
  exit 0
fi

# needsAttention: a turn finished or a notification fired; clears working.
if [ "$event" = "needsAttention" ]; then
  [ -f "$REGISTRY/$agent-$session_id" ] || exit 0
  mkdir -p "$ATTENTION"
  # Ring only on the transition into attention: if the marker already exists
  # the session was flagged, so a repeat event stays silent.
  [ -f "$ATTENTION/$agent-$session_id" ] || ring_bell
  : > "$ATTENTION/$agent-$session_id"
  rm -f "$WORKING/$agent-$session_id"
  exit 0
fi

[ -z "${TMUX_PANE:-}" ] && exit 0
cwd="$(printf '%s' "$parsed" | sed -n 2p)"
# Path to the agent's transcript (Claude Code only; empty otherwise). The
# sidebar reads the session's human-readable title from it live, so we record
# it once here at registration rather than re-reading on every turn event.
transcript="$(printf '%s' "$parsed" | sed -n 3p)"

# Find the agent process among our ancestors so the sidebar can prune the
# entry when it exits. Needed because not every agent fires sessionEnd
# reliably (Copilot CLI fires it per prompt-cycle; see README).
agent_pid=""
pid=$$
for _ in 1 2 3 4 5 6 7 8; do
  pid="$(ps -o ppid= -p "$pid" 2>/dev/null | tr -d ' ')"
  if [ -z "$pid" ] || [ "$pid" -le 1 ]; then
    break
  fi
  cmdline="$(ps -o command= -p "$pid" 2>/dev/null || true)"
  case "$cmdline" in
    *agent-hook*) continue ;;
  esac
  if printf '%s' "$cmdline" | grep -qi "$agent"; then
    agent_pid="$pid"
    break
  fi
done

mkdir -p "$REGISTRY"
printf '%s\t%s\t%s\t%s\t%s\n' "$TMUX_PANE" "$agent" "$agent_pid" "${cwd:-$PWD}" "$transcript" > "$REGISTRY/$agent-$session_id"
