//! The agent lifecycle hook client: reads one hook payload on stdin, resolves
//! the reporting tmux server and pane from the environment, and sends a single
//! `HookEvent` over a unix socket.
//!
//! The whole path is fire-and-forget and best-effort: an unreachable socket
//! triggers a background spawn and the send is retried, but no failure here is
//! ever propagated to the agent whose hook invoked it. The only non-zero exit is
//! a missing agent argument (a misconfiguration), which cannot produce a
//! meaningful event.

use std::io::Read;
use std::os::unix::net::UnixStream;
use std::process::{Command, ExitCode, Stdio};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::paths::daemon_socket;
use crate::proto::{write_message, HookAction, HookMsg};

/// The most connect attempts made after a background spawn before giving up.
const CONNECT_RETRIES: u32 = 40;
/// The pause between connect attempts while waiting for a freshly spawned listener.
const CONNECT_RETRY_INTERVAL: Duration = Duration::from_millis(50);

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
    let cwd = v.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
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

/// Spawn a background process running this executable's `daemon` subcommand,
/// detached from this process's standard streams and not waited on. Any spawn
/// failure is swallowed.
fn spawn_daemon() {
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
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
        let body = r#"{"session_id":"abc/def","cwd":"/home/u/repo","transcript_path":"/t/x.jsonl"}"#;
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
        assert_eq!(normalize_event("copilot", "", Some(true)), HookAction::Start);
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
}
