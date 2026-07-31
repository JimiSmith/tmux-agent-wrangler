#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler: locate the built wrangler binary and
# hand off to its tmux-entry subcommand, which does all the tmux setup.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# A wrangler on PATH is an explicit install: trust it and run it as-is.
bin="$(command -v wrangler 2>/dev/null)"
if [ -x "$bin" ]; then
  exec "$bin" tmux-entry
fi

# Otherwise use the locally built binary, preferring release over debug.
bin="$CURRENT_DIR/target/release/wrangler"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/debug/wrangler"

# A local binary older than its sources (e.g. after a git pull) is stale: fall
# through to the build path rather than running old code.
stale=""
[ -x "$bin" ] && stale="$(find "$CURRENT_DIR/src" "$CURRENT_DIR/Cargo.toml" "$CURRENT_DIR/Cargo.lock" -newer "$bin" -print -quit 2>/dev/null)"

if [ -x "$bin" ] && [ -z "$stale" ]; then
  exec "$bin" tmux-entry
fi

# Binary missing or stale. Build it with cargo if available. The build runs
# backgrounded via run-shell -b so a first-time compile never blocks tmux
# startup; tmux-entry (key binds, hooks, daemon) runs once the binary exists.
# Until then the toggle/focus keys are simply unbound. --replace-daemon makes
# the freshly built binary evict any daemon still running the old code.
if command -v cargo >/dev/null 2>&1; then
  build_log="${TMPDIR:-/tmp}/wrangler-build.log"
  tmux display-message "wrangler: building with cargo (first run), sidebar available shortly..."
  tmux run-shell -b "if cd '$CURRENT_DIR' && cargo build --release >'$build_log' 2>&1; then '$CURRENT_DIR/target/release/wrangler' tmux-entry --replace-daemon; else tmux display-message 'wrangler: cargo build failed (see $build_log)'; fi"
  exit 0
fi

tmux display-message "wrangler: binary not built and cargo not found (install cargo, or run: cargo build --release)"
exit 0
