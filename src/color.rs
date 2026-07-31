//! Theme palette and the rgb->ansi256 color mapping for agent-row colors.
//!
//! A row carries a semantic [`NamedColor`], and this module resolves that name
//! to a concrete terminal color index, matched to the user's Claude theme:
//! RGB-to-ansi256 mapping, theme reading, palette tables, and the name ->
//! ansi256/ANSI-base index lookup.

use std::path::Path;

use indexmap::IndexMap;

use crate::model::NamedColor;

/// The eight agent color names, in the exact order their curses pair ids
/// (10..=17) are allocated. The order is load-bearing: a mid-loop allocation
/// failure in the client would otherwise shift the remaining assignments, so
/// every consumer must iterate in this order.
pub const AGENT_COLOR_NAMES: [&str; 8] = [
    "red", "blue", "green", "yellow", "purple", "orange", "pink", "cyan",
];

/// One theme palette: the eight color names paired with their RGB triples, in
/// [`AGENT_COLOR_NAMES`] order. An ordered array (not a hash map) so iteration —
/// and therefore pair-id assignment — is deterministic.
pub type Palette = [(&'static str, (u8, u8, u8)); 8];

/// The Tailwind-600-ish set shared by the plain `dark` and `light` themes.
/// Copied bit-for-bit from Claude's CLI `*_FOR_SUBAGENTS_ONLY` tokens; do not
/// "correct" or dedupe any value — parity requires the exact numbers Claude
/// emits (e.g. yellow `(202,138,4)` must map to 178, a gold, not 172).
pub const PALETTE_MUTED: Palette = [
    ("red", (220, 38, 38)),
    ("blue", (106, 155, 204)),
    ("green", (22, 163, 74)),
    ("yellow", (202, 138, 4)),
    ("purple", (130, 125, 189)),
    ("orange", (217, 119, 87)),
    ("pink", (196, 102, 134)),
    ("cyan", (8, 145, 178)),
];

/// The `light-daltonized` (deuteranopia, blue-tinted) palette.
pub const PALETTE_SATURATED: Palette = [
    ("red", (204, 0, 0)),
    ("blue", (0, 102, 204)),
    ("green", (0, 204, 0)),
    ("yellow", (255, 204, 0)),
    ("purple", (128, 0, 128)),
    ("orange", (255, 128, 0)),
    ("pink", (255, 102, 178)),
    ("cyan", (0, 178, 178)),
];

/// The `dark-daltonized` (deuteranopia, blue-tinted) palette.
pub const PALETTE_BRIGHT: Palette = [
    ("red", (255, 102, 102)),
    ("blue", (102, 178, 255)),
    ("green", (102, 255, 102)),
    ("yellow", (255, 255, 102)),
    ("purple", (178, 102, 255)),
    ("orange", (255, 178, 102)),
    ("pink", (255, 153, 204)),
    ("cyan", (102, 204, 204)),
];

// Curses `COLOR_*` constants (init_pair foreground indices, NOT the 256-cube
// indices `rgb_to_ansi256` produces). The client's terminal binding exposes the
// same numeric values; they are named here so the ANSI-base fallback and the
// base-UI / indicator pair contracts read literally.
pub const COLOR_RED: i16 = 1;
pub const COLOR_GREEN: i16 = 2;
pub const COLOR_YELLOW: i16 = 3;
pub const COLOR_BLUE: i16 = 4;
pub const COLOR_MAGENTA: i16 = 5;
pub const COLOR_CYAN: i16 = 6;

/// The base UI color pairs allocated before agent colors (fg on the terminal
/// default background, encoded as `-1`). Agent colors start at pair 10 to clear
/// these, and the fallback/indicator ids (1,2,3,4) below refer to them. This is
/// the documented contract the fallbacks in this module depend on.
pub const BASE_UI_PAIRS: [(i16, (i16, i16)); 4] = [
    (1, (COLOR_GREEN, -1)),
    (2, (COLOR_CYAN, -1)),
    (3, (COLOR_YELLOW, -1)),
    (4, (COLOR_RED, -1)),
];

/// The curses pair id an agent row falls back to when its color is unknown,
/// empty, or was skipped during allocation: pair 2 (cyan), the default agent
/// color. Never pair 0/default.
pub const DEFAULT_AGENT_PAIR: i16 = 2;

/// Fallback name -> base curses color for ANSI themes and <256-color terminals.
/// `orange` and `pink` have no base ANSI equivalent, so they deliberately alias
/// yellow and magenta respectively — preserve the aliasing exactly.
pub fn palette_ansi_base(name: &str) -> Option<i16> {
    Some(match name {
        "red" => COLOR_RED,
        "blue" => COLOR_BLUE,
        "green" => COLOR_GREEN,
        "yellow" => COLOR_YELLOW,
        "purple" => COLOR_MAGENTA,
        "orange" => COLOR_YELLOW,
        "pink" => COLOR_MAGENTA,
        "cyan" => COLOR_CYAN,
        _ => return None,
    })
}

/// Round half-to-even (banker's rounding).
///
/// The channel expressions here (`r/255*5`, `(r-8)/247*24` for integer r,g,b)
/// provably never land exactly on x.5, so this can never diverge from Rust's
/// half-away-from-zero `f64::round()` in practice — half-to-even is used so the
/// result is deterministic by construction rather than by accident.
fn round_half_even(x: f64) -> f64 {
    x.round_ties_even()
}

/// The xterm-256 index Claude itself would use for an RGB triple.
///
/// The 6x6x6 color cube with a separate 24-step gray ramp, NOT a
/// nearest-RGB-distance match. The canonical parity case: dark yellow
/// `(202,138,4)` yields 178 (gold), where a nearest-RGB approach would yield
/// 172 (orange).
///
/// Gray-ramp boundaries are strict: an equal-channel value `< 8` (0..=7) -> 16,
/// `> 248` (249..=255) -> 231; and r==8 and r==248 fall into the 232+round(...)
/// branch (8 -> 232, 248 -> 255). The equal-channel test is `r==g==b` on all
/// three channels, not a near-gray heuristic.
pub fn rgb_to_ansi256(r: u8, g: u8, b: u8) -> u16 {
    if r == g && g == b {
        if r < 8 {
            return 16;
        }
        if r > 248 {
            return 231;
        }
        return (232.0 + round_half_even((r as f64 - 8.0) / 247.0 * 24.0)) as u16;
    }
    (16.0
        + 36.0 * round_half_even(r as f64 / 255.0 * 5.0)
        + 6.0 * round_half_even(g as f64 / 255.0 * 5.0)
        + round_half_even(b as f64 / 255.0 * 5.0)) as u16
}

/// The user's Claude theme name (lowercased), read from
/// `<claude_dir>/settings.json`'s top-level `theme` field.
///
/// Defaults to `"dark"` when the field is absent, JSON null, an empty string, a
/// non-string value, or when the file is missing/unreadable or the JSON is
/// malformed. `"dark"` and `"light"` both map to the muted palette downstream,
/// so an unknown/missing value is a safe default.
pub fn read_theme(claude_dir: &Path) -> String {
    let path = claude_dir.join("settings.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(_) => return "dark".to_string(),
    };
    let value: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => return "dark".to_string(),
    };
    match value.get("theme") {
        // Only a non-empty string value counts; everything else defaults.
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.to_lowercase(),
        _ => "dark".to_string(),
    }
}

/// Resolve the Claude config dir the way Claude Code itself does:
/// `$CLAUDE_CONFIG_DIR` if set and non-empty, else `~/.claude`. The `or`
/// semantics matter — an empty `CLAUDE_CONFIG_DIR` falls back to `~/.claude`.
pub fn claude_dir() -> std::path::PathBuf {
    match std::env::var("CLAUDE_CONFIG_DIR") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME").unwrap_or_default();
            std::path::PathBuf::from(home).join(".claude")
        }
    }
}

