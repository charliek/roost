//! `state.json` reader + atomic writer.
//!
//! M3 of the daemon-removal refactor. Replaces the legacy
//! SQLite-backed `Store` with a single JSON file per profile.
//!
//! On-disk schema: the monotonically-increasing id counter, the
//! projects, and — per project — the **layout** of its tabs
//! (title + cwd + position, no live process/scrollback). On
//! relaunch the UI re-opens each project's tabs as **fresh shells**
//! in their saved directories; this reverses the original
//! "no session restore" goal (vision.md DL-7). The `tabs` array and
//! the `active_*` selection fields are `#[serde(default)]` so a file
//! written by an older build (or the other UI) still loads.
//!
//! Atomic write protocol: write to `state.json.tmp`, rename over
//! `state.json`. Killing the process mid-write either leaves the
//! previous `state.json` intact (tmp not renamed yet) or the new one
//! (rename atomic). A `.bak` is also kept as a one-level rollback for
//! diagnostic / "oh no" cases.
//!
//! Durability is controlled by the `sync` flag. During a session
//! `persist_state` is called with `sync = false`: the atomic
//! `tmp + rename` lands in the kernel page cache and is immediately
//! visible to a relaunched process, but `fsync` is skipped so the
//! hot path doesn't block on the disk. `fsync` is forced only on a
//! clean exit (`Workspace::flush`, `sync = true`), which re-asserts
//! physical durability at quit time. Dropping `fsync` mid-session
//! costs only power-loss durability within the kernel writeback
//! window; the atomic rename means the file is never torn.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotFile {
    pub next_id: i64,
    #[serde(default)]
    pub projects: Vec<ProjectSnapshot>,
    /// Project to re-select on relaunch. `0` (the default) means
    /// "no preference" — the UI falls back to the first project.
    #[serde(default)]
    pub active_project_id: i64,
    /// Position of the active tab within `active_project_id`. Tab
    /// ids are not stable across restore (fresh shells), so the
    /// selection is restored by position, not id.
    #[serde(default)]
    pub active_tab_position: i32,
    /// Whether the sidebar was collapsed (hidden) at save time, so a
    /// relaunch restores the user's hide/show choice. Defaulted so a
    /// file from an older build (no key) loads as "expanded". The Mac
    /// UI persists the same choice in UserDefaults (`RoostSidebarVisible`);
    /// this is the GTK equivalent, kept at behavioral parity.
    #[serde(default)]
    pub sidebar_collapsed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectSnapshot {
    pub id: i64,
    pub name: String,
    pub cwd: String,
    pub position: i32,
    pub created_at: i64,
    /// Layout of this project's tabs, in display order. Defaulted so
    /// a file from an older build (no `tabs` key) loads as "no saved
    /// tabs" → the UI seeds a single tab on restore.
    #[serde(default)]
    pub tabs: Vec<TabSnapshot>,
}

