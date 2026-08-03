//! Building the sidebar's structure, with each node's indicator resolved, from
//! the daemon's window/session model. Two groupings: one block whose
//! agent-hosting panes appear as their agents, or that block followed by one per
//! agent.
//!
//! This decides *what* each node is and which semantic [`Indicator`] it carries,
//! with no reference to the animation frame. The paint-time helpers
//! ([`spinner_frame`], [`Indicator::resolve`], [`fit`], [`strip_status_prefix`])
//! also live here.

use indexmap::IndexMap;

use crate::model::{
    Child, Indicator, PaneId, ProgressState, RowKey, RowTree, Section, Session, TurnStatus,
    ViewMode, Window, WindowNode,
};

/// Pane id -> `(pb_state, pb_progress)` from tmux's OSC 9;4 pane vars. A pane
/// absent from the map is treated as `("", None)` (inactive OSC), never a panic.
pub type PaneProgressMap = IndexMap<PaneId, (String, Option<u8>)>;

/// Pane id -> hook turn status string (`"working"`/`"attention"`/`""`) so a
/// window-tree pane running an agent mirrors that agent's glyph.
pub type PaneStatusMap = IndexMap<PaneId, String>;

// Frames of the "busy" spinner. Single-width braille so the pinned indicator
// stays one column wide across every frame.
const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

// pane_pb_state values that mean "no OSC progress to show": the empty string a
// tmux too old to know the var expands to, and `hidden` (tmux 3.7's name for a
// pane that never set progress or cleared it). Exactly these two, no more.
const INACTIVE_STATES: [&str; 2] = ["", "hidden"];

/// The paint-time color key an OSC progress state resolves to. `ProgressState`'s
/// `Plain` inherits the row's own color and yields `None`; the OSC states carry
/// an explicit color so their percentage/spinner stands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StateColor {
    Green,
    Yellow,
    Red,
}

impl ProgressState {
    /// The indicator's color key, or `None` to inherit the row's own color.
    /// Only the normal/paused/error states carry a color.
    pub fn color(self) -> Option<StateColor> {
        match self {
            ProgressState::Plain => None,
            ProgressState::Normal => Some(StateColor::Green),
            ProgressState::Paused => Some(StateColor::Yellow),
            ProgressState::Error => Some(StateColor::Red),
        }
    }
}

impl Indicator {
    /// Client/painting step: turn a semantic indicator into the `(text, color)`
    /// actually drawn for animation frame `frame`. `None` -> `("", None)`;
    /// `Attention` -> the static `●`; indeterminate `Progress` -> the spinner
    /// glyph; determinate `Progress` -> the percentage. The color rides on the
    /// progress state, not the frame.
    pub fn resolve(self, frame: usize) -> (String, Option<StateColor>) {
        match self {
            Indicator::None => (String::new(), None),
            Indicator::Attention => ("●".to_string(), None),
            Indicator::Progress { pct: None, state } => {
                (spinner_frame(frame).to_string(), state.color())
            }
            Indicator::Progress {
                pct: Some(p),
                state,
            } => (format!("{p}%"), state.color()),
        }
    }

    /// Whether this indicator animates (an indeterminate progress spinner). The
    /// main loop uses this over a glyph-membership test so an `Attention` `●` or
    /// a percentage is never misclassified as animating.
    pub fn indeterminate(self) -> bool {
        matches!(self, Indicator::Progress { pct: None, .. })
    }
}

/// The busy-spinner glyph for animation frame `frame` (any `usize`; wraps).
/// A client/painting concern: the frame counter advances on the wall clock
/// (~16fps) independent of the data poll.
pub fn spinner_frame(frame: usize) -> char {
    SPINNER[frame % SPINNER.len()]
}

