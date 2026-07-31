//! Agent-session label logic: session titles, teammate names, and colors read
//! from each agent's metadata, composed into the row label.
//!
//! The pure core: the YAML scalar/field decoders, the Claude transcript tail
//! scan, the label composition, and the title-resolution rules
//! (`resolve_claude_meta`, `copilot_title_from_text`).

use serde_json::Value;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Only the transcript's tail is scanned: the title records sit within a few KB
/// of EOF in practice, so this stays cheap regardless of transcript size. The
/// truncated-first-line drop in `scan_tail` is keyed off `size > TITLE_TAIL_BYTES`.
const TITLE_TAIL_BYTES: u64 = 65536;

/// The row-label mode, normalized from the raw `@wrangler-label` tmux option.
/// `Name` is the default for any unset/unknown value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LabelMode {
    /// Show the working-directory basename.
    Dir,
    /// Show the session title (falling back to the dir basename when untitled).
    Name,
}

/// The four label fields a session's metadata resolves to:
/// `(title, agent_name, team, color)`. Every field is `""` when absent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SessionMeta {
    pub title: String,
    pub agent_name: String,
    pub team: String,
    pub color: String,
}

impl SessionMeta {
    /// The all-empty result for an unknown agent or when no metadata is found.
    pub fn empty() -> Self {
        Self::default()
    }
}

// --- string-classification helpers -------------------------------------------
//
// These classify chars the way the label formats require: the C0 separators
// U+001C..U+001F count as whitespace, and `splitlines` recognizes the full set
// of line boundaries the metadata sources may use. Real files use plain `\n`
// and ASCII indentation, so the exotic-separator branches are rarely hit.

/// Whitespace including the C0 separators U+001C..U+001F (which
/// `char::is_whitespace` does not treat as whitespace) plus Unicode White_Space.
fn py_isspace(c: char) -> bool {
    matches!(c as u32, 0x1c..=0x1f) || c.is_whitespace()
}

/// Trim leading/trailing whitespace by the [`py_isspace`] classification.
fn py_strip(s: &str) -> &str {
    s.trim_matches(py_isspace)
}

/// Split on line boundaries (`\n`, `\r`, `\r\n`, `\v`, `\f`, U+001C..U+001E,
/// U+0085, U+2028, U+2029) and drop the trailing empty element that a
/// terminating boundary would otherwise leave. `scan_tail` instead splits on
/// raw `\n` bytes only, because it must tolerate non-UTF-8 lines.
fn py_splitlines(s: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut chars = s.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        let is_break = matches!(
            c as u32,
            0x0a | 0x0b | 0x0c | 0x0d | 0x1c | 0x1d | 0x1e | 0x85 | 0x2028 | 0x2029
        );
        if !is_break {
            continue;
        }
        lines.push(&s[start..i]);
        let mut next_start = i + c.len_utf8();
        if c == '\r' {
            if let Some(&(j, '\n')) = chars.peek() {
                chars.next();
                next_start = j + '\n'.len_utf8();
            }
        }
        start = next_start;
    }
    if start < s.len() {
        lines.push(&s[start..]);
    }
    lines
}

/// Drop the first and last char, yielding `""` for any input of fewer than two
/// chars (guarding against an underflow on a lone quote char).
fn drop_first_last(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() < 2 {
        return String::new();
    }
    chars[1..chars.len() - 1].iter().collect()
}

// --- pure fixtured functions -------------------------------------------------

/// Decode the minimal set of YAML scalar quoting forms `workspace.yaml` emits.
/// Trims first. Empty / `null` / `~` -> `""`. Double-quoted: JSON-parse the
/// whole token; a JSON string is returned, a non-string JSON value yields `""`,
/// and a parse error falls back to a naive first/last-char strip that does not
/// re-interpret escapes. Single-quoted: strip the quotes and collapse `''` to
/// `'`. Anything else: the trimmed value verbatim.
pub fn yaml_scalar(value: &str) -> String {
    let value = py_strip(value);
    if value.is_empty() || value == "null" || value == "~" {
        return String::new();
    }
    if value.starts_with('"') && value.ends_with('"') {
        return match serde_json::from_str::<Value>(value) {
            Ok(Value::String(s)) => s,
            // A token bounded by literal quotes always decodes to a string when
            // it parses; the non-string arm is unreachable but kept for safety.
            Ok(_) => String::new(),
            Err(_) => drop_first_last(value),
        };
    }
    if value.starts_with('\'') && value.ends_with('\'') {
        return drop_first_last(value).replace("''", "'");
    }
    value.to_string()
}

