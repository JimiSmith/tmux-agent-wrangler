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
import curses
import locale
import os
import subprocess

SIDEBAR_PANE = os.environ["TMUX_PANE"]
STATE_DIR = os.path.join(
    os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
    "tmux-agent-wrangler",
)
REGISTRY = os.path.join(STATE_DIR, "sessions")
ATTENTION = os.path.join(STATE_DIR, "attention")
SELECTION_FILE = os.path.join(STATE_DIR, "selection")
WIDTH_FILE = os.path.join(STATE_DIR, "width")


def tmux(*args):
    result = subprocess.run(("tmux",) + args, capture_output=True, text=True)
    return result.stdout


def min_width():
    value = tmux("show-option", "-gqv", "@wrangler-min-width").strip()
    return int(value) if value.isdigit() else 24


def sync_width_enabled():
    value = tmux("show-option", "-gqv", "@wrangler-sync-width").strip().lower()
    return value not in ("off", "0", "no", "false")


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
    sidebars = set()
    for line in tmux(
        "list-panes", "-s", "-F",
        "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{@wrangler_sidebar}\t#{pane_current_command}",
    ).splitlines():
        wid, pid, index, active, flag, cmd = line.split("\t", 5)
        if wid not in by_id:
            continue
        pane_to_window[pid] = by_id[wid]
        if flag == "1" or pid == SIDEBAR_PANE:
            sidebars.add(pid)
            continue
        by_id[wid]["panes"].append({"id": pid, "index": index, "active": active == "1", "cmd": cmd})
    return windows, pane_to_window, sidebars


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        pass
    return True


def fetch_agent_sessions(pane_to_window, focused_panes):
    """Read the hook registry; prune entries whose pane or process is gone.

    A session carries an attention flag when agent-hook.sh has marked it as
    having finished a turn. The flag (and its marker file) is cleared once the
    session's pane is the focused pane, so the dot means "finished a turn while
    you were not looking at it".
    """
    try:
        names = sorted(os.listdir(REGISTRY))
    except OSError:
        return []
    sessions = []
    for name in names:
        path = os.path.join(REGISTRY, name)
        marker = os.path.join(ATTENTION, name)
        try:
            with open(path) as f:
                fields = f.read().strip().split("\t")
        except OSError:
            continue
        if len(fields) == 2:  # legacy claude-hook.sh format: pane, cwd
            pane, agent, pid, cwd = fields[0], "claude", "", fields[1]
        elif len(fields) >= 4:  # pane, agent, pid, cwd
            pane, agent, pid, cwd = fields[0], fields[1], fields[2], fields[3]
        else:
            continue
        window = pane_to_window.get(pane)
        if window is None or (pid.isdigit() and not pid_alive(int(pid))):
            for stale in (path, marker):
                try:
                    os.unlink(stale)
                except OSError:
                    pass
            continue
        attention = os.path.exists(marker)
        if attention and pane in focused_panes:
            try:
                os.unlink(marker)
            except OSError:
                pass
            attention = False
        sessions.append(
            {"id": name, "agent": agent, "pane": pane, "cwd": cwd, "window": window, "attention": attention}
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
                (f"   {branch}{active}{p['index']}: {p['cmd']}",
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
                name = os.path.basename(s["cwd"].rstrip("/")) or s["cwd"]
                dot = " ●" if s["attention"] else ""
                rows.append(
                    (f"   {branch} {name}{dot}",
                     {"kind": "agent", "key": ("a", s["id"]), "win": w, "pane": s["pane"],
                      "attention": s["attention"]})
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


def draw(stdscr, rows, selected_key, offset):
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
                attr = curses.color_pair(2)
                if item["attention"]:
                    attr = curses.color_pair(3) | curses.A_BOLD
            else:
                attr = curses.A_DIM
            if item["key"] == selected_key:
                attr |= curses.A_REVERSE
        else:
            attr = curses.A_DIM
        try:
            stdscr.addnstr(screen_y, 0, text.ljust(width - 1), width - 1, attr)
        except curses.error:
            pass
    stdscr.refresh()
    return offset


def main(stdscr):
    curses.curs_set(0)
    curses.use_default_colors()
    curses.start_color()
    curses.init_pair(1, curses.COLOR_GREEN, -1)
    curses.init_pair(2, curses.COLOR_CYAN, -1)
    curses.init_pair(3, curses.COLOR_YELLOW, -1)
    curses.mousemask(curses.ALL_MOUSE_EVENTS)
    curses.mouseinterval(0)
    stdscr.timeout(1000)

    selected_key = None
    offset = 0
    floor = min_width()
    sync = sync_width_enabled()
    last_width = stdscr.getmaxyx()[1]
    pending_width = None
    last_pane_set = None
    relayout_grace = 0

    while True:
        windows, pane_to_window, sidebars = fetch_windows()
        if not windows:
            return
        me = pane_to_window.get(SIDEBAR_PANE)
        if me is None:
            return
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
        sessions = fetch_agent_sessions(pane_to_window, focused_panes)
        rows = build_rows(windows, sessions)
        items = [item for _, item in rows if isinstance(item, dict)]
        keys = [item["key"] for item in items]
        if selected_key not in keys:
            selected_key = next(
                (item["key"] for item in items if item["kind"] == "window" and item["win"]["active"]), keys[0]
            )
        offset = draw(stdscr, rows, selected_key, offset)

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
