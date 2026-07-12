#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

key="$(tmux show-option -gqv @wrangler-key)"
key="${key:-Tab}"

tmux bind-key "$key" run-shell "$CURRENT_DIR/scripts/toggle.sh"

# Opt-in: install the agent hooks into Claude/Copilot config on load. Off by
# default; backgrounded so load never blocks on config writes, and idempotent
# so firing every load is harmless.
case "$(tmux show-option -gqv @wrangler-auto-install-hooks)" in
  on|1|yes|true) tmux run-shell -b "$CURRENT_DIR/scripts/install-hooks.py" ;;
esac

# Windows created while the sidebar is toggled on get their own sidebar pane.
tmux set-hook -g after-new-window "run-shell '$CURRENT_DIR/scripts/spawn.sh --if-active'"
tmux set-hook -g after-break-pane "run-shell '$CURRENT_DIR/scripts/spawn.sh --if-active'"

# Unset the hook installed by older releases.
tmux set-hook -gu session-window-changed

# automatic-rename uses the active pane's command, so focusing the sidebar
# briefly renames the window to "Python". Guard the format: while the sidebar
# is the active pane, keep the window's current name.
fmt="$(tmux show-option -gv automatic-rename-format)"
case "$fmt" in
  *@wrangler_sidebar*) ;;
  *) tmux set-option -g automatic-rename-format "#{?#{@wrangler_sidebar},#{window_name},$fmt}" ;;
esac