/// Read one top-level (column-0) scalar or block-scalar field named `key` from
/// a `workspace.yaml` text. Honors the FIRST matching line. A block-scalar
/// indicator (`|`, `|-`, `|+`, `>`, `>-`, `>+`) returns only the first
/// non-empty continuation line, stripped; the block ends at the first
/// continuation whose first char is non-whitespace (a new top-level key). No
/// match -> `""`.
pub fn workspace_field(text: &str, key: &str) -> String {
    let prefix = format!("{key}:");
    let lines = py_splitlines(text);
    for (index, line) in lines.iter().enumerate() {
        let Some(rem) = line.strip_prefix(&prefix) else {
            continue;
        };
        let value = py_strip(rem);
        if !matches!(value, "|" | "|-" | "|+" | ">" | ">-" | ">+") {
            return yaml_scalar(value);
        }
        for cont in &lines[index + 1..] {
            // An empty continuation neither terminates the block nor yields a
            // value: it is skipped. A non-empty continuation whose first char is
            // non-whitespace is a new top-level key and terminates the block.
            if let Some(first) = cont.chars().next() {
                if !py_isspace(first) {
                    break;
                }
            }
            let stripped = py_strip(cont);
            if !stripped.is_empty() {
                return stripped.to_string();
            }
        }
        return String::new();
    }
    String::new()
}

/// Read the final [`TITLE_TAIL_BYTES`] of a Claude transcript and extract
/// `(custom, ai, agent, team, color)`. `custom`/`ai`/`color` are last-wins
/// (each match overwrites while iterating forward); `agent`/`team` are
/// first-wins (filled only while still empty). A per-line byte-substring
/// prefilter selects at most ONE branch before JSON parsing; a parse failure
/// (including invalid UTF-8) skips the line without trying another branch. Any
/// I/O error -> all-empty.
pub fn scan_tail(transcript_path: &Path) -> (String, String, String, String, String) {
    let empty = || {
        (
            String::new(),
            String::new(),
            String::new(),
            String::new(),
            String::new(),
        )
    };
    let mut file = match File::open(transcript_path) {
        Ok(f) => f,
        Err(_) => return empty(),
    };
    let size = match file.seek(SeekFrom::End(0)) {
        Ok(s) => s,
        Err(_) => return empty(),
    };
    let start = size.saturating_sub(TITLE_TAIL_BYTES);
    if file.seek(SeekFrom::Start(start)).is_err() {
        return empty();
    }
    let mut chunk = Vec::new();
    if file.read_to_end(&mut chunk).is_err() {
        return empty();
    }

    let mut lines: Vec<&[u8]> = chunk.split(|&b| b == b'\n').collect();
    // The first line of an oversize tail is likely truncated mid-record; drop
    // it only when the file actually exceeded the scanned window.
    if size > TITLE_TAIL_BYTES && !lines.is_empty() {
        lines.remove(0);
    }

    let (mut custom, mut ai, mut agent, mut team, mut color) = (
        String::new(),
        String::new(),
        String::new(),
        String::new(),
        String::new(),
    );
    for line in lines {
        if contains(line, b"\"custom-title\"") {
            let Ok(rec) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if get_str(&rec, "type") == Some("custom-title") {
                if let Some(t) = truthy_str(&rec, "customTitle") {
                    custom = t.to_string();
                }
            }
        } else if contains(line, b"\"ai-title\"") {
            let Ok(rec) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if get_str(&rec, "type") == Some("ai-title") {
                if let Some(t) = truthy_str(&rec, "aiTitle") {
                    ai = t.to_string();
                }
            }
        } else if contains(line, b"\"agent-color\"") {
            let Ok(rec) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if get_str(&rec, "type") == Some("agent-color") {
                if let Some(c) = truthy_str(&rec, "agentColor") {
                    color = c.to_string();
                }
            }
        } else if (agent.is_empty() || team.is_empty()) && contains(line, b"\"agentName\"") {
            let Ok(rec) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            // agentName/teamName ride together on every conversation record of a
            // teammate; a normal session only carries agentName inside a rename
            // 'agent-name' record, which is excluded here. A missing type also
            // qualifies (it is treated as not-equal to 'agent-name').
            if get_str(&rec, "type") != Some("agent-name") {
                if agent.is_empty() {
                    agent = get_str(&rec, "agentName").unwrap_or("").to_string();
                }
                if team.is_empty() {
                    team = get_str(&rec, "teamName").unwrap_or("").to_string();
                }
            }
        }
    }
    (custom, ai, agent, team, color)
}

/// Normalize a raw `@wrangler-label` option string to a mode: `Dir` only for
/// the exact trimmed+lowercased token `dir`, else `Name`.
pub fn label_mode_from(value: &str) -> LabelMode {
    if py_strip(value).to_lowercase() == "dir" {
        LabelMode::Dir
    } else {
        LabelMode::Name
    }
}

