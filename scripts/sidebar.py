#!/usr/bin/env python3
"""Sidebar TUI for tmux-agent-wrangler.

Lists the session's windows with their panes as a tree, plus active agent
sessions (Claude Code, Copilot CLI, ...) registered by scripts/agent-hook.sh,
one section per agent. Windows, panes, and agent sessions are the interactive
rows: Up/Down or j/k to move, Enter or a mouse click to focus.

Every window has its own sidebar pane (spawned by scripts/spawn.sh), so
switching windows never rearranges a layout. The instances share the current
selection through a state file. A sidebar whose window has no real panes left
exits, letting tmux close the window.
"""
import atexit
import curses
import json
import locale
import os
import subprocess
import sys
import time

# The agent-row label logic is shared with agent-hook.sh (which imports the same
# module to build the OSC 9 notification body), so the notification text matches
# these rows exactly.
from session_labels import agent_label, label_mode_from, session_meta

SIDEBAR_PANE = os.environ["TMUX_PANE"]
STATE_DIR = os.path.join(
    os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
    "tmux-agent-wrangler",
)
REGISTRY = os.path.join(STATE_DIR, "sessions")
ATTENTION = os.path.join(STATE_DIR, "attention")
WORKING = os.path.join(STATE_DIR, "working")
# One flag file per session id for which the bell/notification has already fired
# this attention episode - the atomic single-fire dedup across the per-window
# sidebars (see notify_attention).
NOTIFIED = os.path.join(STATE_DIR, "notified")
SELECTION_FILE = os.path.join(STATE_DIR, "selection")
WIDTH_FILE = os.path.join(STATE_DIR, "width")

# Claude Code's config dir, home to the team configs
# (teams/<id>/config.json) we read agent-teams teammate colors from - those
# are not in the teammate's transcript. Honors CLAUDE_CONFIG_DIR like Claude
# Code itself.
CLAUDE_DIR = os.environ.get("CLAUDE_CONFIG_DIR") or os.path.expanduser("~/.claude")
TEAMS_DIR = os.path.join(CLAUDE_DIR, "teams")

# Claude's own RGB for each named session/teammate color, per theme family, so
# a row matches what Claude shows rather than a saturated stand-in. Values are
# lifted from the CLI's theme palettes (the *_FOR_SUBAGENTS_ONLY tokens):
#  - MUTED: the Tailwind-600-ish set the plain `dark` and `light` themes share.
#  - SATURATED / BRIGHT: the two daltonized themes (blue-tinted for deuteranopia).
# The ANSI themes deliberately defer to the terminal's own palette, so they are
# rendered with the base curses colors (ANSI_BASE) instead of a fixed RGB.
_PALETTE_MUTED = {
    "red": (220, 38, 38), "blue": (106, 155, 204), "green": (22, 163, 74),
    "yellow": (202, 138, 4), "purple": (130, 125, 189), "orange": (217, 119, 87),
    "pink": (196, 102, 134), "cyan": (8, 145, 178),
}
_PALETTE_SATURATED = {
    "red": (204, 0, 0), "blue": (0, 102, 204), "green": (0, 204, 0),
    "yellow": (255, 204, 0), "purple": (128, 0, 128), "orange": (255, 128, 0),
    "pink": (255, 102, 178), "cyan": (0, 178, 178),
}
_PALETTE_BRIGHT = {
    "red": (255, 102, 102), "blue": (102, 178, 255), "green": (102, 255, 102),
    "yellow": (255, 255, 102), "purple": (178, 102, 255), "orange": (255, 178, 102),
    "pink": (255, 153, 204), "cyan": (102, 204, 204),
}
# Fallback for ANSI themes and <256-color terminals: nearest base curses color.
# orange/pink have no base equivalent, so they share with yellow/magenta.
_PALETTE_ANSI_BASE = {
    "red": curses.COLOR_RED, "blue": curses.COLOR_BLUE, "green": curses.COLOR_GREEN,
    "yellow": curses.COLOR_YELLOW, "purple": curses.COLOR_MAGENTA,
    "orange": curses.COLOR_YELLOW, "pink": curses.COLOR_MAGENTA, "cyan": curses.COLOR_CYAN,
}
_AGENT_COLOR_NAMES = ("red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan")

# color name -> allocated curses color-pair id, filled in by init_agent_colors.
_agent_color_pairs = {}

# OSC-progress state color key -> the base UI color pair allocated in main().
# green (1) and yellow (3) are shared with other UI uses; red (4) is added there.
_INDICATOR_PAIRS = {"green": 1, "yellow": 3, "red": 4}


def _rgb_to_ansi256(r, g, b):
    """The xterm-256 index Claude itself would use for this RGB.

    Claude renders a session's color by converting its theme RGB to a 256-color
    index with the ansi-styles (chalk) algorithm: the 6x6x6 cube, rounding each
    channel to its nearest sixth, with a separate 24-step gray ramp. We use that
    exact formula (not a nearest-RGB distance, which lands a shade off - e.g.
    dark yellow 202,138,4 gives 172, an orange, where Claude emits 178, a gold)
    so a row's index equals the one Claude prints and the colors match."""
    if r == g == b:
        if r < 8:
            return 16
        if r > 248:
            return 231
        return 232 + round((r - 8) / 247 * 24)
    return 16 + 36 * round(r / 255 * 5) + 6 * round(g / 255 * 5) + round(b / 255 * 5)