/// The RGB palette for a (lowercased) theme, or `None` meaning "use the
/// terminal's own ANSI colors".
///
/// Rules, in order (the `-ansi` suffix wins over everything, and the daltonized
/// mapping is deliberately crossed):
///   * ends with `-ansi` -> `None`;
///   * `dark-daltonized` -> [`PALETTE_BRIGHT`];
///   * `light-daltonized` -> [`PALETTE_SATURATED`];
///   * anything else (including `dark`, `light`, typos, future themes) ->
///     [`PALETTE_MUTED`].
///
/// Input is expected already-lowercased (see [`read_theme`]); a mixed-case
/// caller would mis-match.
pub fn theme_palette(theme: &str) -> Option<&'static Palette> {
    if theme.ends_with("-ansi") {
        return None;
    }
    match theme {
        "dark-daltonized" => Some(&PALETTE_BRIGHT),
        "light-daltonized" => Some(&PALETTE_SATURATED),
        _ => Some(&PALETTE_MUTED),
    }
}

/// The resolved per-name color index table for a theme and terminal color count.
///
/// `rgb_used` records whether the RGB palette (true) or the ANSI base (false)
/// was in effect: RGB is used only when the terminal has >=256 colors AND the
/// theme is not an ANSI theme (both guards required). `indices` maps each name
/// (in [`AGENT_COLOR_NAMES`] order, preserved by [`IndexMap`]) to the color
/// index used as the terminal foreground.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentColorTable {
    pub rgb_used: bool,
    pub indices: IndexMap<&'static str, i16>,
}

