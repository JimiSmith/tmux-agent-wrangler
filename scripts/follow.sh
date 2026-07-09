#!/usr/bin/env bash
# Move the sidebar pane into the session's current window if it isn't there.
# Runs from the session-window-changed hook; must be cheap and loop-free
# (join-pane does not change the current window, so it cannot re-trigger).
set -euo pipefail

sidebar="$(tmux list-panes -s -F '#{pane_id} #{@wrangler_sidebar}' | awk '$2 == 1 { print $1; exit }')"
[ -z "$sidebar" ] && exit 0

cur_win="$(tmux display-message -p '#{window_id}')"
side_win="$(tmux display-message -p -t "$sidebar" '#{window_id}')"
[ "$cur_win" = "$side_win" ] && exit 0

width="$(tmux display-message -p -t "$sidebar" '#{pane_width}')"
tmux join-pane -d -f -h -b -l "${width:-32}" -s "$sidebar" -t "$cur_win"
