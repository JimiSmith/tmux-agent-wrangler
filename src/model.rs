//! Shared domain vocabulary for the daemon, client, and wire protocol.
//!
//! These types are the common language the pure-logic modules (color, labels,
//! rows, assoc) and the integration layer speak. The daemon builds a [`RowTree`]
//! and the client draws it: the only text the daemon sends is the literal name
//! of a thing (a window's name, a pane's title, an agent's label), and the
//! client composes every glyph around it — the gutter, the tree branches, the
//! index prefix, the spinner frame and the terminal colors — at paint time.

use serde::{Deserialize, Serialize};

/// A tmux server, identified by its socket path (the first field of `$TMUX`).
/// All tmux-facing state is partitioned by this, so panes/windows from
/// different servers are never conflated (pane ids are per-server).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServerKey(pub String);

/// A tmux window id, e.g. `"@3"`. Scoped to one server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WindowId(pub String);

/// A tmux pane id, e.g. `"%5"`. Scoped to one server.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PaneId(pub String);

impl PaneId {
    /// The integer after the leading `%`, used only for the spawn-race tiebreak
    /// (a numeric compare, never a lexical one). `None` if it is not the
    /// `%<digits>` form.
    pub fn numeric(&self) -> Option<u64> {
        self.0.strip_prefix('%').and_then(|d| d.parse().ok())
    }
}

/// An agent session's registry key: the `<agent>-<session_id>` file-name key
/// the registry is stored under, and the identity used in row keys and the
/// notification dedup.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SessionKey(pub String);

/// One of the eight color names an agent can be assigned (Claude's `/color`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum NamedColor {
    Red,
    Blue,
    Green,
    Yellow,
    Purple,
    Orange,
    Pink,
    Cyan,
}

/// The color/state of a progress indicator. `Plain` inherits the row's own
/// color (a generic in-progress signal); the others are OSC 9;4 progress states
/// and carry an explicit color the client applies.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProgressState {
    Plain,
    Normal,
    Paused,
    Error,
}

/// The semantic indicator pinned to a row's right edge. The daemon emits this;
/// the client resolves an indeterminate `Progress` to the current spinner frame
/// at paint time, and a determinate one to a percentage.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Indicator {
    None,
    /// A turn finished or a notification fired: the static "needs input" dot.
    Attention,
    /// Progress: indeterminate (`pct` None -> spinner) or determinate
    /// (`pct` Some -> percentage), colored by `state`.
    Progress {
        pct: Option<u8>,
        state: ProgressState,
    },
}

/// How the sidebar lays out its rows. A distinct type rather than a `bool` so it
/// cannot be confused with the indicator flags it travels alongside.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ViewMode {
    /// One window list: a pane hosting an agent draws as an agent row in place
    /// of its pane row.
    Unified,
    /// The window tree, then one section per agent repeating the windows.
    Sections,
}

/// An agent session's hook turn state. `Working`/`Attention` are mutually
/// exclusive, and reach the row as its [`Indicator`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TurnStatus {
    Idle,
    Working,
    Attention,
}

/// A pane within a window, as the daemon reads it from tmux.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pane {
    pub id: PaneId,
    pub index: String,
    pub active: bool,
    /// The pane title (the agent session title Claude sets on it, before the
    /// status-glyph strip that matches it to a session).
    pub title: String,
    /// tmux `pane_pb_state` (OSC 9;4), e.g. `"normal"`/`"paused"`/`"error"`/
    /// `"indeterminate"`/`"hidden"`; empty on a tmux too old to report it.
    pub pb_state: String,
    /// tmux `pane_pb_progress` percentage, if any.
    pub pb_progress: Option<u8>,
    /// Optional per-pane color (from the tmux pane border color).
    pub color: Option<NamedColor>,
}

/// A tmux window and its panes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub id: WindowId,
    pub index: String,
    pub name: String,
    pub active: bool,
    pub color: Option<NamedColor>,
    pub panes: Vec<Pane>,
}