/// A persisted tab's layout: enough to re-open a fresh shell in the
/// right place, but no live state (no id, process, or scrollback).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TabSnapshot {
    pub title: String,
    pub cwd: String,
    pub position: i32,
    /// True iff the user manually renamed the tab (Cmd+R / tab.set_title).
    /// Persisted so a manual rename survives relaunch — and, after the
    /// model-side title-follows-cwd change in `set_tab_cwd`, isn't silently
    /// re-derived to the basename on the first post-relaunch `cd`.
    /// Defaulted so a state.json from a build predating this field loads
    /// as "not user-titled" (the prior implicit value).
    #[serde(default)]
    pub user_titled: bool,
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
/// 3. Write the JSON to `<path>.tmp` (and `fsync` the file when
///    `sync`).
/// 4. Atomically rename `<path>.tmp` over `path` (and `fsync` the
///    parent directory when `sync`).
///
/// The rename is atomic on POSIX filesystems within the same
/// directory, so the file is never torn regardless of `sync`. With
/// `sync == false` the write lands in the kernel page cache — visible
/// to a relaunched process immediately, durable across a clean exit,
/// but not forced to disk. With `sync == true` both the tmp file and
/// the parent directory are `fsync`-ed, re-asserting physical
/// durability (used by `Workspace::flush` on clean exit).
pub fn persist_state(path: &Path, snapshot: &SnapshotFile, sync: bool) -> std::io::Result<()> {
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
        if sync {
            // `sync_all` flushes the file's data + metadata. Forces
            // physical durability: a power loss between write and
            // rename should still leave `state.json` intact (the
            // previous version) and the half-written .tmp either
            // present (we'll discard) or absent. Skipped on the hot
            // path (`sync == false`) — the page cache is enough for
            // restore across a clean exit.
            f.sync_all()?;
        }
    }
    std::fs::rename(&tmp, path)?;
    // fsync the parent directory so the rename itself is durable.
    // Without this, a crash between the rename returning and the
    // filesystem flushing the directory metadata could lose the
    // rename (leaving state.json pointing at the previous inode).
    // POSIX requires fsync(dir) after rename for atomic-write
    // protocols; ext4 + apfs both honor it. Skipped on the hot path.
    if sync {
        if let Some(parent) = path.parent() {
            match OpenOptions::new().read(true).open(parent) {
                Ok(dir) => {
                    if let Err(err) = dir.sync_all() {
                        // Some filesystems (e.g. tmpfs on Linux) reject
                        // fsync on directories with EINVAL. Treat that
                        // as success — the rename itself is still
                        // atomic, we just can't force a sync.
                        if err.kind() != std::io::ErrorKind::InvalidInput {
                            return Err(err);
                        }
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    // Parent vanished between rename and reopen —
                    // shouldn't happen in practice, but a missing
                    // parent at this stage isn't a write failure.
                }
                Err(err) => return Err(err),
            }
        }
    }
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
            active_project_id: 1,
            active_tab_position: 1,
            sidebar_collapsed: true,
            projects: vec![ProjectSnapshot {
                id: 1,
                name: "Roost".into(),
                cwd: "/tmp".into(),
                position: 0,
                created_at: 1_700_000_000,
                tabs: vec![
                    TabSnapshot {
                        title: "shell".into(),
                        cwd: "/tmp".into(),
                        position: 0,
                        user_titled: false,
                    },
                    TabSnapshot {
                        title: "logs".into(),
                        cwd: "/var/log".into(),
                        position: 1,
                        user_titled: true,
                    },
                ],
            }],
        };
        persist_state(&p, &snap, true).unwrap();
        let back = read_state(&p).unwrap().expect("present");
        assert_eq!(back, snap);
    }

    #[test]
    fn legacy_file_without_tabs_loads_with_defaults() {
        // A state.json written by a build predating tab persistence
        // has no `tabs` / `active_*` keys. It must still load, with
        // those fields defaulted (empty tabs, active selection 0).
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(
            &p,
            br#"{"next_id":5,"projects":[{"id":1,"name":"Old","cwd":"/tmp","position":0,"created_at":1}]}"#,
        )
        .unwrap();
        let back = read_state(&p).unwrap().expect("present");
        assert_eq!(back.next_id, 5);
        assert_eq!(back.active_project_id, 0);
        assert_eq!(back.active_tab_position, 0);
        assert!(!back.sidebar_collapsed, "absent key defaults to expanded");
        assert_eq!(back.projects.len(), 1);
        assert!(back.projects[0].tabs.is_empty());
    }

    #[test]
    fn legacy_tab_without_user_titled_defaults_to_false() {
        // A state.json written by a build predating user_titled
        // persistence has no `user_titled` key per tab. It must still
        // load, with the field defaulted to false (matches the prior
        // implicit "always not user-titled" behavior).
        let dir = tempdir().unwrap();
        let p = dir.path().join("state.json");
        std::fs::write(
            &p,
            br#"{
                "next_id": 5,
                "projects": [{
                    "id": 1, "name": "Old", "cwd": "/tmp",
                    "position": 0, "created_at": 1,
                    "tabs": [{ "title": "docs", "cwd": "/usr", "position": 0 }]
                }]
            }"#,
        )
        .unwrap();
        let back = read_state(&p).unwrap().expect("present");
        let tab = &back.projects[0].tabs[0];
        assert_eq!(tab.title, "docs");
        assert_eq!(tab.cwd, "/usr");
        assert!(
            !tab.user_titled,
            "missing user_titled key must default to false"
        );
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
                ..Default::default()
            },
            false,
        )
        .unwrap();
        persist_state(
            &p,
            &SnapshotFile {
                next_id: 2,
                ..Default::default()
            },
            false,
        )
        .unwrap();
        let bak = with_extension_suffix(&p, "bak");
        let bak_contents = std::fs::read_to_string(&bak).unwrap();
        assert!(bak_contents.contains("\"next_id\": 1"));
    }
}