/// Compose the exact agent-row label text.
///
/// A teammate (`agent_name` non-empty) reads as `@name - tail` (`tail` is the
/// title in `Name` mode, the dir basename in `Dir` mode), or `@name` when the
/// tail is empty. A top-level session shows its title in `Name` mode (dir
/// basename when untitled) or the dir basename in `Dir` mode. The separator is
/// exactly ` - ` (space, ASCII hyphen-minus, space).
pub fn agent_label(mode: LabelMode, title: &str, agent_name: &str, dir_name: &str) -> String {
    if !agent_name.is_empty() {
        let tail = match mode {
            LabelMode::Name => title,
            LabelMode::Dir => dir_name,
        };
        return if tail.is_empty() {
            format!("@{agent_name}")
        } else {
            format!("@{agent_name} - {tail}")
        };
    }
    let primary = match mode {
        LabelMode::Name => title,
        LabelMode::Dir => "",
    };
    if primary.is_empty() {
        dir_name.to_string()
    } else {
        primary.to_string()
    }
}

// --- pure core of the sticky session-meta caches -----------------------------

/// The cached state a Claude session carries between ticks, minus the mtime key
/// (owned by the cache layer). `is_custom` is never surfaced to callers but
/// makes a manual `/rename` title override every subsequent auto `ai-title`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaudeMetaState {
    pub title: String,
    pub is_custom: bool,
    pub agent: String,
    pub team: String,
    pub color: String,
}

/// Resolve the next Claude session state from a fresh [`scan_tail`] result and
/// the previously cached state. A manual `custom` title wins and is sticky
/// (`is_custom` forces it to survive later `ai-title`s); `agent`/`team`/`color`
/// fall back to their previous value when the fresh scan is empty, so an output
/// burst that pushes them out of the tail does not regress them to `""`. The
/// file read, the mtime key, and the cache storage live in the cache layer.
pub fn resolve_claude_meta(
    scan: (String, String, String, String, String),
    prev: &ClaudeMetaState,
) -> ClaudeMetaState {
    let (custom, ai, agent, team, color) = scan;
    let (title, is_custom) = if !custom.is_empty() {
        (custom, true)
    } else if prev.is_custom {
        (prev.title.clone(), true) // keep the manual name
    } else if !ai.is_empty() {
        (ai, false)
    } else {
        (prev.title.clone(), prev.is_custom) // keep last
    };
    ClaudeMetaState {
        title,
        is_custom,
        agent: if agent.is_empty() {
            prev.agent.clone()
        } else {
            agent
        },
        team: if team.is_empty() {
            prev.team.clone()
        } else {
            team
        },
        color: if color.is_empty() {
            prev.color.clone()
        } else {
            color
        },
    }
}

/// The current Copilot session name (or generated summary) from a
/// `workspace.yaml` text, reduced to its first non-empty trimmed line. The file
/// read, the mtime cache key, and the keep-previous-on-empty stickiness live in
/// the cache layer.
pub fn copilot_title_from_text(text: &str) -> String {
    let mut title = workspace_field(text, "name");
    if title.is_empty() {
        title = workspace_field(text, "summary");
    }
    for line in py_splitlines(&title) {
        let stripped = py_strip(line);
        if !stripped.is_empty() {
            return stripped.to_string();
        }
    }
    String::new()
}

// --- small byte/JSON helpers -------------------------------------------------

/// Byte-subsequence containment (`needle` occurs contiguously in `haystack`).
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    if needle.len() > haystack.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|w| w == needle)
}

/// A JSON object's string field, or `None` if absent or non-string.
fn get_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str)
}

