#!/usr/bin/env python3
"""Install (or remove) tmux-agent-wrangler's agent hooks.

Reads scripts/hooks-manifest.json - the per-agent list of which agent
lifecycle event maps to which agent-hook.sh action - and writes the hooks
into each agent's own config, wiring the absolute path to this repo's
agent-hook.sh so it works from any clone/TPM location.

Usage: install-hooks.py [claude|copilot|all] [--uninstall]   (default: all, install)

Two config shapes are handled, chosen by each agent's "format" in the manifest:

- "claude": a *shared* settings.json (~/.claude/settings.json) that also holds
  the user's unrelated hooks and other keys. We merge non-destructively: only
  wrangler's own hook groups (identified by the agent-hook.sh command) are
  replaced, everything else is left untouched, the file mode is preserved, and
  a <target>.wrangler.bak backup is written before the first change.

- "copilot": a dedicated file we own outright (~/.copilot/hooks/wrangler.json),
  which Copilot loads alongside other per-tool hook files. We write it wholesale.

The installer is idempotent: re-running install reproduces identical output.
"""
import json
import os
import re
import shlex
import shutil
import sys
import tempfile

SCRIPT_DIR = os.path.dirname(os.path.realpath(__file__))
MANIFEST = os.path.join(SCRIPT_DIR, "hooks-manifest.json")
HOOK = os.path.join(SCRIPT_DIR, "agent-hook.sh")


def command(agent, action):
    """The shell command a hook runs, with the hook path safely quoted."""
    return f"{shlex.quote(HOOK)} {agent} {action}"


def is_wrangler_command(cmd, agent):
    """Whether a hook command belongs to this agent's wrangler hooks.

    Matches on agent-hook.sh + the agent name rather than an exact string, so
    entries written with an older path form (e.g. the README's ~/.tmux/... path)
    are still recognised and upgraded in place.
    """
    return bool(re.search(rf"\bagent-hook\.sh\b.*\b{re.escape(agent)}\b", cmd))


def atomic_write(path, text, mode):
    """Replace path's contents atomically, creating parents, with file mode."""
    directory = os.path.dirname(path)
    os.makedirs(directory, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=directory)
    try:
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.chmod(tmp, mode)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except OSError:
            pass
        raise


def dumps(data):
    return json.dumps(data, indent=2) + "\n"


def install_claude(agent, spec, uninstall):
    """Merge (or strip) wrangler's hook groups in a shared settings.json."""
    path = os.path.expanduser(spec["target"])
    try:
        with open(path) as f:
            text = f.read()
        data = json.loads(text) if text.strip() else {}
    except FileNotFoundError:
        data = {}
    if not isinstance(data, dict):
        raise SystemExit(f"{path}: expected a JSON object at the top level")

    hooks = data.get("hooks")
    if not isinstance(hooks, dict):
        hooks = {}

    for event, actions in spec["events"].items():
        groups = [
            g for g in hooks.get(event, [])
            if not any(
                is_wrangler_command(h.get("command", ""), agent)
                for h in g.get("hooks", [])
            )
        ]
        if not uninstall:
            groups.append({
                "hooks": [
                    {"type": "command", "command": command(agent, action)}
                    for action in actions
                ]
            })
        if groups:
            hooks[event] = groups
        else:
            hooks.pop(event, None)

    if hooks:
        data["hooks"] = hooks
    else:
        data.pop("hooks", None)

    if os.path.exists(path):
        shutil.copy2(path, path + ".wrangler.bak")
        mode = os.stat(path).st_mode & 0o777
    else:
        mode = 0o600  # settings.json can hold secrets; default to private
    atomic_write(path, dumps(data), mode)
    verb = "Uninstalled from" if uninstall else "Installed into"
    print(f"{agent}: {verb} {path}")


def install_copilot(agent, spec, uninstall):
    """Write (or delete) the dedicated wrangler.json file we own."""
    path = os.path.expanduser(spec["target"])
    if uninstall:
        try:
            os.unlink(path)
            print(f"{agent}: Removed {path}")
        except FileNotFoundError:
            print(f"{agent}: Nothing to remove at {path}")
        return

    doc = {
        "version": 1,
        "hooks": {
            event: [
                {"type": "command", "bash": command(agent, action)}
                for action in actions
            ]
            for event, actions in spec["events"].items()
        },
    }
    mode = os.stat(path).st_mode & 0o777 if os.path.exists(path) else 0o644
    atomic_write(path, dumps(doc), mode)
    print(f"{agent}: Installed into {path}")


FORMATS = {"claude": install_claude, "copilot": install_copilot}


def main(argv):
    selector = "all"
    uninstall = False
    for arg in argv:
        if arg == "--uninstall":
            uninstall = True
        elif arg in ("-h", "--help"):
            print(__doc__)
            return 0
        elif arg in ("all", "claude", "copilot"):
            selector = arg
        else:
            print(f"unknown argument: {arg}\n\n{__doc__}", file=sys.stderr)
            return 2

    with open(MANIFEST) as f:
        manifest = json.load(f)

    agents = manifest.keys() if selector == "all" else [selector]
    for agent in agents:
        spec = manifest.get(agent)
        if spec is None:
            print(f"{agent}: not in manifest, skipping", file=sys.stderr)
            continue
        FORMATS[spec["format"]](agent, spec, uninstall)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
