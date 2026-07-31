//! Wire protocol spoken over the daemon socket: the messages a client, a hook,
//! and a control command send inward, the render payload the daemon pushes back,
//! and the newline-delimited JSON framing that carries them.
//!
//! Every message is serialized as a single line of JSON terminated by one `\n`,
//! so a stream is a sequence of independent lines. [`write_message`] emits one
//! line; [`read_message`] consumes one line, yielding `None` at end of stream.
//! The enums are internally tagged (a `type`/`kind` discriminant field) so a
//! decoder can dispatch on the tag without positional ambiguity.

use std::io::{self, BufRead, Write};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::model::{PaneId, RowKey, RowModel, ServerKey, WindowId};

/// The turn-state action a hook reports, named verbatim as the agent's lifecycle
/// hook named it. One variant per reportable event.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HookAction {
    #[serde(rename = "start")]
    Start,
    #[serde(rename = "end")]
    End,
    #[serde(rename = "working")]
    Working,
    #[serde(rename = "needsAttention")]
    NeedsAttention,
    #[serde(rename = "error")]
    Error,
}

/// A user-interaction event a client reports. Every selection-changing event
/// names the absolute target row by its [`RowKey`], never a relative move: the
/// client already holds the full row list and resolves an up/down keypress or a
/// click to a key itself, so a single message fully determines the shared
/// selection and a dropped or reordered one cannot leave client and daemon
/// disagreeing about where the cursor is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputEvent {
    /// Set the shared selection to this row.
    Select { key: RowKey },
    /// Set the shared selection to this row and focus its target.
    Activate { key: RowKey },
    /// The client's sidebar pane was resized to this column width.
    Resize { cols: u16 },
    /// The client's pane gained terminal focus.
    FocusGained,
    /// The client's pane lost terminal focus.
    FocusLost,
}

/// A message a sidebar client sends inward. `Hello` opens the connection and
/// identifies which window's sidebar this is and on which tmux server; `Input`
/// forwards an interaction event; `Bye` closes the connection.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMsg {
    Hello {
        server: ServerKey,
        window: WindowId,
        pane: PaneId,
        cols: u16,
        rows: u16,
    },
    Input {
        event: InputEvent,
    },
    Bye,
}

/// A message the daemon pushes back to a sidebar client. `Render` carries the
/// per-window row model the client paints; `Width` carries the shared column
/// width another of the server's sidebars was resized to, for this client to
/// adopt.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMsg {
    Render(RowModel),
    Width { cols: u16 },
}

/// A message an agent lifecycle hook sends inward. `server` and `pane` are absent
/// for a pane-less (daemon-hosted) session. `token` is a monotonic-per-session
/// stamp identifying the event.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookMsg {
    HookEvent {
        server: Option<ServerKey>,
        pane: Option<PaneId>,
        agent: String,
        event: HookAction,
        session_id: String,
        cwd: String,
        transcript: String,
        recoverable: Option<bool>,
        /// The agent process id, or `None` when it could not be resolved.
        pid: Option<u32>,
        /// A monotonic-per-session stamp, carried as a decimal string on the
        /// wire so its full 128-bit width round-trips exactly.
        #[serde(with = "i128_str")]
        token: i128,
    },
}

