//! The one function every writer in this crate goes through, and the
//! lock that serialises them.
//!
//! Four things have to be true of every byte Roost writes into someone
//! else's dotfiles:
//!
//! 1. **Symlinks are followed, never replaced.** `stow` and `chezmoi`
//!    are ordinary; `~/.claude/settings.json` being a link into a
//!    tracked dotfile repo is ordinary. Writing a fresh regular file
//!    over the link silently forks the user's tree — their repo keeps
//!    the old content and nothing ever tells them.
//! 2. **The replacement is atomic.** Write a sibling temp file, then
//!    `rename(2)` in the *target's* directory (a rename across
//!    filesystems is not atomic, and following a symlink can cross one).
//!    A crash leaves either the old file or the new one, never a torn
//!    half.
//! 3. **The file did not move underneath us.** Claude rewrites
//!    `settings.json` on its own schedule. [`crate::plan`] records a
//!    digest **and** where the path resolved to; if either no longer
//!    matches, the apply is refused rather than clobbering whatever
//!    arrived in between. The check happens as late as it can — the
//!    replacement is fully written and synced *first*, so the gap
//!    between looking and renaming is one syscall. It is not zero, and
//!    nothing here can make it zero: `rename(2)` cannot be made
//!    conditional on the destination's content. A writer that lands
//!    inside that gap is a lost update this crate does not detect, which
//!    is why [`lock`] exists to keep Roost's own writers out of it.
//! 4. **Mode is preserved, and a new file is created tight.** These
//!    files can hold tokens, so one Roost creates is `0600`; one that
//!    already exists keeps whatever the user chose.
//! 5. **The temp file is Roost's alone.** Its name carries a nonce and
//!    it is opened `O_CREAT|O_EXCL`, so it can neither truncate a file
//!    that is already there nor be aimed, through a planted symlink, at
//!    something else entirely.

use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};

use crate::error::InstallError;

/// Mode for a file Roost creates that can hold credentials.
pub const PRIVATE_MODE: u32 = 0o600;

/// How many links deep to follow before giving up. Linux's own limit.
const MAX_SYMLINK_HOPS: usize = 40;

/// What a file looked like when [`crate::plan`] read it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The path as configured — possibly a symlink.
    pub path: PathBuf,
    /// Where that path actually leads. Writes go here.
    pub target: PathBuf,
    /// [`Self::path`] with its own link chain followed but no directory
    /// resolved. `target` cannot do this job: it canonicalizes the
    /// parent, so it changes the moment a missing directory is created —
    /// which [`stage`] does — and a re-check against it would report
    /// every fresh install as "changed underneath us".
    pub linked: PathBuf,
    /// `None` when the file did not exist.
    pub bytes: Option<Vec<u8>>,
    /// `None` when the file did not exist, so "absent" and "empty" are
    /// distinguishable at re-check time.
    pub digest: Option<[u8; 32]>,
    /// The mode to create with, when the file is absent.
    pub create_mode: u32,
}

impl Image {
    pub fn exists(&self) -> bool {
        self.bytes.is_some()
    }

    /// The file's bytes as text.
    ///
    /// `Ok(None)` is an absent file; `Err` is a file that is not UTF-8.
    /// Deliberately fallible rather than lossy: `from_utf8_lossy` would
    /// put U+FFFD where the user's byte was, the document would parse
    /// perfectly well around it, and the next write would persist the
    /// substitution over — say — an API token. A file Roost cannot
    /// decode is a file Roost skips.
    pub fn text(&self) -> Result<Option<&str>, std::str::Utf8Error> {
        match self.bytes.as_deref() {
            None => Ok(None),
            Some(bytes) => std::str::from_utf8(bytes).map(Some),
        }
    }
}

/// Where `path` really leads.
///
/// The link chain is walked by hand rather than handed to
/// `canonicalize`, because a **broken** link — one whose target does not
/// exist yet — is exactly the case `canonicalize` cannot answer, and
/// treating it as "absent" would replace the user's link with a regular
/// file. Following the chain ourselves and canonicalizing only the final
/// *parent* covers all three shapes at once: a symlinked file, a
/// symlinked config directory, and a link whose target has yet to be
/// created.
pub fn resolve_target(path: &Path) -> PathBuf {
    let current = follow_links(path);
    let (Some(parent), Some(name)) = (current.parent(), current.file_name()) else {
        return current;
    };
    let joined = std::fs::canonicalize(parent).map(|dir| dir.join(name));
    joined.unwrap_or(current)
}