/// An agent session placed under one window/pane. A session displayed in
/// several panes yields several `Session` placements, one per pane, so it
/// appears under each window it is visible in.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionKey,
    /// The agent kind: `"claude"`, `"copilot"`, ...
    pub agent: String,
    pub pane: PaneId,
    pub window: WindowId,
    pub label: String,
    /// The session's assigned color, or `None` for the default agent color.
    pub color: Option<NamedColor>,
    pub status: TurnStatus,
}

/// The stable key of a selectable row. The selection is shared across a
/// server's sidebars, so the key must be identical wherever a logical row is
/// drawn. An agent row's key includes the pane because one session can appear
/// under several windows at once.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RowKey {
    Window {
        window: WindowId,
    },
    AgentWindow {
        agent: String,
        window: WindowId,
    },
    Pane {
        pane: PaneId,
    },
    Agent {
        session: SessionKey,
        pane: PaneId,
    },
    /// A notification-area entry. It names the session alone: the daemon holds
    /// one entry per session and refreshes the pane it points at every poll, so
    /// a session that has moved still opens where it now lives.
    Notification {
        session: SessionKey,
    },
}

/// The sidebar's whole content, as structure: blocks of windows, each with its
/// children. Nothing here is laid out — no branch glyphs, no markers, no
/// prefixes — so the client is free to draw it however it likes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowTree {
    pub sections: Vec<Section>,
}

/// One block of the sidebar. A heading is drawn when present; the unified layout
/// is a single block with none.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub heading: Option<String>,
    pub windows: Vec<WindowNode>,
}

/// A window and the children shown beneath it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WindowNode {
    /// What the client echoes back to select or activate this row. Opaque to the
    /// client, which never builds or inspects one: the same window listed in two
    /// blocks carries two different ids, which is what keeps the two rows
    /// separately selectable.
    pub id: RowKey,
    pub index: String,
    pub name: String,
    /// tmux's active window.
    pub active: bool,
    pub color: Option<NamedColor>,
    pub children: Vec<Child>,
}

/// A window's child: a plain pane, or an agent session displayed in one. The two
/// stay distinct because they are styled differently — an agent row falls back
/// to the theme's agent color, which only the client knows.
///
/// `active` is the one fact tmux reports: this is its own window's active pane.
/// Whether that also means "where you are" depends on the enclosing window, and
/// is resolved by [`RowTree::flatten`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Child {
    Pane {
        id: RowKey,
        index: String,
        title: String,
        active: bool,
        color: Option<NamedColor>,
        indicator: Indicator,
    },
    Agent {
        id: RowKey,
        index: String,
        label: String,
        active: bool,
        color: Option<NamedColor>,
        indicator: Indicator,
    },
}

/// One entry of the notification area: an attention event, captured when it
/// fired and held until it is opened or pushed out by a newer one. It is drawn
/// as its `title` over its `body`, which the client wraps to the pane width.
///
/// Its `id` is a [`RowKey::Notification`] rather than the key of the agent row
/// naming the same session, so opening the entry is distinguishable from
/// selecting that agent in the tree — only the former clears the area.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NotificationNode {
    pub id: RowKey,
    pub title: String,
    pub body: String,
    pub color: Option<NamedColor>,
}

/// A child's position among its siblings, which is what decides the branch glyph
/// the client draws.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Branch {
    /// A sibling follows.
    More,
    /// The last child, which closes the tree.
    Last,
}

/// Where a row sits relative to the focus, which is what its weight and its
/// dimming are read off.
///
/// The two facts it carries are not independent — only the window you are in has
/// a `Here` child — so they travel as one value rather than as two bools a
/// caller could combine into a state that cannot happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Placement {
    /// The one row that is where you are: the active pane of the active window,
    /// or the active window itself.
    Here,
    /// In the window you are in, but not the row you are on.
    Focused,
    /// In a window you are not in.
    Unfocused,
}