/// Decide *which* semantic indicator a row carries, with no reference to the
/// animation frame (frame resolution is [`Indicator::resolve`]).
///
/// Precedence: OSC wins when enabled and the pane reports an active state (not
/// in [`INACTIVE_STATES`]); otherwise the hook glyph; otherwise nothing.
///
/// The subtle case: an OSC active state that is *not* `indeterminate` with no
/// percentage still keeps its colored state (`Normal`/`Paused`/`Error`), so a
/// state-colored spinner is drawn, whereas `indeterminate`/`working` collapse to
/// `Plain` and inherit the row's own color.
pub fn indicator_for(
    hook_status: &str,
    pb_state: &str,
    pb_progress: Option<u8>,
    hook_on: bool,
    osc_on: bool,
) -> Indicator {
    if osc_on && !INACTIVE_STATES.contains(&pb_state) {
        if pb_state == "indeterminate" {
            // No meaningful number: a busy spinner in the row's own color.
            return Indicator::Progress {
                pct: None,
                state: ProgressState::Plain,
            };
        }
        let state = match pb_state {
            "normal" => ProgressState::Normal,
            "paused" => ProgressState::Paused,
            "error" => ProgressState::Error,
            // Any other active state name has no color key: render in the
            // row's own color.
            _ => ProgressState::Plain,
        };
        // pct None here yields a state-colored spinner; pct Some (including 0)
        // yields the percentage. `0` must survive as "0%", never be dropped.
        return Indicator::Progress {
            pct: pb_progress,
            state,
        };
    }
    if hook_on {
        match hook_status {
            "working" => {
                return Indicator::Progress {
                    pct: None,
                    state: ProgressState::Plain,
                }
            }
            "attention" => return Indicator::Attention,
            _ => {}
        }
    }
    Indicator::None
}

/// The hook-status string a session's `TurnStatus` presents to [`indicator_for`]
/// and the pane-status mirror. `Idle` is the empty string (neither working nor
/// attention), the "no status" sentinel.
fn status_str(status: TurnStatus) -> &'static str {
    match status {
        TurnStatus::Working => "working",
        TurnStatus::Attention => "attention",
        TurnStatus::Idle => "",
    }
}

/// The heading naming the sectioned layout's window tree, alongside the agent
/// names that head the other blocks.
const WINDOWS_HEADING: &str = "windows";

/// The pane's own indicator: its mirrored agent hook glyph, and its OSC state.
fn pane_indicator(
    p: &crate::model::Pane,
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
) -> Indicator {
    indicator_for(
        pane_status.get(&p.id).map(String::as_str).unwrap_or(""),
        &p.pb_state,
        p.pb_progress,
        hook_on,
        osc_on,
    )
}

/// Build the sidebar's structure.
///
/// [`ViewMode::Unified`] is one unheaded block, an agent's pane contributing an
/// agent child in place of its pane child. [`ViewMode::Sections`] is the window
/// tree, then a block per distinct agent (sorted), so an agent's pane appears in
/// both. The two differ only in grouping.
///
/// Nodes carry a semantic [`Indicator`] and color *names*, and their literal
/// name as their only text. `pane_progress` supplies OSC 9;4 state;
/// `pane_status` mirrors an agent's hook glyph onto its window-tree pane.
pub fn build_tree(
    windows: &[Window],
    sessions: &[Session],
    pane_progress: &PaneProgressMap,
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
    view_mode: ViewMode,
) -> RowTree {
    match view_mode {
        ViewMode::Unified => unified_tree(windows, sessions, pane_status, hook_on, osc_on),
        ViewMode::Sections => sectioned_tree(
            windows,
            sessions,
            pane_progress,
            pane_status,
            hook_on,
            osc_on,
        ),
    }
}

/// One block holding every window, with each agent-hosting pane replaced by its
/// agent(s).
fn unified_tree(
    windows: &[Window],
    sessions: &[Session],
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
) -> RowTree {
    let nodes = windows
        .iter()
        .map(|w| {
            let mut children = Vec::new();
            for p in &w.panes {
                let hosted: Vec<&Session> = sessions.iter().filter(|s| s.pane == p.id).collect();
                if hosted.is_empty() {
                    children.push(pane_child(p, pane_status, hook_on, osc_on));
                    continue;
                }
                // A pane can host more than one session (one candidate's
                // recorded pane can be another's title-matched pane), and each
                // stays separately selectable, so every one gets a child of its
                // own rather than the pane child it replaces.
                for s in hosted {
                    children.push(Child::Agent {
                        id: agent_id(s),
                        index: p.index.clone(),
                        label: s.label.clone(),
                        active: p.active,
                        color: s.color,
                        // The agent's own turn state, and the OSC state of the
                        // pane it is displayed in.
                        indicator: indicator_for(
                            status_str(s.status),
                            &p.pb_state,
                            p.pb_progress,
                            hook_on,
                            osc_on,
                        ),
                    });
                }
            }
            window_node(
                w,
                RowKey::Window {
                    window: w.id.clone(),
                },
                children,
            )
        })
        .collect();
    RowTree {
        sections: vec![Section {
            heading: None,
            windows: nodes,
        }],
    }
}

