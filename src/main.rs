//! Subcommand dispatch for the wrangler binary.

use std::env;
use std::process::{exit, ExitCode};

const USAGE: &str =
    "usage: wrangler <daemon|client|hook|toggle|focus|spawn|install-hooks|tmux-entry>";

fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("hook") => wrangler::hook::run(&args[2..]),
        Some("daemon") => wrangler::daemon::run(),
        Some("client") => wrangler::client::run(),
        Some("tmux-entry") => wrangler::glue::tmux_entry(),
        Some("toggle") => wrangler::glue::toggle(),
        Some("focus") => wrangler::glue::focus(),
        Some("spawn") => wrangler::glue::spawn(&args[2..]),
        Some("install-hooks") => wrangler::glue::install::run(&args[2..]),
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