/// A JSON object's string field only when present, a string, and non-empty.
fn truthy_str<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    get_str(v, key).filter(|s| !s.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures;
    use serde_json::Value;

    fn s(v: &Value) -> &str {
        v.as_str().unwrap()
    }

    /// Minimal standard-base64 decoder for the one base64-encoded fixture (a
    /// non-UTF-8 transcript tail); avoids a dependency for a single case.
    fn b64_decode(input: &str) -> Vec<u8> {
        const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut rev = [255u8; 256];
        for (i, &c) in T.iter().enumerate() {
            rev[c as usize] = i as u8;
        }
        let mut bits = 0u32;
        let mut nbits = 0u32;
        let mut out = Vec::new();
        for &byte in input.as_bytes() {
            if byte == b'=' {
                break;
            }
            let v = rev[byte as usize];
            if v == 255 {
                continue;
            }
            bits = (bits << 6) | v as u32;
            nbits += 6;
            if nbits >= 8 {
                nbits -= 8;
                out.push((bits >> nbits) as u8);
            }
        }
        out
    }

    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

    /// A unique scratch path for a scan_tail case; the file is written (or, for
    /// the missing-file case, deliberately not) and cleaned up by the caller.
    fn scratch_path(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "wrangler_labels_{}_{}_{}",
            std::process::id(),
            tag,
            COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        p
    }

    fn mode_from_str(v: &str) -> LabelMode {
        match v {
            "dir" => LabelMode::Dir,
            "name" => LabelMode::Name,
            other => panic!("unexpected mode fixture value {other:?}"),
        }
    }

    fn mode_to_str(m: LabelMode) -> &'static str {
        match m {
            LabelMode::Dir => "dir",
            LabelMode::Name => "name",
        }
    }

    #[test]
    fn parity_fixtures() {
        let cases = fixtures::load("labels");
        let mut covered = 0usize;
        for case in &cases {
            let name = s(&case["name"]);
            let input = &case["input"];
            let func = s(&input["fn"]);
            match func {
                "yaml_scalar" => {
                    let got = yaml_scalar(s(&input["value"]));
                    assert_eq!(got, s(&case["expected"]), "case {name}");
                    covered += 1;
                }
                "workspace_field" => {
                    let got = workspace_field(s(&input["text"]), s(&input["key"]));
                    assert_eq!(got, s(&case["expected"]), "case {name}");
                    covered += 1;
                }
                "label_mode_from" => {
                    let got = label_mode_from(s(&input["value"]));
                    assert_eq!(mode_to_str(got), s(&case["expected"]), "case {name}");
                    covered += 1;
                }
                "agent_label" => {
                    let got = agent_label(
                        mode_from_str(s(&input["mode"])),
                        s(&input["title"]),
                        s(&input["agent_name"]),
                        s(&input["dir_name"]),
                    );
                    assert_eq!(got, s(&case["expected"]), "case {name}");
                    covered += 1;
                }
                "scan_tail" => {
                    // The fixture supplies the transcript inline, base64-encoded
                    // for a non-UTF-8 tail, or not at all (a path with no file).
                    let (path, wrote) = if let Some(t) = input.get("transcript") {
                        let p = scratch_path("scan");
                        std::fs::write(&p, s(t).as_bytes()).unwrap();
                        (p, true)
                    } else if let Some(b) = input.get("transcript_b64") {
                        let p = scratch_path("scan");
                        std::fs::write(&p, b64_decode(s(b))).unwrap();
                        (p, true)
                    } else {
                        (scratch_path("missing"), false)
                    };
                    let got = scan_tail(&path);
                    if wrote {
                        let _ = std::fs::remove_file(&path);
                    }
                    let exp = case["expected"].as_array().unwrap();
                    let got = [got.0, got.1, got.2, got.3, got.4];
                    for (i, g) in got.iter().enumerate() {
                        assert_eq!(g.as_str(), s(&exp[i]), "case {name} field {i}");
                    }
                    covered += 1;
                }
                other => panic!("unhandled fixture fn {other:?} in case {name}"),
            }
        }
        assert_eq!(covered, cases.len(), "every fixture case must be exercised");
    }

    #[test]
    fn claude_meta_custom_title_is_sticky() {
        let after_rename = resolve_claude_meta(
            (
                "Manual".into(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            &ClaudeMetaState::default(),
        );
        assert_eq!(after_rename.title, "Manual");
        assert!(after_rename.is_custom);

        // A later tick sees only an ai-title; the manual name must survive.
        let next = resolve_claude_meta(
            (
                String::new(),
                "Auto".into(),
                String::new(),
                String::new(),
                String::new(),
            ),
            &after_rename,
        );
        assert_eq!(next.title, "Manual");
        assert!(next.is_custom);
    }

    #[test]
    fn claude_meta_fields_do_not_regress_on_empty_scan() {
        let prev = ClaudeMetaState {
            title: "T".into(),
            is_custom: false,
            agent: "alice".into(),
            team: "red".into(),
            color: "cyan".into(),
        };
        let got = resolve_claude_meta(
            (
                String::new(),
                String::new(),
                String::new(),
                String::new(),
                String::new(),
            ),
            &prev,
        );
        assert_eq!(got, prev);
    }

    #[test]
    fn claude_meta_ai_title_when_not_custom() {
        let got = resolve_claude_meta(
            (
                String::new(),
                "Auto".into(),
                String::new(),
                String::new(),
                String::new(),
            ),
            &ClaudeMetaState::default(),
        );
        assert_eq!(got.title, "Auto");
        assert!(!got.is_custom);
    }

    #[test]
    fn copilot_title_name_then_summary_first_line() {
        assert_eq!(copilot_title_from_text("name: foo\nsummary: bar\n"), "foo");
        assert_eq!(copilot_title_from_text("summary: only\n"), "only");
        assert_eq!(
            copilot_title_from_text("name: |\n\n  real title\n"),
            "real title"
        );
        assert_eq!(copilot_title_from_text(""), "");
    }
}