impl Placement {
    /// A window row's placement. It is never `Focused`: for a window row, being
    /// in the focused window is the same as being it.
    pub fn window(active: bool) -> Self {
        if active {
            Placement::Here
        } else {
            Placement::Unfocused
        }
    }

    /// A child row's placement, from its window's focus and its own.
    pub fn child(window_active: bool, active: bool) -> Self {
        match (window_active, active) {
            (true, true) => Placement::Here,
            (true, false) => Placement::Focused,
            (false, _) => Placement::Unfocused,
        }
    }

    /// Whether this is the row that is where you are, which is what the gutter
    /// marks.
    pub fn here(self) -> bool {
        matches!(self, Placement::Here)
    }
}

/// What a flattened row holds: the literal name, plus everything the client
/// needs to style and frame it. Windows, panes, and agents may carry their own
/// color; a row's turn state is carried by its [`Indicator`], never by its
/// content.
#[derive(Clone, Debug, PartialEq)]
pub enum RowContent {
    Header {
        text: String,
    },
    Blank,
    Window {
        index: String,
        name: String,
        placement: Placement,
        color: Option<NamedColor>,
    },
    Pane {
        index: String,
        title: String,
        branch: Branch,
        placement: Placement,
        color: Option<NamedColor>,
    },
    Agent {
        index: String,
        label: String,
        branch: Branch,
        placement: Placement,
        color: Option<NamedColor>,
    },
    /// The first line of a notification-area entry: the agent that raised it. It
    /// sits under no window, so it carries neither a branch nor an index.
    NotificationTitle {
        title: String,
        color: Option<NamedColor>,
    },
    /// One line of an entry's description, already wrapped to the pane width.
    NotificationBody {
        text: String,
    },
}

/// One flattened display row. `id` marks a selectable row, and is the token the
/// client sends back to act on it.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub content: RowContent,
    pub id: Option<RowKey>,
    pub indicator: Indicator,
}

impl RowTree {
    /// Linearise the tree in display order: a heading row (and its blanks) per
    /// headed section, then each window and its children.
    ///
    /// This derives the two things that come from a node's *position* — its
    /// branch, and its placement relative to the focus — and copies everything
    /// else, ids included, straight through. Both ends run it, so the order the
    /// daemon resolves the selection against is the order the client navigates
    /// and paints in.
    pub fn flatten(&self) -> Vec<Row> {
        let mut rows = Vec::new();
        for section in &self.sections {
            if let Some(heading) = &section.heading {
                // Blocks are separated by a blank, but the sidebar does not open
                // with one.
                if !rows.is_empty() {
                    rows.push(plain(RowContent::Blank));
                }
                rows.push(plain(RowContent::Header {
                    text: heading.clone(),
                }));
                rows.push(plain(RowContent::Blank));
            }
            for w in &section.windows {
                rows.push(Row {
                    content: RowContent::Window {
                        index: w.index.clone(),
                        name: w.name.clone(),
                        placement: Placement::window(w.active),
                        color: w.color,
                    },
                    id: Some(w.id.clone()),
                    indicator: Indicator::None,
                });
                let last = w.children.len().saturating_sub(1);
                for (i, child) in w.children.iter().enumerate() {
                    let branch = if i == last {
                        Branch::Last
                    } else {
                        Branch::More
                    };
                    rows.push(child.row(branch, w.active));
                }
            }
        }
        rows
    }
}

impl Child {
    /// This child as a display row, given its branch position and whether its
    /// window is the active one.
    fn row(&self, branch: Branch, window_active: bool) -> Row {
        match self {
            Child::Pane {
                id,
                index,
                title,
                active,
                color,
                indicator,
            } => Row {
                content: RowContent::Pane {
                    index: index.clone(),
                    title: title.clone(),
                    branch,
                    placement: Placement::child(window_active, *active),
                    color: *color,
                },
                id: Some(id.clone()),
                indicator: *indicator,
            },
            Child::Agent {
                id,
                index,
                label,
                active,
                color,
                indicator,
            } => Row {
                content: RowContent::Agent {
                    index: index.clone(),
                    label: label.clone(),
                    branch,
                    placement: Placement::child(window_active, *active),
                    color: *color,
                },
                id: Some(id.clone()),
                indicator: *indicator,
            },
        }
    }
}

