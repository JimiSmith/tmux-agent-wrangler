#!/usr/bin/env bash
# Toggle the wrangler sidebar in the current session.
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

width="$(tmux show-option -gqv @wrangler-width)"
width="${width:-32}"

sidebar="$(tmux list-panes -s -F '#{pane_id} #{@wrangler_sidebar}' | awk '$2 == 1 { print $1; exit }')"

if [ -n "$sidebar" ]; then
  tmux kill-pane -t "$sidebar"
  exit 0
fi

pane="$(tmux split-window -d -f -h -b -l "$width" -P -F '#{pane_id}' "python3 '$CURRENT_DIR/sidebar.py'")"
tmux set-option -p -t "$pane" @wrangler_sidebar 1
