"""Shared agent-session label logic.

Imported by both scripts/sidebar.py (to render the agent rows) and
scripts/agent-hook.sh (to build the OSC 9 notification body via `python3 -c`),
so the notification text matches the sidebar row exactly. Depends only on the
stdlib (json/os) - no curses, no TMUX_PANE - so it is cheap and side-effect-free
to import from the hook.
"""
import json
import os

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


def label_mode_from(value):
    """Normalize a raw @wrangler-label option value to the row-label mode:
    'dir' (working-directory basename) or 'name' (session title, the default for
    any unset/unknown value)."""
    return "dir" if value.strip().lower() == "dir" else "name"


def agent_label(mode, title, agent_name, dir_name):
    """The agent-row label text, exactly as the sidebar tree renders it.

    A teammate (agent_name set) reads as '@name - tail', or just '@name' until it
    earns a title; in 'name' mode the tail is the title (dropping the dir
    fallback so an un-titled teammate stays '@name'), in 'dir' mode the dir
    basename. A top-level session is its title in 'name' mode (dir basename when
    untitled), or the dir basename in 'dir' mode."""
    if agent_name:
        tail = title if mode == "name" else dir_name
        return f"@{agent_name} - {tail}" if tail else f"@{agent_name}"
    return (title if mode == "name" else "") or dir_name


def notification_label(transcript, display_cwd, label_opt):
    """The agent-row label for a session, as shown on screen, for the OSC 9
    notification body. `label_opt` is the raw @wrangler-label value (the hook
    forwards it without interpreting it)."""
    dir_name = os.path.basename(display_cwd.rstrip("/")) or display_cwd
    title, agent_name, _team, _color = session_meta(transcript)
    return agent_label(label_mode_from(label_opt), title, agent_name, dir_name)
