//! Test-only loader for the golden parity fixtures.

use serde_json::Value;

/// Load `spec/fixtures/<name>.json` (relative to the crate root) as its array
/// of `{name, input, expected}` case objects. Panics with the path on any I/O
/// or parse error, so a missing or malformed fixture fails the test loudly.
#[allow(dead_code)] // used by each module's fixture-backed tests
pub fn load(name: &str) -> Vec<Value> {
    let path = format!("{}/spec/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}