/// The window tree, then one block per distinct agent (sorted).
fn sectioned_tree(
    windows: &[Window],
    sessions: &[Session],
    pane_progress: &PaneProgressMap,
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
) -> RowTree {
    let tree = Section {
        heading: Some(WINDOWS_HEADING.to_string()),
        windows: windows
            .iter()
            .map(|w| {
                let children = w
                    .panes
                    .iter()
                    .map(|p| pane_child(p, pane_status, hook_on, osc_on))
                    .collect();
                window_node(
                    w,
                    RowKey::Window {
                        window: w.id.clone(),
                    },
                    children,
                )
            })
            .collect(),
    };
    let mut sections = vec![tree];

    // Distinct agent names, deduped then Unicode-order sorted (a BTreeSet gives
    // both).
    let agents: std::collections::BTreeSet<&str> =
        sessions.iter().map(|s| s.agent.as_str()).collect();
    for agent in agents {
        let nodes = windows
            .iter()
            .filter_map(|w| {
                // Group membership follows the `sessions` slice order within the
                // window; window identity is by id (unique per server).
                let group: Vec<&Session> = sessions
                    .iter()
                    .filter(|s| s.agent == agent && s.window == w.id)
                    .collect();
                if group.is_empty() {
                    return None;
                }
                let children = group
                    .iter()
                    .map(|s| agent_child(s, w, pane_progress, hook_on, osc_on))
                    .collect();
                // A DISTINCT id from the window tree's row for the same window:
                // both are separately selectable.
                Some(window_node(
                    w,
                    RowKey::AgentWindow {
                        agent: agent.to_string(),
                        window: w.id.clone(),
                    },
                    children,
                ))
            })
            .collect();
        sections.push(Section {
            heading: Some(agent.to_string()),
            windows: nodes,
        });
    }

    RowTree { sections }
}

/// A pane as a child of its window, showing the pane's own title.
fn pane_child(
    p: &crate::model::Pane,
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
) -> Child {
    Child::Pane {
        id: RowKey::Pane { pane: p.id.clone() },
        index: p.index.clone(),
        title: p.title.clone(),
        active: p.active,
        color: p.color,
        indicator: pane_indicator(p, pane_status, hook_on, osc_on),
    }
}

/// One session as a child of `w`, the window it is displayed in.
///
/// The pane it occupies supplies the index and the active flag; its OSC state
/// comes from `pane_progress`, since a session names its pane by id and that
/// pane need not be among `w`'s when the registry is mid-update.
fn agent_child(
    s: &Session,
    w: &Window,
    pane_progress: &PaneProgressMap,
    hook_on: bool,
    osc_on: bool,
) -> Child {
    let pane = w.panes.iter().find(|p| p.id == s.pane);
    let (pb_state, pb_progress) = pane_progress
        .get(&s.pane)
        .map(|(st, pr)| (st.as_str(), *pr))
        .unwrap_or(("", None));
    Child::Agent {
        id: agent_id(s),
        index: pane.map(|p| p.index.clone()).unwrap_or_default(),
        label: s.label.clone(),
        active: pane.is_some_and(|p| p.active),
        color: s.color,
        indicator: indicator_for(status_str(s.status), pb_state, pb_progress, hook_on, osc_on),
    }
}

/// A session's row id. The pane is part of it: one session can be filed under
/// several windows at once, and the id must stay unique.
fn agent_id(s: &Session) -> RowKey {
    RowKey::Agent {
        session: s.id.clone(),
        pane: s.pane.clone(),
    }
}

/// A window as a tree node under the given id, which is what distinguishes the
/// window tree's row from an agent block's row for the same physical window.
fn window_node(w: &Window, id: RowKey, children: Vec<Child>) -> WindowNode {
    WindowNode {
        id,
        index: w.index.clone(),
        name: w.name.clone(),
        active: w.active,
        color: w.color,
        children,
    }
}

