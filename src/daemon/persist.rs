//! The registry snapshot: one file per session under `sessions/`, its name the
//! session key and its body the five-field record. This is the daemon's only
//! on-disk state; the in-memory turn markers are not persisted.

use std::fs;
use std::path::PathBuf;

use indexmap::IndexMap;

use crate::daemon::assoc::{parse_registry_record, serialize_registry_record};
use crate::daemon::state::RegistryEntry;
use crate::model::SessionKey;
use crate::paths::state_dir;

/// The directory holding one record file per registered session.
fn sessions_dir() -> PathBuf {
    state_dir().join("sessions")
}

/// Load every persisted record. A file whose body is not a valid five-field
/// record is skipped. The key is the file name; the record's own `session_id` is
/// derived from it. An unreadable directory yields no records.
pub fn load() -> Vec<(SessionKey, crate::daemon::assoc::RegistryRecord)> {
    let mut out = Vec::new();
    let entries = match fs::read_dir(sessions_dir()) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let name = match entry.file_name().into_string() {
            Ok(n) => n,
            Err(_) => continue,
        };
        let contents = match fs::read_to_string(entry.path()) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if let Some(record) = parse_registry_record(&name, &contents) {
            out.push((SessionKey(name), record));
        }
    }
    out
}

/// Rewrite the snapshot to exactly match `registry`: write each session's record
/// file and remove any file for a session no longer present. Best-effort; an I/O
/// failure leaves the on-disk snapshot stale rather than erroring.
pub fn save(registry: &IndexMap<SessionKey, RegistryEntry>) {
    let dir = sessions_dir();
    if fs::create_dir_all(&dir).is_err() {
        return;
    }

    if let Ok(existing) = fs::read_dir(&dir) {
        for entry in existing.flatten() {
            if let Ok(name) = entry.file_name().into_string() {
                if !registry.contains_key(&SessionKey(name)) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
    }

    for (key, entry) in registry {
        let _ = fs::write(dir.join(&key.0), serialize_registry_record(&entry.record));
    }
}
