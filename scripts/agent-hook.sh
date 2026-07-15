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

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

# Raise an OSC 9 desktop notification (the ConEmu/iTerm2 toast escape) in the
# session's terminal, gated on @wrangler-osc-notify (off by default). The body
# reads exactly as the sidebar row does: "<window index>: <window> · <label>",
# where the label comes from the shared session_labels module so it matches the
# sidebar's rendering (title / teammate @name / dir fallback).
#
# Unlike the bell, this writes to each attached client's tty, not the pane's:
# tmux 3.7 consumes a pane's OSC 9 into its own OSC 9;4 progress parser, so a
# notification sent through the pane would be swallowed. Writing to the client
# tty reaches the terminal emulator directly (and so notifies whatever window is
# focused). Best-effort: never fail the hook over a notification.
notify_osc9() {
  case "$(tmux show-option -gqv @wrangler-osc-notify 2>/dev/null)" in
    on|1|yes|true) ;;
    *) return 0 ;;
  esac
  local pane cwd transcript display_cwd label_opt win_heading label body session
  pane="$(cut -f1 "$REGISTRY/$agent-$session_id" 2>/dev/null)" || return 0
  [ -n "$pane" ] || return 0
  cwd="$(printf '%s' "$parsed" | sed -n 2p)"
  transcript="$(printf '%s' "$parsed" | sed -n 3p)"
  win_heading="$(tmux display-message -p -t "$pane" '#{window_index}: #{window_name}' 2>/dev/null)" || return 0
  display_cwd="$(tmux display-message -p -t "$pane" '#{pane_current_path}' 2>/dev/null)"
  display_cwd="${display_cwd:-$cwd}"
  label_opt="$(tmux show-option -gqv @wrangler-label 2>/dev/null)"
  label="$(PYTHONPATH="$SCRIPT_DIR" python3 -c 'import sys, session_labels; print(session_labels.notification_label(sys.argv[1], sys.argv[2], sys.argv[3]))' "$transcript" "$display_cwd" "$label_opt" 2>/dev/null)"
  if [ -n "$label" ]; then
    body="$win_heading · $label"
  else
    body="$win_heading"
  fi
  session="$(tmux display-message -p -t "$pane" '#{session_name}' 2>/dev/null)" || return 0
  tmux list-clients -t "$session" -F '#{client_tty}' 2>/dev/null | while read -r ctty; do
    [ -n "$ctty" ] && printf '\033]9;%s\007' "$body" > "$ctty" 2>/dev/null || true
  done
}

# Write (or overwrite) this session's registry record, keyed on session_id. The
# tmux pane is optional. Under Claude Code's daemon architecture the session (and
# so this hook) runs detached from any pane, with no TMUX_PANE, so we record an
# empty pane field and the sidebar files the session under its "Agents" group.
# When a pane *is* present (an agent that does inherit TMUX_PANE) the recorded
# pane lets the sidebar group the session under that pane's window instead.
register_session() {
  local cwd transcript agent_pid pid cmdline
  cwd="$(printf '%s' "$parsed" | sed -n 2p)"
  # Path to the agent's transcript (Claude Code only; empty otherwise). The
  # sidebar reads the session's human-readable title from it live, so we record
  # it once at registration rather than re-reading on every turn event.
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
  printf '%s\t%s\t%s\t%s\t%s\n' "${TMUX_PANE:-}" "$agent" "$agent_pid" "${cwd:-$PWD}" "$transcript" > "$REGISTRY/$agent-$session_id"
}

# Register only when the record is missing, so the per-event work stays cheap
# (the ancestry walk runs once per session, not on every high-frequency event).
# This makes every event self-healing: a session whose `start` was missed (e.g.
# a resumed Claude Code session, where SessionStart does not re-register it)
# reappears in the sidebar as soon as any other hook fires.
ensure_registered() {
  [ -f "$REGISTRY/$agent-$session_id" ] || register_session
}

if [ "$event" = "end" ]; then
  rm -f "$REGISTRY/$agent-$session_id" "$ATTENTION/$agent-$session_id" "$WORKING/$agent-$session_id"
  exit 0
fi

# Turn state changes. Each ensures the session is registered first, then marks
# it, so any event revives a dropped entry. The post-ensure guard is defensive:
# it skips the marker if registration somehow left no record (e.g. the state dir
# could not be created).
#
# working: a turn started; clears any pending attention.
if [ "$event" = "working" ]; then
  ensure_registered
  [ -f "$REGISTRY/$agent-$session_id" ] || exit 0
  mkdir -p "$WORKING"
  : > "$WORKING/$agent-$session_id"
  rm -f "$ATTENTION/$agent-$session_id"
  exit 0
fi

# needsAttention: a turn finished or a notification fired; clears working.
if [ "$event" = "needsAttention" ]; then
  ensure_registered
  [ -f "$REGISTRY/$agent-$session_id" ] || exit 0
  mkdir -p "$ATTENTION"
  # Signal only on the transition into attention: if the marker already exists
  # the session was flagged, so a repeat event stays silent. The bell and the
  # OSC 9 notification are gated independently (see each function).
  if [ ! -f "$ATTENTION/$agent-$session_id" ]; then
    ring_bell
    notify_osc9
  fi
  : > "$ATTENTION/$agent-$session_id"
  rm -f "$WORKING/$agent-$session_id"
  exit 0
fi

# start (or any other event): (re)register, refreshing pane/pid/cwd/transcript.
register_session