/// `path` with its own link chain followed, lexically — no directory is
/// resolved, so the answer does not move when a parent is created.
pub fn follow_links(path: &Path) -> PathBuf {
    let mut current = path.to_path_buf();
    for _ in 0..MAX_SYMLINK_HOPS {
        let Ok(link) = std::fs::read_link(&current) else {
            break;
        };
        current = match current.parent() {
            Some(dir) if link.is_relative() => dir.join(link),
            _ => link,
        };
    }
    current
}

/// Read `path` into an [`Image`], following symlinks.
pub fn read_image(path: &Path, create_mode: u32) -> Result<Image, InstallError> {
    let target = resolve_target(path);
    let bytes = match std::fs::read(&target) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(InstallError::io(&target, e)),
    };
    Ok(Image {
        digest: bytes.as_deref().map(digest),
        bytes,
        linked: follow_links(path),
        path: path.to_path_buf(),
        target,
        create_mode,
    })
}

pub fn digest(bytes: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher.finalize().into()
}

/// Refuse unless what is on disk still matches what `plan` saw.
///
/// Two things have to hold, not one. The digest catches an edit to the
/// file; re-resolving the path catches a **link retargeted** since the
/// plan, where the digest would still match happily because it was taken
/// on the target the link used to have — and the write would land on a
/// file the agent no longer reads.
pub fn check_unchanged(image: &Image) -> Result<(), InstallError> {
    if follow_links(&image.path) != image.linked {
        return Err(InstallError::ChangedUnderneath {
            path: image.path.clone(),
        });
    }
    let now = match std::fs::read(&image.target) {
        Ok(bytes) => Some(digest(&bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(InstallError::io(&image.target, e)),
    };
    if now == image.digest {
        return Ok(());
    }
    Err(InstallError::ChangedUnderneath {
        path: image.target.clone(),
    })
}

/// A replacement written and synced, waiting for its rename.
///
/// Splitting the write from the rename is what lets [`crate::apply`] put
/// the digest re-check in between: everything slow happens before the
/// look, so the window between looking and replacing is as small as a
/// `rename(2)`. A `Staged` that is dropped without [`Self::commit`]
/// takes its temp file with it.
#[derive(Debug)]
pub struct Staged {
    tmp: PathBuf,
    committed: bool,
}

impl Drop for Staged {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

impl Staged {
    /// Put the staged file where `target` is, atomically.
    pub fn commit(mut self, target: &Path) -> Result<(), InstallError> {
        std::fs::rename(&self.tmp, target).map_err(|e| InstallError::io(target, e))?;
        self.committed = true;
        // Without this the rename itself can be lost to a crash, leaving
        // the directory entry pointing at the previous inode. Some
        // filesystems (tmpfs) refuse `fsync` on a directory; that is not
        // a failure.
        if let Some(dir) = target.parent() {
            if let Ok(handle) = File::open(dir) {
                let _ = handle.sync_all();
            }
        }
        Ok(())
    }
}

/// Enough to make a temp name nobody can predict or pre-create.
///
/// Not a cryptographic nonce, and it does not need to be: `O_EXCL` is
/// what makes a collision — accidental or planted — safe, and this only
/// has to make one unlikely enough that the retry loop below never runs.
fn nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    format!("{:x}{:x}{:x}", std::process::id(), nanos, count)
}

/// Write `bytes` into a fresh temp file beside `target`, preserving
/// `target`'s mode.
pub fn stage(target: &Path, bytes: &[u8], create_mode: u32) -> Result<Staged, InstallError> {
    let dir = target.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| InstallError::io(dir, e))?;

    let mode = std::fs::metadata(target)
        .ok()
        .map(|m| m.permissions().mode() & 0o7777)
        .unwrap_or(create_mode);
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "roost".to_string());

    let mut last: Option<InstallError> = None;
    for _ in 0..8 {
        let tmp = dir.join(format!(".{name}.{}.roost-tmp", nonce()));
        // `create_new` is `O_CREAT|O_EXCL`, which never follows a
        // symlink and never opens a file that is already there. A
        // planted sibling of this name — worse, a link to something
        // else — costs one retry instead of the user's data.
        let file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&tmp)
        {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                last = Some(InstallError::io(&tmp, e));
                continue;
            }
            Err(e) => return Err(InstallError::io(&tmp, e)),
        };
        let staged = Staged {
            tmp,
            committed: false,
        };
        // The mode passed to `open` is masked by the umask; this is not.
        let _ = file.set_permissions(std::fs::Permissions::from_mode(mode));
        let mut file = file;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|e| InstallError::io(&staged.tmp, e))?;
        return Ok(staged);
    }
    Err(last
        .unwrap_or_else(|| InstallError::io(dir, std::io::Error::other("no free temp file name"))))
}

