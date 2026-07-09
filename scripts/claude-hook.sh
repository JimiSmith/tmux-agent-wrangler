#!/usr/bin/env bash
# Claude Code SessionStart/SessionEnd hook: registers the session in the
# wrangler registry so the sidebar can list it. Usage: claude-hook.sh start|end
# Reads the hook JSON payload on stdin; exits silently outside tmux.
set -euo pipefail

REGISTRY="${XDG_STATE_HOME:-$HOME/.local/state}/tmux-agent-wrangler/sessions"

event="${1:-start}"
input="$(cat || true)"

parsed="$(printf '%s' "$input" | python3 -c '
import json, sys
d = json.load(sys.stdin)
print(d.get("session_id", ""))
print(d.get("cwd", ""))
' 2>/dev/null || true)"

session_id="$(printf '%s' "$parsed" | sed -n 1p)"
session_id="${session_id//\//_}"
[ -z "$session_id" ] && exit 0

if [ "$event" = "end" ]; then
  rm -f "$REGISTRY/$session_id"
  exit 0
fi

[ -z "${TMUX_PANE:-}" ] && exit 0
cwd="$(printf '%s' "$parsed" | sed -n 2p)"
mkdir -p "$REGISTRY"
printf '%s\t%s\n' "$TMUX_PANE" "${cwd:-$PWD}" > "$REGISTRY/$session_id"
