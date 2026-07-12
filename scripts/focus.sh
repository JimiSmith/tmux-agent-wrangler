#!/usr/bin/env bash
# Give keyboard focus to the current window's sidebar pane, if it has one.
# A no-op when the sidebar is toggled off (no sidebar pane in this window).
set -euo pipefail

pane="$(tmux list-panes -F '#{pane_id} #{@wrangler_sidebar}' | awk '$2 == 1 { print $1; exit }')"

if [ -n "$pane" ]; then
  tmux select-pane -t "$pane"
fi