def read_theme():
    """The user's Claude theme name (settings.json 'theme'), defaulting to 'dark'
    - which, like 'light', uses the muted palette, so an unknown/missing value is
    a safe default."""
    try:
        with open(os.path.join(CLAUDE_DIR, "settings.json")) as f:
            return (json.load(f).get("theme") or "dark").lower()
    except (OSError, ValueError):
        return "dark"


def _theme_palette(theme):
    """The RGB palette dict for a theme, or None to use the terminal's own ANSI
    colors (the ANSI themes, or a <256-color terminal). `dark`/`light` and any
    unrecognized theme share the muted palette."""
    if theme.endswith("-ansi"):
        return None
    if theme == "dark-daltonized":
        return _PALETTE_BRIGHT
    if theme == "light-daltonized":
        return _PALETTE_SATURATED
    return _PALETTE_MUTED


def tmux(*args):
    result = subprocess.run(("tmux",) + args, capture_output=True, text=True)
    return result.stdout


def min_width():
    value = tmux("show-option", "-gqv", "@wrangler-min-width").strip()
    return int(value) if value.isdigit() else 24


def sync_width_enabled():
    value = tmux("show-option", "-gqv", "@wrangler-sync-width").strip().lower()
    return value not in ("off", "0", "no", "false")


def label_mode():
    """How to label an agent session row: its title ('name', default) or the
    working-directory basename ('dir'). Any unset/unknown value means 'name'."""
    return label_mode_from(tmux("show-option", "-gqv", "@wrangler-label"))


def hook_progress_enabled():
    """Whether to show the hook-driven ◐/● working/attention indicators.
    Default on (opt-out); a display toggle only - agent-hook.sh keeps writing
    its markers regardless."""
    value = tmux("show-option", "-gqv", "@wrangler-hook-progress").strip().lower()
    return value not in ("off", "0", "no", "false")


def osc_progress_enabled():
    """Whether to show OSC 9;4 progress (pane_pb_state/pane_pb_progress) as a
    percentage. Default off (opt-in)."""
    value = tmux("show-option", "-gqv", "@wrangler-osc-progress").strip().lower()
    return value in ("on", "1", "yes", "true")


def bell_enabled():
    """Whether to ring the terminal bell when an agent needs attention. Default
    off (opt-in)."""
    value = tmux("show-option", "-gqv", "@wrangler-bell").strip().lower()
    return value in ("on", "1", "yes", "true")


def osc_notify_mode():
    """The desktop-notification protocol for an agent needing attention:
    '777' (also the meaning of on/1/yes/true), '9', or '' when disabled (the
    default). See notify_attention."""
    value = tmux("show-option", "-gqv", "@wrangler-osc-notify").strip().lower()
    if value in ("777", "on", "1", "yes", "true"):
        return "777"
    return "9" if value == "9" else ""


# team id -> (mtime, {pane_id: color}). A team's config is read only when we
# have a teammate from that team (keyed by its transcript's teamName), and
# re-parsed only when the config's mtime moves - no directory scanning.
_team_colors_cache = {}


def team_pane_colors(team):
    """Map tmux pane id -> assigned color for the members of one agent-teams
    team, read from TEAMS_DIR/<team>/config.json.

    Claude records a teammate's color there (one member per teammate, with a
    `tmuxPaneId` and `color`), never in the teammate's own transcript - so unlike
    a top-level session's /color (see session_meta) it has to be looked up here.
    Empty `team`, or a missing/unreadable config, yields {}."""
    if not team:
        return {}
    cfg = os.path.join(TEAMS_DIR, team, "config.json")
    try:
        mtime = os.path.getmtime(cfg)
    except OSError:
        return {}
    cached = _team_colors_cache.get(team)
    if cached and cached[0] == mtime:
        return cached[1]
    panes = {}
    try:
        with open(cfg) as f:
            members = json.load(f).get("members", [])
    except (OSError, ValueError):
        members = []
    for m in members:
        pane, color = m.get("tmuxPaneId"), m.get("color")
        if pane and color:
            panes[pane] = color
    _team_colors_cache[team] = (mtime, panes)
    return panes


def read_width():
    try:
        with open(WIDTH_FILE) as f:
            value = f.read().strip()
    except OSError:
        return None
    return int(value) if value.isdigit() else None


def write_width(width):
    try:
        os.makedirs(STATE_DIR, exist_ok=True)
        with open(WIDTH_FILE, "w") as f:
            f.write(str(width))
    except OSError:
        pass


def strip_status_prefix(title):
    """Drop the status glyph Claude Code prefixes onto the terminal/pane title.

    Claude sets the pane title to "<glyph> <session title>" - a spinner frame
    (braille) while working, `✳` when idle; the glyph is always a single
    non-alphanumeric token. Split on the first space and drop that first token
    when it holds no alphanumeric character, so the glyph goes but a real title
    (which starts with a word) is kept intact. Returns the remainder stripped."""
    head, sep, rest = title.strip().partition(" ")
    if sep and not any(c.isalnum() for c in head):
        return rest.strip()
    return title.strip()


