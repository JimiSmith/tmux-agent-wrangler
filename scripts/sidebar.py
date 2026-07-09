#!/usr/bin/env python3
"""Sidebar TUI for tmux-agent-wrangler.

Lists the session's windows with their panes as a tree. Windows are the
interactive elements: Up/Down or j/k to move, Enter or a mouse click to
focus. The sidebar moves itself into the target window before selecting
it, so it stays visible.
"""
import curses
import locale
import os
import subprocess

SIDEBAR_PANE = os.environ["TMUX_PANE"]
HEADER_LINES = 2


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
    for line in tmux(
        "list-panes", "-s", "-F", "#{window_id}\t#{pane_id}\t#{pane_index}\t#{pane_active}\t#{pane_current_command}"
    ).splitlines():
        wid, pid, index, active, cmd = line.split("\t", 4)
        if pid == SIDEBAR_PANE or wid not in by_id:
            continue
        by_id[wid]["panes"].append({"id": pid, "index": index, "active": active == "1", "cmd": cmd})
    return windows


def build_rows(windows):
    """Return a flat list of (text, window_or_None) rows."""
    rows = []
    for w in windows:
        marker = "*" if w["active"] else " "
        rows.append((f"{marker} {w['index']}: {w['name']}", w))
        last = len(w["panes"]) - 1
        for i, p in enumerate(w["panes"]):
            branch = "└─" if i == last else "├─"
            active = "*" if p["active"] else " "
            rows.append((f"   {branch}{active}{p['index']}: {p['cmd']}", None))
    return rows


def focus_window(win_id):
    side_win = tmux("display-message", "-p", "-t", SIDEBAR_PANE, "#{window_id}").strip()
    if side_win != win_id:
        width = tmux("display-message", "-p", "-t", SIDEBAR_PANE, "#{pane_width}").strip() or "32"
        tmux("join-pane", "-d", "-f", "-h", "-b", "-l", width, "-s", SIDEBAR_PANE, "-t", win_id)
    tmux("select-window", "-t", win_id)
    # Land focus on a real pane, not the sidebar.
    if tmux("display-message", "-p", "-t", win_id, "#{pane_id}").strip() == SIDEBAR_PANE:
        others = [p for p in tmux("list-panes", "-t", win_id, "-F", "#{pane_id}").split() if p != SIDEBAR_PANE]
        if others:
            tmux("select-pane", "-t", others[0])


def draw(stdscr, rows, selected_id, offset):
    height, width = stdscr.getmaxyx()
    visible = height - HEADER_LINES
    sel_row = next((i for i, (_, w) in enumerate(rows) if w and w["id"] == selected_id), 0)
    if sel_row < offset:
        offset = sel_row
    elif sel_row >= offset + visible:
        offset = sel_row - visible + 1
    offset = max(0, min(offset, max(0, len(rows) - visible)))

    stdscr.erase()
    try:
        stdscr.addnstr(0, 0, " WINDOWS".ljust(width - 1), width - 1, curses.A_BOLD | curses.A_UNDERLINE)
    except curses.error:
        pass
    for screen_y, row_idx in enumerate(range(offset, min(len(rows), offset + visible))):
        text, window = rows[row_idx]
        attr = curses.A_NORMAL
        if window:
            attr = curses.A_BOLD
            if window["active"]:
                attr |= curses.color_pair(1)
            if window["id"] == selected_id:
                attr |= curses.A_REVERSE
        else:
            attr = curses.A_DIM
        try:
            stdscr.addnstr(screen_y + HEADER_LINES, 0, text.ljust(width - 1), width - 1, attr)
        except curses.error:
            pass
    stdscr.refresh()
    return offset


def main(stdscr):
    curses.curs_set(0)
    curses.use_default_colors()
    curses.start_color()
    curses.init_pair(1, curses.COLOR_GREEN, -1)
    curses.mousemask(curses.ALL_MOUSE_EVENTS)
    curses.mouseinterval(0)
    stdscr.timeout(1000)

    selected_id = None
    offset = 0
    floor = min_width()

    while True:
        windows = fetch_windows()
        if not windows:
            return
        if selected_id not in {w["id"] for w in windows}:
            selected_id = next((w["id"] for w in windows if w["active"]), windows[0]["id"])
        rows = build_rows(windows)
        window_ids = [w["id"] for w in windows]
        offset = draw(stdscr, rows, selected_id, offset)

        ch = stdscr.getch()
        if ch == curses.KEY_RESIZE:
            if stdscr.getmaxyx()[1] < floor:
                tmux("resize-pane", "-t", SIDEBAR_PANE, "-x", str(floor))
            continue
        if ch in (ord("q"), ord("Q")):
            return
        if ch in (curses.KEY_UP, ord("k")):
            pos = window_ids.index(selected_id)
            selected_id = window_ids[max(0, pos - 1)]
        elif ch in (curses.KEY_DOWN, ord("j")):
            pos = window_ids.index(selected_id)
            selected_id = window_ids[min(len(window_ids) - 1, pos + 1)]
        elif ch in (curses.KEY_ENTER, 10, 13):
            focus_window(selected_id)
        elif ch == curses.KEY_MOUSE:
            try:
                _, _, my, _, bstate = curses.getmouse()
            except curses.error:
                continue
            if not bstate & (curses.BUTTON1_PRESSED | curses.BUTTON1_CLICKED):
                continue
            row_idx = my - HEADER_LINES + offset
            if 0 <= row_idx < len(rows) and rows[row_idx][1]:
                selected_id = rows[row_idx][1]["id"]
                focus_window(selected_id)


if __name__ == "__main__":
    locale.setlocale(locale.LC_ALL, "")
    curses.wrapper(main)
