//! Subcommand dispatch for the wrangler binary.

use std::env;
use std::process::{exit, ExitCode};

const USAGE: &str =
    "usage: wrangler <daemon|client|hook|toggle|focus|spawn|install-hooks|tmux-entry>";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("hook") => wrangler::hook::run(&args[2..]),
        // Wired up in the integration phase.
        Some(
            cmd @ ("daemon" | "client" | "toggle" | "focus" | "spawn" | "install-hooks"
            | "tmux-entry"),
        ) => {
            eprintln!("wrangler: '{cmd}' is not implemented yet");
            exit(2);
        }
        Some(other) => {
            eprintln!("wrangler: unknown subcommand '{other}'\n{USAGE}");
            exit(2);
        }
        None => {
            eprintln!("{USAGE}");
            exit(2);
        }
    }
}