def fetch_windows():
    windows = []
    for line in tmux(
        "list-windows", "-F", "#{window_id}\t#{window_index}\t#{window_name}\t#{window_active}"
    ).splitlines():
        wid, index, name, active = line.split("\t", 3)
        windows.append({"id": wid, "index": index, "name": name, "active": active == "1", "panes": []})

    by_id = {w["id"]: w for w in windows}
    pane_to_window = {}
    pane_paths = {}
    pane_progress = {}
    pane_titles = {}
    sidebars = set()
    sidebar_active = False
    # The OSC 9;4 pane vars (pb_state / pb_progress) are fixed enum/number
    # fields, so they sit before pane_current_path and pane_title; the free-form
    # title stays the trailing field and a path is very unlikely to contain a
    # tab. On a tmux too old to know these vars they expand to empty, which we
    # read as "no progress".
    for line in tmux(
        "list-panes", "-s", "-F",
        "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{@wrangler_sidebar}\t#{pane_pb_state}\t#{pane_pb_progress}\t#{pane_current_path}\t#{pane_title}",
    ).splitlines():
        wid, pid, index, active, flag, pb_state, pb_progress, path, title = line.split("\t", 8)
        if wid not in by_id:
            continue
        pane_to_window[pid] = by_id[wid]
        pane_paths[pid] = path
        progress = int(pb_progress) if pb_progress.isdigit() else None
        pane_progress[pid] = (pb_state, progress)
        if flag == "1" or pid == SIDEBAR_PANE:
            sidebars.add(pid)
            if pid == SIDEBAR_PANE:
                sidebar_active = active == "1"
            continue
        # A real pane's title is the session title Claude sets on it (minus its
        # status glyph); fetch_agent_sessions matches it to associate a
        # daemon-hosted session with the pane(s) currently displaying it.
        pane_titles[pid] = strip_status_prefix(title)
        by_id[wid]["panes"].append(
            {"id": pid, "index": index, "active": active == "1", "title": title,
             "pb_state": pb_state, "pb_progress": progress}
        )
    return windows, pane_to_window, sidebars, pane_paths, pane_progress, pane_titles, sidebar_active


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        pass
    return True


