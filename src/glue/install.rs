//! Installing (and removing) the agent lifecycle hooks that call `wrangler hook`.
//!
//! Reads the embedded manifest (which agent event maps to which hook action) and
//! writes the hooks into each agent's own config, wiring the absolute path to
//! this executable so it works from any install location. Two config shapes,
//! chosen by each agent's `format`:
//!
//! - `claude`: a shared `settings.json` holding the user's other keys too. Only
//!   wrangler's own hook groups (identified by the hook command) are replaced;
//!   everything else, the key order, the file mode, and a `.wrangler.bak` backup
//!   are preserved.
//! - `copilot`: a dedicated file written wholesale.
//!
//! Idempotent: re-running install reproduces identical output.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use serde_json::{json, Map, Value};

/// The per-agent event -> action manifest, embedded so the installed binary
/// carries it.
const MANIFEST_JSON: &str = include_str!("../../scripts/hooks-manifest.json");

/// Quote a string for a POSIX shell the way Python's `shlex.quote` does: return
/// it unchanged when every character is shell-safe, else wrap it in single quotes
/// with embedded quotes escaped as `'"'"'`. An empty string becomes `''`.
pub fn shlex_quote(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s.bytes().all(|b| {
        b.is_ascii_alphanumeric()
            || matches!(
                b,
                b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
            )
    });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\"'\"'"))
    }
}

/// The shell command a hook runs: this executable's `hook <agent> <action>`, with
/// the executable path shell-quoted.
fn hook_command(exe: &str, agent: &str, action: &str) -> String {
    format!("{} hook {agent} {action}", shlex_quote(exe))
}

/// Whether a hook command belongs to this agent's wrangler hooks, so it is
/// replaced rather than duplicated. Matches the hook mechanism (this binary's
/// `hook` subcommand, or the legacy `agent-hook.sh` script) plus the agent name
/// as a token, so an entry written by an older install is recognized and upgraded.
fn is_wrangler_command(cmd: &str, agent: &str) -> bool {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let is_hook =
        cmd.contains("agent-hook.sh") || (cmd.contains("wrangler") && tokens.contains(&"hook"));
    is_hook && tokens.contains(&agent)
}

/// Escape every non-ASCII scalar as `\uXXXX` (surrogate pairs above the BMP),
/// matching Python `json.dumps`'s default `ensure_ascii`. Only string contents in
/// the pretty JSON hold non-ASCII, and escaping them is an equivalent encoding.
fn ensure_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii() {
            out.push(c);
            continue;
        }
        let cp = c as u32;
        if cp <= 0xFFFF {
            out.push_str(&format!("\\u{cp:04x}"));
        } else {
            let v = cp - 0x10000;
            out.push_str(&format!(
                "\\u{:04x}\\u{:04x}",
                0xD800 + (v >> 10),
                0xDC00 + (v & 0x3FF)
            ));
        }
    }
    out
}

/// Serialize as 2-space-indented JSON with ASCII escaping and one trailing
/// newline, matching the installer's on-disk format.
fn dumps(value: &Value) -> String {
    let mut text = ensure_ascii(&serde_json::to_string_pretty(value).unwrap_or_default());
    text.push('\n');
    text
}

/// Normalize a manifest event value into `(matcher, actions)` groups: a list of
/// action strings is one matcher-less group; a list of `{matcher, actions}`
/// objects is one group each.
fn groups(value: &Value) -> Vec<(Option<String>, Vec<String>)> {
    let arr = match value.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    if arr.first().map(Value::is_object).unwrap_or(false) {
        return arr
            .iter()
            .map(|g| {
                let matcher = g.get("matcher").and_then(Value::as_str).map(str::to_string);
                let actions = str_array(g.get("actions"));
                (matcher, actions)
            })
            .collect();
    }
    vec![(None, str_array(Some(value)))]
}

/// The string elements of an optional JSON array (non-strings dropped).
fn str_array(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// Expand a leading `~` to `$HOME`.
fn expand_user(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

/// The permission bits of an existing file, or `None` when it does not exist.
fn file_mode(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777)
}

/// The `<path>.wrangler.bak` sibling.
fn backup_path(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".wrangler.bak");
    PathBuf::from(s)
}

/// The directory `path` is written in: its parent, or the working directory when
/// it has none.
fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

