//! wrangler: the library core of the tmux-agent-wrangler binary.

pub mod model;
pub mod paths;

pub mod color;
pub mod hook;
pub mod labels;
pub mod proto;
pub mod tmux;
pub mod daemon;

#[cfg(test)]
pub(crate) mod fixtures;