def fetch_agent_sessions(pane_to_window, focused_panes, pane_paths, pane_titles):
    """Read the hook registry; prune dead entries and file each session under
    the window(s) of the pane(s) currently displaying it.

    A session carries a turn status set by agent-hook.sh: "working" while a
    turn is in progress, "attention" once it finishes a turn or notifies. The
    attention marker (and so the dot) clears once any pane showing the session
    is the focused pane, meaning "wanted attention while you were not looking at
    it"; the working marker persists until the turn ends, since it reflects the
    agent actually being busy.

    A daemon-hosted session registers with no pane (no TMUX_PANE at hook time),
    so it is associated by matching its title (session_meta) against each pane's
    live title (pane_titles). Claude Code sets the pane title to the session
    title however the session is viewed - `claude attach`, `--resume`, or the
    agents view - so a title match means that pane is displaying the session. A
    session shown in several panes yields one placement per pane (listed under
    each window); one shown nowhere stays detached under "Agents".

    The displayed cwd tracks the pane's live path (pane_paths) so it follows the
    agent as it changes directory, falling back to the cwd recorded at
    registration if the pane reports no path.
    """
    try:
        names = sorted(os.listdir(REGISTRY))
    except OSError:
        return []
    mode = label_mode()
    # Every pane id on the tmux server (all sessions), so a record whose pane
    # lives in another session can be told apart from one whose pane is gone.
    all_panes = set(tmux("list-panes", "-a", "-F", "#{pane_id}").split())

    # Pass 1: parse and prune records, resolving each surviving one's identity
    # and title (the string matched against pane titles below).
    candidates = []
    for name in names:
        path = os.path.join(REGISTRY, name)
        attn_marker = os.path.join(ATTENTION, name)
        work_marker = os.path.join(WORKING, name)

        def prune():
            for stale in (path, attn_marker, work_marker, os.path.join(NOTIFIED, name)):
                try:
                    os.unlink(stale)
                except OSError:
                    pass

        try:
            # rstrip only the newline, not tabs: the pane field (field 1) may be
            # empty for a pane-less session, and .strip() would eat that leading
            # tab and shift every field left.
            with open(path) as f:
                fields = f.read().rstrip("\n").split("\t")
        except OSError:
            continue
        if len(fields) == 2:  # legacy claude-hook.sh format: pane, cwd
            pane, agent, pid, cwd, transcript = fields[0], "claude", "", fields[1], ""
        elif len(fields) >= 4:  # pane, agent, pid, cwd[, transcript]
            pane, agent, pid, cwd = fields[0], fields[1], fields[2], fields[3]
            transcript = fields[4] if len(fields) >= 5 else ""
        else:
            continue
        # A dead agent process is gone everywhere: drop the record outright.
        if pid.isdigit() and not pid_alive(int(pid)):
            prune()
            continue
        # A recorded pane not in this tmux session: if it exists nowhere and no
        # live PID vouches for it (legacy / failed capture), the record is stale
        # - prune it. Otherwise it belongs to another session (whose own sidebar
        # lists it) or is unverifiable, so it just is not a local pane here.
        if pane and pane not in pane_to_window and pane not in all_panes and not pid.isdigit():
            prune()
            continue
        prefix = agent + "-"
        session_id = name[len(prefix):] if name.startswith(prefix) else name
        title, agent_name, team, color = session_meta(transcript, agent, session_id)
        candidates.append(
            {"id": name, "agent": agent, "recorded_pane": pane, "cwd": cwd,
             "title": title, "agent_name": agent_name, "team": team, "color": color,
             "attn_marker": attn_marker, "work_marker": work_marker}
        )

    # Resolve which session owns each local pane by title: a pane showing
    # exactly one session's title associates to it; a title shared by several
    # sessions is broken by the recorded pane, then the cwd, and left unassigned
    # if still ambiguous (better no jump than a wrong one). Empty titles never
    # match.
    pane_owner = {}
    for pid, ptitle in pane_titles.items():
        if not ptitle:
            continue
        matches = [c for c in candidates if c["title"] == ptitle]
        if len(matches) == 1:
            pane_owner[pid] = matches[0]["id"]
        elif len(matches) > 1:
            pool = [c for c in matches if c["recorded_pane"] == pid] or \
                   [c for c in matches if c["cwd"] == pane_paths.get(pid)]
            if len(pool) == 1:
                pane_owner[pid] = pool[0]["id"]

    # Pass 2: place each session under its matched pane(s), or detached.
    sessions = []
    for c in candidates:
        matched = [c["recorded_pane"]] if c["recorded_pane"] in pane_to_window else []
        for pid, owner in pane_owner.items():
            if owner == c["id"] and pid not in matched:
                matched.append(pid)

        attention = os.path.exists(c["attn_marker"])
        if attention and any(p in focused_panes for p in matched):
            try:
                os.unlink(c["attn_marker"])
            except OSError:
                pass
            attention = False
        status = "attention" if attention else ("working" if os.path.exists(c["work_marker"]) else "")
        # A top-level session records its color in the transcript; a teammate's
        # lives in its team config instead, so fall back to that (by pane), read
        # only for actual teammates.
        team_colors = team_pane_colors(c["team"]) if (not c["color"] and c["agent_name"]) else {}

        def place(pane, window):
            display_cwd = pane_paths.get(pane) or c["cwd"]
            dir_name = os.path.basename(display_cwd.rstrip("/")) or display_cwd
            sessions.append(
                {"id": c["id"], "agent": c["agent"], "pane": pane, "cwd": display_cwd,
                 "label": agent_label(mode, c["title"], c["agent_name"], dir_name),
                 "window": window, "status": status,
                 "color": c["color"] or team_colors.get(pane, "")}
            )

        if not matched:
            # A recorded pane that is not local means the session lives in
            # another tmux session (listed by its own sidebar), not here; only a
            # pane-less (daemon) session is genuinely detached under "Agents".
            if c["recorded_pane"]:
                continue
            place("", None)
            continue
        for pid in matched:
            place(pid, pane_to_window[pid])
    return sessions


def read_selection():
    try:
        with open(SELECTION_FILE) as f:
            parts = f.read().rstrip("\n").split("\t")
    except OSError:
        return None
    return tuple(parts) if parts and parts[0] else None


def write_selection(key):
    try:
        os.makedirs(STATE_DIR, exist_ok=True)
        with open(SELECTION_FILE, "w") as f:
            f.write("\t".join(key))
    except OSError:
        pass


def _write_tty(path, data):
    """Best-effort write of raw bytes to a tty (a pane tty for the bell, a client
    tty for the notification). Never disturbs the draw loop."""
    if not path:
        return
    try:
        with open(path, "w") as f:
            f.write(data)
    except OSError:
        pass


def _client_ttys():
    """The tty of every client attached to this sidebar's tmux session - the
    terminals a desktop notification is written to."""
    session = tmux("display-message", "-p", "-t", SIDEBAR_PANE, "#{session_name}").strip()
    if not session:
        return []
    out = tmux("list-clients", "-t", session, "-F", "#{client_tty}")
    return [t for t in out.split("\n") if t.strip()]


