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

# The bell and the OSC desktop notification are raised by the sidebar from its
# poll loop (it reacts to the attention marker written below, and knows the
# pane/window/client to target); this hook only writes the marker.

# Write (or overwrite) this session's registry record, keyed on session_id. The
# tmux pane is optional. Under Claude Code's daemon architecture the session (and
# so this hook) runs detached from any pane, with no TMUX_PANE, so we record an
# empty pane field and the sidebar files the session under its "Agents" group.
# When a pane *is* present (an agent that does inherit TMUX_PANE) the recorded
# pane lets the sidebar group the session under that pane's window instead.
register_session() {
  local cwd transcript agent_pid pid cmdline
  cwd="$(printf '%s' "$parsed" | sed -n 2p)"
  # Path to the agent's transcript when the event provides one (Claude
  # lifecycle events and Copilot agentStop). Claude titles are read from it;
  # Copilot titles come from workspace.yaml.
  transcript="$(printf '%s' "$parsed" | sed -n 3p)"

  # Find the agent process among our ancestors so the sidebar can prune the
  # entry if an end hook is skipped, for example when the agent crashes.
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

# Refresh an existing record's cwd/transcript in place when the current event
# reports a transcript path different from the stored one, preserving pane and
# pid (so we skip the ancestry walk). A session that relocates mid-life - most
# visibly one that enters a worktree, moving its cwd and so relocating its
# transcript to a new project dir under the same session id - would otherwise
# keep the stale path forever (turn events never re-register), and the sidebar
# reads the title and /color from that transcript, so both would go missing.
refresh_record() {
  local cwd transcript rec line old_pane old_agent old_pid old_cwd old_transcript
  transcript="$(printf '%s' "$parsed" | sed -n 3p)"
  [ -n "$transcript" ] || return 0  # no transcript in payload: nothing to refresh
  rec="$REGISTRY/$agent-$session_id"
  # Split the record with cut, not `IFS=$'\t' read`: tab is IFS whitespace, so
  # read trims the empty leading pane field of a daemon (pane-less) record and
  # shifts every field left (pane<-agent, ...), corrupting the record - the
  # sidebar then prunes it as a bogus non-local pane. cut preserves empties.
  IFS= read -r line < "$rec" || return 0
  old_pane="$(printf '%s' "$line" | cut -f1)"
  old_agent="$(printf '%s' "$line" | cut -f2)"
  old_pid="$(printf '%s' "$line" | cut -f3)"
  old_cwd="$(printf '%s' "$line" | cut -f4)"
  old_transcript="$(printf '%s' "$line" | cut -f5)"
  [ "$transcript" = "$old_transcript" ] && return 0  # unchanged: leave it be
  cwd="$(printf '%s' "$parsed" | sed -n 2p)"
  printf '%s\t%s\t%s\t%s\t%s\n' \
    "$old_pane" "$old_agent" "$old_pid" "${cwd:-$old_cwd}" "$transcript" \
    > "$rec"
}

# Register when the record is missing, else refresh a relocated transcript. The
# ancestry walk runs once per session (at registration), not on every
# high-frequency event. This makes every event self-healing: a session whose
# `start` was missed (e.g. a resumed Claude Code session, where SessionStart
# does not re-register it) reappears in the sidebar as soon as any other hook
# fires, and one that moved its transcript picks up the new path.
ensure_registered() {
  if [ -f "$REGISTRY/$agent-$session_id" ]; then
    refresh_record
  else
    register_session
  fi
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
  : > "$ATTENTION/$agent-$session_id"
  rm -f "$WORKING/$agent-$session_id"
  exit 0
fi

# start (or any other event): (re)register, refreshing pane/pid/cwd/transcript.
register_session