/// Serialize an `i128` as its decimal string and parse it back, so the value
/// round-trips exactly as text on the wire.
mod i128_str {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(v: &i128, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i128, D::Error> {
        let text = String::deserialize(d)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A message a control command (bound by tmux) sends inward. Each targets the
/// requesting tmux server: `Toggle` carries a request to turn that server's
/// sidebars on or off; `Focus` carries a request to select the sidebar pane of
/// one of its windows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum CtlMsg {
    Toggle { server: ServerKey },
    Focus { server: ServerKey, window: WindowId },
}

/// Any message a connection may send inward, decoded without knowing the sender's
/// role up front. The three inner enums tag on disjoint `type` values (client:
/// `hello`/`input`/`bye`; hook: `hook_event`; ctl: `toggle`/`focus`), so an
/// untagged decode resolves each line to exactly one variant.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(untagged)]
pub enum Inbound {
    Client(ClientMsg),
    Hook(HookMsg),
    Ctl(CtlMsg),
}

/// Serialize one message as a single JSON line and write it, followed by exactly
/// one `\n`, to `w`. The whole line is emitted in one `write_all` so a message is
/// never interleaved with another writer's line.
pub fn write_message<W: Write, M: Serialize>(w: &mut W, msg: &M) -> io::Result<()> {
    let mut line =
        serde_json::to_vec(msg).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push(b'\n');
    w.write_all(&line)
}

/// Read one newline-terminated JSON line from `r` and decode it. Returns
/// `Ok(None)` at end of stream (a zero-length read). A line that is not valid
/// JSON for `M` is surfaced as an `InvalidData` error.
pub fn read_message<R: BufRead, M: DeserializeOwned>(r: &mut R) -> io::Result<Option<M>> {
    let mut line = String::new();
    if r.read_line(&mut line)? == 0 {
        return Ok(None);
    }
    let msg =
        serde_json::from_str(&line).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    Ok(Some(msg))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Indicator, NamedColor, ProgressState, Row, RowKey, RowKind, SessionKey};
    use std::io::{BufReader, Cursor};

    /// Write `msg`, read it back from the resulting bytes, and assert the decoded
    /// value equals the original. Also asserts the frame ends in exactly one
    /// newline and holds no interior newline (single-line framing).
    fn round_trip<M>(msg: &M)
    where
        M: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
    {
        let mut buf = Vec::new();
        write_message(&mut buf, msg).unwrap();
        assert_eq!(buf.last(), Some(&b'\n'), "frame must end in a newline");
        assert_eq!(
            buf.iter().filter(|&&b| b == b'\n').count(),
            1,
            "frame must be a single line"
        );

        let mut reader = BufReader::new(Cursor::new(buf));
        let got: M = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(&got, msg);
    }

    fn sample_row_model() -> RowModel {
        RowModel {
            rows: vec![
                Row {
                    text: "windows".into(),
                    kind: RowKind::Header,
                    key: None,
                    indicator: Indicator::None,
                },
                Row {
                    text: "1: main".into(),
                    kind: RowKind::Window {
                        active: true,
                        color: Some(NamedColor::Cyan),
                    },
                    key: Some(RowKey::Window {
                        window: WindowId("@1".into()),
                    }),
                    indicator: Indicator::Attention,
                },
                Row {
                    text: "claude · repo".into(),
                    kind: RowKind::Agent {
                        color: Some(NamedColor::Purple),
                        emphatic: true,
                    },
                    key: Some(RowKey::Agent {
                        session: SessionKey("claude-abc".into()),
                        pane: PaneId("%5".into()),
                    }),
                    indicator: Indicator::Progress {
                        pct: Some(42),
                        state: ProgressState::Normal,
                    },
                },
            ],
            selection: Some(RowKey::Pane {
                pane: PaneId("%5".into()),
            }),
            has_focus: true,
        }
    }

    #[test]
    fn client_hello_round_trips() {
        round_trip(&ClientMsg::Hello {
            server: ServerKey("/tmp/tmux-1000/default".into()),
            window: WindowId("@3".into()),
            pane: PaneId("%9".into()),
            cols: 30,
            rows: 50,
        });
    }

    #[test]
    fn client_input_variants_round_trip() {
        for event in [
            InputEvent::Select {
                key: RowKey::Window {
                    window: WindowId("@1".into()),
                },
            },
            InputEvent::Activate {
                key: RowKey::Agent {
                    session: SessionKey("claude-abc".into()),
                    pane: PaneId("%5".into()),
                },
            },
            InputEvent::Resize { cols: 24 },
            InputEvent::FocusGained,
            InputEvent::FocusLost,
        ] {
            round_trip(&ClientMsg::Input { event });
        }
    }

    #[test]
    fn client_bye_round_trips() {
        round_trip(&ClientMsg::Bye);
    }

    #[test]
    fn server_render_round_trips() {
        round_trip(&ServerMsg::Render(sample_row_model()));
    }

    #[test]
    fn server_width_round_trips() {
        round_trip(&ServerMsg::Width { cols: 28 });
    }

    /// A line written as each concrete inbound message decodes back to the
    /// matching `Inbound` variant, so the daemon can read any sender's line off
    /// one socket without knowing its role first.
    #[test]
    fn inbound_resolves_each_sender_role() {
        let client = ClientMsg::Hello {
            server: ServerKey("/s".into()),
            window: WindowId("@1".into()),
            pane: PaneId("%1".into()),
            cols: 30,
            rows: 40,
        };
        let hook = HookMsg::HookEvent {
            server: Some(ServerKey("/s".into())),
            pane: Some(PaneId("%1".into())),
            agent: "claude".into(),
            event: HookAction::Working,
            session_id: "abc".into(),
            cwd: "/c".into(),
            transcript: "/t".into(),
            recoverable: None,
            pid: Some(7),
            token: 42i128,
        };
        let ctl = CtlMsg::Toggle {
            server: ServerKey("/s".into()),
        };

        let mut buf = Vec::new();
        write_message(&mut buf, &client).unwrap();
        write_message(&mut buf, &hook).unwrap();
        write_message(&mut buf, &ctl).unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let a: Inbound = read_message(&mut reader).unwrap().unwrap();
        let b: Inbound = read_message(&mut reader).unwrap().unwrap();
        let c: Inbound = read_message(&mut reader).unwrap().unwrap();
        assert_eq!(a, Inbound::Client(client));
        assert_eq!(b, Inbound::Hook(hook));
        assert_eq!(c, Inbound::Ctl(ctl));
    }

    #[test]
    fn hook_event_variants_round_trip() {
        for (event, recoverable) in [
            (HookAction::Start, None),
            (HookAction::End, None),
            (HookAction::Working, Some(false)),
            (HookAction::NeedsAttention, None),
            (HookAction::Error, Some(true)),
        ] {
            round_trip(&HookMsg::HookEvent {
                server: Some(ServerKey("/tmp/tmux-1000/default".into())),
                pane: Some(PaneId("%5".into())),
                agent: "copilot".into(),
                event,
                session_id: "abc123".into(),
                cwd: "/home/u/repo".into(),
                transcript: "/home/u/.claude/x.jsonl".into(),
                recoverable,
                pid: Some(43_210),
                token: 1_700_000_000_123_456_789i128,
            });
        }
    }

    #[test]
    fn hook_event_paneless_round_trips() {
        round_trip(&HookMsg::HookEvent {
            server: None,
            pane: None,
            agent: "claude".into(),
            event: HookAction::NeedsAttention,
            session_id: "daemon-hosted".into(),
            cwd: String::new(),
            transcript: String::new(),
            recoverable: None,
            pid: None,
            token: -1i128,
        });
    }

    #[test]
    fn ctl_variants_round_trip() {
        round_trip(&CtlMsg::Toggle {
            server: ServerKey("/tmp/tmux-1000/default".into()),
        });
        round_trip(&CtlMsg::Focus {
            server: ServerKey("/tmp/tmux-1000/default".into()),
            window: WindowId("@2".into()),
        });
    }

    #[test]
    fn read_message_returns_none_at_eof() {
        let mut reader = BufReader::new(Cursor::new(Vec::new()));
        let got: Option<ClientMsg> = read_message(&mut reader).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn multiple_messages_stream_in_order() {
        let mut buf = Vec::new();
        write_message(&mut buf, &ClientMsg::Bye).unwrap();
        write_message(
            &mut buf,
            &ClientMsg::Input {
                event: InputEvent::Activate {
                    key: RowKey::Window {
                        window: WindowId("@1".into()),
                    },
                },
            },
        )
        .unwrap();

        let mut reader = BufReader::new(Cursor::new(buf));
        let first: ClientMsg = read_message(&mut reader).unwrap().unwrap();
        let second: ClientMsg = read_message(&mut reader).unwrap().unwrap();
        let end: Option<ClientMsg> = read_message(&mut reader).unwrap();
        assert_eq!(first, ClientMsg::Bye);
        assert_eq!(
            second,
            ClientMsg::Input {
                event: InputEvent::Activate {
                    key: RowKey::Window {
                        window: WindowId("@1".into()),
                    },
                },
            }
        );
        assert!(end.is_none());
    }

    #[test]
    fn invalid_json_is_invalid_data() {
        let mut reader = BufReader::new(Cursor::new(b"not json\n".to_vec()));
        let err = read_message::<_, ClientMsg>(&mut reader).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
