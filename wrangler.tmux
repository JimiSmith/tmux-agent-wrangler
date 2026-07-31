#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler: locate the built wrangler binary and
# hand off to its tmux-entry subcommand, which does all the tmux setup.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

bin="$(command -v wrangler 2>/dev/null)"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/release/wrangler"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/debug/wrangler"

if [ -x "$bin" ]; then
  exec "$bin" tmux-entry
fi

# No prebuilt binary. Build it with cargo if available. The build runs
# backgrounded via run-shell -b so a first-time compile never blocks tmux
# startup; tmux-entry (key binds, hooks, daemon) runs once the binary exists.
# Until then the toggle/focus keys are simply unbound.
if command -v cargo >/dev/null 2>&1; then
  build_log="${TMPDIR:-/tmp}/wrangler-build.log"
  tmux display-message "wrangler: building with cargo (first run), sidebar available shortly..."
  tmux run-shell -b "if cd '$CURRENT_DIR' && cargo build --release >'$build_log' 2>&1; then '$CURRENT_DIR/target/release/wrangler' tmux-entry; else tmux display-message 'wrangler: cargo build failed (see $build_log)'; fi"
  exit 0
fi

tmux display-message "wrangler: binary not built and cargo not found (install cargo, or run: cargo build --release)"
exit 0
