#!/usr/bin/env bash
# Give keyboard focus to the current window's sidebar pane, if it has one.
# A no-op when the sidebar is toggled off (no sidebar pane in this window).
set -euo pipefail

pane="$(tmux list-panes -F '#{pane_id} #{@wrangler_sidebar}' | awk '$2 == 1 { print $1; exit }')"

if [ -n "$pane" ]; then
  tmux select-pane -t "$pane"
  # The sidebar only repaints when its getch() returns; select-pane delivers no
  # input to the pane, so its blocking read would sit on the 1s poll before the
  # focus highlight appears. Nudge it with C-l, which the loop treats as a
  # redraw, so the highlight lands at once (as it does on a mouse click).
  tmux send-keys -t "$pane" C-l
fi
