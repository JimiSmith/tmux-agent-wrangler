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
    fn process_under_bound_stops_a_cycle() {
        // Without the 4096 cap this 2-cycle would loop forever; the ancestor is
        // never on the cycle, so it must terminate false.
        let mut parents = HashMap::new();
        parents.insert(10u32, 20u32);
        parents.insert(20u32, 10u32);
        assert!(!process_under(10, 999, &parents));
    }
}
