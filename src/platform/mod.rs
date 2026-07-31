//! OS-specific process primitives, selected at compile time.
//!
//! Each supported platform provides a `spawn_detached` with the same signature;
//! the rest of the crate calls the re-exported one without a `cfg`. Only the unix
//! backend exists today, so a build for any other target fails to link this
//! symbol until that platform's module is added.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::spawn_detached;