/// Build the [`AgentColorTable`] for a theme and terminal color count.
pub fn agent_color_table(theme: &str, colors: i32) -> AgentColorTable {
    // The >=256 gate AND a non-ANSI theme are both required for RGB.
    let rgb = if colors >= 256 {
        theme_palette(theme)
    } else {
        None
    };
    let mut indices = IndexMap::with_capacity(AGENT_COLOR_NAMES.len());
    for &name in &AGENT_COLOR_NAMES {
        let cnum = match rgb {
            Some(palette) => {
                let (_, (r, g, b)) = palette
                    .iter()
                    .copied()
                    .find(|(n, _)| *n == name)
                    .expect("every palette holds all eight agent color names");
                rgb_to_ansi256(r, g, b) as i16
            }
            None => palette_ansi_base(name).expect("AGENT_COLOR_NAMES all have an ANSI base"),
        };
        indices.insert(name, cnum);
    }
    AgentColorTable {
        rgb_used: rgb.is_some(),
        indices,
    }
}

/// The curses pair id to color a whole agent row with, given the allocated
/// name -> pair-id map and the row's `color` field.
///
/// Looks up `color` (an empty/missing color becomes `""`, which is never a key)
/// and returns its pair, else falls back to [`DEFAULT_AGENT_PAIR`] (pair 2,
/// cyan). A color that was skipped during allocation is absent from the map and
/// so also falls back — indistinguishable from "no color". This is the
/// "empty color never matches" quirk: `""`/`None`/unknown all deterministically
/// yield pair 2, never a real color.
pub fn agent_pair_for(pairs: &IndexMap<String, i16>, color: Option<&str>) -> i16 {
    let key = color.unwrap_or("");
    pairs.get(key).copied().unwrap_or(DEFAULT_AGENT_PAIR)
}

/// The base UI curses pair for an OSC/hook indicator state color, used to color
/// the pinned right-edge progress indicator independently of the row color:
/// `green` -> 1, `yellow` -> 3, `red` -> 4.
///
/// Returns `None` when there is no indicator color (the indicator then inherits
/// the row's own attr) — and defensively also for any value outside the three
/// keys, rather than panicking; the producers only ever emit these three. These
/// ids are shared with, and coupled to, [`BASE_UI_PAIRS`]; if those renumber,
/// this must move in lockstep.
pub fn indicator_pair_for(color: Option<&str>) -> Option<i16> {
    match color {
        Some("green") => Some(1),
        Some("yellow") => Some(3),
        Some("red") => Some(4),
        _ => None,
    }
}

