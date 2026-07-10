#!/usr/bin/env bash
# Spawn a sidebar pane in a window unless it already has one.
# Usage: spawn.sh [--if-active] [window]
#   --if-active  only spawn when the session has sidebars (i.e. toggled on);
#                used by the new-window hooks
#   window       target window id; defaults to the current window
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if_active=0
if [ "${1:-}" = "--if-active" ]; then
  if_active=1
  shift
fi
win="${1:-$(tmux display-message -p '#{window_id}')}"

if [ "$if_active" = 1 ]; then
  if ! tmux list-panes -s -F '#{@wrangler_sidebar}' | grep -q 1; then
    exit 0
  fi
fi

if tmux list-panes -t "$win" -F '#{@wrangler_sidebar}' | grep -q 1; then
  exit 0
fi

width="$(tmux show-option -gqv @wrangler-width)"
width="${width:-32}"

pane="$(tmux split-window -d -f -h -b -l "$width" -t "$win" -P -F '#{pane_id}' "python3 '$CURRENT_DIR/sidebar.py'")"
tmux set-option -p -t "$pane" @wrangler_sidebar 1
