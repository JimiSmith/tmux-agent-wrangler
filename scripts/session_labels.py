"""Agent-session label logic for scripts/sidebar.py.

Reads session metadata from each agent's state files (session_meta) and
composes the agent-row label (agent_label). Kept in its own stdlib-only module
(no curses, no TMUX_PANE) rather than inline in sidebar.py so the metadata
scanning logic stays isolated and testable.
"""
import json
import os

# transcript_path -> (mtime, title, custom, agent, team, color). Holds the last
# title, teammate identity, and color resolved for the session so the render
# loop only re-reads when the file changes; `custom` marks a title from a manual
# /rename, which is sticky (see session_meta). A scan that finds nothing keeps
# the cached values rather than regressing.
_title_cache = {}
_copilot_title_cache = {}

# Scan only the transcript's tail: the title records sit within a few KB of EOF
# in practice (Claude rewrites them roughly every turn), so this stays cheap
# regardless of how large the transcript grows.
_TITLE_TAIL_BYTES = 65536


def _yaml_scalar(value):
    """Decode the scalar forms Copilot uses for workspace title fields."""
    value = value.strip()
    if not value or value in ("null", "~"):
        return ""
    if value.startswith('"') and value.endswith('"'):
        try:
            decoded = json.loads(value)
            return decoded if isinstance(decoded, str) else ""
        except ValueError:
            return value[1:-1]
    if value.startswith("'") and value.endswith("'"):
        return value[1:-1].replace("''", "'")
    return value


def _workspace_field(text, key):
    """Read one top-level scalar or block-scalar field from workspace.yaml."""
    prefix = key + ":"
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if not line.startswith(prefix):
            continue
        value = line[len(prefix):].strip()
        if value not in ("|", "|-", "|+", ">", ">-", ">+"):
            return _yaml_scalar(value)
        for continuation in lines[index + 1:]:
            if continuation and not continuation[0].isspace():
                break
            if continuation.strip():
                return continuation.strip()
        return ""
    return ""


def _copilot_title(session_id):
    """Current Copilot session name, falling back to its generated summary."""
    if not session_id:
        return ""
    workspace = os.path.expanduser(
        os.path.join("~", ".copilot", "session-state", session_id, "workspace.yaml")
    )
    try:
        stat = os.stat(workspace)
    except OSError:
        return ""
    cached = _copilot_title_cache.get(workspace)
    if cached and cached[0] == stat.st_mtime_ns:
        return cached[1]
    try:
        with open(workspace, encoding="utf-8") as f:
            text = f.read()
    except OSError:
        return ""
    title = _workspace_field(text, "name") or _workspace_field(text, "summary")
    title = next((line.strip() for line in title.splitlines() if line.strip()), "")
    if not title and cached:
        title = cached[1]
    _copilot_title_cache[workspace] = (stat.st_mtime_ns, title)
    return title


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


def _claude_session_meta(transcript_path, _session_id):
    """(title, agent_name, team, color) for a Claude session.

    Claude records an auto-generated 'ai-title' (rewritten roughly every turn)
    and, on a /rename, a 'custom-title'; the manual one wins and is sticky,
    overriding the auto title that Claude goes on emitting. agent_name / team:
    the teammate name and team id when this is a teammate session (see
    _scan_tail), else "". color: the session's assigned color from the last
    /color ('agent-color' record), else "" (a teammate's color is not recorded
    here - resolve it from `team` via team_pane_colors). Empty transcript_path
    (a legacy record or a just-started session) yields ("", "", "", "").

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


def _copilot_session_meta(_transcript_path, session_id):
    """(title, agent_name, team, color) for a Copilot session.

    The current `name` (or generated `summary`) comes from
    ~/.copilot/session-state/<session_id>/workspace.yaml, which updates live
    after /rename. Copilot has no teammate identity or color metadata here."""
    return _copilot_title(session_id), "", "", ""


_SESSION_META_HANDLERS = {
    "claude": _claude_session_meta,
    "copilot": _copilot_session_meta,
}


def session_meta(transcript_path, agent="", session_id=""):
    """Dispatch metadata lookup for supported agents; ignore unknown agents."""
    handler = _SESSION_META_HANDLERS.get(agent)
    return handler(transcript_path, session_id) if handler else ("", "", "", "")


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
