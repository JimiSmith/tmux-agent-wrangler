#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler: obtain the wrangler binary and hand off
# to its tmux-entry subcommand, which does all the tmux setup. The binary is
# resolved in preference order: an explicit install on PATH, a fresh local build,
# the prebuilt release matching the checked-out commit, then a from-source build.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# 1. An explicit install on PATH is trusted and run as-is.
bin="$(command -v wrangler 2>/dev/null)"
if [ -x "$bin" ]; then
  exec "$bin" tmux-entry
fi

# 2. A local build (a developer's cargo output) wins next, preferring release
#    over debug. A binary older than its sources (e.g. after a git pull) is stale
#    and falls through to the paths below rather than running old code.
bin="$CURRENT_DIR/target/release/wrangler"
[ -x "$bin" ] || bin="$CURRENT_DIR/target/debug/wrangler"
stale=""
[ -x "$bin" ] && stale="$(find "$CURRENT_DIR/src" "$CURRENT_DIR/Cargo.toml" "$CURRENT_DIR/Cargo.lock" -newer "$bin" -print -quit 2>/dev/null)"
if [ -x "$bin" ] && [ -z "$stale" ]; then
  exec "$bin" tmux-entry
fi

# 3. Prebuilt release binary for the checked-out commit: the path for a plain
#    (non-developer) install, needing no toolchain. It applies only to a clean
#    checkout of a published commit -- any local modification means the prebuilt
#    would not match the tree, so those build from source instead (step 4).
short=""
clean=""
repo=""
if git -C "$CURRENT_DIR" rev-parse HEAD >/dev/null 2>&1; then
  full="$(git -C "$CURRENT_DIR" rev-parse HEAD 2>/dev/null)"
  short="${full:0:7}"
  [ -z "$(git -C "$CURRENT_DIR" status --porcelain 2>/dev/null)" ] && clean=1
  # owner/repo from the origin remote (an ssh or https GitHub url), so a fork
  # that runs the build workflow fetches its own releases.
  repo="$(git -C "$CURRENT_DIR" remote get-url origin 2>/dev/null |
    sed -E 's#^(git@github.com:|https://github.com/)##; s#\.git$##')"
fi

# The release asset for this platform, empty if none is published for it.
asset=""
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) asset="wrangler-linux-x64" ;;
  Darwin/arm64 | Darwin/aarch64) asset="wrangler-macos-arm64" ;;
esac

cache="${XDG_CACHE_HOME:-$HOME/.cache}/tmux-agent-wrangler"
pbin="$cache/wrangler-$short"

if [ -n "$short" ] && [ -n "$clean" ] && [ -n "$asset" ]; then
  # Already downloaded for this commit: run it.
  if [ -x "$pbin" ]; then
    exec "$pbin" tmux-entry
  fi
  # Otherwise fetch it over plain HTTPS (the release assets are public). A
  # freshly fetched binary is a new version, so it evicts any daemon still on the
  # old code. A failure here (offline, or CI has not published this commit yet)
  # falls through to the from-source build.
  if [ -n "$repo" ] && command -v curl >/dev/null 2>&1; then
    url="https://github.com/$repo/releases/download/$short/$asset"
    mkdir -p "$cache"
    tmp="$cache/.download.$$"
    if curl -fsSL --connect-timeout 8 --max-time 30 -o "$tmp" "$url" 2>/dev/null; then
      chmod +x "$tmp"
      mv -f "$tmp" "$pbin"
      # Drop downloads cached for other commits.
      find "$cache" -maxdepth 1 -type f -name 'wrangler-*' ! -name "wrangler-$short" -delete 2>/dev/null
      exec "$pbin" tmux-entry --replace-daemon
    fi
    rm -f "$tmp"
  fi
fi

# 4. Build from source with cargo. Backgrounded via run-shell -b so a first-time
#    compile never blocks tmux startup; tmux-entry (key binds, hooks, daemon) runs
#    once the binary exists. Until then the toggle/focus keys are simply unbound.
#    --replace-daemon makes the freshly built binary evict a daemon on old code.
if command -v cargo >/dev/null 2>&1; then
  build_log="${TMPDIR:-/tmp}/wrangler-build.log"
  tmux display-message "wrangler: building with cargo (first run), sidebar available shortly..."
  tmux run-shell -b "if cd '$CURRENT_DIR' && cargo build --release >'$build_log' 2>&1; then '$CURRENT_DIR/target/release/wrangler' tmux-entry --replace-daemon; else tmux display-message 'wrangler: cargo build failed (see $build_log)'; fi"
  exit 0
fi

# 5. Nothing available: no prebuilt for this platform/commit and no cargo.
tmux display-message "wrangler: no prebuilt binary for ${asset:-this platform} at ${short:-unknown} and cargo not found (install cargo, or run: cargo build --release)"
exit 0
