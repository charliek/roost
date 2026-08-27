//! Create-or-validate the directory an IPC socket is about to be bound
//! in.
//!
//! The socket's own `0600` mode is only worth what the directory holding
//! it is worth: a world-writable or attacker-controlled parent lets
//! someone else unlink our socket and bind their own at the same path,
//! and every client dials the path. So the leaf is ours (`0700`, our
//! uid), no component of the path is a symlink, and no ancestor is
//! world-writable without the sticky bit.
//!
//! # Call order
//!
//! [`validate_runtime_dir`] runs **before** the stale-socket
//! probe → unlink → bind sequence in [`crate::IpcServer::bind`]. That
//! ordering is the contract: probing (and especially unlinking) inside a
//! directory that has not been vouched for is the thing this function
//! exists to prevent. HS-0 lands the function and its tests; the session
//! server wires it in at HS-1, so nothing in production calls it yet.
//!
//! The ordering is functional as well as security-critical, which is the
//! part that bites late: [`crate::IpcServer::bind`] and
//! `roost_engine::single_instance::acquire` both `create_dir_all` this
//! directory at umask-default mode (typically `0755`), and this function
//! rejects rather than repairs — so a leaf either of them materializes
//! first is refused from then on, with no recovery short of an `rmdir`.
//! HS-1 therefore has to validate before it takes the socket lock, not
//! merely before it binds.
//!
//! # What this deliberately does not do
//!
//! * **Bound-inode identity.** Recording `(dev, ino)` at bind and
//!   unlinking only a socket that still matches at shutdown is HS-1.
//! * **Ancestor ownership.** Ancestors are screened only for symlinks,
//!   non-directories, and world-writability-without-sticky — not for
//!   foreign ownership or group-writability. A runtime dir deliberately
//!   nested under another principal's directory is outside the threat
//!   model; the intended paths (`$XDG_RUNTIME_DIR`, `~/Library`, sticky
//!   `/tmp`) never construct one.
//! * **Closing the validate→bind window.** Validation and bind happen in
//!   one process, microseconds apart, in a directory whose ancestors we
//!   just proved are not world-writable-without-sticky — so the only
//!   party who could swap the leaf in that window is the one who already
//!   owns it (us, or root). Accepted for HS-0; the flock over the
//!   probe→bind sequence is the other half of the answer.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

use anyhow::{bail, Context};

use crate::peer::current_euid;

/// Create `path` as a `0700` directory, or validate the one already
/// there.
///
/// Rejects (never repairs): a leaf that is a symlink, a non-directory,
/// owned by another uid, or carrying any bit outside `0700`; any symlink
/// component among the ancestors; any ancestor that is world-writable
/// without the sticky bit.
///
/// The symlink rule is strict on purpose — a symlinked component is
/// indistinguishable from a redirected one — which means callers must
/// pass a path that is already real. On macOS `$TMPDIR` sits under
/// `/var`, itself a symlink to `private/var`, so anything rooted there
/// has to be canonicalized first.
pub fn validate_runtime_dir(path: impl AsRef<Path>) -> anyhow::Result<()> {
    let path = path.as_ref();
    if !path.is_absolute() {
        bail!(
            "runtime dir {} is not absolute; a relative path resolves against the \
             process cwd, which cannot be validated",
            path.display()
        );
    }

    let mut ancestors: Vec<&Path> = path.ancestors().skip(1).collect();
    ancestors.reverse();
    for ancestor in ancestors {
        check_ancestor(ancestor)?;
    }

    check_leaf(path)
}

/// The two rejections the leaf and every ancestor share: a symlink
/// component is indistinguishable from a redirected one, and a
/// non-directory cannot hold the socket. Shared so a future tightening
/// of either rule cannot land on one caller and miss the other.
fn reject_non_dir(meta: &fs::Metadata, path: &Path, what: &str) -> anyhow::Result<()> {
    if meta.file_type().is_symlink() {
        bail!(
            "{what} {} is a symlink; refusing to bind under a redirectable path",
            path.display()
        );
    }
    if !meta.is_dir() {
        bail!(
            "{what} {} is not a directory (file type {:?})",
            path.display(),
            meta.file_type()
        );
    }
    Ok(())
}

fn check_ancestor(dir: &Path) -> anyhow::Result<()> {
    let meta = fs::symlink_metadata(dir)
        .with_context(|| format!("stat runtime-dir ancestor {}", dir.display()))?;
    reject_non_dir(&meta, dir, "runtime-dir ancestor")?;
    let mode = meta.mode() & 0o7777;
    // The sticky bit is what makes `/tmp` legal as an ancestor: it is
    // world-writable, but sticky means only the owner of an entry can
    // rename or unlink it, so nobody else can swap our leaf directory
    // out from under us. World-writable without it is a free rename.
    if mode & 0o002 != 0 && mode & 0o1000 == 0 {
        bail!(
            "runtime-dir ancestor {} is world-writable without the sticky bit (mode {:04o})",
            dir.display(),
            mode
        );
    }
    Ok(())
}

fn check_leaf(path: &Path) -> anyhow::Result<()> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            // `mkdir` masks the mode with the umask, which can only
            // clear bits — never widen — so the directory is never more
            // permissive than 0700 for even an instant. The chmod that
            // follows only restores owner bits a hostile umask stripped.
            fs::DirBuilder::new()
                .mode(0o700)
                .create(path)
                .with_context(|| format!("create runtime dir {}", path.display()))?;
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", path.display()))?;
            return Ok(());
        }
        Err(err) => {
            return Err(err).with_context(|| format!("stat runtime dir {}", path.display()))
        }
    };

    reject_non_dir(&meta, path, "runtime dir")?;
    let euid = current_euid();
    if meta.uid() != euid {
        bail!(
            "runtime dir {} is owned by uid {}, not {}",
            path.display(),
            meta.uid(),
            euid
        );
    }
    let mode = meta.mode() & 0o7777;
    if mode != 0o700 {
        bail!(
            "runtime dir {} has mode {:04o}; 0700 is required",
            path.display(),
            mode
        );
    }
    Ok(())
}
