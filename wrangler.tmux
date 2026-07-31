#!/usr/bin/env bash
# TPM entry point for tmux-agent-wrangler: obtain the wrangler binary and hand off
# to its tmux-entry subcommand, which does all the tmux setup.
#
# The plugin keeps its binary at one path in the cache, keyed by the checked-out
# commit, whether that binary was downloaded or built. Where it came from is not
# a factor in resolving it: a commit whose binary is already cached runs it, and
# any other commit obtains one, preferring the prebuilt release for this platform
# and building from source when there is none.

CURRENT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

# An explicit install on PATH overrides the managed binary. This is how a
# developer runs their own build, since a working tree with uncommitted changes
# still resolves to the release binary for its commit.
path_bin="$(command -v wrangler 2>/dev/null)"
if [ -x "$path_bin" ]; then
  exec "$path_bin" tmux-entry
fi

# The commit names the binary, so an update resolves to a path that does not
# exist yet and is obtained afresh. Only a git checkout can be identified.
short="$(git -C "$CURRENT_DIR" rev-parse --short=7 HEAD 2>/dev/null)"
if [ -z "$short" ]; then
  tmux display-message "wrangler: the plugin must be a git checkout (TPM installs one for you)"
  exit 0
fi

cache="${XDG_CACHE_HOME:-$HOME/.cache}/tmux-agent-wrangler"
bin="$cache/wrangler-$short"
if [ -x "$bin" ]; then
  exec "$bin" tmux-entry
fi

# This commit has no binary yet. Anything cached for another commit is now
# superseded; an already-running daemon keeps its own unlinked copy alive.
mkdir -p "$cache"
find "$cache" -maxdepth 1 -type f -name 'wrangler-*' ! -name "wrangler-$short" -delete 2>/dev/null

# The release asset for this platform, empty if none is published for it.
asset=""
case "$(uname -s)/$(uname -m)" in
  Linux/x86_64) asset="wrangler-linux-x64" ;;
  Darwin/arm64 | Darwin/aarch64) asset="wrangler-macos-arm64" ;;
esac

# owner/repo from the origin remote, so a fork that runs the build workflow
# fetches its own releases. Matched from the host rather than a fixed scheme:
# TPM clones with a credential-suppressing "https://git::@github.com/" prefix,
# and ssh remotes use "git@github.com:". A remote that is not GitHub leaves this
# empty, which builds from source instead.
repo=""
origin_url="$(git -C "$CURRENT_DIR" remote get-url origin 2>/dev/null)"
case "$origin_url" in
  *github.com[:/]*)
    repo="$(printf '%s' "$origin_url" | sed -E 's#^.*github\.com[:/]##; s#\.git$##')"
    ;;
esac

# Prefer the prebuilt binary: it needs no toolchain and is ready immediately. The
# release assets are public, so this is an unauthenticated fetch. A new binary
# evicts a daemon still running the old one.
if [ -n "$asset" ] && [ -n "$repo" ] && command -v curl >/dev/null 2>&1; then
  tmp="$cache/.download.$$"
  if curl -fsSL --connect-timeout 8 --max-time 30 -o "$tmp" \
    "https://github.com/$repo/releases/download/$short/$asset" 2>/dev/null; then
    chmod +x "$tmp"
    mv -f "$tmp" "$bin"
    exec "$bin" tmux-entry --replace-daemon
  fi
  rm -f "$tmp"
fi

# No prebuilt for this platform, or it could not be fetched (offline, or the
# release for this commit is not published yet): build from source. The build is
# backgrounded via run-shell -b so a first-time compile never blocks tmux
# startup, and its artifact is copied into the cache so it is run from the same
# path a downloaded one would be. Until it lands the toggle/focus keys are unbound.
if command -v cargo >/dev/null 2>&1; then
  build_log="${TMPDIR:-/tmp}/wrangler-build.log"
  tmux display-message "wrangler: building with cargo, sidebar available shortly..."
  tmux run-shell -b "if cd '$CURRENT_DIR' && cargo build --release >'$build_log' 2>&1; then cp -f '$CURRENT_DIR/target/release/wrangler' '$bin' && '$bin' tmux-entry --replace-daemon; else tmux display-message 'wrangler: cargo build failed (see $build_log)'; fi"
  exit 0
fi

tmux display-message "wrangler: no prebuilt binary for ${asset:-this platform} at $short and cargo not found (install cargo, or put a wrangler binary on your PATH)"
exit 0
