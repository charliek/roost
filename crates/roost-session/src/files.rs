//! Where a client's uploaded files land on this host, and when they go
//! away (plan 047 §3.1).
//!
//! Two things happen here that the engine's [`FileStore`] deliberately
//! does not do. The **root is chosen**: `session.put_file` returns a
//! path that a client pastes into a shell *bare*, so if `$HOME` or
//! `$XDG_CACHE_HOME` puts anything outside `[A-Za-z0-9._/-]` into the
//! cache path — a space, unicode, a byte that is not UTF-8 at all — the
//! store moves to a `/tmp` root that cannot have that problem, and says
//! so once. And the root is **swept**: entirely at start, because a
//! crash or a signal leaves it behind, and again on the clean wire-stop
//! path. Nothing else ever deletes from it, because a path this session
//! handed back may sit unsubmitted in an agent's composer for an hour.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use roost_engine::ipc::FileStore;
use tracing::warn;

use crate::consts::OWNER_ONLY_DIR_MODE;

/// Sweep `configured` (or the fallback it forces), create it 0700, and
/// account for what is in it.
///
/// The error is the caller's to log: a session that cannot open a store
/// still serves everything else, and `session.put_file` answers
/// `not-supported`.
pub fn open_store(configured: &Path, fallback: &Path) -> Result<FileStore> {
    let root = paste_safe_root(configured, fallback);
    sweep(&root).with_context(|| format!("sweep the file store {}", root.display()))?;
    create_private_dir_all(&root)
        .with_context(|| format!("create the file store {}", root.display()))?;
    FileStore::new(root.clone()).with_context(|| format!("open the file store {}", root.display()))
}

/// Remove the store and everything in it. A root that is already gone is
/// a swept root.
pub fn sweep(root: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(root) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

/// `configured` when the whole absolute path is pasteable bare, and the
/// `/tmp` fallback otherwise — logged once, because this is resolved
/// once at start.
fn paste_safe_root(configured: &Path, fallback: &Path) -> PathBuf {
    if is_paste_safe(configured) {
        return configured.to_path_buf();
    }
    warn!(
        configured = %configured.display(),
        fallback = %fallback.display(),
        "the resolved file-store path cannot be pasted bare; falling back"
    );
    fallback.to_path_buf()
}

/// The **entire** path is checked, not just the part this session
/// chose: the grammar is what the client re-checks the returned path
/// against, and one space anywhere in `$HOME` would break every upload.
fn is_paste_safe(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    path.is_absolute()
        && path
            .as_os_str()
            .as_bytes()
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'/' | b'-'))
}

fn create_private_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;

    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(OWNER_ONLY_DIR_MODE)
        .create(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_bare_pasteable_absolute_path_is_kept() {
        for safe in [
            "/home/charlie/.cache/roost-session/files",
            "/tmp/roost-session-1000/files",
            "/a-b_c.d/files",
        ] {
            assert!(is_paste_safe(Path::new(safe)), "{safe}");
        }

        for unsafe_path in [
            "/Users/charlie k/Library/Caches/Roost/files",
            "/home/charlié/.cache/files",
            "/home/c/my files",
            "/home/c/$HOME/files",
            "/home/c/'q'/files",
            "relative/files",
        ] {
            assert!(!is_paste_safe(Path::new(unsafe_path)), "{unsafe_path}");
        }
    }

    /// A path is bytes, not a string: a `$HOME` that is not valid UTF-8
    /// must take the fallback rather than be lossily accepted.
    #[test]
    fn a_non_utf8_path_is_not_pasteable() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt as _;

        let raw = OsString::from_vec(b"/home/\xff/files".to_vec());
        let fallback = Path::new("/tmp/roost-session-1000/files");
        assert!(!is_paste_safe(Path::new(&raw)));
        assert_eq!(paste_safe_root(Path::new(&raw), fallback), fallback);
    }

    #[test]
    fn a_safe_path_is_used_as_is() {
        let path = Path::new("/tmp/roost-session-test/files");
        assert_eq!(paste_safe_root(path, Path::new("/tmp/unused/files")), path);
    }

    #[test]
    fn sweeping_removes_the_tree_and_forgives_an_absent_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("files");
        std::fs::create_dir_all(root.join("abc")).expect("seed");
        std::fs::write(root.join("abc/shot.png"), b"x").expect("seed");

        sweep(&root).expect("sweep");
        assert!(!root.exists());
        sweep(&root).expect("a second sweep must be a no-op");
    }

    #[test]
    fn opening_a_store_sweeps_leftovers_and_creates_a_private_root() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("files");
        std::fs::create_dir_all(root.join("stale")).expect("seed");
        std::fs::write(root.join("stale/old.png"), b"leftover").expect("seed");

        let store = open_store(&root, &dir.path().join("fallback")).expect("open the store");
        assert_eq!(store.root(), root);
        assert!(!root.join("stale").exists(), "start must sweep leftovers");
        let mode = std::fs::metadata(&root).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }

    #[test]
    fn a_root_that_cannot_be_created_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let blocker = dir.path().join("blocker");
        std::fs::write(&blocker, b"not a directory").expect("seed");

        open_store(&blocker.join("files"), &dir.path().join("fallback"))
            .expect_err("a file cannot hold the store");
    }
}