def notify_attention(sessions, bell_on, osc_mode):
    """Ring the bell / raise a desktop notification once per attention episode.

    Reacts to the same attention state as the `●` row indicator: a session is
    signalled while it holds an attention marker. fetch_agent_sessions has already
    cleared that marker for a session whose pane is focused, so one you are
    looking at never fires ("wanted you while you weren't looking"). Firing from
    the poll loop means nothing signals while the sidebar is off.

    Dedup: there is one sidebar per window, all polling, and a session can be
    placed under several windows at once - so the fire is gated on an atomic
    create of a per-session-id flag under NOTIFIED. Exactly one sidebar wins per
    episode; the flag is pruned once the episode ends (its attention marker gone),
    rearming the next one. The bell writes BEL to the session's pane tty (so tmux
    applies its own monitor-bell); the notification writes the OSC escape to each
    client tty. Both best-effort.
    """
    # One representative placement per session id, preferring the one under the
    # active window (whose name titles the notification). Only sessions visible in
    # a local window are signalled: a fully-detached session (shown nowhere) has
    # no pane to bell and no window to name.
    attention = {}
    for s in sessions:
        if s["status"] != "attention" or s["window"] is None:
            continue
        cur = attention.get(s["id"])
        if cur is None or (s["window"]["active"] and not cur["window"]["active"]):
            attention[s["id"]] = s

    if bell_on or osc_mode:
        try:
            os.makedirs(NOTIFIED, exist_ok=True)
        except OSError:
            attention = {}  # can't dedup safely, so skip firing (still prune below)
        for sid, s in attention.items():
            try:
                fd = os.open(os.path.join(NOTIFIED, sid),
                             os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o644)
            except FileExistsError:
                continue  # another sidebar, or an earlier poll, already fired
            except OSError:
                continue
            os.close(fd)
            if osc_mode:
                win = s["window"]["name"]
                text = f"{win} · {s['label']}" if s["label"] else win
                if osc_mode == "777":
                    esc = f"\033]777;notify;{s['agent']};{text}\007"
                else:
                    esc = f"\033]9;{text}\007"
                for tty in _client_ttys():
                    _write_tty(tty, esc)
            if bell_on and s["pane"]:
                _write_tty(tmux("display-message", "-p", "-t", s["pane"], "#{pane_tty}").strip(), "\a")

    # Prune flags whose episode has ended (no attention marker), rearming the next
    # attention on that session. Runs even when disabled, so a disable/enable
    # cycle starts clean. Sessions still in attention this poll keep their flag.
    try:
        flags = os.listdir(NOTIFIED)
    except OSError:
        flags = []
    for name in flags:
        if name not in attention and not os.path.exists(os.path.join(ATTENTION, name)):
            try:
                os.unlink(os.path.join(NOTIFIED, name))
            except OSError:
                pass


# pane_pb_state values that mean "no progress to show": `hidden` (a pane that
# never set progress or cleared it via OSC 9;4;0) and the empty string a tmux
# too old to know the var expands to.
_INACTIVE_STATES = ("", "hidden")

# OSC 9;4 state -> the color key used for its percentage. 'indeterminate' has no
# meaningful number so it renders as a busy spinner in the row's own color
# instead.
_STATE_COLOR = {"normal": "green", "paused": "yellow", "error": "red"}

# Frames of the "busy" spinner cycled through for the working / indeterminate
# indicator. The main loop advances the frame index on a sub-second timer
# (independent of the 1s data poll), so a caller passes the current index and
# `spinner_frame` wraps it. Single-width braille glyphs keep the pinned
# indicator one column wide across every frame.
_SPINNER = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"
_SPINNER_FRAMES = frozenset(_SPINNER)


def spinner_frame(frame):
    """The busy-spinner glyph for animation frame `frame` (any int; wraps)."""
    return _SPINNER[frame % len(_SPINNER)]


def progress_indicator(hook_status, pb_state, pb_progress, hook_on, osc_on, frame):
    """(text, color) for the right-pinned indicator, or ("", None).

    OSC wins when the pane reports an active state (see _INACTIVE_STATES);
    otherwise the hook glyph. A busy state (hook `working`, OSC `indeterminate`)
    draws the animated spinner for `frame`; `attention` stays the static ●.
    `color` is a state-color key for OSC percentages, or None to inherit the
    row's own attribute (the hook glyph and OSC 'indeterminate').
    """
    if osc_on and pb_state not in _INACTIVE_STATES:
        if pb_state == "indeterminate":
            return spinner_frame(frame), None
        text = f"{pb_progress}%" if pb_progress is not None else spinner_frame(frame)
        return text, _STATE_COLOR.get(pb_state)
    if hook_on and hook_status in ("working", "attention"):
        return {"attention": "●", "working": spinner_frame(frame)}[hook_status], None
    return "", None


def _append_agent_rows(rows, group, win, pane_progress, hook_on, osc_on, frame):
    """Append the tree rows for one group of agent sessions under a heading.

    `win` is the window the group is filed under, or None for the pane-less
    "Agents" group; it rides on each row so activate() can focus the pane (a
    None win is a no-op there, since a detached session has no pane to jump to).
    """
    last = len(group) - 1
    for i, s in enumerate(group):
        branch = "└─" if i == last else "├─"
        pb_state, pb_progress = pane_progress.get(s["pane"], ("", None))
        indicator, indicator_color = progress_indicator(
            s["status"], pb_state, pb_progress, hook_on, osc_on, frame
        )
        # The indicator rides on the item, not the text: draw() pins it to the
        # right edge so it survives a long title being truncated.
        rows.append(
            (f"   {branch} {s['label']}",
             {"kind": "agent", "key": ("a", s["id"], s["pane"]), "win": win, "pane": s["pane"],
              "status": s["status"], "color": s["color"],
              "indicator": indicator, "indicator_color": indicator_color})
        )


