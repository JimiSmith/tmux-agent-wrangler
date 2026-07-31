//! Building the flat, semantic row list (window tree plus agent sections) with
//! each row's indicator resolved, from the daemon's window/session model.
//!
//! This decides *what* each row is and which semantic [`Indicator`] it carries,
//! with no reference to the animation frame. The paint-time helpers that render
//! those decisions ([`spinner_frame`], [`Indicator::resolve`], [`fit`],
//! [`strip_status_prefix`]) also live here.

use indexmap::IndexMap;

use crate::model::{
    Indicator, PaneId, ProgressState, Row, RowKey, RowKind, Session, TurnStatus, Window,
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

/// Append one tree row per session in `group` under an already-emitted window
/// heading.
///
/// Agent rows differ from pane rows in framing: `"   {branch} {label}"` — a
/// space between the branch and the label, and no active marker. No window
/// argument is needed: the pane in each row's key already identifies the focus
/// target, so the enclosing window is not stored per row.
fn append_agent_rows(
    rows: &mut Vec<Row>,
    group: &[&Session],
    pane_progress: &PaneProgressMap,
    hook_on: bool,
    osc_on: bool,
) {
    let last = group.len().saturating_sub(1);
    for (i, s) in group.iter().enumerate() {
        let branch = if i == last { "└─" } else { "├─" };
        let (pb_state, pb_progress) = pane_progress
            .get(&s.pane)
            .map(|(st, pr)| (st.as_str(), *pr))
            .unwrap_or(("", None));
        let indicator = indicator_for(status_str(s.status), pb_state, pb_progress, hook_on, osc_on);
        // The indicator rides on the row, not the text: the painter pins it to
        // the right edge so it survives a long label being truncated.
        rows.push(Row {
            text: format!("   {branch} {}", s.label),
            kind: RowKind::Agent {
                color: s.color,
                // Bold is driven by the hook turn state alone (working/
                // attention), independent of any OSC progress indicator.
                emphatic: matches!(s.status, TurnStatus::Working | TurnStatus::Attention),
            },
            // The pane is part of the key: one session can be filed under
            // several windows at once, and the key must stay unique.
            key: Some(RowKey::Agent {
                session: s.id.clone(),
                pane: s.pane.clone(),
            }),
            indicator,
        });
    }
}

/// Build the flat, semantic row list: the WINDOWS tree, then one section per
/// distinct agent (sorted).
///
/// Rows carry a semantic [`Indicator`] and color *names*; the client resolves
/// the spinner frame, the percentage text, and the terminal colors at paint
/// time ([`fit`] + [`Indicator::resolve`]). `pane_progress` supplies OSC 9;4 state;
/// `pane_status` mirrors an agent's hook glyph onto its window-tree pane.
pub fn build_rows(
    windows: &[Window],
    sessions: &[Session],
    pane_progress: &PaneProgressMap,
    pane_status: &PaneStatusMap,
    hook_on: bool,
    osc_on: bool,
) -> Vec<Row> {
    let mut rows = Vec::new();
    rows.push(header(" WINDOWS"));
    rows.push(blank());

    for w in windows {
        rows.push(window_row(
            w,
            RowKey::Window {
                window: w.id.clone(),
            },
        ));
        let last = w.panes.len().saturating_sub(1);
        for (i, p) in w.panes.iter().enumerate() {
            let branch = if i == last { "└─" } else { "├─" };
            let active = if p.active { '*' } else { ' ' };
            let indicator = indicator_for(
                pane_status.get(&p.id).map(String::as_str).unwrap_or(""),
                &p.pb_state,
                p.pb_progress,
                hook_on,
                osc_on,
            );
            rows.push(Row {
                // NOTE: no space after the branch, single-char active marker
                // glued to the index (asymmetric with agent rows on purpose).
                text: format!("   {branch}{active}{}: {}", p.index, p.title),
                kind: RowKind::Pane { color: p.color },
                key: Some(RowKey::Pane { pane: p.id.clone() }),
                indicator,
            });
        }
    }

    // Distinct agent names, deduped then Unicode-order sorted (a BTreeSet gives
    // both).
    let agents: std::collections::BTreeSet<&str> =
        sessions.iter().map(|s| s.agent.as_str()).collect();
    for agent in agents {
        rows.push(blank());
        rows.push(header(&format!(" {}", agent.to_uppercase())));
        rows.push(blank());

        for w in windows {
            // Group membership follows the `sessions` slice order within the
            // window; window identity is by id (unique per server).
            let group: Vec<&Session> = sessions
                .iter()
                .filter(|s| s.agent == agent && s.window == w.id)
                .collect();
            if group.is_empty() {
                continue;
            }
            // A DISTINCT key from the top-section window row for the same
            // window: both are separately selectable.
            rows.push(window_row(
                w,
                RowKey::AgentWindow {
                    agent: agent.to_string(),
                    window: w.id.clone(),
                },
            ));
            append_agent_rows(&mut rows, &group, pane_progress, hook_on, osc_on);
        }
    }

    rows
}

/// A window heading row (`"{marker} {index}: {name}"`, marker `*` if active).
/// The `key` distinguishes the top-section row from an agent-section row for the
/// same physical window.
fn window_row(w: &Window, key: RowKey) -> Row {
    let marker = if w.active { '*' } else { ' ' };
    Row {
        text: format!("{marker} {}: {}", w.index, w.name),
        kind: RowKind::Window {
            active: w.active,
            color: w.color,
        },
        key: Some(key),
        indicator: Indicator::None,
    }
}

/// A section header row (`" WINDOWS"`, `" CLAUDE"`): the single leading space is
/// load-bearing (it aligns the underline) and must not be trimmed.
fn header(text: &str) -> Row {
    Row {
        text: text.to_string(),
        kind: RowKind::Header,
        key: None,
        indicator: Indicator::None,
    }
}

fn blank() -> Row {
    Row {
        text: String::new(),
        kind: RowKind::Blank,
        key: None,
        indicator: Indicator::None,
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
    use crate::fixtures;
    use crate::model::{NamedColor, SessionKey, WindowId};
    use serde_json::Value;

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

        let rows = build_rows(
            &windows,
            &sessions,
            &pane_progress,
            &pane_status,
            hook_on,
            osc_on,
        );

        assert_eq!(
            rows.len(),
            expected.len(),
            "row count mismatch in build_rows/{name}"
        );

        for (idx, (row, exp)) in rows.iter().zip(expected).enumerate() {
            let item = &exp["item"];
            let ctx = format!("build_rows/{name} row {idx}");
            assert_eq!(row.text, exp["text"].as_str().unwrap(), "text: {ctx}");
            match item["type"].as_str().unwrap() {
                "header" => {
                    assert!(matches!(row.kind, RowKind::Header), "kind header: {ctx}");
                    assert!(row.key.is_none(), "header key: {ctx}");
                }
                "blank" => {
                    assert!(matches!(row.kind, RowKind::Blank), "kind blank: {ctx}");
                    assert!(row.key.is_none(), "blank key: {ctx}");
                }
                "window" => {
                    assert!(
                        matches!(row.kind, RowKind::Window { .. }),
                        "kind window: {ctx}"
                    );
                    assert_eq!(
                        row.key.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "window key: {ctx}"
                    );
                }
                "pane" => {
                    assert!(matches!(row.kind, RowKind::Pane { .. }), "kind pane: {ctx}");
                    assert_eq!(
                        row.key.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "pane key: {ctx}"
                    );
                    assert_indicator(row, item, frame);
                }
                "agent" => {
                    if let RowKind::Agent { color, emphatic } = &row.kind {
                        assert_eq!(
                            *color,
                            parse_color(item["color"].as_str().unwrap()),
                            "agent color: {ctx}"
                        );
                        let st = item["status"].as_str().unwrap();
                        assert_eq!(
                            *emphatic,
                            st == "working" || st == "attention",
                            "agent emphatic: {ctx}"
                        );
                    } else {
                        panic!("kind agent: {ctx}");
                    }
                    assert_eq!(
                        row.key.as_ref(),
                        Some(&parse_key(&item["key"])),
                        "agent key: {ctx}"
                    );
                    assert_indicator(row, item, frame);
                }
                other => panic!("unknown item type {other}: {ctx}"),
            }
        }
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