/// The sibling temp file [`atomic_write`] renames over `path`.
///
/// Named after the file it becomes as well as the writing process: two targets
/// sharing a directory are written by the same pid whenever one process installs
/// both, so the pid alone does not make the name unique and the two writes would
/// otherwise trample each other's temp file.
fn temp_path(path: &Path) -> PathBuf {
    let mut name = std::ffi::OsString::from(format!(".wrangler-hooks-tmp-{}-", std::process::id()));
    name.push(path.file_name().unwrap_or_default());
    parent_dir(path).join(name)
}

/// Replace `path`'s contents atomically, creating parents, with permission bits
/// `mode`: write a sibling temp file, set its mode, then rename over `path`.
fn atomic_write(path: &Path, text: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::create_dir_all(parent_dir(path))?;
    let tmp = temp_path(path);
    let result = (|| {
        fs::write(&tmp, text)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(mode))?;
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// Merge (or strip) wrangler's hook groups in a shared `settings.json`.
fn install_claude(agent: &str, spec: &Value, exe: &str, uninstall: bool) -> Result<String, String> {
    let path = expand_user(spec["target"].as_str().unwrap_or_default());

    let mut data: Value = match fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .map_err(|e| format!("{}: invalid JSON: {e}", path.display()))?,
        _ => json!({}),
    };
    let obj = data.as_object_mut().ok_or_else(|| {
        format!(
            "{}: expected a JSON object at the top level",
            path.display()
        )
    })?;

    let mut hooks: Map<String, Value> = obj
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();

    let events = spec["events"].as_object().cloned().unwrap_or_default();
    for (event, value) in &events {
        let mut kept: Vec<Value> = hooks
            .get(event)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|group| !group_is_wrangler(group, agent))
            .collect();

        if !uninstall {
            for (matcher, actions) in groups(value) {
                kept.push(claude_group(matcher, &actions, exe, agent));
            }
        }

        if kept.is_empty() {
            hooks.shift_remove(event);
        } else {
            hooks.insert(event.clone(), Value::Array(kept));
        }
    }

    if hooks.is_empty() {
        obj.shift_remove("hooks");
    } else {
        obj.insert("hooks".to_string(), Value::Object(hooks));
    }

    let (mode, existed) = match file_mode(&path) {
        Some(m) => (m, true),
        None => (0o600, false), // settings.json can hold secrets; default private
    };
    if existed {
        let _ = fs::copy(&path, backup_path(&path));
    }
    atomic_write(&path, &dumps(&data), mode).map_err(|e| format!("{}: {e}", path.display()))?;

    let verb = if uninstall {
        "Uninstalled from"
    } else {
        "Installed into"
    };
    Ok(format!("{agent}: {verb} {}", path.display()))
}

/// Whether a claude hook group contains a wrangler command for `agent`.
fn group_is_wrangler(group: &Value, agent: &str) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .map(|c| is_wrangler_command(c, agent))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// One claude hook group: `{matcher?, hooks: [{type, command}, ...]}` (matcher
/// first when present).
fn claude_group(matcher: Option<String>, actions: &[String], exe: &str, agent: &str) -> Value {
    let hooks: Vec<Value> = actions
        .iter()
        .map(|action| {
            let mut h = Map::new();
            h.insert("type".to_string(), json!("command"));
            h.insert(
                "command".to_string(),
                json!(hook_command(exe, agent, action)),
            );
            Value::Object(h)
        })
        .collect();
    let mut group = Map::new();
    if let Some(m) = matcher {
        group.insert("matcher".to_string(), json!(m));
    }
    group.insert("hooks".to_string(), Value::Array(hooks));
    Value::Object(group)
}

/// Write (or delete) the dedicated `wrangler.json` file we own.
fn install_copilot(
    agent: &str,
    spec: &Value,
    exe: &str,
    uninstall: bool,
) -> Result<String, String> {
    let path = expand_user(spec["target"].as_str().unwrap_or_default());

    if uninstall {
        return match fs::remove_file(&path) {
            Ok(()) => Ok(format!("{agent}: Removed {}", path.display())),
            Err(_) => Ok(format!("{agent}: Nothing to remove at {}", path.display())),
        };
    }

    let events = spec["events"].as_object().cloned().unwrap_or_default();
    let mut hooks = Map::new();
    for (event, value) in &events {
        let mut entries = Vec::new();
        for (matcher, actions) in groups(value) {
            for action in &actions {
                let mut h = Map::new();
                h.insert("type".to_string(), json!("command"));
                if let Some(m) = &matcher {
                    h.insert("matcher".to_string(), json!(m));
                }
                h.insert("bash".to_string(), json!(hook_command(exe, agent, action)));
                entries.push(Value::Object(h));
            }
        }
        hooks.insert(event.clone(), Value::Array(entries));
    }

    let mut doc = Map::new();
    doc.insert("version".to_string(), json!(1));
    doc.insert("hooks".to_string(), Value::Object(hooks));

    let mode = file_mode(&path).unwrap_or(0o644);
    atomic_write(&path, &dumps(&Value::Object(doc)), mode)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(format!("{agent}: Installed into {}", path.display()))
}

