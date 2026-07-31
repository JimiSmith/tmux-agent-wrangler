//! The agent lifecycle hook client: reads one hook payload on stdin, resolves
//! the reporting tmux server and pane from the environment, and sends a single
//! `HookEvent` over a unix socket.
//!
//! The whole path is fire-and-forget and best-effort: an unreachable socket
//! triggers a background spawn and the send is retried, but no failure here is
//! ever propagated to the agent whose hook invoked it. The only non-zero exit is
//! a missing agent argument (a misconfiguration), which cannot produce a
//! meaningful event.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::daemon::assoc::{parse_field_int, ppid_map};
use crate::paths::daemon_socket;
use crate::proto::{write_message, HookAction, HookMsg};

/// The most connect attempts made after a background spawn before giving up.
const CONNECT_RETRIES: u32 = 40;
/// The pause between connect attempts while waiting for a freshly spawned listener.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);
/// The most ancestry levels the agent-pid search climbs.
const AGENT_PID_MAX_HOPS: u32 = 8;

/// The fields extracted from a hook payload. `session_id` is already
/// slash-sanitized; `cwd` and `transcript` are empty when the payload omitted
/// them; `recoverable` is present only when the payload carried a JSON boolean.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Payload {
    pub session_id: String,
    pub cwd: String,
    pub transcript: String,
    pub recoverable: Option<bool>,
}

/// Extract the hook fields from the raw JSON body, accepting both the snake_case
/// (`session_id`, `transcript_path`) and camelCase (`sessionId`, `transcriptPath`)
/// key spellings, snake_case preferred. An empty-string value is skipped so the
/// alternate spelling can supply the field. `recoverable` becomes `Some` only for
/// a genuine JSON boolean. A body that is not a JSON object (or does not parse)
/// yields an all-empty payload; it never panics.
pub fn parse_payload(body: &str) -> Payload {
    let v: Value = serde_json::from_str(body).unwrap_or(Value::Null);

    let first_nonempty = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(s) = v.get(*k).and_then(Value::as_str) {
                if !s.is_empty() {
                    return s.to_string();
                }
            }
        }
        String::new()
    };

    let session_id = first_nonempty(&["session_id", "sessionId"]).replace('/', "_");
    let transcript = first_nonempty(&["transcript_path", "transcriptPath"]);
    let cwd = v
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let recoverable = v.get("recoverable").and_then(Value::as_bool);

    Payload {
        session_id,
        cwd,
        transcript,
        recoverable,
    }
}

/// Resolve the reported event name to its turn-state action.
/// `end`/`working`/`needsAttention` map directly; `error` resolves to `Working`
/// only for a Copilot session flagged recoverable, else `NeedsAttention`; every
/// other name (including `start`) is a (re)registering `Start`.
pub fn normalize_event(agent: &str, raw_event: &str, recoverable: Option<bool>) -> HookAction {
    match raw_event {
        "end" => HookAction::End,
        "working" => HookAction::Working,
        "needsAttention" => HookAction::NeedsAttention,
        "error" => {
            if agent == "copilot" && recoverable == Some(true) {
                HookAction::Working
            } else {
                HookAction::NeedsAttention
            }
        }
        _ => HookAction::Start,
    }
}

/// The tmux server socket path from `$TMUX` (its first comma-separated field),
/// or `None` outside tmux or when the field is empty.
fn server_from_env() -> Option<String> {
    let tmux = std::env::var("TMUX").ok()?;
    let socket = tmux.split(',').next().unwrap_or("");
    if socket.is_empty() {
        None
    } else {
        Some(socket.to_string())
    }
}

/// The pane id from `$TMUX_PANE`, or `None` outside a pane or when it is empty.
fn pane_from_env() -> Option<String> {
    match std::env::var("TMUX_PANE") {
        Ok(p) if !p.is_empty() => Some(p),
        _ => None,
    }
}

/// Whether `command` names `agent`: a case-insensitive substring test, mirroring
/// how the agent name is sought within a process's command line.
fn command_names_agent(command: &str, agent: &str) -> bool {
    command.to_lowercase().contains(&agent.to_lowercase())
}

