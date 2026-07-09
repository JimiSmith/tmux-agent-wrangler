#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

key="$(tmux show-option -gqv @wrangler-key)"
key="${key:-Tab}"

tmux bind-key "$key" run-shell "$CURRENT_DIR/scripts/toggle.sh"

# Keep the sidebar in view when the current window changes by any means
# (select-window, next/previous, clicking the status line, etc.).
tmux set-hook -g session-window-changed "run-shell '$CURRENT_DIR/scripts/follow.sh'"

# automatic-rename uses the active pane's command, so focusing the sidebar
# briefly renames the window to "Python". Guard the format: while the sidebar
# is the active pane, keep the window's current name.
fmt="$(tmux show-option -gv automatic-rename-format)"
case "$fmt" in
  *@wrangler_sidebar*) ;;
  *) tmux set-option -g automatic-rename-format "#{?#{@wrangler_sidebar},#{window_name},$fmt}" ;;
esac