/// Fit `text` to exactly `field` chars (code points, NOT display columns):
/// ellipsize on overflow, else left-pad with spaces so the row fills its width
/// and the reverse-video selection bar stays solid. A client/painting concern.
///
/// The char-count (not unicode-width) counting is deliberate: the painter's
/// right-edge indicator reserve math is built on the same assumption, and
/// switching one to display width desyncs truncation.
pub fn fit(text: &str, field: usize) -> String {
    if field == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count > field {
        if field == 1 {
            return "…".to_string();
        }
        let mut s: String = text.chars().take(field - 1).collect();
        s.push('…');
        s
    } else {
        let mut s = text.to_string();
        s.extend(std::iter::repeat_n(' ', field - count));
        s
    }
}

/// Drop the single-token status glyph Claude Code prefixes onto a pane title
/// (`"<glyph> <session title>"`), used by the association layer to match a
/// pane's live title to a session.
///
/// Split on the first space; drop the head only when it holds no alphanumeric
/// character (a glyph), so a real title starting with a word is kept intact.
pub fn strip_status_prefix(title: &str) -> String {
    let trimmed = title.trim();
    match trimmed.split_once(' ') {
        Some((head, rest)) if !head.chars().any(|c| c.is_alphanumeric()) => rest.trim().to_string(),
        _ => trimmed.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::render::row_text;
    use crate::fixtures;
    use crate::model::{Branch, NamedColor, Row, RowContent, SessionKey, WindowId};
    use serde_json::Value;

    /// The lines a built tree draws, which is what the fixtures pin. Composing
    /// the daemon's structure with the client's renderer is deliberate: parity
    /// is a property of the two together, and asserting it here fails whichever
    /// layer drifts.
    fn drawn(tree: &RowTree) -> Vec<String> {
        tree.flatten()
            .iter()
            .map(|r| row_text(&r.content))
            .collect()
    }

    fn parse_color(s: &str) -> Option<NamedColor> {
        match s {
            "red" => Some(NamedColor::Red),
            "blue" => Some(NamedColor::Blue),
            "green" => Some(NamedColor::Green),
            "yellow" => Some(NamedColor::Yellow),
            "purple" => Some(NamedColor::Purple),
            "orange" => Some(NamedColor::Orange),
            "pink" => Some(NamedColor::Pink),
            "cyan" => Some(NamedColor::Cyan),
            _ => None,
        }
    }

    fn parse_status(s: &str) -> TurnStatus {
        match s {
            "working" => TurnStatus::Working,
            "attention" => TurnStatus::Attention,
            _ => TurnStatus::Idle,
        }
    }

    fn statecolor_str(c: StateColor) -> &'static str {
        match c {
            StateColor::Green => "green",
            StateColor::Yellow => "yellow",
            StateColor::Red => "red",
        }
    }

    fn parse_pane(v: &Value) -> crate::model::Pane {
        crate::model::Pane {
            id: PaneId(v["id"].as_str().unwrap().to_string()),
            index: v["index"].as_str().unwrap().to_string(),
            active: v["active"].as_bool().unwrap(),
            title: v["title"].as_str().unwrap().to_string(),
            pb_state: v["pb_state"].as_str().unwrap().to_string(),
            pb_progress: v["pb_progress"].as_u64().map(|n| n as u8),
            color: None,
        }
    }

    fn parse_window(v: &Value) -> Window {
        Window {
            id: WindowId(v["id"].as_str().unwrap().to_string()),
            index: v["index"].as_str().unwrap().to_string(),
            name: v["name"].as_str().unwrap().to_string(),
            active: v["active"].as_bool().unwrap(),
            color: None,
            panes: v["panes"]
                .as_array()
                .unwrap()
                .iter()
                .map(parse_pane)
                .collect(),
        }
    }

    fn parse_session(v: &Value) -> Session {
        Session {
            id: SessionKey(v["id"].as_str().unwrap().to_string()),
            agent: v["agent"].as_str().unwrap().to_string(),
            pane: PaneId(v["pane"].as_str().unwrap().to_string()),
            window: WindowId(v["window_id"].as_str().unwrap().to_string()),
            label: v["label"].as_str().unwrap().to_string(),
            color: parse_color(v["color"].as_str().unwrap()),
            status: parse_status(v["status"].as_str().unwrap()),
        }
    }

    fn parse_key(v: &Value) -> RowKey {
        let a = v.as_array().unwrap();
        match a[0].as_str().unwrap() {
            "w" => {
                if a.len() == 2 {
                    RowKey::Window {
                        window: WindowId(a[1].as_str().unwrap().to_string()),
                    }
                } else {
                    RowKey::AgentWindow {
                        agent: a[1].as_str().unwrap().to_string(),
                        window: WindowId(a[2].as_str().unwrap().to_string()),
                    }
                }
            }
            "p" => RowKey::Pane {
                pane: PaneId(a[1].as_str().unwrap().to_string()),
            },
            "a" => RowKey::Agent {
                session: SessionKey(a[1].as_str().unwrap().to_string()),
                pane: PaneId(a[2].as_str().unwrap().to_string()),
            },
            other => panic!("bad key tag {other}"),
        }
    }

    /// Compare a built row's resolved indicator against the fixture's baked
    /// `indicator` text / `indicator_color`.
    fn assert_indicator(row: &Row, item: &Value, frame: usize) {
        let (text, color) = row.indicator.resolve(frame);
        assert_eq!(text, item["indicator"].as_str().unwrap());
        assert_eq!(color.map(statecolor_str), item["indicator_color"].as_str());
    }

    fn run_build_rows_case(name: &str, input: &Value, expected: &[Value]) {
        let windows: Vec<Window> = input["windows"]
            .as_array()
            .unwrap()
            .iter()
            .map(parse_window)
            .collect();
        let sessions: Vec<Session> = input["sessions"]
            .as_array()
            .unwrap()
            .iter()
            .map(parse_session)
            .collect();

        let mut pane_progress = PaneProgressMap::new();
        for (k, val) in input["pane_progress"].as_object().unwrap() {
            let arr = val.as_array().unwrap();
            pane_progress.insert(
                PaneId(k.clone()),
                (
                    arr[0].as_str().unwrap().to_string(),
                    arr[1].as_u64().map(|n| n as u8),
                ),
            );
        }
        let mut pane_status = PaneStatusMap::new();
        for (k, val) in input["pane_status"].as_object().unwrap() {
            pane_status.insert(PaneId(k.clone()), val.as_str().unwrap().to_string());
        }

        let frame = input["frame"].as_u64().unwrap() as usize;
        let hook_on = input["hook_on"].as_bool().unwrap();
        let osc_on = input["osc_on"].as_bool().unwrap();

        // The fixtures are goldens of the sectioned grouping.
        let tree = build_tree(
            &windows,
            &sessions,
            &pane_progress,
            &pane_status,
            hook_on,
            osc_on,
            ViewMode::Sections,
        );
        let rows = tree.flatten();

        assert_eq!(
            rows.len(),
            expected.len(),
            "row count mismatch in build_rows/{name}"
        );

        for (idx, (row, exp)) in rows.iter().zip(expected).enumerate() {
            let item = &exp["item"];
            let ctx = format!("build_rows/{name} row {idx}");
            assert_eq!(
                row_text(&row.content),
                exp["text"].as_str().unwrap(),
                "text: {ctx}"
            );
            match item["type"].as_str().unwrap() {
                "header" => {
                    assert!(
                        matches!(row.content, RowContent::Header { .. }),
                        "header: {ctx}"
                    );
                    assert!(row.id.is_none(), "header id: {ctx}");
                }
                "blank" => {
                    assert!(matches!(row.content, RowContent::Blank), "blank: {ctx}");
                    assert!(row.id.is_none(), "blank id: {ctx}");
                }
                "window" => {
                    assert!(
                        matches!(row.content, RowContent::Window { .. }),
                        "window: {ctx}"
                    );
                    assert_eq!(
                        row.id.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "window id: {ctx}"
                    );
                }
                "pane" => {
                    assert!(
                        matches!(row.content, RowContent::Pane { .. }),
                        "pane: {ctx}"
                    );
                    assert_eq!(
                        row.id.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "pane id: {ctx}"
                    );
                    assert_indicator(row, item, frame);
                }
                "agent" => {
                    if let RowContent::Agent { color, .. } = &row.content {
                        assert_eq!(
                            *color,
                            parse_color(item["color"].as_str().unwrap()),
                            "agent color: {ctx}"
                        );
                    } else {
                        panic!("agent: {ctx}");
                    }
                    assert_eq!(
                        row.id.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "agent id: {ctx}"
                    );
                    assert_indicator(row, item, frame);
                }
                other => panic!("unknown item type {other}: {ctx}"),
            }
        }
    }

    fn pane(id: &str, index: &str, active: bool, title: &str) -> crate::model::Pane {
        crate::model::Pane {
            id: PaneId(id.to_string()),
            index: index.to_string(),
            active,
            title: title.to_string(),
            pb_state: String::new(),
            pb_progress: None,
            color: None,
        }
    }

    fn window(
        id: &str,
        index: &str,
        name: &str,
        active: bool,
        panes: Vec<crate::model::Pane>,
    ) -> Window {
        Window {
            id: WindowId(id.to_string()),
            index: index.to_string(),
            name: name.to_string(),
            active,
            color: None,
            panes,
        }
    }

    fn session(id: &str, pane: &str, window: &str, label: &str, status: TurnStatus) -> Session {
        Session {
            id: SessionKey(id.to_string()),
            agent: "claude".to_string(),
            pane: PaneId(pane.to_string()),
            window: WindowId(window.to_string()),
            label: label.to_string(),
            color: None,
            status,
        }
    }

    /// Build in unified mode with no OSC state and both indicator sources' defaults.
    fn unified(windows: &[Window], sessions: &[Session]) -> Vec<Row> {
        build_tree(
            windows,
            sessions,
            &PaneProgressMap::new(),
            &PaneStatusMap::new(),
            true,
            false,
            ViewMode::Unified,
        )
        .flatten()
    }

    #[test]
    fn unified_replaces_an_agents_pane_row_and_leaves_the_others() {
        let windows = vec![window(
            "@1",
            "1",
            "editor",
            true,
            vec![
                pane("%1", "1", true, "nvim"),
                pane("%2", "2", false, "✳ some pane title"),
            ],
        )];
        let sessions = vec![session(
            "claude-s1",
            "%2",
            "@1",
            "Fix the bug",
            TurnStatus::Working,
        )];

        let rows = unified(&windows, &sessions);

        // No heading, no blank, no repeated agent block: window, pane, agent row.
        let texts: Vec<String> = rows.iter().map(|r| row_text(&r.content)).collect();
        assert_eq!(
            texts,
            vec![
                "▌ 1: editor",
                "▌ ├─ 1: \u{f489}  nvim",
                "  └─ 2: \u{f167a}  Fix the bug"
            ]
        );

        assert_eq!(
            rows[1].content,
            RowContent::Pane {
                index: "1".into(),
                title: "nvim".into(),
                branch: Branch::More,
                here: true,
                color: None,
            },
            "the agent-free pane is untouched, and is where you are"
        );
        assert_eq!(
            rows[1].id,
            Some(RowKey::Pane {
                pane: PaneId("%1".into())
            })
        );

        assert_eq!(
            rows[2].content,
            RowContent::Agent {
                index: "2".into(),
                label: "Fix the bug".into(),
                branch: Branch::Last,
                here: false,
                color: None,
            }
        );
        assert_eq!(
            rows[2].id,
            Some(RowKey::Agent {
                session: SessionKey("claude-s1".into()),
                pane: PaneId("%2".into()),
            })
        );
        assert_eq!(
            rows[2].indicator,
            Indicator::Progress {
                pct: None,
                state: ProgressState::Plain,
            },
            "the row carries the agent's own turn state"
        );
    }

    #[test]
    fn unified_files_a_session_under_every_window_showing_it() {
        // @2 is not the active window, but %2 is its active pane.
        let windows = vec![
            window("@1", "1", "one", true, vec![pane("%1", "1", true, "a")]),
            window("@2", "2", "two", false, vec![pane("%2", "1", true, "b")]),
        ];
        // The same session displayed in two panes is two placements.
        let sessions = vec![
            session("claude-s1", "%1", "@1", "Fix the bug", TurnStatus::Idle),
            session("claude-s1", "%2", "@2", "Fix the bug", TurnStatus::Idle),
        ];

        let rows = unified(&windows, &sessions);

        let texts: Vec<String> = rows.iter().map(|r| row_text(&r.content)).collect();
        assert_eq!(
            texts,
            vec![
                "▌ 1: one",
                "▌ └─ 1: \u{f167a}  Fix the bug",
                // No gutter anywhere in @2: its active pane is not where you are.
                "  2: two",
                "  └─ 1: \u{f167a}  Fix the bug",
            ]
        );
        // The pane in the id is what keeps the two rows separately selectable.
        assert_ne!(rows[1].id, rows[3].id);
    }

    #[test]
    fn unified_marks_nothing_active_when_the_sidebar_holds_the_focus() {
        // The sidebar is not among a window's panes, so an active window whose
        // panes are all inactive is a window whose sidebar has the focus.
        let windows = vec![window(
            "@1",
            "1",
            "editor",
            true,
            vec![pane("%1", "1", false, "nvim")],
        )];

        let rows = unified(&windows, &[]);

        // The window heading still marks itself; no pane claims to be where you
        // are.
        let texts: Vec<String> = rows.iter().map(|r| row_text(&r.content)).collect();
        assert_eq!(texts, vec!["▌ 1: editor", "  └─ 1: \u{f489}  nvim"]);
    }

    #[test]
    fn unified_gives_every_session_sharing_a_pane_its_own_row() {
        let windows = vec![window(
            "@1",
            "1",
            "editor",
            true,
            vec![pane("%1", "1", false, "shared")],
        )];
        let sessions = vec![
            session("claude-s1", "%1", "@1", "first", TurnStatus::Idle),
            session("claude-s2", "%1", "@1", "second", TurnStatus::Idle),
        ];

        let rows = unified(&windows, &sessions);

        let texts: Vec<String> = rows.iter().map(|r| row_text(&r.content)).collect();
        // Two rows for the one pane, and the tree closes on the second: the
        // branch follows a row's position among its window's children, not the
        // pane's position among its window's panes.
        assert_eq!(
            texts,
            vec![
                "▌ 1: editor",
                "  ├─ 1: \u{f167a}  first",
                "  └─ 1: \u{f167a}  second"
            ]
        );
    }

    #[test]
    fn unified_agent_row_prefers_osc_progress_over_the_hook_glyph() {
        let mut p = pane("%1", "1", false, "claude");
        p.pb_state = "normal".to_string();
        p.pb_progress = Some(42);
        let windows = vec![window("@1", "1", "editor", true, vec![p])];
        let sessions = vec![session(
            "claude-s1",
            "%1",
            "@1",
            "Fix the bug",
            TurnStatus::Working,
        )];

        let rows = build_tree(
            &windows,
            &sessions,
            &PaneProgressMap::new(),
            &PaneStatusMap::new(),
            true,
            true,
            ViewMode::Unified,
        )
        .flatten();

        assert_eq!(
            rows[1].indicator,
            Indicator::Progress {
                pct: Some(42),
                state: ProgressState::Normal,
            }
        );
        assert!(
            matches!(rows[1].content, RowContent::Agent { here: false, .. }),
            "the turn state rides on the indicator, not the row content"
        );
    }

    #[test]
    fn sections_mode_groups_the_same_rows_under_headings() {
        let windows = vec![window(
            "@1",
            "1",
            "editor",
            true,
            vec![pane("%1", "1", true, "claude")],
        )];
        let sessions = vec![session(
            "claude-s1",
            "%1",
            "@1",
            "Fix the bug",
            TurnStatus::Idle,
        )];

        let tree = build_tree(
            &windows,
            &sessions,
            &PaneProgressMap::new(),
            &PaneStatusMap::new(),
            true,
            false,
            ViewMode::Sections,
        );

        // The pane appears twice, as itself and as the session displayed in it,
        // and both rows are framed exactly as the unified grouping frames them.
        assert_eq!(
            drawn(&tree),
            vec![
                " WINDOWS",
                "",
                "▌ 1: editor",
                "▌ └─ 1: \u{f489}  claude",
                "",
                " CLAUDE",
                "",
                "▌ 1: editor",
                "▌ └─ 1: \u{f167a}  Fix the bug",
            ]
        );
    }

    #[test]
    fn a_windows_two_rows_carry_different_ids() {
        let windows = vec![window(
            "@1",
            "1",
            "editor",
            true,
            vec![pane("%1", "1", true, "claude")],
        )];
        let sessions = vec![session(
            "claude-s1",
            "%1",
            "@1",
            "Fix the bug",
            TurnStatus::Idle,
        )];

        let rows = build_tree(
            &windows,
            &sessions,
            &PaneProgressMap::new(),
            &PaneStatusMap::new(),
            true,
            false,
            ViewMode::Sections,
        )
        .flatten();

        // Rows 2 and 7 are the same physical window in the two blocks.
        assert_eq!(
            rows[2].id,
            Some(RowKey::Window {
                window: WindowId("@1".into())
            })
        );
        assert_eq!(
            rows[7].id,
            Some(RowKey::AgentWindow {
                agent: "claude".into(),
                window: WindowId("@1".into()),
            })
        );
    }

    #[test]
    fn indeterminate_only_for_progress_without_pct() {
        // Only an indeterminate progress bar (no percentage) animates.
        assert!(Indicator::Progress {
            pct: None,
            state: ProgressState::Plain,
        }
        .indeterminate());
        assert!(Indicator::Progress {
            pct: None,
            state: ProgressState::Normal,
        }
        .indeterminate());

        // A static glyph, no indicator, and a determinate percentage do not.
        assert!(!Indicator::None.indeterminate());
        assert!(!Indicator::Attention.indeterminate());
        assert!(!Indicator::Progress {
            pct: Some(0),
            state: ProgressState::Normal,
        }
        .indeterminate());
        assert!(!Indicator::Progress {
            pct: Some(50),
            state: ProgressState::Plain,
        }
        .indeterminate());
    }

    #[test]
    fn fixture_parity() {
        let cases = fixtures::load("rows");
        let mut covered = 0usize;
        for case in &cases {
            let group = case["group"].as_str().unwrap();
            let name = case["name"].as_str().unwrap_or("");
            let input = &case["input"];
            match group {
                "spinner_frame" => {
                    let frame = input["frame"].as_u64().unwrap() as usize;
                    assert_eq!(
                        spinner_frame(frame).to_string(),
                        case["expected"].as_str().unwrap(),
                        "spinner_frame/{name}"
                    );
                    covered += 1;
                }
                "progress_indicator" => {
                    let ind = indicator_for(
                        input["hook_status"].as_str().unwrap(),
                        input["pb_state"].as_str().unwrap(),
                        input["pb_progress"].as_u64().map(|n| n as u8),
                        input["hook_on"].as_bool().unwrap(),
                        input["osc_on"].as_bool().unwrap(),
                    );
                    let (text, color) = ind.resolve(input["frame"].as_u64().unwrap() as usize);
                    let exp = &case["expected"];
                    assert_eq!(
                        text,
                        exp["text"].as_str().unwrap(),
                        "progress_indicator/{name} text"
                    );
                    assert_eq!(
                        color.map(statecolor_str),
                        exp["color"].as_str(),
                        "progress_indicator/{name} color"
                    );
                    covered += 1;
                }
                "fit" => {
                    assert_eq!(
                        fit(
                            input["text"].as_str().unwrap(),
                            input["field"].as_u64().unwrap() as usize
                        ),
                        case["expected"].as_str().unwrap(),
                        "fit/{name}"
                    );
                    covered += 1;
                }
                "strip_status_prefix" => {
                    assert_eq!(
                        strip_status_prefix(input["title"].as_str().unwrap()),
                        case["expected"].as_str().unwrap(),
                        "strip_status_prefix/{name}"
                    );
                    covered += 1;
                }
                "build_rows" => {
                    run_build_rows_case(name, input, case["expected"].as_array().unwrap());
                    covered += 1;
                }
                // Other groups in the shared fixture file (rgb_to_ansi256,
                // process_under, label_mode_from, agent_label, scan_tail) belong
                // to the color/assoc/labels modules and are asserted there.
                _ => {}
            }
        }
        // Sanity: every rows-owned family was exercised.
        assert!(
            covered >= 5,
            "expected to cover rows fixtures, covered {covered}"
        );
    }
}