/// Replace `target`'s contents atomically, preserving its mode.
pub fn write_atomic(target: &Path, bytes: &[u8], create_mode: u32) -> Result<(), InstallError> {
    stage(target, bytes, create_mode)?.commit(target)
}

/// Remove `target`. An already-absent file is a success — uninstall is
/// idempotent by construction.
pub fn remove(target: &Path) -> Result<(), InstallError> {
    match std::fs::remove_file(target) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(InstallError::io(target, e)),
    }
}

/// The advisory lock held across plan **and** apply.
///
/// The iced UI, the Swift app, `roostctl` and a remote connect can all
/// ensure at the same moment. An atomic rename stops a *torn* file but
/// not a lost update: two readers, two plans built on the same
/// pre-image, and the second rename silently discards the first's work.
/// One lock per home closes that, and the digest re-check turns anything
/// that still slips through into a reported skip rather than a clobber.
#[derive(Debug)]
pub struct HooksLock {
    _file: File,
}

impl Drop for HooksLock {
    fn drop(&mut self) {
        // `flock` belongs to the open file description, so a forked
        // child holding an inherited fd would keep it past our close.
        // Unlock explicitly rather than relying on that.
        let _ = self._file.unlock();
    }
}

/// How long [`lock`] waits for the current holder before it gives up.
///
/// Bounded rather than blocking, because of who waits behind it: an
/// `ensure` on a host session runs holding that session's mutation
/// barrier, and `session.stop` takes the same barrier for write — so an
/// unbounded `flock` on a `$HOME` that may be network-mounted was a
/// wedge that no client disconnect and no shutdown could clear. Ten
/// seconds is far longer than an honest ensure (five small files) and
/// comfortably shorter than the 15 s a client gives
/// `session.set_agent_hooks`, so the caller hears
/// [`InstallError::LockBusy`] instead of timing out on the wire.
pub const LOCK_DEADLINE: Duration = Duration::from_secs(10);

/// How often a waiter re-asks. Short enough that the normal hand-off is
/// imperceptible, long enough that a full deadline is 400 syscalls
/// rather than a spin.
const LOCK_POLL: Duration = Duration::from_millis(25);

/// Take `path`'s lock, waiting at most [`LOCK_DEADLINE`].
pub fn lock(path: &Path) -> Result<HooksLock, InstallError> {
    lock_within(path, LOCK_DEADLINE)
}

