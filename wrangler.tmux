#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler: locate the built wrangler binary and
# hand off to its tmux-entry subcommand, which does all the tmux setup.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

bin="$(command -v wrangler 2>/dev/null)"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/release/wrangler"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/debug/wrangler"

if [ ! -x "$bin" ]; then
  tmux display-message "wrangler: binary not built (run: cargo build --release)"
  exit 0
fi

exec "$bin" tmux-entry