/// Install (or uninstall) the hooks for one agent per its manifest format.
fn install_agent(agent: &str, spec: &Value, exe: &str, uninstall: bool) -> Result<String, String> {
    match spec.get("format").and_then(Value::as_str) {
        Some("claude") => install_claude(agent, spec, exe, uninstall),
        Some("copilot") => install_copilot(agent, spec, exe, uninstall),
        other => Err(format!("{agent}: unknown format {other:?}")),
    }
}

/// This executable's path for the hook command, or `wrangler` if it cannot be
/// resolved.
fn exe_path() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "wrangler".to_string())
}

const USAGE: &str = "usage: wrangler install-hooks [claude|copilot|all] [--uninstall]";

/// The `install-hooks` subcommand: `[claude|copilot|all] [--uninstall]`
/// (default: all, install).
pub fn run(args: &[String]) -> ExitCode {
    let mut selector = "all".to_string();
    let mut uninstall = false;
    for arg in args {
        match arg.as_str() {
            "--uninstall" => uninstall = true,
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "all" | "claude" | "copilot" => selector = arg.clone(),
            other => {
                eprintln!("wrangler install-hooks: unknown argument '{other}'\n{USAGE}");
                return ExitCode::from(2);
            }
        }
    }

    let manifest: Value =
        serde_json::from_str(MANIFEST_JSON).expect("embedded manifest is valid JSON");
    let exe = exe_path();

    let agents: Vec<String> = if selector == "all" {
        manifest
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default()
    } else {
        vec![selector.clone()]
    };

    let mut failed = false;
    for agent in agents {
        match manifest.get(&agent) {
            Some(spec) => match install_agent(&agent, spec, &exe, uninstall) {
                Ok(msg) => println!("{msg}"),
                Err(msg) => {
                    eprintln!("{msg}");
                    failed = true;
                }
            },
            None => eprintln!("{agent}: not in manifest, skipping"),
        }
    }

    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shlex_quote_matches_python() {
        assert_eq!(shlex_quote("/usr/bin/wrangler"), "/usr/bin/wrangler");
        assert_eq!(shlex_quote(""), "''");
        assert_eq!(shlex_quote("/my dir/wrangler"), "'/my dir/wrangler'");
        assert_eq!(shlex_quote("a'b"), "'a'\"'\"'b'");
        // Every shell-safe character stays unquoted.
        assert_eq!(shlex_quote("A9_@%+=:,./-"), "A9_@%+=:,./-");
    }

    #[test]
    fn ensure_ascii_escapes_non_ascii() {
        assert_eq!(ensure_ascii("caf\u{e9}"), "caf\\u00e9");
        // Above the BMP -> a surrogate pair, matching json.dumps.
        assert_eq!(ensure_ascii("\u{1f600}"), "\\ud83d\\ude00");
        assert_eq!(ensure_ascii("plain"), "plain");
    }

    #[test]
    fn hook_command_quotes_and_orders() {
        assert_eq!(
            hook_command("/opt/wrangler", "claude", "working"),
            "/opt/wrangler hook claude working"
        );
    }

    #[test]
    fn is_wrangler_command_recognizes_forms() {
        assert!(is_wrangler_command(
            "/opt/wrangler hook claude working",
            "claude"
        ));
        // A legacy agent-hook.sh entry is still recognized for upgrade.
        assert!(is_wrangler_command(
            "/x/scripts/agent-hook.sh claude start",
            "claude"
        ));
        // A different agent, or an unrelated command, does not match.
        assert!(!is_wrangler_command(
            "/opt/wrangler hook claude working",
            "copilot"
        ));
        assert!(!is_wrangler_command("my-linter --fix claude/", "claude"));
    }

    #[test]
    fn groups_normalizes_both_forms() {
        let plain = json!(["working", "start"]);
        assert_eq!(
            groups(&plain),
            vec![(None, vec!["working".into(), "start".into()])]
        );

        let matched = json!([{"matcher": "A|B", "actions": ["needsAttention"]}]);
        assert_eq!(
            groups(&matched),
            vec![(Some("A|B".to_string()), vec!["needsAttention".to_string()])]
        );
    }

    fn claude_spec(target: &str) -> Value {
        json!({
            "target": target,
            "format": "claude",
            "events": {
                "SessionStart": ["start"],
                "PreToolUse": [{"matcher": "AskUserQuestion|ExitPlanMode", "actions": ["needsAttention"]}],
            }
        })
    }

    fn tmp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wrangler-install-{}-{}", std::process::id(), name))
    }

    #[test]
    fn two_targets_in_one_directory_get_different_temp_files() {
        // The temp file is what a concurrent write to a sibling target would
        // trample, taking that write's content and mode with it.
        let dir = std::env::temp_dir();
        assert_ne!(
            temp_path(&dir.join("settings.json")),
            temp_path(&dir.join("wrangler.json"))
        );
        // And it stays in the target's own directory, so the rename is a rename.
        assert_eq!(temp_path(&dir.join("settings.json")).parent(), Some(&*dir));
    }

    #[test]
    fn claude_install_is_idempotent_and_preserves_other_keys() {
        let path = tmp_path("settings.json");
        let _ = fs::remove_file(&path);
        // A pre-existing file with an unrelated key and an unrelated hook group.
        fs::write(
            &path,
            r#"{"theme":"dark","hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"other-tool"}]}]}}"#,
        )
        .unwrap();
        let spec = claude_spec(path.to_str().unwrap());

        install_claude("claude", &spec, "/opt/wrangler", false).unwrap();
        let first = fs::read_to_string(&path).unwrap();
        // The user's own key and their unrelated SessionStart hook survive.
        assert!(first.contains("\"theme\": \"dark\""));
        assert!(first.contains("other-tool"));
        // Wrangler's hooks landed.
        assert!(first.contains("/opt/wrangler hook claude start"));
        assert!(first.contains("AskUserQuestion|ExitPlanMode"));
        // The user's key comes before hooks (order preserved).
        assert!(first.find("\"theme\"").unwrap() < first.find("\"hooks\"").unwrap());

        // Re-installing reproduces byte-identical output.
        install_claude("claude", &spec, "/opt/wrangler", false).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), first);

        // Uninstall strips wrangler's groups but keeps the unrelated ones.
        install_claude("claude", &spec, "/opt/wrangler", true).unwrap();
        let after = fs::read_to_string(&path).unwrap();
        assert!(after.contains("other-tool"));
        assert!(!after.contains("/opt/wrangler hook claude"));
        assert!(after.contains("\"theme\": \"dark\""));

        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(backup_path(&path));
    }

    #[test]
    fn claude_install_creates_private_file_when_absent() {
        use std::os::unix::fs::PermissionsExt;
        let path = tmp_path("new-settings.json");
        let _ = fs::remove_file(&path);
        let spec = claude_spec(path.to_str().unwrap());

        install_claude("claude", &spec, "/opt/wrangler", false).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "a fresh settings.json is private");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn copilot_doc_has_version_and_flattened_hooks() {
        let path = tmp_path("wrangler.json");
        let _ = fs::remove_file(&path);
        let spec = json!({
            "target": path.to_str().unwrap(),
            "format": "copilot",
            "events": {
                "sessionStart": ["start"],
                "notification": [
                    {"matcher": "agent_idle", "actions": ["working"]},
                    {"matcher": "permission_prompt", "actions": ["needsAttention"]}
                ]
            }
        });

        install_copilot("copilot", &spec, "/opt/wrangler", false).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(doc["version"], json!(1));
        // The two-matcher notification event flattens to two entries with their
        // matchers and bash commands.
        let notif = doc["hooks"]["notification"].as_array().unwrap();
        assert_eq!(notif.len(), 2);
        assert_eq!(notif[0]["matcher"], json!("agent_idle"));
        assert_eq!(
            notif[0]["bash"],
            json!("/opt/wrangler hook copilot working")
        );
        assert_eq!(notif[1]["matcher"], json!("permission_prompt"));
        assert_eq!(
            notif[1]["bash"],
            json!("/opt/wrangler hook copilot needsAttention")
        );
        // version precedes hooks in the output.
        assert!(text.find("\"version\"").unwrap() < text.find("\"hooks\"").unwrap());

        // Uninstall removes the owned file.
        install_copilot("copilot", &spec, "/opt/wrangler", true).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn embedded_manifest_parses_and_covers_both_agents() {
        let manifest: Value = serde_json::from_str(MANIFEST_JSON).unwrap();
        assert_eq!(manifest["claude"]["format"], json!("claude"));
        assert_eq!(manifest["copilot"]["format"], json!("copilot"));
    }
}
