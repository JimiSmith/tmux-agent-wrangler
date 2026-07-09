#!/usr/bin/env python3
"""Sidebar TUI for tmux-agent-wrangler.

Lists the session's windows with their panes as a tree, plus active agent
sessions (Claude Code, Copilot CLI, ...) registered by scripts/agent-hook.sh,
one section per agent. Windows and agent sessions are the interactive rows:
Up/Down or j/k to move, Enter or a mouse click to focus. The sidebar moves
itself into the target window before selecting it, so it stays visible.
"""
import curses
import locale
import os
import subprocess

SIDEBAR_PANE = os.environ["TMUX_PANE"]
REGISTRY = os.path.join(
    os.environ.get("XDG_STATE_HOME") or os.path.expanduser("~/.local/state"),
    "tmux-agent-wrangler",
    "sessions",
)


def tmux(*args):
    result = subprocess.run(("tmux",) + args, capture_output=True, text=True)
    return result.stdout


def min_width():
    value = tmux("show-option", "-gqv", "@wrangler-min-width").strip()
    return int(value) if value.isdigit() else 24


def fetch_windows():
    windows = []
    for line in tmux(
        "list-windows", "-F", "#{window_id}\t#{window_index}\t#{window_name}\t#{window_active}"
    ).splitlines():
        wid, index, name, active = line.split("\t", 3)
        windows.append({"id": wid, "index": index, "name": name, "active": active == "1", "panes": []})

    by_id = {w["id"]: w for w in windows}
    pane_to_window = {}
    for line in tmux(
        "list-panes", "-s", "-F", "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_current_command}"
    ).splitlines():
        wid, pid, index, active, cmd = line.split("\t", 4)
        if wid not in by_id:
            continue
        pane_to_window[pid] = by_id[wid]
        if pid == SIDEBAR_PANE:
            continue
        by_id[wid]["panes"].append({"id": pid, "index": index, "active": active == "1", "cmd": cmd})
    return windows, pane_to_window


def pid_alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except OSError:
        pass
    return True


def fetch_agent_sessions(pane_to_window):
    """Read the hook registry; prune entries whose pane or process is gone."""
    try:
        names = sorted(os.listdir(REGISTRY))
    except OSError:
        return []
    sessions = []
    for name in names:
        path = os.path.join(REGISTRY, name)
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
            try:
                os.unlink(path)
            except OSError:
                pass
            continue
        sessions.append({"id": name, "agent": agent, "pane": pane, "cwd": cwd, "window": window})
    return sessions


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
                rows.append(
                    (f"   {branch} {name}",
                     {"kind": "agent", "key": ("a", s["id"]), "win": w, "pane": s["pane"]})
                )
    return rows


def focus(win_id, pane_id=None):
    side_win = tmux("display-message", "-p", "-t", SIDEBAR_PANE, "#{window_id}").strip()
    if side_win != win_id:
        width = tmux("display-message", "-p", "-t", SIDEBAR_PANE, "#{pane_width}").strip() or "32"
        tmux("join-pane", "-d", "-f", "-h", "-b", "-l", width, "-s", SIDEBAR_PANE, "-t", win_id)
    tmux("select-window", "-t", win_id)
    if pane_id:
        tmux("select-pane", "-t", pane_id)
        return
    # Land focus on a real pane, not the sidebar.
    if tmux("display-message", "-p", "-t", win_id, "#{pane_id}").strip() == SIDEBAR_PANE:
        others = [p for p in tmux("list-panes", "-t", win_id, "-F", "#{pane_id}").split() if p != SIDEBAR_PANE]
        if others:
            tmux("select-pane", "-t", others[0])


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
    curses.mousemask(curses.ALL_MOUSE_EVENTS)
    curses.mouseinterval(0)
    stdscr.timeout(1000)

    selected_key = None
    offset = 0
    floor = min_width()

    while True:
        windows, pane_to_window = fetch_windows()
        if not windows:
            return
        # If the sidebar is the only pane left in its window, move on to the
        # next window rather than sitting there full-width. Leaving makes
        # tmux destroy the emptied window.
        me = pane_to_window.get(SIDEBAR_PANE)
        if me and not me["panes"]:
            nxt = windows[(windows.index(me) + 1) % len(windows)]
            if nxt is me:
                return
            focus(nxt["id"])
            continue
        sessions = fetch_agent_sessions(pane_to_window)
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
            if stdscr.getmaxyx()[1] < floor:
                tmux("resize-pane", "-t", SIDEBAR_PANE, "-x", str(floor))
            continue
        if ch in (ord("q"), ord("Q")):
            return
        if ch in (curses.KEY_UP, ord("k")):
            selected_key = keys[max(0, keys.index(selected_key) - 1)]
        elif ch in (curses.KEY_DOWN, ord("j")):
            selected_key = keys[min(len(keys) - 1, keys.index(selected_key) + 1)]
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
                activate(rows[row_idx][1])


if __name__ == "__main__":
    locale.setlocale(locale.LC_ALL, "")
    curses.wrapper(main)