/// The notification area's selectable ids, in display order.
///
/// The area is painted as its own region rather than appended to the tree, but
/// its entries are selectable like any other row. Both ends read this — the
/// daemon to resolve the selection, the client to navigate — so the area's nav
/// order cannot drift from the order it is drawn in. An entry is one selectable
/// thing however many lines its description wraps to.
pub fn notification_ids(nodes: &[NotificationNode]) -> Vec<RowKey> {
    nodes.iter().map(|n| n.id.clone()).collect()
}

/// A row that is neither selectable nor indicated: the headings and blanks.
fn plain(content: RowContent) -> Row {
    Row {
        content,
        id: None,
        indicator: Indicator::None,
    }
}

/// The per-window render payload the daemon pushes to a client: the tree to
/// draw, the notification area beneath it, the shared selection, and whether
/// this sidebar currently has focus (the active pane of the active window) so
/// only it shows the selection bar.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RowModel {
    pub tree: RowTree,
    pub notifications: Vec<NotificationNode>,
    pub selection: Option<RowKey>,
    pub has_focus: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_child(id: &str, index: &str, active: bool) -> Child {
        Child::Pane {
            id: RowKey::Pane {
                pane: PaneId(id.to_string()),
            },
            index: index.to_string(),
            title: format!("title {index}"),
            active,
            color: None,
            indicator: Indicator::None,
        }
    }

    fn window_node(id: RowKey, active: bool, children: Vec<Child>) -> WindowNode {
        WindowNode {
            id,
            index: "0".to_string(),
            name: "w".to_string(),
            active,
            color: None,
            children,
        }
    }

    fn window_key(id: &str) -> RowKey {
        RowKey::Window {
            window: WindowId(id.to_string()),
        }
    }

    fn branches(rows: &[Row]) -> Vec<Branch> {
        rows.iter()
            .filter_map(|r| match &r.content {
                RowContent::Pane { branch, .. } | RowContent::Agent { branch, .. } => Some(*branch),
                _ => None,
            })
            .collect()
    }

    /// The kind of each row in order, which is what a layout assertion is about.
    fn shape(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match &r.content {
                RowContent::Header { text } => format!("header:{text}"),
                RowContent::Blank => "blank".to_string(),
                RowContent::Window { .. } => "window".to_string(),
                RowContent::Pane { .. } => "pane".to_string(),
                RowContent::Agent { .. } => "agent".to_string(),
                RowContent::NotificationTitle { title, .. } => format!("title:{title}"),
                RowContent::NotificationBody { text } => format!("body:{text}"),
            })
            .collect()
    }

    fn placements(rows: &[Row]) -> Vec<Placement> {
        rows.iter()
            .filter_map(|r| match &r.content {
                RowContent::Pane { placement, .. } | RowContent::Agent { placement, .. } => {
                    Some(*placement)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn only_the_final_child_closes_the_tree() {
        for len in 1..=3 {
            let children: Vec<Child> = (0..len)
                .map(|i| pane_child(&format!("%{i}"), &i.to_string(), false))
                .collect();
            let tree = RowTree {
                sections: vec![Section {
                    heading: None,
                    windows: vec![window_node(window_key("@1"), false, children)],
                }],
            };
            let mut expected = vec![Branch::More; len - 1];
            expected.push(Branch::Last);
            assert_eq!(branches(&tree.flatten()), expected, "{len} children");
        }
    }

    #[test]
    fn a_child_is_placed_by_its_window_then_itself() {
        let tree = RowTree {
            sections: vec![Section {
                heading: None,
                windows: vec![
                    window_node(
                        window_key("@1"),
                        true,
                        vec![pane_child("%1", "0", true), pane_child("%2", "1", false)],
                    ),
                    // An inactive window still has an active pane; it is not
                    // where you are, and neither is anything else under it.
                    window_node(window_key("@2"), false, vec![pane_child("%3", "0", true)]),
                ],
            }],
        };
        assert_eq!(
            placements(&tree.flatten()),
            vec![Placement::Here, Placement::Focused, Placement::Unfocused]
        );
    }

    #[test]
    fn a_window_whose_sidebar_holds_the_focus_has_no_pane_here() {
        // The sidebar is not among a window's panes, so an active window whose
        // panes are all inactive is one whose sidebar has the focus.
        let tree = RowTree {
            sections: vec![Section {
                heading: None,
                windows: vec![window_node(
                    window_key("@1"),
                    true,
                    vec![pane_child("%1", "0", false)],
                )],
            }],
        };
        assert_eq!(placements(&tree.flatten()), vec![Placement::Focused]);
    }

    #[test]
    fn headed_sections_get_a_heading_and_are_separated_by_a_blank() {
        let tree = RowTree {
            sections: vec![
                Section {
                    heading: Some("windows".to_string()),
                    windows: vec![window_node(window_key("@1"), true, vec![])],
                },
                Section {
                    heading: Some("claude".to_string()),
                    windows: vec![],
                },
            ],
        };
        let rows = tree.flatten();
        let shape = shape(&rows);
        // No blank opens the sidebar, and one separates the two blocks.
        assert_eq!(
            shape,
            vec![
                "header:windows",
                "blank",
                "window",
                "blank",
                "header:claude",
                "blank",
            ]
        );
        assert!(
            rows.iter()
                .filter(|r| !matches!(r.content, RowContent::Window { .. }))
                .all(|r| r.id.is_none()),
            "headings and blanks are not selectable"
        );
    }

    #[test]
    fn an_unheaded_section_opens_straight_into_its_windows() {
        let tree = RowTree {
            sections: vec![Section {
                heading: None,
                windows: vec![window_node(window_key("@1"), true, vec![])],
            }],
        };
        assert!(matches!(
            tree.flatten().as_slice(),
            [Row {
                content: RowContent::Window { .. },
                ..
            }]
        ));
    }

    fn notification(session: &str) -> NotificationNode {
        NotificationNode {
            id: RowKey::Notification {
                session: SessionKey(session.to_string()),
            },
            title: "claude".to_string(),
            body: "win · label".to_string(),
            color: None,
        }
    }

    #[test]
    fn an_empty_notification_area_has_nothing_to_select() {
        assert!(notification_ids(&[]).is_empty());
    }

    #[test]
    fn each_notification_is_one_selectable_thing_in_order() {
        assert_eq!(
            notification_ids(&[notification("claude-a"), notification("claude-b")]),
            vec![
                RowKey::Notification {
                    session: SessionKey("claude-a".to_string())
                },
                RowKey::Notification {
                    session: SessionKey("claude-b".to_string())
                },
            ]
        );
    }

    #[test]
    fn ids_survive_the_walk_in_row_order() {
        // The same window listed under two blocks carries two distinct ids, which
        // is what keeps both rows selectable.
        let agent_window = RowKey::AgentWindow {
            agent: "claude".to_string(),
            window: WindowId("@1".to_string()),
        };
        let tree = RowTree {
            sections: vec![
                Section {
                    heading: Some("windows".to_string()),
                    windows: vec![window_node(
                        window_key("@1"),
                        true,
                        vec![pane_child("%1", "0", true)],
                    )],
                },
                Section {
                    heading: Some("claude".to_string()),
                    windows: vec![window_node(agent_window.clone(), true, vec![])],
                },
            ],
        };
        let ids: Vec<RowKey> = tree.flatten().into_iter().filter_map(|r| r.id).collect();
        assert_eq!(
            ids,
            vec![
                window_key("@1"),
                RowKey::Pane {
                    pane: PaneId("%1".to_string())
                },
                agent_window,
            ]
        );
    }
}
