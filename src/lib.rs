//! wrangler: the library core of the tmux-agent-wrangler binary.

pub mod model;
pub mod paths;
pub mod platform;

pub mod color;
pub mod daemon;
pub mod hook;
pub mod labels;
pub mod proto;
pub mod tmux;

#[cfg(test)]
pub(crate) mod fixtures;