/// Climb the pid -> ppid chain from `start`, up to `max_hops` levels, returning
/// the first ancestor whose command names `agent`.
///
/// A level whose command contains `skip` (a case-sensitive substring identifying
/// this hook's own invocation) is stepped over while the climb continues, so a
/// hook command line that carries the agent name as an argument is never mistaken
/// for the agent. Climbing stops at a missing parent or a parent pid of 1 or below
/// (a missing entry resolves to 0, which stops).
fn walk_ancestry(
    start: u32,
    agent: &str,
    skip: &str,
    parents: &HashMap<u32, u32>,
    commands: &HashMap<u32, String>,
    max_hops: u32,
) -> Option<u32> {
    let mut pid = start;
    for _ in 0..max_hops {
        let parent = parents.get(&pid).copied().unwrap_or(0);
        if parent <= 1 {
            return None;
        }
        pid = parent;
        let command = commands.get(&pid).map(String::as_str).unwrap_or("");
        if !skip.is_empty() && command.contains(skip) {
            continue;
        }
        if command_names_agent(command, agent) {
            return Some(pid);
        }
    }
    None
}

/// Parse `ps -e -o pid= -o args=` stdout into a pid -> command map. Each line
/// begins with the pid (leading padding tolerated), then a whitespace run, then
/// the command line (which may itself hold spaces) as the remainder. A line
/// contributes only when its first token is ASCII digits; the remainder is kept
/// as the command, left-trimmed.
fn parse_command_stdout(out: &str) -> HashMap<u32, String> {
    let mut commands = HashMap::new();
    for line in out.lines() {
        let trimmed = line.trim_start();
        let Some(sep) = trimmed.find(char::is_whitespace) else {
            continue;
        };
        let (pid_str, rest) = trimmed.split_at(sep);
        if let Some(pid) = parse_field_int(pid_str) {
            commands.insert(pid, rest.trim_start().to_string());
        }
    }
    commands
}

/// Snapshot every process's command line as a pid -> command map by running `ps`
/// once. Empty when the subprocess cannot be spawned; a non-zero exit still
/// parses whatever stdout was produced.
///
/// Side effect: spawns the read-only `ps` subprocess. No filesystem writes.
fn command_map() -> HashMap<u32, String> {
    match Command::new("ps")
        .args(["-e", "-o", "pid=", "-o", "args="])
        .output()
    {
        Ok(o) => parse_command_stdout(&String::from_utf8_lossy(&o.stdout)),
        Err(_) => HashMap::new(),
    }
}

/// Resolve the agent process's pid by climbing this process's ancestry: the hook
/// runs as a descendant of the agent, so an ancestor whose command names the
/// agent is that agent, and finding it lets the record be pruned if the agent
/// dies without an end event. Returns `None` when no such ancestor is found
/// within the hop bound. Levels whose command is this executable's own invocation
/// are stepped over, since that command line carries the agent name as an
/// argument; when the executable path cannot be determined, `"wrangler"` stands
/// in as the skip marker.
///
/// Side effect: spawns the read-only `ps` subprocesses backing the ancestry and
/// command snapshots.
pub fn find_agent_pid(agent: &str) -> Option<u32> {
    let skip = std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "wrangler".to_string());
    walk_ancestry(
        std::process::id(),
        agent,
        &skip,
        &ppid_map(),
        &command_map(),
        AGENT_PID_MAX_HOPS,
    )
}

/// Wall-clock nanoseconds since the epoch, identifying this event. `0` if the
/// clock reads before the epoch. Held as `i128` so the full range is comparable.
fn event_token() -> i128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i128)
        .unwrap_or(0)
}

/// Connect to the socket and write one framed message. `Err` when the socket is
/// unreachable (nothing is listening) or the write fails.
fn try_send(msg: &HookMsg) -> std::io::Result<()> {
    let mut stream = UnixStream::connect(daemon_socket())?;
    write_message(&mut stream, msg)
}

/// Spawn this executable's `daemon` subcommand, fully detached so it outlives the
/// tmux server that triggered this hook, and not waited on. Any spawn failure is
/// swallowed.
fn spawn_daemon() {
    if let Ok(exe) = std::env::current_exe() {
        let mut command = Command::new(exe);
        command.arg("daemon");
        crate::platform::spawn_detached(command);
    }
}

/// Deliver the event, best-effort: try once, and if the socket is not reachable,
/// spawn the background process and retry the connect a bounded number of times
/// before giving up.
fn deliver(msg: &HookMsg) {
    if try_send(msg).is_ok() {
        return;
    }
    spawn_daemon();
    for _ in 0..CONNECT_RETRIES {
        thread::sleep(CONNECT_RETRY_INTERVAL);
        if try_send(msg).is_ok() {
            return;
        }
    }
}

