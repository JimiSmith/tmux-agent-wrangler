//! Associating agent sessions to the panes/windows displaying them: process
//! ancestry checks, registry-record parsing, and title matching.
//!
//! Process ancestry (`process_under`, `ppid_map`), the attention-marker token
//! read, title stripping, and the registry-record parse/serialize. The record
//! write format is five tab-separated fields with a trailing newline (see
//! [`serialize_registry_record`]).

use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

use indexmap::{IndexMap, IndexSet};

use crate::labels::{agent_label, LabelCache, LabelMode};
use crate::model::{NamedColor, PaneId, Session, SessionKey, TurnStatus, Window, WindowId};

/// One parsed hook-registry file. `pane`..`transcript` are the on-disk body
/// fields; `session_id` is derived from the file name (the `<agent>-` prefix
/// stripped). All body fields are kept as raw strings so an empty pane (a
/// pane-less / daemon-hosted record) and an empty pid (the agent pid was not
/// captured) stay distinguishable from a present value — the downstream gates
/// key off exactly those emptiness distinctions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryRecord {
    pub pane: String,
    pub agent: String,
    pub pid: String,
    pub cwd: String,
    pub transcript: String,
    pub session_id: String,
}

/// One live registry entry as the association pass consumes it: the parsed
/// record plus the turn state distilled from its markers. `status` reflects the
/// working marker (`Working`) or its absence (`Idle`); `attention_token` is
/// `Some` exactly while an attention marker is present, carrying that event's
/// token. When a token is present the emitted status is `Attention` regardless
/// of `status`, so the two are consistent even if set independently.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistrySession {
    pub record: RegistryRecord,
    pub status: TurnStatus,
    pub attention_token: Option<i128>,
}

