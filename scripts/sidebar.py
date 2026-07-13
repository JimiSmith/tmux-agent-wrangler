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

SIDEBAR_PANE = os.environ["TMUX_PANE"]
STATE_DIR = os.path.join(
    os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
    "tmux-agent-wrangler",
)
REGISTRY = os.path.join(STATE_DIR, "sessions")
ATTENTION = os.path.join(STATE_DIR, "attention")
WORKING = os.path.join(STATE_DIR, "working")
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

# The 6 steps of each xterm-256 color-cube axis, for matching an RGB to the
# nearest of the 216 cube colors (indices 16-231) plus the 24 grays (232-255).
_CUBE_STEPS = (0, 95, 135, 175, 215, 255)

# color name -> allocated curses color-pair id, filled in by init_agent_colors.
_agent_color_pairs = {}


def _nearest_256(r, g, b):
    """The xterm-256 palette index (16-255) closest to the given RGB, by squared
    distance over the color cube and gray ramp. Lets us approximate Claude's
    exact colors on a 256-color terminal (curses here cannot emit 24-bit)."""
    best_i, best_d = 16, None
    for i in range(16, 256):
        if i < 232:
            j = i - 16
            cr, cg, cb = _CUBE_STEPS[j // 36], _CUBE_STEPS[(j // 6) % 6], _CUBE_STEPS[j % 6]
        else:
            cr = cg = cb = 8 + (i - 232) * 10
        d = (cr - r) ** 2 + (cg - g) ** 2 + (cb - b) ** 2
        if best_d is None or d < best_d:
            best_i, best_d = i, d
    return best_i


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
    value = tmux("show-option", "-gqv", "@wrangler-label").strip().lower()
    return "dir" if value == "dir" else "name"


# transcript_path -> (mtime, title, custom, agent, team, color). Holds the last
# title, teammate identity, and color resolved for the session so the render
# loop only re-reads when the file changes; `custom` marks a title from a manual
# /rename, which is sticky (see session_meta). A scan that finds nothing keeps
# the cached values rather than regressing.
_title_cache = {}

# Scan only the transcript's tail: the title records sit within a few KB of EOF
# in practice (Claude rewrites them roughly every turn), so this stays cheap
# regardless of how large the transcript grows.
_TITLE_TAIL_BYTES = 65536


def _scan_tail(transcript_path):
    """Titles, teammate identity, and color from the file's trailing chunk, as
    (custom, ai, agent, team, color): `custom` = last /rename title
    ('custom-title'), `ai` = last auto title ('ai-title'), `agent` / `team` =
    the teammate's agentName / teamName when this is a teammate session, `color`
    = last color set via /color ('agent-color'). Each "" if not found there /
    unreadable.

    A teammate stamps agentName (and teamName) on every conversation record,
    whereas a normal session carries agentName only inside a /rename
    'agent-name' record, so we read them from any record that is *not* an
    'agent-name' record. Reads only the final _TITLE_TAIL_BYTES bytes."""
    try:
        with open(transcript_path, "rb") as f:
            f.seek(0, os.SEEK_END)
            size = f.tell()
            f.seek(max(0, size - _TITLE_TAIL_BYTES))
            chunk = f.read()
    except OSError:
        return "", "", "", "", ""
    lines = chunk.split(b"\n")
    if size > _TITLE_TAIL_BYTES:
        del lines[0]  # first line is likely truncated mid-record
    custom = ai = agent = team = color = ""
    for line in lines:  # keep scanning; the last record of each kind wins
        if b'"custom-title"' in line:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("type") == "custom-title" and rec.get("customTitle"):
                custom = rec["customTitle"]
        elif b'"ai-title"' in line:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("type") == "ai-title" and rec.get("aiTitle"):
                ai = rec["aiTitle"]
        elif b'"agent-color"' in line:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("type") == "agent-color" and rec.get("agentColor"):
                color = rec["agentColor"]
        elif (not agent or not team) and b'"agentName"' in line:
            try:
                rec = json.loads(line)
            except ValueError:
                continue
            if rec.get("type") != "agent-name":  # agentName/teamName ride together
                agent = agent or rec.get("agentName") or ""
                team = team or rec.get("teamName") or ""
    return custom, ai, agent, team, color


def session_meta(transcript_path):
    """(title, agent_name, team, color) for a Claude session.

    title: the current display title. Claude records an auto-generated
    'ai-title' (rewritten roughly every turn) and, on a /rename, a
    'custom-title'; the manual one wins and is sticky, overriding the auto title
    that Claude goes on emitting. agent_name / team: the teammate name and team
    id when this is a teammate session (see _scan_tail), else "". color: the
    session's assigned color from the last /color ('agent-color' record), else
    "" (a teammate's color is not recorded here - resolve it from `team` via
    team_pane_colors). Empty transcript_path (Copilot, a legacy record, or a
    just-started session) yields ("", "", "", "").

    A scan that comes up empty keeps the last values seen: a long burst of output
    can briefly push them out of the scanned tail, but we rescan every changed
    tick, so we captured them while recent."""
    if not transcript_path:
        return "", "", "", ""
    try:
        mtime = os.path.getmtime(transcript_path)
    except OSError:
        return "", "", "", ""
    cached = _title_cache.get(transcript_path)
    if cached and cached[0] == mtime:
        return cached[1], cached[3], cached[4], cached[5]
    prev_title = cached[1] if cached else ""
    prev_custom = cached[2] if cached else False
    prev_agent = cached[3] if cached else ""
    prev_team = cached[4] if cached else ""
    prev_color = cached[5] if cached else ""
    custom, ai, agent, team, color = _scan_tail(transcript_path)
    if custom:
        title, is_custom = custom, True
    elif prev_custom:
        title, is_custom = prev_title, True  # keep the manual name
    elif ai:
        title, is_custom = ai, False
    else:
        title, is_custom = prev_title, prev_custom
    agent = agent or prev_agent  # sticky, like the title
    team = team or prev_team
    color = color or prev_color
    _title_cache[transcript_path] = (mtime, title, is_custom, agent, team, color)
    return title, agent, team, color


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
    sidebars = set()
    sidebar_active = False
    # pane_current_path sits before pane_title so the free-form title stays the
    # trailing field; a path is very unlikely to contain a tab.
    for line in tmux(
        "list-panes", "-s", "-F",
        "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{@wrangler_sidebar}\t#{pane_current_path}\t#{pane_title}",
    ).splitlines():
        wid, pid, index, active, flag, path, title = line.split("\t", 6)
        if wid not in by_id:
            continue
        pane_to_window[pid] = by_id[wid]
        pane_paths[pid] = path
        if flag == "1" or pid == SIDEBAR_PANE:
            sidebars.add(pid)
            if pid == SIDEBAR_PANE:
                sidebar_active = active == "1"
            continue
        by_id[wid]["panes"].append({"id": pid, "index": index, "active": active == "1", "title": title})
    return windows, pane_to_window, sidebars, pane_paths, sidebar_active


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        pass
    return True


def fetch_agent_sessions(pane_to_window, focused_panes, pane_paths):
    """Read the hook registry; prune entries whose pane or process is gone.

    A session carries a turn status set by agent-hook.sh: "working" while a
    turn is in progress, "attention" once it finishes a turn or notifies. The
    attention marker (and so the dot) clears once the session's pane is the
    focused pane, meaning "wanted attention while you were not looking at it";
    the working marker persists until the turn ends, since it reflects the
    agent actually being busy.

    The displayed cwd tracks the pane's live path (pane_paths) so it follows
    the agent as it changes directory, falling back to the cwd recorded at
    registration if the pane reports no path.
    """
    try:
        names = sorted(os.listdir(REGISTRY))
    except OSError:
        return []
    mode = label_mode()
    sessions = []
    for name in names:
        path = os.path.join(REGISTRY, name)
        attn_marker = os.path.join(ATTENTION, name)
        work_marker = os.path.join(WORKING, name)
        try:
            with open(path) as f:
                fields = f.read().strip().split("\t")
        except OSError:
            continue
        if len(fields) == 2:  # legacy claude-hook.sh format: pane, cwd
            pane, agent, pid, cwd, transcript = fields[0], "claude", "", fields[1], ""
        elif len(fields) >= 4:  # pane, agent, pid, cwd[, transcript]
            pane, agent, pid, cwd = fields[0], fields[1], fields[2], fields[3]
            transcript = fields[4] if len(fields) >= 5 else ""
        else:
            continue
        window = pane_to_window.get(pane)
        if window is None or (pid.isdigit() and not pid_alive(int(pid))):
            for stale in (path, attn_marker, work_marker):
                try:
                    os.unlink(stale)
                except OSError:
                    pass
            continue
        attention = os.path.exists(attn_marker)
        if attention and pane in focused_panes:
            try:
                os.unlink(attn_marker)
            except OSError:
                pass
            attention = False
        status = "attention" if attention else ("working" if os.path.exists(work_marker) else "")
        display_cwd = pane_paths.get(pane) or cwd
        dir_name = os.path.basename(display_cwd.rstrip("/")) or display_cwd
        title, agent_name, team, color = session_meta(transcript)
        # A top-level session records its color in the transcript; a teammate's
        # lives in its team config instead, so fall back to that (by pane), read
        # only for actual teammates.
        if not color and agent_name:
            color = team_pane_colors(team).get(pane, "")
        if agent_name:
            # Agent-teams teammate: prefix "@name". In name mode we drop the dir
            # fallback so an un-titled teammate reads as just "@name" until it
            # earns a title.
            tail = title if mode == "name" else dir_name
            label = f"@{agent_name} - {tail}" if tail else f"@{agent_name}"
        else:
            label = (title if mode == "name" else "") or dir_name
        sessions.append(
            {"id": name, "agent": agent, "pane": pane, "cwd": display_cwd,
             "label": label, "window": window, "status": status, "color": color}
        )
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


def build_rows(windows, sessions):
    """Flat list of (text, item) rows; item is a selectable dict, "header", or None."""
    rows = [(" WINDOWS", "header"), ("", None)]
    for w in windows:
        marker = "*" if w["active"] else " "
        rows.append((f"{marker} {w['index']}: {w['name']}", {"kind": "window", "key": ("w", w["id"]), "win": w}))
        last = len(w["panes"]) - 1
        for i, p in enumerate(w["panes"]):
            branch = "└─" if i == last else "├─"
            active = "*" if p["active"] else " "
            rows.append(
                (f"   {branch}{active}{p['index']}: {p['title']}",
                 {"kind": "pane", "key": ("p", p["id"]), "win": w, "pane": p["id"]})
            )
    for agent in sorted({s["agent"] for s in sessions}):
        rows.append(("", None))
        rows.append((f" {agent.upper()}", "header"))
        rows.append(("", None))
        agent_sessions = [s for s in sessions if s["agent"] == agent]
        for w in windows:
            group = [s for s in agent_sessions if s["window"] is w]
            if not group:
                continue
            marker = "*" if w["active"] else " "
            rows.append(
                (f"{marker} {w['index']}: {w['name']}",
                 {"kind": "window", "key": ("w", agent, w["id"]), "win": w})
            )
            last = len(group) - 1
            for i, s in enumerate(group):
                branch = "└─" if i == last else "├─"
                name = s["label"]
                # The glyph rides on the item, not the text: draw() pins it to
                # the right edge so it survives a long title being truncated.
                rows.append(
                    (f"   {branch} {name}",
                     {"kind": "agent", "key": ("a", s["id"]), "win": w, "pane": s["pane"],
                      "status": s["status"], "color": s["color"],
                      "glyph": {"attention": "●", "working": "◐"}.get(s["status"], "")})
                )
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
    focus(item["win"]["id"], item.get("pane"))


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
    to the user's theme: the theme's RGB approximated to the nearest xterm-256
    shade on a 256-color terminal, else the base ANSI color. Pairs 1-3 are taken
    by the base UI colors, so these start at 10."""
    rgb = _theme_palette(read_theme()) if curses.COLORS >= 256 else None
    pair_id = 10
    for cname in _AGENT_COLOR_NAMES:
        cnum = _nearest_256(*rgb[cname]) if rgb else _PALETTE_ANSI_BASE[cname]
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
                # by the pinned glyph (●/◐) and bold, so it survives here.
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
        glyph = item.get("glyph", "") if isinstance(item, dict) else ""
        if glyph and field >= 3:
            # Reserve the last two columns for a space + the glyph, so it stays
            # visible in the rightmost cell however narrow the pane gets.
            line = f"{_fit(text, field - 2)} {glyph}"
        else:
            line = _fit(text, field)
        try:
            stdscr.addnstr(screen_y, 0, line, field, attr)
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
    init_agent_colors()
    curses.mousemask(curses.ALL_MOUSE_EVENTS)
    curses.mouseinterval(0)
    stdscr.timeout(1000)

    # Report focus changes while running and stop on exit.
    set_focus_reporting(True)
    atexit.register(set_focus_reporting, False)

    selected_key = None
    offset = 0
    floor = min_width()
    sync = sync_width_enabled()
    last_width = stdscr.getmaxyx()[1]
    pending_width = None
    last_pane_set = None
    relayout_grace = 0

    while True:
        windows, pane_to_window, sidebars, pane_paths, sidebar_active = fetch_windows()
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
        sessions = fetch_agent_sessions(pane_to_window, focused_panes, pane_paths)
        rows = build_rows(windows, sessions)
        items = [item for _, item in rows if isinstance(item, dict)]
        keys = [item["key"] for item in items]
        if selected_key not in keys:
            selected_key = next(
                (item["key"] for item in items if item["kind"] == "window" and item["win"]["active"]), keys[0]
            )
        offset = draw(stdscr, rows, selected_key, offset, has_focus)

        ch = stdscr.getch()
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