/// Read all of standard input into a string, or an empty string on any read error.
fn read_stdin() -> String {
    let mut buf = String::new();
    let _ = std::io::stdin().read_to_string(&mut buf);
    buf
}

/// The hook entry point. `args` are the arguments after the subcommand name:
/// `args[0]` is the required agent name, `args[1]` the optional event (default
/// `start`). Reads the payload on stdin, builds one event, and delivers it over
/// the socket. Exits 0 on every normal path; exits 2 only when the agent name is
/// absent.
pub fn run(args: &[String]) -> ExitCode {
    let agent = match args.first() {
        Some(a) => a.as_str(),
        None => {
            eprintln!("wrangler hook: agent name required");
            return ExitCode::from(2);
        }
    };
    let raw_event = args.get(1).map(String::as_str).unwrap_or("start");

    let payload = parse_payload(&read_stdin());
    // An event that names no session cannot be placed; drop it silently.
    if payload.session_id.is_empty() {
        return ExitCode::SUCCESS;
    }

    let event = normalize_event(agent, raw_event, payload.recoverable);

    let msg = HookMsg::HookEvent {
        server: server_from_env().map(crate::model::ServerKey),
        pane: pane_from_env().map(crate::model::PaneId),
        agent: agent.to_string(),
        event,
        session_id: payload.session_id,
        cwd: payload.cwd,
        transcript: payload.transcript,
        recoverable: payload.recoverable,
        pid: find_agent_pid(agent),
        token: event_token(),
    };

    deliver(&msg);
    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_payload_claude_keys() {
        let body =
            r#"{"session_id":"abc/def","cwd":"/home/u/repo","transcript_path":"/t/x.jsonl"}"#;
        let p = parse_payload(body);
        assert_eq!(p.session_id, "abc_def");
        assert_eq!(p.cwd, "/home/u/repo");
        assert_eq!(p.transcript, "/t/x.jsonl");
        assert_eq!(p.recoverable, None);
    }

    #[test]
    fn parse_payload_copilot_keys() {
        let body = r#"{"sessionId":"xyz","transcriptPath":"/tp.jsonl","recoverable":true}"#;
        let p = parse_payload(body);
        assert_eq!(p.session_id, "xyz");
        assert_eq!(p.transcript, "/tp.jsonl");
        assert_eq!(p.recoverable, Some(true));
    }

    #[test]
    fn parse_payload_recoverable_false() {
        let p = parse_payload(r#"{"sessionId":"s","recoverable":false}"#);
        assert_eq!(p.recoverable, Some(false));
    }

    #[test]
    fn parse_payload_recoverable_non_boolean_is_none() {
        // A string "true" is not a JSON boolean, so it does not set the flag.
        let p = parse_payload(r#"{"sessionId":"s","recoverable":"true"}"#);
        assert_eq!(p.recoverable, None);
    }

    #[test]
    fn parse_payload_snake_case_wins_over_camel() {
        let body = r#"{"session_id":"snake","sessionId":"camel","transcript_path":"/a","transcriptPath":"/b"}"#;
        let p = parse_payload(body);
        assert_eq!(p.session_id, "snake");
        assert_eq!(p.transcript, "/a");
    }

    #[test]
    fn parse_payload_empty_value_falls_through_to_alternate() {
        let body = r#"{"session_id":"","sessionId":"camel"}"#;
        let p = parse_payload(body);
        assert_eq!(p.session_id, "camel");
    }

    #[test]
    fn parse_payload_slash_sanitized_globally() {
        let p = parse_payload(r#"{"session_id":"a/b/c"}"#);
        assert_eq!(p.session_id, "a_b_c");
    }

    #[test]
    fn parse_payload_invalid_json_is_empty() {
        let p = parse_payload("not json at all");
        assert_eq!(
            p,
            Payload {
                session_id: String::new(),
                cwd: String::new(),
                transcript: String::new(),
                recoverable: None,
            }
        );
    }

    #[test]
    fn parse_payload_non_object_json_is_empty() {
        let p = parse_payload("[1,2,3]");
        assert!(p.session_id.is_empty() && p.recoverable.is_none());
    }

    #[test]
    fn normalize_direct_events_pass_through() {
        assert_eq!(normalize_event("claude", "start", None), HookAction::Start);
        assert_eq!(normalize_event("claude", "end", None), HookAction::End);
        assert_eq!(
            normalize_event("claude", "working", None),
            HookAction::Working
        );
        assert_eq!(
            normalize_event("claude", "needsAttention", None),
            HookAction::NeedsAttention
        );
    }

    #[test]
    fn normalize_unknown_event_registers() {
        assert_eq!(normalize_event("claude", "wat", None), HookAction::Start);
        assert_eq!(
            normalize_event("copilot", "", Some(true)),
            HookAction::Start
        );
    }

    #[test]
    fn normalize_copilot_recoverable_error_is_working() {
        assert_eq!(
            normalize_event("copilot", "error", Some(true)),
            HookAction::Working
        );
    }

    #[test]
    fn normalize_copilot_nonrecoverable_error_is_attention() {
        assert_eq!(
            normalize_event("copilot", "error", Some(false)),
            HookAction::NeedsAttention
        );
        assert_eq!(
            normalize_event("copilot", "error", None),
            HookAction::NeedsAttention
        );
    }

    #[test]
    fn normalize_non_copilot_error_is_attention_even_if_recoverable() {
        assert_eq!(
            normalize_event("claude", "error", Some(true)),
            HookAction::NeedsAttention
        );
    }

    /// Build the pid -> ppid and pid -> command maps from `(pid, ppid, command)`
    /// triples for the ancestry-walk tests.
    fn build_maps(rows: &[(u32, u32, &str)]) -> (HashMap<u32, u32>, HashMap<u32, String>) {
        let mut parents = HashMap::new();
        let mut commands = HashMap::new();
        for &(pid, ppid, cmd) in rows {
            parents.insert(pid, ppid);
            commands.insert(pid, cmd.to_string());
        }
        (parents, commands)
    }

    #[test]
    fn command_names_agent_is_case_insensitive() {
        assert!(command_names_agent(
            "node /opt/Claude-Code/cli.js",
            "claude"
        ));
        assert!(command_names_agent("COPILOT --serve", "copilot"));
        assert!(!command_names_agent("node /opt/other/cli.js", "claude"));
    }

    #[test]
    fn walk_returns_first_agent_ancestor() {
        // 100 (hook) -> 90 (claude) -> 1.
        let (parents, commands) = build_maps(&[
            (100, 90, "/abs/wrangler hook claude working"),
            (90, 1, "claude --resume"),
        ]);
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 8),
            Some(90)
        );
    }

    #[test]
    fn walk_skips_hook_wrapper_carrying_the_agent_name() {
        // The wrapper's command line carries the agent name as an argument, so
        // matching it would wrongly return the wrapper's pid; the skip marker
        // steps over it and the climb reaches the real agent process.
        let (parents, commands) = build_maps(&[
            (100, 90, "/abs/wrangler hook claude working"),
            (90, 80, "sh -c /abs/wrangler hook claude working"),
            (80, 1, "claude"),
        ]);
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 8),
            Some(80)
        );
    }

    #[test]
    fn walk_stops_at_pid_one() {
        // The only agent-named ancestor sits above pid 1, unreachable.
        let (parents, commands) = build_maps(&[(100, 1, "sh")]);
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 8),
            None
        );
    }

    #[test]
    fn walk_respects_the_hop_bound() {
        // A chain of non-matching ancestors longer than the bound: the agent
        // process is beyond reach.
        let (parents, commands) = build_maps(&[
            (100, 99, "sh"),
            (99, 98, "sh"),
            (98, 97, "sh"),
            (97, 1, "claude"),
        ]);
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 2),
            None
        );
        // With enough hops the same chain resolves.
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 8),
            Some(97)
        );
    }

    #[test]
    fn walk_stops_on_a_missing_parent() {
        // 100's parent is not in the map, resolving to 0, which stops the climb.
        let (parents, commands) = build_maps(&[(100, 0, "claude")]);
        assert_eq!(
            walk_ancestry(100, "claude", "/abs/wrangler", &parents, &commands, 8),
            None
        );
    }

    #[test]
    fn parse_command_stdout_splits_pid_from_command() {
        let out = "\
  100 /abs/wrangler hook claude working
   90 claude --resume
garbage-without-a-space
";
        let commands = parse_command_stdout(out);
        assert_eq!(
            commands.get(&100).map(String::as_str),
            Some("/abs/wrangler hook claude working")
        );
        assert_eq!(
            commands.get(&90).map(String::as_str),
            Some("claude --resume")
        );
        assert_eq!(commands.len(), 2);
    }
}
