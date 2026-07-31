//! Unix process detachment: start the daemon in its own session so it survives
//! the tmux server that launched it.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};

/// Spawn `command` fully detached and do not wait for it. Its standard streams
/// are redirected to `/dev/null`, and it is placed in a new session (`setsid`)
/// before exec so it has no controlling terminal and outlives the process group
/// that spawned it. A spawn failure is swallowed: starting the daemon is
/// best-effort.
pub fn spawn_detached(mut command: Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: the closure runs in the forked child before exec and calls only
    // async-signal-safe `setsid`. A failure (e.g. already a session leader) is
    // ignored so exec still proceeds.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = command.spawn();
}