/// All-ASCII-digit gate (tokens/pids are ASCII from `printf`/tmux). True only
/// for a non-empty all-ASCII-digit string. Rejects "", "-5", "+5", "24px", and
/// any string with whitespace or a sign.
///
/// Note: only ASCII digits count; non-ASCII digit codepoints (e.g. superscripts,
/// fullwidth digits) never occur in the numeric fields here, so restricting to
/// ASCII is practically unreachable.
pub fn is_field_digits(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

/// Parse a numeric registry/pid field behind the `is_field_digits` gate: `Some`
/// only when the whole string is ASCII digits, else `None`.
///
/// Note: a digit string that overflows `u32` yields `None`. Real pids fit
/// `u32`, so this is unreachable in practice.
pub fn parse_field_int(s: &str) -> Option<u32> {
    if is_field_digits(s) {
        s.parse().ok()
    } else {
        None
    }
}

/// Drop the status glyph Claude Code prefixes onto the terminal/pane title
/// (`"<glyph> <session title>"`; the glyph is a single non-alphanumeric token — a
/// braille spinner frame while working, `✳` when idle). Trims (Unicode
/// whitespace), partitions on the FIRST ASCII space, and drops that head token
/// when it holds no alphanumeric character; otherwise keeps the whole trimmed
/// title. A lone glyph with no following space is kept as-is (only
/// glyph+space+title strips).
///
/// The partition is on a single ASCII space (NOT whitespace-splitting), while
/// the outer trims strip Unicode whitespace.
///
/// Note: the alphanumeric test is `char::is_alphanumeric` (Unicode L*/N*);
/// low-risk divergence for exotic codepoints only.
pub fn strip_status_prefix(title: &str) -> String {
    let trimmed = title.trim();
    match trimmed.find(' ') {
        // A separator was found.
        Some(i) => {
            let head = &trimmed[..i];
            if !head.chars().any(|c| c.is_alphanumeric()) {
                // Glyph head: return the remainder, re-trimmed.
                trimmed[i + 1..].trim().to_string()
            } else {
                trimmed.to_string()
            }
        }
        // No space: partition yields no separator, so keep the trimmed whole.
        None => trimmed.to_string(),
    }
}

/// Whether `ancestor` is `pid` itself or one of its forebears, walking the
/// pid -> ppid map. Bounded to exactly 4096 hops so a cycle or a truncated/corrupt
/// map cannot loop forever.
///
/// This is how a recorded pane is confirmed to genuinely host the agent: an
/// in-pane agent descends from the pane's top-level process (`#{pane_pid}`),
/// whereas one launched into a detached/GUI host merely inherited `TMUX_PANE`.
///
/// Order is load-bearing: `pid == ancestor` is tested BEFORE the `pid <= 1`
/// guard (so `pid == ancestor == 1` returns true), and `ancestor == 0` (a missing
/// / falsy recorded pane_pid) short-circuits to false before any lookup. A
/// missing key defaults to 0, which reaches the `pid <= 1` guard next iteration.
pub fn process_under(mut pid: u32, ancestor: u32, parents: &HashMap<u32, u32>) -> bool {
    if ancestor == 0 {
        return false;
    }
    for _ in 0..4096 {
        if pid == ancestor {
            return true;
        }
        if pid <= 1 {
            return false;
        }
        pid = parents.get(&pid).copied().unwrap_or(0);
    }
    false
}

/// Parse `ps -e -o pid= -o ppid=` stdout into a pid -> ppid map. Each line is
/// split on any whitespace run (ends trimmed, NOT split on a single space); a
/// line contributes only when it has >= 2 tokens and both the first two are
/// ASCII digits. Duplicate pids: last write wins.
pub fn parse_ppid_stdout(out: &str) -> HashMap<u32, u32> {
    let mut parents = HashMap::new();
    for line in out.lines() {
        let mut it = line.split_whitespace();
        if let (Some(a), Some(b)) = (it.next(), it.next()) {
            if is_field_digits(a) && is_field_digits(b) {
                if let (Ok(pid), Ok(ppid)) = (a.parse::<u32>(), b.parse::<u32>()) {
                    parents.insert(pid, ppid);
                }
            }
        }
    }
    parents
}

/// Run `ps -e -o pid= -o ppid=` once and parse it. The header-less two-column
/// form prints identically on Linux and macOS/BSD (no `/proc` reliance). Returns
/// an empty map if the subprocess cannot be spawned; a non-zero exit still
/// parses whatever stdout was produced.
///
/// Side effect: spawns the read-only `ps` subprocess. No filesystem writes.
pub fn ppid_map() -> HashMap<u32, u32> {
    match std::process::Command::new("ps")
        .args(["-e", "-o", "pid=", "-o", "ppid="])
        .output()
    {
        Ok(o) => parse_ppid_stdout(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => HashMap::new(),
    }
}

/// Whether a process with `pid` currently exists, probed with a zero signal that
/// performs error-checking without delivering anything. A successful probe, or an
/// `EPERM` (the process exists but is not ours to signal), means alive; `ESRCH`
/// (no such process) means dead. A zero pid is treated as dead, never probed, so
/// the zero-pid process-group semantics of the underlying call are avoided.
///
/// Side effect: sends signal 0 to `pid` (no signal is delivered).
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if rc == 0 {
        return true;
    }
    // Only "no such process" is dead; every other error (e.g. a permission
    // denial for another user's process) means the process is alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Read an attention marker's event token, including tokenless markers.
///
/// Reads the file, trims (Unicode whitespace) its contents; if the trimmed body
/// is all ASCII digits, returns it as the token (the monotonic counter written
/// as `"<token>\n"`). Otherwise returns the file's `st_mtime` in nanoseconds —
/// a tokenless marker has no number, so its mtime is the event identity. Returns
/// `None` if the file cannot be read/stat'd (this is how callers detect "no
/// attention").
///
/// The token is returned as `i128`; mtime is true `st_mtime_ns` (sub-second
/// resolution preserved), never `seconds * 1e9`.
pub fn attention_token(path: &Path) -> Option<i128> {
    // OSError -> None: an absent/unreadable marker means "no attention".
    let contents = std::fs::read_to_string(path).ok()?;
    let trimmed = contents.trim();
    if is_field_digits(trimmed) {
        // A tokened marker. (An overflow past i128 is unreachable for a real
        // counter; it would fall through to the mtime identity below.)
        if let Ok(token) = trimmed.parse::<i128>() {
            return Some(token);
        }
    }
    // Empty / non-numeric marker: its mtime in ns is the event identity.
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(UNIX_EPOCH).ok()?;
    Some(dur.as_nanos() as i128)
}

/// Parse one registry file body into a [`RegistryRecord`], or `None` when the
/// body is not exactly five fields.
///
/// Strips ONLY a trailing newline (never tabs/spaces), then splits on TAB with
/// no max-split, so genuinely empty fields are preserved: the empty leading
/// field of a pane-less (daemon) record and the empty trailing transcript of
/// `"pane\tagent\tpid\tcwd\t"` both survive. A record is exactly
/// `pane\tagent\tpid\tcwd\ttranscript`; any other field count yields `None`.
///
/// `session_id` strips the body agent's `"<agent>-"` prefix from the file name
/// (so both `claude-<id>` and `copilot-<id>` work); a name without that prefix
/// is kept whole.
pub fn parse_registry_record(name: &str, contents: &str) -> Option<RegistryRecord> {
    let body = contents.trim_end_matches('\n');
    let mut fields = body.split('\t');
    // Exactly five fields: a sixth `next()` must be `None`.
    let (pane, agent, pid, cwd, transcript) = match (
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
        fields.next(),
    ) {
        (Some(pane), Some(agent), Some(pid), Some(cwd), Some(transcript), None) => {
            (pane, agent, pid, cwd, transcript)
        }
        _ => return None,
    };
    let prefix = format!("{agent}-");
    let session_id = name.strip_prefix(&prefix).unwrap_or(name).to_string();
    Some(RegistryRecord {
        pane: pane.to_string(),
        agent: agent.to_string(),
        pid: pid.to_string(),
        cwd: cwd.to_string(),
        transcript: transcript.to_string(),
        session_id,
    })
}

/// Serialize a record body in the on-disk wire format: five tab-separated fields
/// (pane, agent, pid, cwd, transcript) and a single trailing newline. Genuinely
/// empty fields (a pane-less record's leading pane, an empty transcript) are
/// preserved as empty between their tabs. `session_id` is file-name identity,
/// not a body field, so it is not written here.
pub fn serialize_registry_record(rec: &RegistryRecord) -> String {
    format!(
        "{}\t{}\t{}\t{}\t{}\n",
        rec.pane, rec.agent, rec.pid, rec.cwd, rec.transcript
    )
}

/// Map a color-name string to its [`NamedColor`], or `None` for the empty string
/// or any unrecognized name (which then paints in the default agent color).
fn named_color(name: &str) -> Option<NamedColor> {
    Some(match name {
        "red" => NamedColor::Red,
        "blue" => NamedColor::Blue,
        "green" => NamedColor::Green,
        "yellow" => NamedColor::Yellow,
        "purple" => NamedColor::Purple,
        "orange" => NamedColor::Orange,
        "pink" => NamedColor::Pink,
        "cyan" => NamedColor::Cyan,
        _ => return None,
    })
}

/// The trailing path component displayed for a session's directory: the basename
/// after trailing slashes are trimmed, or the whole path when that leaves nothing
/// (a root-only or empty path).
fn dir_basename(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    let base = trimmed.rsplit('/').next().unwrap_or("");
    if base.is_empty() {
        path.to_string()
    } else {
        base.to_string()
    }
}

/// The registry key for a record: the `<agent>-<session_id>` file-name identity
/// used in row keys and as the prune handle.
fn registry_key(rec: &RegistryRecord) -> SessionKey {
    SessionKey(format!("{}-{}", rec.agent, rec.session_id))
}

/// A surviving registry entry with its identity and turn state resolved, ready
/// to be placed under the panes displaying it.
struct Candidate {
    key: SessionKey,
    agent: String,
    recorded_pane: String,
    pid: String,
    cwd: String,
    title: String,
    agent_name: String,
    team: String,
    color: Option<NamedColor>,
    status: TurnStatus,
}

/// Associate live agent sessions to the panes/windows displaying them.
///
/// Returns the per-window/per-pane placements and the keys of entries to prune.
/// A session displayed in no local pane is dropped, not placed: it reappears the
/// instant a pane shows it again. A session shown in several panes yields one
/// placement per pane, so it can appear under more than one window.
///
/// Pruned in a first pass: an entry whose numeric pid is dead, and an entry whose
/// recorded pane exists in neither the local window tree nor `all_panes` while no
/// numeric pid vouches for it. A pruned entry is never placed.
///
/// A recorded pane counts as a placement only when the agent genuinely occupies
/// it — its recorded pid descends from that pane's top-level process
/// (`pane_pids`), confirmed against `parents`. A record with no numeric pid cannot
/// be verified and is trusted. A pane-less entry, or one whose recorded pane is
/// not occupied, is associated purely by title: a pane whose glyph-stripped title
/// equals exactly one entry's title is owned by it; a title shared by several
/// entries is broken by the recorded pane, then the displayed cwd, and left
/// unassigned if still ambiguous. An empty title never matches.
///
/// The emitted status is `Attention` when an attention token is present, else
/// `Working`, else `Idle`. The displayed directory tracks the pane's live path
/// (`pane_paths`), falling back to the recorded cwd when the pane reports none.
///
/// A row's color is the session's own recorded color; a teammate that recorded
/// none instead takes the color assigned to its pane in its team's config (looked
/// up only when the record has a name but no color of its own).
#[allow(clippy::too_many_arguments)]
pub fn fetch_agent_sessions(
    registry: &[RegistrySession],
    windows: &[Window],
    all_panes: &IndexSet<PaneId>,
    pane_paths: &IndexMap<PaneId, String>,
    pane_pids: &IndexMap<PaneId, u32>,
    parents: &HashMap<u32, u32>,
    labels: &mut LabelCache,
    label_mode: LabelMode,
) -> (Vec<Session>, Vec<SessionKey>) {
    // The local window tree, indexed for placement. `pane_titles` holds each real
    // pane's glyph-stripped title, the string matched against a session's own
    // title; a pane with an empty stripped title cannot own a session.
    let mut pane_to_window: IndexMap<PaneId, WindowId> = IndexMap::new();
    let mut pane_titles: IndexMap<PaneId, String> = IndexMap::new();
    for w in windows {
        for p in &w.panes {
            pane_to_window.insert(p.id.clone(), w.id.clone());
            pane_titles.insert(p.id.clone(), strip_status_prefix(&p.title));
        }
    }

    // Pass 1: prune the dead/stale, resolve each survivor's identity and title.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut prune: Vec<SessionKey> = Vec::new();
    for rs in registry {
        let rec = &rs.record;
        // A dead agent process is gone everywhere.
        if let Some(pid) = parse_field_int(&rec.pid) {
            if !pid_alive(pid) {
                prune.push(registry_key(rec));
                continue;
            }
        }
        let pid_is_digit = is_field_digits(&rec.pid);
        // A recorded pane present nowhere, with no numeric pid to vouch for it,
        // is a stale record. A pane held by another window/session (in
        // `all_panes`) is not local here but is not pruned.
        if !rec.pane.is_empty()
            && !pane_to_window.contains_key(&PaneId(rec.pane.clone()))
            && !all_panes.contains(&PaneId(rec.pane.clone()))
            && !pid_is_digit
        {
            prune.push(registry_key(rec));
            continue;
        }
        let meta = labels.session_meta(&rec.agent, &rec.transcript, &rec.session_id);
        let status = if rs.attention_token.is_some() {
            TurnStatus::Attention
        } else if matches!(rs.status, TurnStatus::Working) {
            TurnStatus::Working
        } else {
            TurnStatus::Idle
        };
        candidates.push(Candidate {
            key: registry_key(rec),
            agent: rec.agent.clone(),
            recorded_pane: rec.pane.clone(),
            pid: rec.pid.clone(),
            cwd: rec.cwd.clone(),
            title: meta.title,
            agent_name: meta.agent_name,
            team: meta.team,
            color: named_color(&meta.color),
            status,
        });
    }

    // Resolve which candidate owns each local pane by title. A single match wins
    // outright; a collision is broken by the recorded pane, then the displayed
    // cwd, and left unowned if still ambiguous (better no jump than a wrong one).
    let mut pane_owner: IndexMap<PaneId, usize> = IndexMap::new();
    for (pid, ptitle) in &pane_titles {
        if ptitle.is_empty() {
            continue;
        }
        let matched: Vec<usize> = candidates
            .iter()
            .enumerate()
            .filter(|(_, c)| &c.title == ptitle)
            .map(|(i, _)| i)
            .collect();
        let owner = match matched.len() {
            1 => Some(matched[0]),
            0 => None,
            _ => {
                let by_pane: Vec<usize> = matched
                    .iter()
                    .copied()
                    .filter(|&i| candidates[i].recorded_pane == pid.0)
                    .collect();
                let pool = if by_pane.is_empty() {
                    matched
                        .iter()
                        .copied()
                        .filter(|&i| pane_paths.get(pid) == Some(&candidates[i].cwd))
                        .collect()
                } else {
                    by_pane
                };
                (pool.len() == 1).then(|| pool[0])
            }
        };
        if let Some(o) = owner {
            pane_owner.insert(pid.clone(), o);
        }
    }

    // Pass 2: place each candidate under every pane displaying it — its occupied
    // recorded pane first, then each pane whose title it owns.
    let mut sessions: Vec<Session> = Vec::new();
    for (ci, c) in candidates.iter().enumerate() {
        let mut matched: Vec<PaneId> = Vec::new();
        if !c.recorded_pane.is_empty() {
            let rp = PaneId(c.recorded_pane.clone());
            if pane_to_window.contains_key(&rp) {
                let occupies = match parse_field_int(&c.pid) {
                    Some(pid) => {
                        process_under(pid, pane_pids.get(&rp).copied().unwrap_or(0), parents)
                    }
                    // No numeric pid to verify against: the recorded pane is trusted.
                    None => true,
                };
                if occupies {
                    matched.push(rp);
                }
            }
        }
        for (pid, &owner_idx) in &pane_owner {
            if owner_idx == ci && !matched.contains(pid) {
                matched.push(pid.clone());
            }
        }

        // A teammate row with no color of its own falls back to the per-pane
        // color name from its team config; the lookup is skipped for a record
        // with a color already or with no name (a non-teammate).
        let team_map = if c.color.is_none() && !c.agent_name.is_empty() {
            labels.team_pane_colors(&c.team)
        } else {
            IndexMap::new()
        };

        for pane in matched {
            let window = pane_to_window
                .get(&pane)
                .expect("a matched pane is always a local pane")
                .clone();
            let display_cwd = pane_paths
                .get(&pane)
                .filter(|p| !p.is_empty())
                .cloned()
                .unwrap_or_else(|| c.cwd.clone());
            let dir_name = dir_basename(&display_cwd);
            let color = c
                .color
                .or_else(|| team_map.get(&pane.0).and_then(|name| named_color(name)));
            sessions.push(Session {
                id: c.key.clone(),
                agent: c.agent.clone(),
                pane,
                window,
                label: agent_label(label_mode, &c.title, &c.agent_name, &dir_name),
                color,
                status: c.status,
            });
        }
    }

    (sessions, prune)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::load;
    use serde_json::Value;
    use std::collections::HashMap;

    fn as_u32(v: &Value) -> u32 {
        v.as_u64().expect("integer fixture value") as u32
    }

    /// Every applicable `assoc.json` case: `strip_status_prefix`,
    /// `process_under`, `attention_token`, and `parse_registry_record`. The
    /// `label_mode_from` / `agent_label` / `scan_tail` groups in the same fixture
    /// file are the label logic's responsibility and are counted as skipped here.
    #[test]
    fn fixtures_parity() {
        let cases = load("assoc");
        let tmp = std::env::temp_dir();
        let mut covered = 0usize;
        let mut skipped = 0usize;

        for (idx, case) in cases.iter().enumerate() {
            let name = case["name"].as_str().unwrap();
            let input = &case["input"];
            let expected = &case["expected"];
            let group = name.split('/').next().unwrap();

            match group {
                "strip_status_prefix" => {
                    let got = strip_status_prefix(input["title"].as_str().unwrap());
                    assert_eq!(got, expected.as_str().unwrap(), "case {name}");
                    covered += 1;
                }
                "process_under" => {
                    let pid = as_u32(&input["pid"]);
                    let ancestor = as_u32(&input["ancestor"]);
                    let mut parents = HashMap::new();
                    for (k, val) in input["parents"].as_object().unwrap() {
                        parents.insert(k.parse::<u32>().unwrap(), as_u32(val));
                    }
                    let got = process_under(pid, ancestor, &parents);
                    assert_eq!(got, expected.as_bool().unwrap(), "case {name}");
                    covered += 1;
                }
                "attention_token" => {
                    if input.get("missing").and_then(Value::as_bool) == Some(true) {
                        let p = tmp.join("wrangler_assoc_missing_marker_nonexistent");
                        let _ = std::fs::remove_file(&p);
                        assert_eq!(attention_token(&p), None, "case {name}");
                    } else {
                        let contents = input["contents"].as_str().unwrap();
                        let p = tmp.join(format!("wrangler_assoc_attn_{idx}"));
                        std::fs::write(&p, contents).unwrap();
                        let got = attention_token(&p);
                        let exp = expected.as_i64().unwrap() as i128;
                        assert_eq!(got, Some(exp), "case {name}");
                        let _ = std::fs::remove_file(&p);
                    }
                    covered += 1;
                }
                "parse_registry_record" => {
                    let got = parse_registry_record(
                        input["name"].as_str().unwrap(),
                        input["contents"].as_str().unwrap(),
                    );
                    if expected.is_null() {
                        assert_eq!(got, None, "case {name}");
                    } else {
                        let g = got.unwrap_or_else(|| panic!("case {name}: expected Some"));
                        assert_eq!(g.pane, expected["pane"].as_str().unwrap(), "pane {name}");
                        assert_eq!(g.agent, expected["agent"].as_str().unwrap(), "agent {name}");
                        assert_eq!(g.pid, expected["pid"].as_str().unwrap(), "pid {name}");
                        assert_eq!(g.cwd, expected["cwd"].as_str().unwrap(), "cwd {name}");
                        assert_eq!(
                            g.transcript,
                            expected["transcript"].as_str().unwrap(),
                            "transcript {name}"
                        );
                        assert_eq!(
                            g.session_id,
                            expected["session_id"].as_str().unwrap(),
                            "session_id {name}"
                        );
                    }
                    covered += 1;
                }
                // labels-module groups: not this module's responsibility.
                "label_mode_from" | "agent_label" | "scan_tail" => {
                    skipped += 1;
                }
                other => panic!("unexpected fixture group {other} in assoc.json"),
            }
        }

        // 11 strip + 10 process_under + 3 attention + 12 parse = 36 covered;
        // 8 label_mode_from + 9 agent_label + 10 scan_tail = 27 skipped (labels).
        // 36 + 27 = 63, the full assoc.json case count.
        assert_eq!(covered, 36, "covered assoc fixture count");
        assert_eq!(skipped, 27, "skipped (labels) fixture count");
        assert_eq!(covered + skipped, cases.len(), "every case dispatched");
    }

    #[test]
    fn parse_field_int_isdigit_gate() {
        assert_eq!(parse_field_int("24"), Some(24));
        assert_eq!(parse_field_int("0"), Some(0));
        assert_eq!(parse_field_int(""), None);
        assert_eq!(parse_field_int("-5"), None);
        assert_eq!(parse_field_int("+5"), None);
        assert_eq!(parse_field_int("24px"), None);
        assert_eq!(parse_field_int(" 24 "), None);
        assert_eq!(parse_field_int(" "), None);
        assert_eq!(parse_field_int("1234567890"), Some(1234567890));

        assert!(is_field_digits("7"));
        assert!(!is_field_digits(""));
        assert!(!is_field_digits("7a"));
    }

    #[test]
    fn parse_ppid_stdout_sample() {
        // A representative `ps -e -o pid= -o ppid=` dump: leading padding,
        // whitespace runs, a bracketed kernel-thread command with spaces, junk,
        // an extra-column line, and a duplicate-pid last-write-wins row.
        let out = "\
    1     0
 1234   5678
   42      1
[kernel]  1
garbage line here
 9   8   7
 1234  6000
";
        let parents = parse_ppid_stdout(out);
        assert_eq!(parents.get(&1), Some(&0));
        assert_eq!(parents.get(&42), Some(&1));
        assert_eq!(parents.get(&9), Some(&8)); // third column ignored
        assert_eq!(parents.get(&1234), Some(&6000)); // last write wins
                                                     // `[kernel] 1` and `garbage line here` contribute nothing.
        assert_eq!(parents.len(), 4);

        assert!(parse_ppid_stdout("").is_empty());
    }

    #[test]
    fn ppid_map_runs_ps() {
        // Exercises the real subprocess path; `ps -e` always lists at least
        // pid 1 on any host with `ps`.
        let parents = ppid_map();
        assert!(!parents.is_empty(), "ppid_map produced no entries");
    }

    #[test]
    fn serialize_five_fields_and_roundtrip() {
        let rec = RegistryRecord {
            pane: "%3".into(),
            agent: "claude".into(),
            pid: "1234".into(),
            cwd: "/home/x".into(),
            transcript: "/t/x.jsonl".into(),
            session_id: "abc".into(),
        };
        assert_eq!(
            serialize_registry_record(&rec),
            "%3\tclaude\t1234\t/home/x\t/t/x.jsonl\n"
        );
        // A current 5-field record round-trips exactly.
        let round = parse_registry_record("claude-abc", &serialize_registry_record(&rec)).unwrap();
        assert_eq!(round, rec);

        // A pane-less record keeps its empty leading field through a round-trip.
        let paneless = RegistryRecord {
            pane: "".into(),
            agent: "claude".into(),
            pid: "1234".into(),
            cwd: "/home/x".into(),
            transcript: "/t/x.jsonl".into(),
            session_id: "dae".into(),
        };
        assert_eq!(
            serialize_registry_record(&paneless),
            "\tclaude\t1234\t/home/x\t/t/x.jsonl\n"
        );
        let round_paneless =
            parse_registry_record("claude-dae", &serialize_registry_record(&paneless)).unwrap();
        assert_eq!(round_paneless, paneless);

        // An empty trailing transcript survives (5th field kept empty).
        let empty_transcript = RegistryRecord {
            pane: "%3".into(),
            agent: "claude".into(),
            pid: "1234".into(),
            cwd: "/home/x".into(),
            transcript: "".into(),
            session_id: "abc".into(),
        };
        assert_eq!(
            serialize_registry_record(&empty_transcript),
            "%3\tclaude\t1234\t/home/x\t\n"
        );
        let round_empty =
            parse_registry_record("claude-abc", &serialize_registry_record(&empty_transcript))
                .unwrap();
        assert_eq!(round_empty, empty_transcript);
    }

    #[test]
    fn attention_token_mtime_fallback() {
        let tmp = std::env::temp_dir();

        // Empty marker: token is the file's true st_mtime_ns.
        let empty = tmp.join("wrangler_assoc_attn_empty");
        std::fs::write(&empty, "").unwrap();
        let meta = std::fs::metadata(&empty).unwrap();
        let expected_ns = meta
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos() as i128;
        assert_eq!(attention_token(&empty), Some(expected_ns));
        let _ = std::fs::remove_file(&empty);

        // Non-numeric contents also fall back to mtime, not None.
        let junk = tmp.join("wrangler_assoc_attn_junk");
        std::fs::write(&junk, "not-a-number\n").unwrap();
        assert!(attention_token(&junk).is_some());
        let _ = std::fs::remove_file(&junk);
    }

    #[test]
    fn pid_alive_self_is_alive() {
        // This process is running, so its own pid must probe alive.
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_zero_is_dead() {
        // A zero pid is never probed and reports dead.
        assert!(!pid_alive(0));
    }

    #[test]
    fn pid_alive_reaped_child_is_dead() {
        // A child that has exited and been waited on no longer exists.
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        assert!(!pid_alive(pid));
    }

    #[test]
    fn process_under_bound_stops_a_cycle() {
        // Without the 4096 cap this 2-cycle would loop forever; the ancestor is
        // never on the cycle, so it must terminate false.
        let mut parents = HashMap::new();
        parents.insert(10u32, 20u32);
        parents.insert(20u32, 10u32);
        assert!(!process_under(10, 999, &parents));
    }

    // --- fetch_agent_sessions ------------------------------------------------

    static ASSOC_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    // Serializes tests that mutate the process-wide `CLAUDE_CONFIG_DIR`, so a
    // concurrent test cannot observe another's temporary value. Poison from a
    // failed assertion in one holder is recovered so it does not cascade.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn pane(id: &str, title: &str) -> crate::model::Pane {
        crate::model::Pane {
            id: PaneId(id.into()),
            index: "0".into(),
            active: false,
            title: title.into(),
            pb_state: String::new(),
            pb_progress: None,
            color: None,
        }
    }

    fn window(id: &str, panes: Vec<crate::model::Pane>) -> Window {
        Window {
            id: WindowId(id.into()),
            index: "1".into(),
            name: "w".into(),
            active: false,
            color: None,
            panes,
        }
    }

    fn record(
        pane: &str,
        agent: &str,
        pid: &str,
        cwd: &str,
        transcript: &str,
        session_id: &str,
    ) -> RegistryRecord {
        RegistryRecord {
            pane: pane.into(),
            agent: agent.into(),
            pid: pid.into(),
            cwd: cwd.into(),
            transcript: transcript.into(),
            session_id: session_id.into(),
        }
    }

    fn reg(record: RegistryRecord, status: TurnStatus, token: Option<i128>) -> RegistrySession {
        RegistrySession {
            record,
            status,
            attention_token: token,
        }
    }

    /// A Claude transcript file whose tail resolves to `title`. The caller removes
    /// it; each call gets a unique path so parallel tests do not collide.
    fn claude_transcript(title: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wrangler_assoc_scan_{}_{}",
            std::process::id(),
            ASSOC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::write(
            &p,
            format!("{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\"}}\n"),
        )
        .unwrap();
        p
    }

    fn ipaths(pairs: &[(&str, &str)]) -> IndexMap<PaneId, String> {
        pairs
            .iter()
            .map(|(k, v)| (PaneId((*k).into()), (*v).into()))
            .collect()
    }

    fn ipids(pairs: &[(&str, u32)]) -> IndexMap<PaneId, u32> {
        pairs
            .iter()
            .map(|(k, v)| (PaneId((*k).into()), *v))
            .collect()
    }

    fn ipanes(ids: &[&str]) -> IndexSet<PaneId> {
        ids.iter().map(|id| PaneId((*id).into())).collect()
    }

    /// A pid that is guaranteed dead: a child spawned and reaped, so its id no
    /// longer names a live process.
    fn dead_pid() -> u32 {
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait true");
        pid
    }

    #[test]
    fn recorded_pane_occupied_is_placed() {
        // The agent pid descends from the pane's top-level process, so the
        // recorded pane counts as a placement.
        let self_pid = std::process::id();
        let windows = vec![window("@1", vec![pane("%1", "")])];
        let registry = vec![reg(
            record("%1", "claude", &self_pid.to_string(), "/home/x", "", "abc"),
            TurnStatus::Idle,
            None,
        )];
        let mut parents = HashMap::new();
        parents.insert(self_pid, 2u32);
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live/dir")]),
            &ipids(&[("%1", 2)]),
            &parents,
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane, PaneId("%1".into()));
        assert_eq!(sessions[0].window, WindowId("@1".into()));
        assert_eq!(sessions[0].id, SessionKey("claude-abc".into()));
        // No title, so the label falls back to the live path's basename.
        assert_eq!(sessions[0].label, "dir");
        assert_eq!(sessions[0].status, TurnStatus::Idle);
    }

    #[test]
    fn recorded_pane_inherited_is_not_placed() {
        // The pid does not descend from the pane's top-level process (an inherited
        // pane id), so the recorded pane is not a placement and, with no title
        // match, the session is dropped — but the live entry is not pruned.
        let self_pid = std::process::id();
        let windows = vec![window("@1", vec![pane("%1", "")])];
        let registry = vec![reg(
            record("%1", "claude", &self_pid.to_string(), "/home/x", "", "abc"),
            TurnStatus::Idle,
            None,
        )];
        let mut parents = HashMap::new();
        parents.insert(self_pid, 1u32);
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live")]),
            &ipids(&[("%1", 2)]),
            &parents,
            &mut cache,
            LabelMode::Name,
        );

        assert!(sessions.is_empty());
        assert!(prune.is_empty());
    }

    #[test]
    fn title_matched_daemon_session_is_placed() {
        // A pane-less entry is associated to the local pane whose stripped title
        // equals its own title.
        let transcript = claude_transcript("MySession");
        let windows = vec![window("@1", vec![pane("%1", "MySession")])];
        let registry = vec![reg(
            record(
                "",
                "claude",
                "",
                "/rec/cwd",
                transcript.to_str().unwrap(),
                "dae",
            ),
            TurnStatus::Working,
            None,
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live/proj")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane, PaneId("%1".into()));
        assert_eq!(sessions[0].window, WindowId("@1".into()));
        assert_eq!(sessions[0].id, SessionKey("claude-dae".into()));
        assert_eq!(sessions[0].label, "MySession");
        assert_eq!(sessions[0].status, TurnStatus::Working);

        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn one_session_under_two_windows() {
        // The same title shown in two panes (two windows) yields one placement
        // per pane.
        let transcript = claude_transcript("Shared");
        let windows = vec![
            window("@1", vec![pane("%1", "Shared")]),
            window("@2", vec![pane("%2", "Shared")]),
        ];
        let registry = vec![reg(
            record("", "claude", "", "/c", transcript.to_str().unwrap(), "dae"),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1", "%2"]),
            &ipaths(&[("%1", "/a"), ("%2", "/b")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 2);
        // Placement order follows the window/pane iteration order.
        assert_eq!(sessions[0].pane, PaneId("%1".into()));
        assert_eq!(sessions[0].window, WindowId("@1".into()));
        assert_eq!(sessions[1].pane, PaneId("%2".into()));
        assert_eq!(sessions[1].window, WindowId("@2".into()));
        assert!(sessions
            .iter()
            .all(|s| s.id == SessionKey("claude-dae".into())));

        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn title_collision_broken_by_recorded_pane() {
        // Two entries share a title; the pane displaying it is owned by the one
        // whose recorded pane is that very pane.
        let transcript = claude_transcript("Dup");
        let t = transcript.to_str().unwrap();
        let windows = vec![window("@1", vec![pane("%1", "Dup")])];
        let registry = vec![
            reg(
                record("%1", "claude", "", "/x", t, "one"),
                TurnStatus::Idle,
                None,
            ),
            reg(
                record("%9", "claude", "", "/y", t, "two"),
                TurnStatus::Idle,
                None,
            ),
        ];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1", "%9"]),
            &ipaths(&[("%1", "/live")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane, PaneId("%1".into()));
        assert_eq!(sessions[0].id, SessionKey("claude-one".into()));

        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn title_collision_broken_by_cwd() {
        // Neither entry's recorded pane is the displaying pane, so the tie falls
        // to the entry whose recorded cwd equals the pane's live path.
        let transcript = claude_transcript("Dup");
        let t = transcript.to_str().unwrap();
        let windows = vec![window("@1", vec![pane("%1", "Dup")])];
        let registry = vec![
            reg(
                record("", "claude", "", "/proj/a", t, "one"),
                TurnStatus::Idle,
                None,
            ),
            reg(
                record("", "claude", "", "/proj/b", t, "two"),
                TurnStatus::Idle,
                None,
            ),
        ];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/proj/a")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].pane, PaneId("%1".into()));
        assert_eq!(sessions[0].id, SessionKey("claude-one".into()));

        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn dropped_when_no_local_pane_shows_it() {
        // The recorded pane belongs to another window/session (present in
        // `all_panes`, absent from the local tree) and no title matches, so the
        // session is dropped but not pruned.
        let transcript = claude_transcript("Zzz");
        let self_pid = std::process::id();
        let windows = vec![window("@1", vec![pane("%1", "Other")])];
        let registry = vec![reg(
            record(
                "%2",
                "claude",
                &self_pid.to_string(),
                "/c",
                transcript.to_str().unwrap(),
                "elsewhere",
            ),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1", "%2"]),
            &ipaths(&[("%1", "/a"), ("%2", "/b")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(sessions.is_empty());
        assert!(prune.is_empty());

        let _ = std::fs::remove_file(&transcript);
    }

    #[test]
    fn prunes_dead_pid() {
        let dead = dead_pid();
        let windows = vec![window("@1", vec![pane("%1", "")])];
        let registry = vec![reg(
            record("%1", "claude", &dead.to_string(), "/c", "", "gone"),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(sessions.is_empty());
        assert_eq!(prune, vec![SessionKey("claude-gone".into())]);
    }

    #[test]
    fn prunes_stale_recorded_pane() {
        // A recorded pane present nowhere, with no numeric pid to vouch for it,
        // is stale and pruned.
        let windows = vec![window("@1", vec![pane("%1", "")])];
        let registry = vec![reg(
            record("%9", "claude", "", "/c", "", "stale"),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(sessions.is_empty());
        assert_eq!(prune, vec![SessionKey("claude-stale".into())]);
    }

    #[test]
    fn attention_token_forces_attention_status() {
        // An attention token overrides the marker status: the placement reports
        // Attention even when the carried status is Idle.
        let transcript = claude_transcript("A");
        let windows = vec![window("@1", vec![pane("%1", "A")])];
        let registry = vec![reg(
            record("", "claude", "", "/c", transcript.to_str().unwrap(), "att"),
            TurnStatus::Idle,
            Some(123),
        )];
        let mut cache = LabelCache::new();

        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].status, TurnStatus::Attention);

        let _ = std::fs::remove_file(&transcript);
    }

    /// Write a Claude teammate transcript whose tail resolves to `title`,
    /// `agentName`, `teamName`, and (when non-empty) an `agent-color`. Each call
    /// gets a unique path; the caller removes it.
    fn teammate_transcript(title: &str, team: &str, color: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wrangler_assoc_team_{}_{}",
            std::process::id(),
            ASSOC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let mut body = format!(
            "{{\"type\":\"ai-title\",\"aiTitle\":\"{title}\"}}\n\
             {{\"agentName\":\"tm\",\"teamName\":\"{team}\"}}\n"
        );
        if !color.is_empty() {
            body.push_str(&format!(
                "{{\"type\":\"agent-color\",\"agentColor\":\"{color}\"}}\n"
            ));
        }
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn teammate_color_from_team_config_and_own_color_wins() {
        // A teammate that records no color of its own takes the color assigned to
        // its pane in the team config; a teammate that records its own color keeps
        // it and ignores the team config.
        let dir = std::env::temp_dir().join(format!(
            "wrangler_assoc_teamdir_{}_{}",
            std::process::id(),
            ASSOC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let team_cfg = dir.join("teams").join("myteam");
        std::fs::create_dir_all(&team_cfg).unwrap();
        std::fs::write(
            team_cfg.join("config.json"),
            r#"{"members":[{"tmuxPaneId":"%1","color":"purple"},{"tmuxPaneId":"%2","color":"orange"}]}"#,
        )
        .unwrap();

        let ta = teammate_transcript("RowA", "myteam", "");
        let tb = teammate_transcript("RowB", "myteam", "red");
        let windows = vec![window("@1", vec![pane("%1", "RowA"), pane("%2", "RowB")])];
        let registry = vec![
            reg(
                record("", "claude", "", "/c", ta.to_str().unwrap(), "aa"),
                TurnStatus::Idle,
                None,
            ),
            reg(
                record("", "claude", "", "/c", tb.to_str().unwrap(), "bb"),
                TurnStatus::Idle,
                None,
            ),
        ];
        let mut cache = LabelCache::new();

        let _env = lock_env();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1", "%2"]),
            &ipaths(&[("%1", "/live"), ("%2", "/live")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 2);
        let a = sessions
            .iter()
            .find(|s| s.id == SessionKey("claude-aa".into()))
            .unwrap();
        let b = sessions
            .iter()
            .find(|s| s.id == SessionKey("claude-bb".into()))
            .unwrap();
        // No own color -> the team config's pane color.
        assert_eq!(a.color, Some(NamedColor::Purple));
        // Own color present -> the team config is not consulted.
        assert_eq!(b.color, Some(NamedColor::Red));

        let _ = std::fs::remove_file(&ta);
        let _ = std::fs::remove_file(&tb);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn top_level_session_never_takes_a_team_color() {
        // A non-teammate (empty agent_name) records no team, so even with a team
        // config present for a pane it occupies, its color stays unset.
        let dir = std::env::temp_dir().join(format!(
            "wrangler_assoc_teamdir_{}_{}",
            std::process::id(),
            ASSOC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // A config for the empty team, keyed so that %1 would resolve to a color
        // if it were ever consulted for a non-teammate.
        let team_cfg = dir.join("teams").join("");
        std::fs::create_dir_all(&team_cfg).unwrap();
        std::fs::write(
            team_cfg.join("config.json"),
            r#"{"members":[{"tmuxPaneId":"%1","color":"purple"}]}"#,
        )
        .unwrap();

        let transcript = claude_transcript("Solo");
        let windows = vec![window("@1", vec![pane("%1", "Solo")])];
        let registry = vec![reg(
            record("", "claude", "", "/c", transcript.to_str().unwrap(), "solo"),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let _env = lock_env();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].color, None);

        let _ = std::fs::remove_file(&transcript);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn teammate_with_missing_team_config_gets_no_color() {
        // A teammate with no own color and no team config file on disk falls back
        // to nothing rather than inventing a color.
        let dir = std::env::temp_dir().join(format!(
            "wrangler_assoc_teamdir_{}_{}",
            std::process::id(),
            ASSOC_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        // Deliberately no teams/<team>/config.json is written under `dir`.

        let ta = teammate_transcript("RowA", "ghostteam", "");
        let windows = vec![window("@1", vec![pane("%1", "RowA")])];
        let registry = vec![reg(
            record("", "claude", "", "/c", ta.to_str().unwrap(), "aa"),
            TurnStatus::Idle,
            None,
        )];
        let mut cache = LabelCache::new();

        let _env = lock_env();
        let prev = std::env::var_os("CLAUDE_CONFIG_DIR");
        std::env::set_var("CLAUDE_CONFIG_DIR", &dir);
        let (sessions, prune) = fetch_agent_sessions(
            &registry,
            &windows,
            &ipanes(&["%1"]),
            &ipaths(&[("%1", "/live")]),
            &ipids(&[]),
            &HashMap::new(),
            &mut cache,
            LabelMode::Name,
        );
        match prev {
            Some(v) => std::env::set_var("CLAUDE_CONFIG_DIR", v),
            None => std::env::remove_var("CLAUDE_CONFIG_DIR"),
        }

        assert!(prune.is_empty());
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].color, None);

        let _ = std::fs::remove_file(&ta);
    }

    #[test]
    fn dir_basename_edge_cases() {
        assert_eq!(dir_basename("/home/user/proj"), "proj");
        assert_eq!(dir_basename("/home/user/proj/"), "proj");
        assert_eq!(dir_basename("proj"), "proj");
        // A root-only path has no basename, so the whole path stands.
        assert_eq!(dir_basename("/"), "/");
        assert_eq!(dir_basename(""), "");
    }
}