def build_rows(windows, sessions, pane_progress, pane_status, hook_on, osc_on, frame):
    """Flat list of (text, item) rows; item is a selectable dict, "header", or None.

    `pane_progress` maps pane id -> (pb_state, pb_progress) for OSC 9;4;
    `pane_status` maps pane id -> hook turn status ("attention"/"working") so a
    window-tree pane running an agent mirrors that agent's glyph. `frame` is the
    current spinner animation index (see progress_indicator). hook_on / osc_on
    gate the two indicator sources.
    """
    rows = [(" WINDOWS", "header"), ("", None)]
    for w in windows:
        marker = "*" if w["active"] else " "
        rows.append((f"{marker} {w['index']}: {w['name']}", {"kind": "window", "key": ("w", w["id"]), "win": w}))
        last = len(w["panes"]) - 1
        for i, p in enumerate(w["panes"]):
            branch = "└─" if i == last else "├─"
            active = "*" if p["active"] else " "
            indicator, indicator_color = progress_indicator(
                pane_status.get(p["id"], ""), p["pb_state"], p["pb_progress"], hook_on, osc_on, frame
            )
            rows.append(
                (f"   {branch}{active}{p['index']}: {p['title']}",
                 {"kind": "pane", "key": ("p", p["id"]), "win": w, "pane": p["id"],
                  "indicator": indicator, "indicator_color": indicator_color})
            )
    for agent in sorted({s["agent"] for s in sessions}):
        rows.append(("", None))
        rows.append((f" {agent.upper()}", "header"))
        rows.append(("", None))
        agent_sessions = [s for s in sessions if s["agent"] == agent]
        # Sessions that have a pane group under that pane's window.
        for w in windows:
            group = [s for s in agent_sessions if s["window"] is w]
            if not group:
                continue
            marker = "*" if w["active"] else " "
            rows.append(
                (f"{marker} {w['index']}: {w['name']}",
                 {"kind": "window", "key": ("w", agent, w["id"]), "win": w})
            )
            _append_agent_rows(rows, group, w, pane_progress, hook_on, osc_on, frame)
        # Pane-less sessions collect under a non-selectable "Agents" heading.
        detached = [s for s in agent_sessions if s["window"] is None]
        if detached:
            rows.append(("  Agents", None))
            _append_agent_rows(rows, detached, None, pane_progress, hook_on, osc_on, frame)
    return rows


def window_real_panes(win_id):
    panes = []
    for line in tmux("list-panes", "-t", win_id, "-F", "#{pane_id}\t#{@wrangler_sidebar}").splitlines():
        pid, _, flag = line.partition("\t")
        if flag != "1" and pid != SIDEBAR_PANE:
            panes.append(pid)
    return panes


def focus(win_id, pane_id=None):
    target_pane = pane_id
    if not target_pane:
        # Land focus on a real pane, not a sidebar.
        line = tmux("display-message", "-p", "-t", win_id, "#{pane_id}\t#{@wrangler_sidebar}").strip()
        active, _, flag = line.partition("\t")
        if flag == "1" or active == SIDEBAR_PANE:
            real = window_real_panes(win_id)
            target_pane = real[0] if real else None
    cmds = ["select-window", "-t", win_id]
    if target_pane:
        cmds += [";", "select-pane", "-t", target_pane]
    tmux(*cmds)


def activate(item):
    win = item.get("win")
    if win is None:
        return  # detached agent session: no pane to focus
    focus(win["id"], item.get("pane"))


def _fit(text, field):
    """Fit `text` to exactly `field` columns: ellipsize when it overflows,
    otherwise left-pad so the row fills its width (for the selection bar)."""
    if field <= 0:
        return ""
    if len(text) > field:
        return text[: field - 1] + "…" if field > 1 else "…"
    return text.ljust(field)


def init_agent_colors():
    """Allocate a curses color pair per Claude color name for agent rows, matched
    to the user's theme: the theme's RGB mapped to the same xterm-256 index
    Claude uses (see _rgb_to_ansi256) on a 256-color terminal, else the base ANSI
    color. Pairs 1-4 are taken by the base UI colors, so these start at 10."""
    rgb = _theme_palette(read_theme()) if curses.COLORS >= 256 else None
    pair_id = 10
    for cname in _AGENT_COLOR_NAMES:
        cnum = _rgb_to_ansi256(*rgb[cname]) if rgb else _PALETTE_ANSI_BASE[cname]
        try:
            curses.init_pair(pair_id, cnum, -1)
        except curses.error:
            continue
        _agent_color_pairs[cname] = pair_id
        pair_id += 1


