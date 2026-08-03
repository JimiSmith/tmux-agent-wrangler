//! Turning a row's structure into the line drawn for it.
//!
//! Every glyph the sidebar shows that is not the literal name of a thing is
//! chosen here: the gutter marking where you are, the icon marking what kind of
//! thing the row is, the tree branches, the index prefix, the heading's spacing
//! and case, and the styling. Nothing in this
//! module knows how the rows were grouped, so a window, a pane or an agent draws
//! the same wherever it appears.
//!
//! A row is drawn as a run of [`Segment`]s rather than one styled line, which is
//! what lets a pane's or agent's color sit on its icon alone while the name
//! beside it stays in the terminal's default.

use ratatui::style::Style;

use crate::daemon::rows::fit;
use crate::model::{Branch, NamedColor, Placement, RowContent};

use super::Colors;

/// Column 0: a block marks "where you are", a space does not.
///
/// A fixed position no row color can imitate, carried by exactly two rows: the
/// active window, and the pane you are in inside it.
fn gutter(here: bool) -> char {
    if here {
        '▌'
    } else {
        ' '
    }
}

/// The glyph for a child's position in its window: the last one closes the tree.
fn branch(branch: Branch) -> char {
    match branch {
        Branch::More => '├',
        Branch::Last => '└',
    }
}

/// What kind of thing the row is, drawn immediately before its name. Nerd Font
/// glyphs, one column wide.
///
/// This is the only thing distinguishing an agent from a plain pane: a child row
/// draws its name in the terminal's default whatever color the thing carries, so
/// color cannot be read as "this is an agent".
const ICON_PANE: char = '\u{f489}';
const ICON_AGENT: char = '\u{f167a}';

/// A description line hangs beneath its title, indented to the column the
/// title's text starts in (past the gutter, the icon and the gap after it).
const BODY_INDENT: &str = "    ";

/// The columns a description line has for its text in a pane `width` columns
/// wide: the indent comes off the front and the reserved right-hand column off
/// the end. Never zero, so wrapping to it always terminates.
pub fn notification_body_field(width: usize) -> usize {
    width.saturating_sub(BODY_INDENT.len() + 1).max(1)
}

/// A row's text, split so a color can land on the kind icon alone.
enum Parts {
    /// One undivided line: a heading, a blank, or a window row, which has no
    /// icon column of its own.
    Whole(String),
    /// A child row, carrying the color its icon is drawn in.
    Split {
        head: String,
        icon: char,
        tail: String,
        color: Option<NamedColor>,
    },
}

/// A child row's pieces: the gutter, the branch and index, then its kind icon
/// and name. The icon sits with the name it labels rather than out at the
/// margin, so the tree it hangs off reads as one unbroken structure.
fn child_parts(
    placement: Placement,
    icon: char,
    position: Branch,
    index: &str,
    name: &str,
    color: Option<NamedColor>,
) -> Parts {
    Parts::Split {
        head: format!(
            "{} {}─ {index}: ",
            gutter(placement.here()),
            branch(position)
        ),
        icon,
        // Two spaces, not one: the icons overhang the single column they are
        // declared as, and one space leaves the name touching the glyph.
        tail: format!("  {name}"),
        color,
    }
}

/// Split a row into the pieces it is drawn as.
fn parts(content: &RowContent) -> Parts {
    match content {
        // The single leading space is load-bearing: it aligns the underline.
        RowContent::Header { text } => Parts::Whole(format!(" {}", text.to_uppercase())),
        RowContent::Blank => Parts::Whole(String::new()),
        RowContent::Window {
            index,
            name,
            placement,
            ..
        } => Parts::Whole(format!("{} {index}: {name}", gutter(placement.here()))),
        RowContent::Pane {
            index,
            title,
            branch,
            placement,
            color,
        } => child_parts(*placement, ICON_PANE, *branch, index, title, *color),
        RowContent::Agent {
            index,
            label,
            branch,
            placement,
            color,
        } => child_parts(*placement, ICON_AGENT, *branch, index, label, *color),
        // No gutter and no branch: the entry hangs off nothing, and the area it
        // sits in is never where you are.
        RowContent::NotificationTitle { title, color } => Parts::Split {
            head: " ".to_string(),
            icon: ICON_AGENT,
            tail: format!("  {title}"),
            color: *color,
        },
        RowContent::NotificationBody { text } => Parts::Whole(format!("{BODY_INDENT}{text}")),
    }
}

