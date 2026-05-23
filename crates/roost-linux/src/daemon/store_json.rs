//! `state.json` reader + atomic writer.
//!
//! M3 of the daemon-removal refactor. Replaces the legacy
//! SQLite-backed `Store` with a single JSON file per profile.
//!
//! On-disk schema is intentionally narrow: only projects + the
//! monotonically-increasing id counter. Tabs do not persist
//! across UI quits — the "no session restore" goal in the
//! refactor plan.
//!
//! Atomic write protocol: write to `state.json.tmp`, `fsync`,
//! rename over `state.json`. Killing the process mid-write either
//! leaves the previous `state.json` intact (tmp not renamed yet)
//! or the new one (rename atomic). A `.bak` is also kept as a
//! one-level rollback for diagnostic / "oh no" cases.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub next_id: i64,
    #[serde(default)]
    pub projects: Vec<ProjectSnapshot>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
}

/// Read `state.json` at `path`. Returns:
/// * `Ok(Some(s))` — file present and well-formed.
/// * `Ok(None)` — file absent. First launch.
/// * `Err(_)` — present but malformed or unreadable. Caller logs
///   and starts empty.
pub fn read_state(path: &Path) -> std::io::Result<Option<SnapshotFile>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };
    if raw.trim().is_empty() {
        return Ok(None);
    }
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
}

/// Atomic write of `snapshot` to `path`.
///
/// Sequence:
/// 1. Ensure the parent directory exists.
/// 2. If `path` exists, copy it to `<path>.bak` (best-effort —
///    the previous backup is overwritten).
/// 3. Write the JSON to `<path>.tmp` and `fsync` the file.
/// 4. Atomically rename `<path>.tmp` over `path`.
///
/// `fsync` is the durability gate. The rename is atomic on POSIX
/// filesystems within the same directory.
pub fn persist_state(path: &Path, snapshot: &SnapshotFile) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Best-effort backup of the previous version. Ignore failures
    // since the next step overwrites the file.
    if path.exists() {
        let bak: PathBuf = with_extension_suffix(path, "bak");
        let _ = std::fs::copy(path, &bak);
    }

    let tmp: PathBuf = with_extension_suffix(path, "tmp");
    {
        let mut f = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&tmp)?;
        let bytes = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        f.write_all(&bytes)?;
        f.write_all(b"\n")?;
        // `sync_all` flushes the file's data + metadata. Required
        // for the durability guarantee: a power loss between write
        // and rename should still leave `state.json` intact (the
        // previous version) and the half-written .tmp either
        // present (we'll discard) or absent.
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Build a sibling path with the suffix replaced. `/a/state.json`
/// → `/a/state.json.tmp` (for suffix "tmp"). PathBuf's
/// `with_extension` would overwrite `.json`, which is wrong here.
fn with_extension_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(suffix);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn read_state_missing_returns_none() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("nope.json");
        assert!(read_state(&p).unwrap().is_none());
    }

    #[test]
    fn persist_then_read_round_trips() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        let snap = SnapshotFile {
            next_id: 42,
            projects: vec![ProjectSnapshot {
                id: 1,
                name: "Roost".into(),
                cwd: "/tmp".into(),
                position: 0,
                created_at: 1_700_000_000,
            }],
        };
        persist_state(&p, &snap).unwrap();
        let back = read_state(&p).unwrap().expect("present");
        assert_eq!(back, snap);
    }

    #[test]
    fn corrupted_file_surfaces_as_error() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(&p, b"not json").unwrap();
        let err = read_state(&p).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
    }

    #[test]
    fn second_write_preserves_bak() {
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        persist_state(
            &p,
            &SnapshotFile {
                next_id: 1,
                projects: vec![],
            },
        )
        .unwrap();
        persist_state(
            &p,
            &SnapshotFile {
                next_id: 2,
                projects: vec![],
            },
        )
        .unwrap();
        let bak = with_extension_suffix(&p, "bak");
        let bak_contents = std::fs::read_to_string(&bak).unwrap();
        assert!(bak_contents.contains("\"next_id\": 1"));
    }
}