def draw(stdscr, rows, selected_key, offset, has_focus):
    height, width = stdscr.getmaxyx()
    sel_row = next(
        (i for i, (_, item) in enumerate(rows) if isinstance(item, dict) and item["key"] == selected_key), 0
    )
    if sel_row < offset:
        offset = sel_row
    elif sel_row >= offset + height:
        offset = sel_row - height + 1
    offset = max(0, min(offset, max(0, len(rows) - height)))

    stdscr.erase()
    for screen_y, row_idx in enumerate(range(offset, min(len(rows), offset + height))):
        text, item = rows[row_idx]
        if item == "header":
            attr = curses.A_BOLD | curses.A_UNDERLINE
        elif isinstance(item, dict):
            if item["kind"] == "window":
                attr = curses.A_BOLD
                if item["win"]["active"]:
                    attr |= curses.color_pair(1)
            elif item["kind"] == "agent":
                # Color the whole row in the agent's own assigned color (Claude's
                # /color or the teammate's team color), falling back to the
                # default agent color when none is known. Turn state is carried
                # by the pinned indicator (●/◐) and bold, so it survives here.
                pair = _agent_color_pairs.get(item.get("color") or "")
                attr = curses.color_pair(pair if pair else 2)
                if item["status"] in ("attention", "working"):
                    attr |= curses.A_BOLD
            else:
                attr = curses.A_DIM
            if has_focus and item["key"] == selected_key:
                attr |= curses.A_REVERSE
        else:
            attr = curses.A_DIM
        field = width - 1
        selected = has_focus and isinstance(item, dict) and item["key"] == selected_key
        indicator = item.get("indicator", "") if isinstance(item, dict) else ""
        # Reserve a space plus the indicator's own width at the right edge, so it
        # stays visible in the rightmost cells however narrow the pane gets. The
        # indicator gets its own attribute (an OSC state color), keeping the
        # selection bar (reverse video) continuous across the whole row.
        reserve = len(indicator) + 1
        if indicator and field >= reserve + 1:
            try:
                stdscr.addnstr(screen_y, 0, _fit(text, field - reserve), field - reserve, attr)
            except curses.error:
                pass
            color = item.get("indicator_color")
            ind_attr = curses.color_pair(_INDICATOR_PAIRS[color]) if color else attr
            if selected:
                ind_attr |= curses.A_REVERSE
            try:
                stdscr.addnstr(screen_y, field - reserve, f" {indicator}", reserve, ind_attr)
            except curses.error:
                pass
        else:
            try:
                stdscr.addnstr(screen_y, 0, _fit(text, field), field, attr)
            except curses.error:
                pass
    stdscr.refresh()
    return offset


# DEC private mode 1004: with it enabled the terminal sends a focus report
# (ESC[I on focus-in, ESC[O on focus-out) when this pane gains or loses focus.
# We do not decode it - curses hands it back as an unrecognised key code, which
# is enough to wake the blocking getch() so the loop redraws and the selection
# highlight tracks the focus change at once instead of on the next 1s poll.
# tmux only sends these when its focus-events option is on, so the payoff
# depends on the user setting `focus-events on`; without it the sidebar falls
# back to the poll (and focus.sh's nudge still covers the focus key). We must
# not intercept a raw ESC ourselves: an arrow key that arrives just after a
# focus report can fragment into a bare ESC + '[' + letter, and swallowing it
# would eat the keypress.
FOCUS_ON = b"\x1b[?1004h"
FOCUS_OFF = b"\x1b[?1004l"


def set_focus_reporting(enabled):
    """Turn terminal focus reporting (mode 1004) on or off for this tty."""
    try:
        os.write(sys.stdout.fileno(), FOCUS_ON if enabled else FOCUS_OFF)
    except OSError:
        pass


