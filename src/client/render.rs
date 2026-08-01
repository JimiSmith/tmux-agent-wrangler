//! Turning a row's structure into the line drawn for it.
//!
//! Every glyph the sidebar shows that is not the literal name of a thing is
//! chosen here: the gutter marking where you are, the tree branches, the index
//! prefix, the heading's spacing and case, and the styling. Nothing in this
//! module knows how the rows were grouped, so a window, a pane or an agent draws
//! the same wherever it appears.

use ratatui::style::Style;

use crate::model::{Branch, NamedColor, RowContent};

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

/// The opening of a child row: the gutter, the indent, and the branch. Its width
/// does not depend on any of them, so the tree stays aligned.
fn child_prefix(here: bool, position: Branch) -> String {
    format!("{}  {}─ ", gutter(here), branch(position))
}

/// The line drawn for a row, before it is fitted to the pane width.
pub fn row_text(content: &RowContent) -> String {
    match content {
        // The single leading space is load-bearing: it aligns the underline.
        RowContent::Header { text } => format!(" {}", text.to_uppercase()),
        RowContent::Blank => String::new(),
        RowContent::Window {
            index,
            name,
            active,
            ..
        } => format!("{} {index}: {name}", gutter(*active)),
        RowContent::Pane {
            index,
            title,
            branch,
            here,
            ..
        } => format!("{}{index}: {title}", child_prefix(*here, *branch)),
        RowContent::Agent {
            index,
            label,
            branch,
            here,
            ..
        } => format!("{}{index}: {label}", child_prefix(*here, *branch)),
    }
}

/// The base style for a row from its content, before the selection bar is
/// applied.
///
/// The two channels are kept apart: weight says where you are, and color says
/// *which* window or agent a row belongs to. Nothing here varies with a row's
/// turn state, which the indicator carries on its own.
pub fn base_style(content: &RowContent, colors: &Colors) -> Style {
    match content {
        RowContent::Header { .. } => Style::new().bold().underlined(),
        RowContent::Blank => Style::new().dim(),
        RowContent::Window { active, color, .. } => own_color(weight(*active), colors, *color),
        RowContent::Pane { here, color, .. } => own_color(weight(*here), colors, *color),
        RowContent::Agent { here, color, .. } => weight(*here).fg(colors.agent(*color)),
    }
}

/// Bold for a row that is where you are, normal weight otherwise.
fn weight(here: bool) -> Style {
    if here {
        Style::new().bold()
    } else {
        Style::new()
    }
}

/// Apply a window/pane's own color, leaving the style untouched when it has
/// none.
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

    fn window(index: &str, name: &str, active: bool) -> RowContent {
        RowContent::Window {
            index: index.to_string(),
            name: name.to_string(),
            active,
            color: None,
        }
    }

    fn pane(index: &str, title: &str, position: Branch, here: bool) -> RowContent {
        RowContent::Pane {
            index: index.to_string(),
            title: title.to_string(),
            branch: position,
            here,
            color: None,
        }
    }

    fn agent(index: &str, label: &str, position: Branch, here: bool) -> RowContent {
        RowContent::Agent {
            index: index.to_string(),
            label: label.to_string(),
            branch: position,
            here,
            color: None,
        }
    }

    fn is_bold(content: &RowContent, colors: &Colors) -> bool {
        base_style(content, colors)
            .add_modifier
            .contains(Modifier::BOLD)
    }

    #[test]
    fn a_window_row_leads_with_its_gutter() {
        assert_eq!(row_text(&window("1", "editor", true)), "▌ 1: editor");
        assert_eq!(row_text(&window("2", "shell", false)), "  2: shell");
    }

    #[test]
    fn a_child_row_is_indented_under_its_window() {
        assert_eq!(
            row_text(&pane("0", "nvim", Branch::More, false)),
            "   ├─ 0: nvim"
        );
        assert_eq!(
            row_text(&pane("1", "bash", Branch::Last, true)),
            "▌  └─ 1: bash"
        );
    }

    #[test]
    fn a_pane_and_an_agent_land_in_the_same_columns() {
        // Swapping which of a window's panes runs an agent must not shift the
        // tree, so the two forms differ only in the name they carry.
        assert_eq!(
            row_text(&pane("0", "name", Branch::Last, true)),
            row_text(&agent("0", "name", Branch::Last, true))
        );
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
        assert!(is_bold(&window("1", "w", true), &colors));
        assert!(!is_bold(&window("1", "w", false), &colors));
        assert!(is_bold(&pane("0", "p", Branch::Last, true), &colors));
        assert!(!is_bold(&agent("0", "a", Branch::Last, false), &colors));
    }

    #[test]
    fn an_agent_row_is_always_colored_and_a_pane_row_only_when_it_has_one() {
        let colors = Colors::new();
        // An agent with no color of its own still gets the theme's agent color.
        assert!(base_style(&agent("0", "a", Branch::Last, false), &colors)
            .fg
            .is_some());
        assert!(base_style(&pane("0", "p", Branch::Last, false), &colors)
            .fg
            .is_none());
        let colored = RowContent::Pane {
            index: "0".to_string(),
            title: "p".to_string(),
            branch: Branch::Last,
            here: false,
            color: Some(NamedColor::Red),
        };
        assert!(base_style(&colored, &colors).fg.is_some());
    }
}
