//! wrangler: the library core of the tmux-agent-wrangler binary.

pub mod model;

pub mod color;
pub mod labels;
pub mod proto;
pub mod daemon;

#[cfg(test)]
pub(crate) mod fixtures;