/// The line drawn for a row, before it is fitted to the pane width.
pub fn row_text(content: &RowContent) -> String {
    match parts(content) {
        Parts::Whole(text) => text,
        Parts::Split {
            head, icon, tail, ..
        } => format!("{head}{icon}{tail}"),
    }
}

/// One piece of a drawn row: its text and the style that piece carries.
pub struct Segment {
    pub text: String,
    pub style: Style,
}

/// The styled pieces a row is drawn as, before they are fitted to the pane
/// width.
///
/// A child's color rides on its icon alone. A whole row in an agent's color
/// drowns the list once more than a couple of agents are up, and the icon is
/// enough to tie the row to the thing it points at.
pub fn row_segments(content: &RowContent, colors: &Colors) -> Vec<Segment> {
    let base = base_style(content, colors);
    match parts(content) {
        Parts::Whole(text) => vec![Segment { text, style: base }],
        Parts::Split {
            head,
            icon,
            tail,
            color,
        } => vec![
            Segment {
                text: head,
                style: base,
            },
            Segment {
                text: icon.to_string(),
                style: own_color(base, colors, color),
            },
            Segment {
                text: tail,
                style: base,
            },
        ],
    }
}

/// Fit a row's segments to `field` columns, truncating or padding the line as a
/// whole.
///
/// Only the tail of the line moves, so a truncated segment empties from the
/// right and the padding lands on the last segment — which is what makes the
/// selection bar span the full width rather than stopping at the name.
pub fn fit_segments(segments: Vec<Segment>, field: usize) -> Vec<Segment> {
    let joined: String = segments.iter().map(|s| s.text.as_str()).collect();
    let fitted = fit(&joined, field);
    let mut chars = fitted.chars();
    let mut out: Vec<Segment> = segments
        .into_iter()
        .map(|seg| Segment {
            text: chars.by_ref().take(seg.text.chars().count()).collect(),
            style: seg.style,
        })
        .collect();
    let padding: String = chars.collect();
    if let Some(last) = out.last_mut() {
        last.text.push_str(&padding);
    }
    out
}

/// The style a row's own text draws in, which the right-edge indicator inherits
/// when it carries no state color of its own.
///
/// The channels are kept apart: intensity says where you are, and the kind icon
/// says what the row is. A child's color is deliberately absent here — it
/// belongs to the icon, not the name — so only a window row styles its whole
/// line with a color. Nothing here varies with a row's turn state, which the
/// indicator carries on its own.
pub fn base_style(content: &RowContent, colors: &Colors) -> Style {
    match content {
        RowContent::Header { .. } => Style::new().bold().underlined(),
        RowContent::Blank => Style::new().dim(),
        RowContent::Window {
            placement, color, ..
        } => own_color(intensity(*placement), colors, *color),
        RowContent::Pane { placement, .. } | RowContent::Agent { placement, .. } => {
            intensity(*placement)
        }
        RowContent::NotificationTitle { .. } => Style::new(),
        // Dimmed, so the title leads and the description reads as its detail.
        // Intensity is not available for that: it says where you are.
        RowContent::NotificationBody { .. } => Style::new().dim(),
    }
}

/// How brightly a row draws, which is the one channel saying where you are: bold
/// for the row you are on, dim for a window you are not in, and plain for the
/// rest of the window you are in.
///
/// Dimming the whole of an unfocused window (its icons and its inherited
/// indicators included) sets it behind the current window as a block, which is
/// what makes the current window findable at a glance in a long list.
fn intensity(placement: Placement) -> Style {
    match placement {
        Placement::Here => Style::new().bold(),
        Placement::Focused => Style::new(),
        Placement::Unfocused => Style::new().dim(),
    }
}

