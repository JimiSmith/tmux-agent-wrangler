#!/usr/bin/env bash
# Toggle the wrangler sidebar: one sidebar pane in every window of the session.
set -euo pipefail

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

sidebars="$(tmux list-panes -s -F '#{pane_id} #{@wrangler_sidebar}' | awk '$2 == 1 { print $1 }')"

if [ -n "$sidebars" ]; then
  for pane in $sidebars; do
    tmux kill-pane -t "$pane" 2>/dev/null || true
  done
  exit 0
fi

for win in $(tmux list-windows -F '#{window_id}'); do
  "$CURRENT_DIR/spawn.sh" "$win"
done
