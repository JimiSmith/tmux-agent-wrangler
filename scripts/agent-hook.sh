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
' 2>/dev/null || true)"

session_id="$(printf '%s' "$parsed" | sed -n 1p)"
session_id="${session_id//\//_}"
[ -z "$session_id" ] && exit 0

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
  : > "$ATTENTION/$agent-$session_id"
  rm -f "$WORKING/$agent-$session_id"
  exit 0
fi

[ -z "${TMUX_PANE:-}" ] && exit 0
cwd="$(printf '%s' "$parsed" | sed -n 2p)"

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
printf '%s\t%s\t%s\t%s\n' "$TMUX_PANE" "$agent" "$agent_pid" "${cwd:-$PWD}" > "$REGISTRY/$agent-$session_id"