/// [`lock`] with the deadline stated, for the tests that need a short
/// one.
pub fn lock_within(path: &Path, deadline: Duration) -> Result<HooksLock, InstallError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| InstallError::io(dir, e))?;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(PRIVATE_MODE)
        .open(path)
        .map_err(|e| InstallError::io(path, e))?;

    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(HooksLock { _file: file }),
            // `flock` is per open file description, so this is reached
            // by a second writer *in this process* too — which is what
            // lets a test prove a check ran while the lock was held.
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(e)) => return Err(InstallError::io(path, e)),
        }
        let waited = started.elapsed();
        if waited >= deadline {
            return Err(InstallError::LockBusy {
                path: path.to_path_buf(),
                waited: deadline,
            });
        }
        std::thread::sleep(LOCK_POLL.min(deadline - waited));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lock waits, and then stops waiting.
    ///
    /// Blocking forever was the defect, not the waiting: an `ensure` on a
    /// host session runs holding that session's mutation barrier, and
    /// `session.stop` takes the same barrier for write — so a holder that
    /// never releases (a crashed writer, a stale `flock` on a network
    /// home) meant the daemon never flushed, never reaped and never
    /// answered. Both halves are asserted: a busy lock is still waited
    /// for, and the wait ends in a named refusal rather than never.
    #[test]
    fn a_lock_nobody_releases_is_refused_at_the_deadline() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent-hooks.lock");
        // `flock` belongs to the open file description, so a second
        // holder in *this* process contends exactly like another one.
        let held = lock(&path).expect("take the lock");

        let started = Instant::now();
        let refused = lock_within(&path, Duration::from_millis(200))
            .expect_err("a lock that never frees must not be waited on forever");
        let waited = started.elapsed();

        assert!(
            matches!(refused, InstallError::LockBusy { .. }),
            "{refused:?}"
        );
        assert!(
            refused.to_string().contains("agent-hooks lock"),
            "{refused}"
        );
        assert!(waited >= Duration::from_millis(200), "{waited:?}");
        assert!(
            waited < Duration::from_secs(5),
            "the bound did not hold: {waited:?}"
        );

        drop(held);
        lock_within(&path, Duration::from_millis(200)).expect("free again once the holder is gone");
    }

    /// The production default is the one the callers reason about: long
    /// enough that an honest ensure never reaches it, short enough that a
    /// client's own 15 s budget for `session.set_agent_hooks` is not what
    /// gives up first.
    #[test]
    fn the_default_deadline_stays_inside_the_op_budget() {
        assert_eq!(LOCK_DEADLINE, Duration::from_secs(10));
    }

    #[test]
    fn a_write_replaces_contents_and_leaves_no_temp_behind() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_atomic(&path, b"{}\n", PRIVATE_MODE).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"{}\n");

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("roost-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn a_file_roost_creates_is_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        write_atomic(&path, b"{}\n", PRIVATE_MODE).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "{mode:o}");
    }

    #[test]
    fn an_existing_file_keeps_the_mode_the_user_chose() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_atomic(&path, b"new", PRIVATE_MODE).unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644, "{mode:o}");
    }

    /// The dotfile-manager case. A regular file where the link was is
    /// how a user's tracked tree silently forks.
    #[test]
    fn a_symlinked_file_is_written_through_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        std::fs::write(&real, b"old\n").unwrap();

        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let image = read_image(&link, PRIVATE_MODE).unwrap();
        assert_eq!(image.target, std::fs::canonicalize(&real).unwrap());
        assert_eq!(image.bytes.as_deref(), Some(&b"old\n"[..]));

        write_atomic(&image.target, b"new\n", PRIVATE_MODE).unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new\n");
    }

    /// A symlinked config *directory* is the other half of the same
    /// problem: the file does not exist yet, so only the parent can be
    /// resolved.
    #[test]
    fn a_file_inside_a_symlinked_directory_lands_in_the_real_one() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/claude");
        std::fs::create_dir_all(&real).unwrap();
        let link = dir.path().join(".claude");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let image = read_image(&link.join("settings.json"), PRIVATE_MODE).unwrap();
        assert_eq!(
            image.target,
            std::fs::canonicalize(&real).unwrap().join("settings.json")
        );
        assert!(!image.exists());
    }

    #[test]
    fn a_file_that_moved_underneath_us_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"first").unwrap();

        let image = read_image(&path, PRIVATE_MODE).unwrap();
        check_unchanged(&image).unwrap();

        std::fs::write(&path, b"second").unwrap();
        assert!(matches!(
            check_unchanged(&image),
            Err(InstallError::ChangedUnderneath { .. })
        ));
    }

    /// Absence is a state, not a missing digest: a file that appeared
    /// after `plan` read nothing is just as much a change.
    #[test]
    fn a_file_that_appeared_underneath_us_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        let image = read_image(&path, PRIVATE_MODE).unwrap();
        assert!(!image.exists());

        std::fs::write(&path, b"someone else got here first").unwrap();
        assert!(matches!(
            check_unchanged(&image),
            Err(InstallError::ChangedUnderneath { .. })
        ));
    }

    #[test]
    fn a_read_only_directory_is_reported_not_retried() {
        let dir = tempfile::tempdir().unwrap();
        let locked = dir.path().join("locked");
        std::fs::create_dir(&locked).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).unwrap();

        let err = write_atomic(&locked.join("settings.json"), b"{}", PRIVATE_MODE).unwrap_err();
        // Restore before the assertion so a failure still cleans up.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(matches!(err, InstallError::ReadOnly { .. }), "{err:?}");
    }

    /// The temp file's name must not be guessable. A sibling of a fixed
    /// name — worse, a *symlink* of it — would otherwise be destroyed,
    /// and `open()` would clobber whatever the link pointed at before
    /// the rename ever happened.
    #[test]
    fn a_symlinked_temp_sibling_is_never_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        std::fs::write(&victim, b"the user wrote this\n").unwrap();
        let target = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&victim, dir.path().join(".settings.json.roost-tmp")).unwrap();

        write_atomic(&target, b"{}\n", PRIVATE_MODE).unwrap();

        assert_eq!(std::fs::read(&victim).unwrap(), b"the user wrote this\n");
        assert_eq!(std::fs::read(&target).unwrap(), b"{}\n");
    }

    /// The same name as an ordinary file. Nothing Roost did not create
    /// may be truncated.
    #[test]
    fn an_existing_temp_sibling_is_never_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let sibling = dir.path().join(".settings.json.roost-tmp");
        std::fs::write(&sibling, b"somebody else's work\n").unwrap();

        write_atomic(&dir.path().join("settings.json"), b"{}\n", PRIVATE_MODE).unwrap();
        assert_eq!(std::fs::read(&sibling).unwrap(), b"somebody else's work\n");
    }

    /// A link whose target does not exist yet is still a link. The write
    /// has to create the target, not replace the link with a regular
    /// file.
    #[test]
    fn a_broken_symlink_is_written_through_not_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("dotfiles/settings.json");
        std::fs::create_dir_all(real.parent().unwrap()).unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let image = read_image(&link, PRIVATE_MODE).unwrap();
        assert!(!image.exists());
        write_atomic(&image.target, b"new\n", PRIVATE_MODE).unwrap();

        assert!(
            link.symlink_metadata().unwrap().file_type().is_symlink(),
            "the broken link was replaced by a regular file"
        );
        assert_eq!(std::fs::read(&real).unwrap(), b"new\n");
    }

    /// The digest was taken on the link's *old* target, so it still
    /// matches after a retarget — and the write would land on a file the
    /// agent no longer reads.
    #[test]
    fn a_symlink_retargeted_since_the_plan_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let old = dir.path().join("old.json");
        let new = dir.path().join("new.json");
        std::fs::write(&old, b"old\n").unwrap();
        std::fs::write(&new, b"new\n").unwrap();
        let link = dir.path().join("settings.json");
        std::os::unix::fs::symlink(&old, &link).unwrap();

        let image = read_image(&link, PRIVATE_MODE).unwrap();
        check_unchanged(&image).unwrap();

        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(&new, &link).unwrap();
        assert!(
            matches!(
                check_unchanged(&image),
                Err(InstallError::ChangedUnderneath { .. })
            ),
            "a retargeted link passed the check"
        );
    }

    /// The digest is re-checked after the replacement is staged, so a
    /// refusal still leaves no temp file behind.
    #[test]
    fn a_staged_replacement_that_is_never_committed_cleans_itself_up() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        std::fs::write(&path, b"first").unwrap();
        let image = read_image(&path, PRIVATE_MODE).unwrap();

        let staged = stage(&path, b"ours", PRIVATE_MODE).unwrap();
        std::fs::write(&path, b"second").unwrap();
        assert!(check_unchanged(&image).is_err());
        drop(staged);

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("roost-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        assert_eq!(std::fs::read(&path).unwrap(), b"second");
    }

    #[test]
    fn bytes_that_are_not_utf8_are_an_error_not_a_substitution() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, [b'a', b'=', 0xff]).unwrap();

        let image = read_image(&path, PRIVATE_MODE).unwrap();
        assert!(image.text().is_err());
        assert_eq!(
            read_image(&dir.path().join("absent"), PRIVATE_MODE)
                .unwrap()
                .text(),
            Ok(None)
        );
    }

    #[test]
    fn removing_an_absent_file_is_success() {
        let dir = tempfile::tempdir().unwrap();
        remove(&dir.path().join("never-existed")).unwrap();
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("roost/agent-hooks.lock");

        let held = lock(&path).unwrap();
        // A second lock on a *different* open file description from the
        // same process still contends, which is what makes this a real
        // check rather than a no-op.
        let path2 = path.clone();
        let waiter = std::thread::spawn(move || {
            let _second = lock(&path2).unwrap();
            true
        });
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(!waiter.is_finished(), "the second lock did not contend");
        drop(held);
        assert!(waiter.join().unwrap());
    }
}