impl NamedColor {
    /// The lowercase color-name string this variant serializes to, the key used
    /// against the palette tables and the allocated pair map.
    pub fn as_str(self) -> &'static str {
        match self {
            NamedColor::Red => "red",
            NamedColor::Blue => "blue",
            NamedColor::Green => "green",
            NamedColor::Yellow => "yellow",
            NamedColor::Purple => "purple",
            NamedColor::Orange => "orange",
            NamedColor::Pink => "pink",
            NamedColor::Cyan => "cyan",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn palette_name(p: Option<&'static Palette>) -> Option<&'static str> {
        match p {
            None => None,
            Some(pal) if *pal == PALETTE_MUTED => Some("muted"),
            Some(pal) if *pal == PALETTE_SATURATED => Some("saturated"),
            Some(pal) if *pal == PALETTE_BRIGHT => Some("bright"),
            Some(_) => panic!("theme_palette returned an unknown palette instance"),
        }
    }

    fn palette_by_name(name: &str) -> &'static Palette {
        match name {
            "PALETTE_MUTED" => &PALETTE_MUTED,
            "PALETTE_SATURATED" => &PALETTE_SATURATED,
            "PALETTE_BRIGHT" => &PALETTE_BRIGHT,
            other => panic!("unknown palette {other}"),
        }
    }

    #[test]
    fn fixtures_parity() {
        let cases = crate::fixtures::load("color");
        let mut covered = 0usize;
        for case in &cases {
            let name = case["name"].as_str().unwrap();
            let input = &case["input"];
            let expected = &case["expected"];

            if let Some(rest) = name.strip_prefix("rgb_to_ansi256:") {
                let _ = rest;
                let r = input["r"].as_u64().unwrap() as u8;
                let g = input["g"].as_u64().unwrap() as u8;
                let b = input["b"].as_u64().unwrap() as u8;
                let got = rgb_to_ansi256(r, g, b);
                assert_eq!(
                    got as u64,
                    expected.as_u64().unwrap(),
                    "rgb_to_ansi256({r},{g},{b}) [{name}]"
                );
            } else if let Some(rest) = name.strip_prefix("theme_palette:") {
                let _ = rest;
                let theme = input["theme"].as_str().unwrap();
                let got = palette_name(theme_palette(theme));
                let want = expected["palette"].as_str();
                assert_eq!(got, want, "theme_palette({theme:?}) [{name}]");
            } else if name.starts_with("read_theme:") {
                covered += check_read_theme(name, input, expected);
                continue;
            } else if let Some(rest) = name.strip_prefix("agent_color_index:") {
                let _ = rest;
                let theme = input["theme"].as_str().unwrap();
                let colors = input["colors"].as_i64().unwrap() as i32;
                let table = agent_color_table(theme, colors);
                assert_eq!(
                    table.rgb_used,
                    expected["rgb_palette_used"].as_bool().unwrap(),
                    "rgb_palette_used [{name}]"
                );
                let want_order: Vec<&str> = expected["order"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                let got_order: Vec<&str> = table.indices.keys().copied().collect();
                assert_eq!(got_order, want_order, "order [{name}]");
                for (k, v) in expected["cnum"].as_object().unwrap() {
                    assert_eq!(
                        table.indices.get(k.as_str()).copied(),
                        Some(v.as_i64().unwrap() as i16),
                        "cnum[{k}] [{name}]"
                    );
                }
            } else if name.starts_with("agent_pair_for:") {
                let mut pairs: IndexMap<String, i16> = IndexMap::new();
                for (k, v) in input["pairs"].as_object().unwrap() {
                    pairs.insert(k.clone(), v.as_i64().unwrap() as i16);
                }
                let color = match &input["color"] {
                    Value::String(s) => Some(s.as_str()),
                    Value::Null => None,
                    other => panic!("unexpected color input {other:?}"),
                };
                let got = agent_pair_for(&pairs, color);
                assert_eq!(got as i64, expected.as_i64().unwrap(), "[{name}]");
            } else if name.starts_with("indicator_pair_for:") {
                let color = match &input["color"] {
                    Value::String(s) => Some(s.as_str()),
                    Value::Null => None,
                    other => panic!("unexpected color input {other:?}"),
                };
                let got = indicator_pair_for(color);
                if let Some(n) = expected.as_i64() {
                    assert_eq!(got, Some(n as i16), "[{name}]");
                } else {
                    // {pair: null} — no indicator color, no pair lookup.
                    assert_eq!(got, None, "[{name}]");
                }
            } else if name.starts_with("data:") {
                covered += check_data(name, input, expected);
                continue;
            } else {
                panic!("unhandled fixture case: {name}");
            }
            covered += 1;
        }
        // Every case in the fixture must be exercised.
        assert_eq!(
            covered,
            cases.len(),
            "covered {covered} of {} cases",
            cases.len()
        );
    }

    fn check_read_theme(name: &str, input: &Value, expected: &Value) -> usize {
        let dir = std::env::temp_dir().join(format!(
            "wrangler-color-test-{}-{}",
            std::process::id(),
            name.replace([':', '/'], "_")
        ));
        // Clean slate.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        match &input["settings.json"] {
            Value::Null => { /* missing file: do not create it */ }
            Value::String(s) => std::fs::write(&path, s).unwrap(),
            other => panic!("unexpected settings.json input {other:?}"),
        }
        let got = read_theme(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, expected.as_str().unwrap(), "[{name}]");
        1
    }

    fn check_data(name: &str, input: &Value, expected: &Value) -> usize {
        match name {
            "data:PALETTE_MUTED" | "data:PALETTE_SATURATED" | "data:PALETTE_BRIGHT" => {
                let pal = palette_by_name(input["table"].as_str().unwrap());
                let want_order: Vec<&str> = expected["order"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                let got_order: Vec<&str> = pal.iter().map(|(n, _)| *n).collect();
                assert_eq!(got_order, want_order, "order [{name}]");
                for (k, v) in expected["rgb"].as_object().unwrap() {
                    let triple = v.as_array().unwrap();
                    let want = (
                        triple[0].as_u64().unwrap() as u8,
                        triple[1].as_u64().unwrap() as u8,
                        triple[2].as_u64().unwrap() as u8,
                    );
                    let got = pal
                        .iter()
                        .find(|(n, _)| *n == k)
                        .map(|(_, rgb)| *rgb)
                        .unwrap();
                    assert_eq!(got, want, "rgb[{k}] [{name}]");
                }
            }
            "data:AGENT_COLOR_NAMES" => {
                let want: Vec<&str> = expected["names"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|v| v.as_str().unwrap())
                    .collect();
                assert_eq!(AGENT_COLOR_NAMES.to_vec(), want, "[{name}]");
            }
            "data:PALETTE_ANSI_BASE" => {
                for (k, v) in expected.as_object().unwrap() {
                    assert_eq!(
                        palette_ansi_base(k),
                        Some(v.as_i64().unwrap() as i16),
                        "ansi_base[{k}] [{name}]"
                    );
                }
            }
            "data:CURSES_COLOR_CONSTANTS" => {
                let want = expected.as_object().unwrap();
                assert_eq!(COLOR_RED as i64, want["red"].as_i64().unwrap());
                assert_eq!(COLOR_GREEN as i64, want["green"].as_i64().unwrap());
                assert_eq!(COLOR_YELLOW as i64, want["yellow"].as_i64().unwrap());
                assert_eq!(COLOR_BLUE as i64, want["blue"].as_i64().unwrap());
                assert_eq!(COLOR_MAGENTA as i64, want["magenta"].as_i64().unwrap());
                assert_eq!(COLOR_CYAN as i64, want["cyan"].as_i64().unwrap());
            }
            "data:BASE_UI_PAIRS" => {
                for (id, (fg, bg)) in BASE_UI_PAIRS {
                    let e = &expected[id.to_string()];
                    assert_eq!(
                        fg as i64,
                        e["fg"].as_i64().unwrap(),
                        "pair {id} fg [{name}]"
                    );
                    assert_eq!(
                        bg as i64,
                        e["bg"].as_i64().unwrap(),
                        "pair {id} bg [{name}]"
                    );
                }
            }
            "data:INDICATOR_PAIRS" => {
                for (k, v) in expected.as_object().unwrap() {
                    assert_eq!(
                        indicator_pair_for(Some(k)),
                        Some(v.as_i64().unwrap() as i16),
                        "indicator[{k}] [{name}]"
                    );
                }
            }
            other => panic!("unhandled data fixture: {other}"),
        }
        1
    }

    #[test]
    fn read_theme_defaults_dark_for_non_string_theme() {
        // A non-string `theme` value (number, bool) is not a valid theme name,
        // so it must fall back to "dark" like an absent field.
        for (idx, body) in [r#"{"theme": 5}"#, r#"{"theme": true}"#].iter().enumerate() {
            let dir = std::env::temp_dir().join(format!(
                "wrangler-color-nonstring-{}-{}",
                std::process::id(),
                idx
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("settings.json"), body).unwrap();
            let got = read_theme(&dir);
            let _ = std::fs::remove_dir_all(&dir);
            assert_eq!(got, "dark", "non-string theme {body}");
        }
    }

    #[test]
    fn named_color_str_round_trips_palette_keys() {
        // Each NamedColor variant must key a real palette entry, in order.
        let variants = [
            NamedColor::Red,
            NamedColor::Blue,
            NamedColor::Green,
            NamedColor::Yellow,
            NamedColor::Purple,
            NamedColor::Orange,
            NamedColor::Pink,
            NamedColor::Cyan,
        ];
        let names: Vec<&str> = variants.iter().map(|c| c.as_str()).collect();
        assert_eq!(names, AGENT_COLOR_NAMES.to_vec());
    }
}