/// Apply a thing's own color, leaving the style untouched when it has none.
fn own_color(style: Style, colors: &Colors, color: Option<NamedColor>) -> Style {
    match colors.optional(color) {
        Some(c) => style.fg(c),
        None => style,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn window(index: &str, name: &str, placement: Placement) -> RowContent {
        RowContent::Window {
            index: index.to_string(),
            name: name.to_string(),
            placement,
            color: None,
        }
    }

    fn pane(index: &str, title: &str, position: Branch, placement: Placement) -> RowContent {
        RowContent::Pane {
            index: index.to_string(),
            title: title.to_string(),
            branch: position,
            placement,
            color: None,
        }
    }

    fn agent(index: &str, label: &str, position: Branch, placement: Placement) -> RowContent {
        RowContent::Agent {
            index: index.to_string(),
            label: label.to_string(),
            branch: position,
            placement,
            color: None,
        }
    }

    fn colored_agent(color: NamedColor) -> RowContent {
        RowContent::Agent {
            index: "0".to_string(),
            label: "a".to_string(),
            branch: Branch::Last,
            placement: Placement::Focused,
            color: Some(color),
        }
    }

    fn has(content: &RowContent, colors: &Colors, modifier: Modifier) -> bool {
        base_style(content, colors).add_modifier.contains(modifier)
    }

    #[test]
    fn a_window_row_leads_with_its_gutter() {
        assert_eq!(
            row_text(&window("1", "editor", Placement::Here)),
            "▌ 1: editor"
        );
        assert_eq!(
            row_text(&window("2", "shell", Placement::Unfocused)),
            "  2: shell"
        );
    }

    #[test]
    fn a_child_row_is_indented_under_its_window() {
        assert_eq!(
            row_text(&pane("0", "nvim", Branch::More, Placement::Focused)),
            "  ├─ 0: \u{f489}  nvim"
        );
        assert_eq!(
            row_text(&pane("1", "bash", Branch::Last, Placement::Here)),
            "▌ └─ 1: \u{f489}  bash"
        );
    }

    #[test]
    fn a_pane_and_an_agent_land_in_the_same_columns() {
        // Swapping which of a window's panes runs an agent must not shift the
        // tree, so the two forms differ only in the icon and the name.
        let pane_row = row_text(&pane("0", "name", Branch::Last, Placement::Here));
        let agent_row = row_text(&agent("0", "name", Branch::Last, Placement::Here));
        assert_ne!(pane_row, agent_row);
        assert_eq!(pane_row.chars().count(), agent_row.chars().count());
        assert_eq!(
            pane_row.replace(ICON_PANE, ""),
            agent_row.replace(ICON_AGENT, "")
        );
    }

    #[test]
    fn a_notification_title_is_an_agent_row_without_the_tree() {
        // It hangs off no window, so it keeps the agent's icon column but drops
        // the gutter, the branch and the index.
        assert_eq!(
            row_text(&RowContent::NotificationTitle {
                title: "claude".to_string(),
                color: None,
            }),
            " \u{f167a}  claude"
        );
    }

    #[test]
    fn a_description_line_starts_under_its_titles_text() {
        let title = row_text(&RowContent::NotificationTitle {
            title: "claude".to_string(),
            color: None,
        });
        let body = row_text(&RowContent::NotificationBody {
            text: "vim · api".to_string(),
        });
        assert_eq!(body, "    vim · api");
        // Columns, not byte offsets: the icon is one column and several bytes.
        let column = |line: &str, text: &str| line.chars().count() - text.chars().count();
        assert_eq!(
            column(&title, "claude"),
            column(&body, "vim · api"),
            "the description hangs under the title's text"
        );
    }

    #[test]
    fn a_notifications_color_lands_on_its_icon_and_nowhere_else() {
        let colors = Colors::new();
        let content = RowContent::NotificationTitle {
            title: "a".to_string(),
            color: Some(NamedColor::Cyan),
        };
        let segments = row_segments(&content, &colors);
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec![" ", "\u{f167a}", "  a"]);
        assert!(segments[1].style.fg.is_some(), "the icon carries the color");
        assert!(segments[2].style.fg.is_none(), "the title stays default");
    }

    #[test]
    fn the_description_field_leaves_room_for_the_indent_and_the_edge() {
        // 24 columns: four of indent and one reserved at the right edge.
        assert_eq!(notification_body_field(24), 19);
        // Absurdly narrow panes still yield a field to wrap into.
        assert_eq!(notification_body_field(2), 1);
    }

    #[test]
    fn a_heading_is_spaced_and_upper_cased() {
        assert_eq!(
            row_text(&RowContent::Header {
                text: "claude".to_string()
            }),
            " CLAUDE"
        );
        assert_eq!(row_text(&RowContent::Blank), "");
    }

    #[test]
    fn only_a_row_you_are_on_is_bold() {
        let colors = Colors::new();
        assert!(has(
            &window("1", "w", Placement::Here),
            &colors,
            Modifier::BOLD
        ));
        assert!(!has(
            &window("1", "w", Placement::Unfocused),
            &colors,
            Modifier::BOLD
        ));
        assert!(has(
            &pane("0", "p", Branch::Last, Placement::Here),
            &colors,
            Modifier::BOLD
        ));
        assert!(!has(
            &agent("0", "a", Branch::Last, Placement::Focused),
            &colors,
            Modifier::BOLD
        ));
    }

    #[test]
    fn only_a_window_you_are_not_in_is_dim() {
        let colors = Colors::new();
        // The whole of an unfocused window recedes: its window row and every
        // child under it, whichever of them tmux calls active.
        for content in [
            window("2", "w", Placement::Unfocused),
            pane("0", "p", Branch::Last, Placement::Unfocused),
            agent("0", "a", Branch::More, Placement::Unfocused),
        ] {
            assert!(has(&content, &colors, Modifier::DIM), "{content:?}");
        }
        for content in [
            window("1", "w", Placement::Here),
            pane("0", "p", Branch::Last, Placement::Here),
            agent("0", "a", Branch::More, Placement::Focused),
        ] {
            assert!(!has(&content, &colors, Modifier::DIM), "{content:?}");
        }
    }

    #[test]
    fn a_dimmed_rows_color_still_lands_on_its_icon_alone() {
        // Dimming is the placement channel and the color the identity one, so an
        // unfocused agent keeps its icon color rather than trading it for dim.
        let colors = Colors::new();
        let content = RowContent::Agent {
            index: "0".to_string(),
            label: "a".to_string(),
            branch: Branch::Last,
            placement: Placement::Unfocused,
            color: Some(NamedColor::Cyan),
        };
        let segments = row_segments(&content, &colors);
        assert!(segments[1].style.fg.is_some(), "the icon keeps its color");
        assert!(
            segments
                .iter()
                .all(|s| s.style.add_modifier.contains(Modifier::DIM)),
            "every piece of the row is dimmed"
        );
    }

    #[test]
    fn a_childs_color_lands_on_its_icon_and_nowhere_else() {
        let colors = Colors::new();
        let segments = row_segments(&colored_agent(NamedColor::Cyan), &colors);
        let texts: Vec<&str> = segments.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(texts, vec!["  └─ 0: ", "\u{f167a}", "  a"]);
        assert!(segments[1].style.fg.is_some(), "the icon carries the color");
        assert!(segments[0].style.fg.is_none(), "the tree stays default");
        assert!(segments[2].style.fg.is_none(), "the name stays default");
    }

    #[test]
    fn a_child_with_no_color_draws_entirely_in_the_default() {
        let colors = Colors::new();
        for content in [
            agent("0", "a", Branch::Last, Placement::Focused),
            pane("0", "p", Branch::Last, Placement::Focused),
        ] {
            for segment in row_segments(&content, &colors) {
                assert!(segment.style.fg.is_none());
            }
        }
    }

    #[test]
    fn a_window_row_is_drawn_as_one_colored_piece() {
        // A window has no icon column, so its color stays on the whole line.
        let colors = Colors::new();
        let content = RowContent::Window {
            index: "1".to_string(),
            name: "w".to_string(),
            placement: Placement::Unfocused,
            color: Some(NamedColor::Red),
        };
        let segments = row_segments(&content, &colors);
        assert_eq!(segments.len(), 1);
        assert!(segments[0].style.fg.is_some());
    }

    #[test]
    fn fitting_pads_the_last_segment_and_empties_the_others_from_the_right() {
        let colors = Colors::new();
        let row = agent("0", "a", Branch::Last, Placement::Focused);
        let segments = row_segments(&row, &colors);
        let width = row_text(&row).chars().count();

        let padded = fit_segments(row_segments(&row, &colors), width + 3);
        let texts: Vec<String> = padded.iter().map(|s| s.text.clone()).collect();
        assert_eq!(texts[0], "  └─ 0: ", "the tree is untouched");
        assert_eq!(texts[1], "\u{f167a}", "the icon keeps its own segment");
        assert!(
            texts[2].ends_with("   "),
            "padding lands on the last segment"
        );

        // Narrower than the prefix: the tail empties, the head is what survives.
        let cut = fit_segments(segments, 2);
        assert_eq!(cut.iter().map(|s| s.text.chars().count()).sum::<usize>(), 2);
        assert_eq!(cut[2].text, "");
    }
}