def main(stdscr):
    curses.curs_set(0)
    curses.use_default_colors()
    curses.start_color()
    curses.init_pair(1, curses.COLOR_GREEN, -1)
    curses.init_pair(2, curses.COLOR_CYAN, -1)
    curses.init_pair(3, curses.COLOR_YELLOW, -1)
    curses.init_pair(4, curses.COLOR_RED, -1)
    init_agent_colors()
    curses.mousemask(curses.ALL_MOUSE_EVENTS)
    curses.mouseinterval(0)

    # Report focus changes while running and stop on exit.
    set_focus_reporting(True)
    atexit.register(set_focus_reporting, False)

    # The data poll (tmux queries, width sync, shared selection) runs at
    # POLL_INTERVAL; the spinner animates faster, so between polls we only
    # advance the frame and repaint the rows rebuilt from the cached poll data.
    # Any real input - a key, a focus report, a resize - forces a fresh poll, so
    # focus/selection stay as responsive as the old per-tick fetch. The fast
    # timer is used only while a spinner is on screen; an idle sidebar keeps
    # blocking for POLL_INTERVAL as before.
    POLL_INTERVAL = 1.0
    ANIM_INTERVAL_MS = 125

    selected_key = None
    offset = 0
    floor = min_width()
    sync = sync_width_enabled()
    last_width = stdscr.getmaxyx()[1]
    pending_width = None
    last_pane_set = None
    relayout_grace = 0
    frame = 0
    last_poll = 0.0
    force_poll = True
    cached = None

    while True:
        now = time.monotonic()
        if force_poll or cached is None or now - last_poll >= POLL_INTERVAL - 0.1:
            force_poll = False
            last_poll = now
            windows, pane_to_window, sidebars, pane_paths, pane_progress, pane_titles, sidebar_active = fetch_windows()
            if not windows:
                return
            me = pane_to_window.get(SIDEBAR_PANE)
            if me is None:
                return
            # Only the focused sidebar (active pane of the active window) shows the
            # keyboard-selection bar; the shared selection is otherwise painted on
            # every window's sidebar at once, which misreads as a live cursor.
            has_focus = me["active"] and sidebar_active
            # Exit if a lower-numbered sidebar occupies this window (spawn race),
            # or if no real panes remain here (tmux then closes the window).
            for p in sidebars:
                if p != SIDEBAR_PANE and pane_to_window.get(p) is me and int(p[1:]) < int(SIDEBAR_PANE[1:]):
                    return
            if not me["panes"]:
                return

            # A change in this window's pane set means an imminent width change
            # is tmux redistributing space, not a user resize. The grace covers
            # the following tick too, since the resize event and the pane-list
            # fetch are not ordered.
            my_panes = {p["id"] for p in me["panes"]}
            if last_pane_set is not None and my_panes != last_pane_set:
                relayout_grace = 2
            last_pane_set = my_panes

            # Enforce the minimum width and keep widths in sync. A width change
            # we did not request ourselves (i.e. a user resize) is clamped to
            # the floor and published; a relayout-caused one is snapped back; an
            # unchanged width adopts a differing published one. pending_width
            # marks our own resize-pane requests so their landing is not
            # mistaken for a user resize and republished.
            width_now = stdscr.getmaxyx()[1]
            if width_now != last_width:
                requested = pending_width
                pending_width = None
                if width_now != requested:
                    if relayout_grace:
                        restore = read_width() if sync else None
                        if not restore or restore < floor:
                            restore = max(last_width, floor)
                        if restore != width_now:
                            tmux("resize-pane", "-t", SIDEBAR_PANE, "-x", str(restore))
                            pending_width = restore
                    else:
                        target = max(width_now, floor)
                        if target != width_now:
                            tmux("resize-pane", "-t", SIDEBAR_PANE, "-x", str(target))
                            pending_width = target
                        if sync and read_width() != target:
                            write_width(target)
            elif sync and pending_width is None:
                shared_width = read_width()
                if shared_width and shared_width >= floor and shared_width != width_now:
                    tmux("resize-pane", "-t", SIDEBAR_PANE, "-x", str(shared_width))
                    pending_width = shared_width
            last_width = width_now
            relayout_grace = max(0, relayout_grace - 1)

            shared = read_selection()
            if shared:
                selected_key = shared
            # The focused pane is the active pane of the active window; a session
            # there has its attention dot cleared.
            focused_panes = {p["id"] for w in windows if w["active"] for p in w["panes"] if p["active"]}
            sessions = fetch_agent_sessions(pane_to_window, focused_panes, pane_paths, pane_titles)
            # Ring the bell / raise the desktop notification for sessions newly
            # needing attention (deduped across every window's sidebar).
            notify_attention(sessions, bell_enabled(), osc_notify_mode())
            # Mirror each pane's agent turn-state into the window tree: attention
            # wins over working, matching fetch_agent_sessions' own precedence.
            pane_status = {}
            for s in sessions:
                if s["pane"] and (s["status"] == "attention" or (s["status"] == "working" and pane_status.get(s["pane"]) != "attention")):
                    pane_status[s["pane"]] = s["status"]
            hook_on = hook_progress_enabled()
            osc_on = osc_progress_enabled()
            cached = (windows, sessions, pane_progress, pane_status, hook_on, osc_on, has_focus)

        windows, sessions, pane_progress, pane_status, hook_on, osc_on, has_focus = cached
        rows = build_rows(windows, sessions, pane_progress, pane_status, hook_on, osc_on, frame)
        items = [item for _, item in rows if isinstance(item, dict)]
        keys = [item["key"] for item in items]
        if selected_key not in keys:
            selected_key = next(
                (item["key"] for item in items if item["kind"] == "window" and item["win"]["active"]), keys[0]
            )
        offset = draw(stdscr, rows, selected_key, offset, has_focus)

        # Tick fast only while a spinner glyph is on screen, so an idle sidebar
        # still just blocks for the poll interval.
        animating = any(isinstance(it, dict) and it.get("indicator") in _SPINNER_FRAMES for _, it in rows)
        stdscr.timeout(ANIM_INTERVAL_MS if animating else int(POLL_INTERVAL * 1000))

        ch = stdscr.getch()
        frame += 1
        if ch != -1:
            force_poll = True
        if ch == curses.KEY_RESIZE:
            continue
        if ch in (ord("q"), ord("Q")):
            # Close every sidebar, not just this one. The server must run the
            # toggle: a child of this pane would be killed along with it.
            tmux("run-shell", "-b", os.path.join(os.path.dirname(os.path.abspath(__file__)), "toggle.sh"))
            return
        if ch in (curses.KEY_UP, ord("k")):
            selected_key = keys[max(0, keys.index(selected_key) - 1)]
            write_selection(selected_key)
        elif ch in (curses.KEY_DOWN, ord("j")):
            selected_key = keys[min(len(keys) - 1, keys.index(selected_key) + 1)]
            write_selection(selected_key)
        elif ch in (curses.KEY_ENTER, 10, 13):
            activate(items[keys.index(selected_key)])
        elif ch == curses.KEY_MOUSE:
            try:
                _, _, my, _, bstate = curses.getmouse()
            except curses.error:
                continue
            if not bstate & (curses.BUTTON1_PRESSED | curses.BUTTON1_CLICKED):
                continue
            row_idx = my + offset
            if 0 <= row_idx < len(rows) and isinstance(rows[row_idx][1], dict):
                selected_key = rows[row_idx][1]["key"]
                write_selection(selected_key)
                activate(rows[row_idx][1])


if __name__ == "__main__":
    locale.setlocale(locale.LC_ALL, "")
    curses.wrapper(main)
