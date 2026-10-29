//! The Linux session socket's default directory,
//! `<passwd home>/.local/state/<ns>/run`, created and vouched for one
//! level at a time (plan 083 D-A2).
//!
//! [`crate::validate_runtime_dir`] is the wrong rule here. It screens
//! every ancestor for a symlink or a group-writable directory, and a
//! home is exactly where both are ordinary: a dotfile manager symlinks
//! `~/.local`, Fedora Atomic puts `/home` behind a symlink, and a umask
//! of `002` leaves `~/.local` group-writable. Those directories are the
//! user's, so they are only required to *be* directories. What the
//! socket's safety rests on is the two levels this module owns:
//!
//! * `<ns>` — owned by this uid, not a symlink, not group- or
//!   world-writable. Not "exactly `0700`": it is also the session's log
//!   directory, which a user may have `chmod 750`'d.
//! * `run/` — owned by this uid, not a symlink, exactly `0700`. A `run/`
//!   that `IpcServer::bind`'s or `single_instance::acquire`'s
//!   `create_dir_all` materialized at the umask's `0755` is refused, not
//!   repaired, exactly as `validate_runtime_dir` refuses its own leaf.
//!
//! Every step tolerates a concurrent creator — two first starts race
//! here before any lock serializes them — by re-stating a directory a
//! peer made first. `.local/state` may be group-writable, so a member of
//! that group can rename `<ns>` and put a symlink in its place at any
//! moment: `<ns>` and `run/` are therefore created, opened and checked
//! relative to the descriptor of the directory above them
//! (`mkdirat`/`openat` with `O_NOFOLLOW`, `fstat`, `fchmod`), never
//! through a pathname that could resolve somewhere else between two
//! steps. The canonical `.local/state` is what is opened; the literal
//! path is still what is bound and dialed, because it is the one
//! launcher and client both compute — so a swap of `<ns>` between this
//! check and the bind, in a group-writable `.local/state`, remains the
//! one window left, and only that group's members can use it.

use std::ffi::CString;
use std::fs;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use anyhow::anyhow;

use crate::paths::SESSION_SOCKET_DIR_ENV;
use crate::peer::current_euid;

/// Create `<state>/<namespace>/run`, or vouch for the one already there.
///
/// `state` is the literal `<home>/.local/state`.
pub fn prepare_session_socket_dir(state: &Path, namespace: &str) -> anyhow::Result<()> {
    let local = state
        .parent()
        .ok_or_else(|| refused(state, "has no parent directory"))?;
    ensure_users_dir(local)?;
    ensure_users_dir(state)?;
    let state = fs::canonicalize(state).map_err(|error| refused(state, error))?;
    let state_dir = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(&state)
        .map(OwnedFd::from)
        .map_err(|error| refused(&state, error))?;

    let namespace_path = state.join(namespace);
    let namespace_dir = open_own_dir_at(&state_dir, namespace, &namespace_path)?;
    let mode = mode_of(&namespace_dir, &namespace_path)?;
    if mode & 0o022 != 0 {
        return Err(refused(
            &namespace_path,
            format_args!("is group- or world-writable (mode {mode:04o})"),
        ));
    }
    open_run_dir(&namespace_dir, &namespace_path.join("run"))
}

/// `run/` inside the already-vouched-for `<ns>`, reached through that
/// directory's descriptor and nothing else.
fn open_run_dir(namespace_dir: &OwnedFd, run_path: &Path) -> anyhow::Result<()> {
    let run = open_own_dir_at(namespace_dir, "run", run_path)?;
    let mode = mode_of(&run, run_path)?;
    if mode != 0o700 {
        return Err(refused(
            run_path,
            format_args!("has mode {mode:04o}; 0700 is required"),
        ));
    }
    Ok(())
}

fn refused(path: &Path, why: impl std::fmt::Display) -> anyhow::Error {
    anyhow!(
        "session socket directory {}: {why}; fix it, or set {SESSION_SOCKET_DIR_ENV} to put \
         the session socket somewhere else",
        path.display()
    )
}

/// `~/.local` or `~/.local/state`: made `0700` when missing, and an
/// existing one — a symlink to a directory included — left exactly as it
/// is. A dangling symlink reads as missing to the stat and as present to
/// the `mkdir`, so it is refused rather than created through.
fn ensure_users_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    let meta = match fs::metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match fs::DirBuilder::new().mode(0o700).create(path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    fs::metadata(path).map_err(|error| refused(path, error))?
                }
                Err(error) => return Err(refused(path, error)),
            }
        }
        Err(error) => return Err(refused(path, error)),
    };
    if !meta.is_dir() {
        return Err(refused(path, "is not a directory"));
    }
    Ok(())
}

/// `name` in the directory `parent`: made `0700` when missing (`mkdirat`
/// masks with the umask, which only clears bits; the `fchmod` restores
/// owner bits a hostile umask stripped), then opened without following a
/// link and — whoever made it — required to be a directory this uid
/// owns. `shown` is only what errors call it.
fn open_own_dir_at(parent: &OwnedFd, name: &str, shown: &Path) -> anyhow::Result<OwnedFd> {
    let name_c =
        CString::new(name.as_bytes()).map_err(|_| refused(shown, "has a NUL in its name"))?;
    // SAFETY: `parent` is an open directory descriptor and `name_c` a
    // NUL-terminated string, both live for the call.
    let made = unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } == 0;
    if !made {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EEXIST) {
            return Err(refused(shown, error));
        }
    }
    // SAFETY: as above; the flags make it an open of a directory that is
    // not itself a symlink.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name_c.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        return Err(match lstat_at(parent, &name_c) {
            Ok(st) if st.st_mode & libc::S_IFMT == libc::S_IFLNK => refused(shown, "is a symlink"),
            Ok(st) if st.st_mode & libc::S_IFMT != libc::S_IFDIR => {
                refused(shown, "is not a directory")
            }
            _ => refused(shown, error),
        });
    }
    // SAFETY: `openat` just returned this descriptor, and nothing else
    // owns it.
    let dir = unsafe { OwnedFd::from_raw_fd(fd) };
    let st = fstat(&dir).map_err(|error| refused(shown, error))?;
    let euid = current_euid();
    if st.st_uid != euid {
        return Err(refused(
            shown,
            format_args!("is owned by uid {}, not {euid}", st.st_uid),
        ));
    }
    if made {
        // SAFETY: `dir` is an open descriptor this function owns.
        if unsafe { libc::fchmod(dir.as_raw_fd(), 0o700) } != 0 {
            return Err(refused(shown, std::io::Error::last_os_error()));
        }
    }
    Ok(dir)
}

// `st_mode` is a `u32` on Linux and a `u16` on macOS.
#[allow(clippy::useless_conversion)]
fn mode_of(dir: &OwnedFd, shown: &Path) -> anyhow::Result<u32> {
    let st = fstat(dir).map_err(|error| refused(shown, error))?;
    Ok(u32::from(st.st_mode) & 0o7777)
}

fn fstat(fd: &OwnedFd) -> std::io::Result<libc::stat> {
    // SAFETY: an all-zero `stat` is a valid value for `fstat` to fill.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` is open and `st` is writable for the call.
    if unsafe { libc::fstat(fd.as_raw_fd(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

fn lstat_at(parent: &OwnedFd, name: &CString) -> std::io::Result<libc::stat> {
    // SAFETY: as in `fstat`.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: `parent` is open, `name` NUL-terminated, `st` writable.
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            &mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(st)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    use std::path::PathBuf;

    const NS: &str = "roost-session-dev";

    /// A scratch passwd home, never the real one.
    fn home() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        fs::create_dir(&home).unwrap();
        (dir, home)
    }

    fn mode_at(path: &Path) -> u32 {
        use std::os::unix::fs::MetadataExt;
        fs::symlink_metadata(path).unwrap().mode() & 0o7777
    }

    fn chmod(path: &Path, mode: u32) {
        fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
    }

    fn refusal(state: &Path) -> String {
        format!(
            "{:#}",
            prepare_session_socket_dir(state, NS).expect_err("must be refused")
        )
    }

    #[test]
    fn a_fresh_home_gets_every_level_and_an_owner_only_run_dir() {
        let (_dir, home) = home();
        let state = home.join(".local/state");
        prepare_session_socket_dir(&state, NS).expect("a fresh home");
        assert_eq!(mode_at(&home.join(".local")), 0o700);
        assert_eq!(mode_at(&state), 0o700);
        assert_eq!(mode_at(&state.join(NS)), 0o700);
        assert_eq!(mode_at(&state.join(NS).join("run")), 0o700);
        // Idempotent: the second start finds everything in place.
        prepare_session_socket_dir(&state, NS).expect("a second start");
    }

    /// Dotfile managers and Fedora Atomic both put a symlink here; the
    /// directories land in its target.
    #[test]
    fn a_symlinked_state_dir_is_followed() {
        let (dir, home) = home();
        let elsewhere = dir.path().join("dotfiles-state");
        fs::create_dir(&elsewhere).unwrap();
        fs::create_dir(home.join(".local")).unwrap();
        symlink(&elsewhere, home.join(".local/state")).unwrap();
        prepare_session_socket_dir(&home.join(".local/state"), NS).expect("followed");
        assert_eq!(mode_at(&elsewhere.join(NS).join("run")), 0o700);
        assert!(home.join(".local/state").join(NS).join("run").is_dir());
    }

    /// `umask 002` makes `~/.local` group-writable; it is the user's, and
    /// the session's own levels below it are what get checked.
    #[test]
    fn a_group_writable_local_dir_is_accepted() {
        let (_dir, home) = home();
        fs::create_dir(home.join(".local")).unwrap();
        chmod(&home.join(".local"), 0o775);
        prepare_session_socket_dir(&home.join(".local/state"), NS).expect("accepted");
    }

    #[test]
    fn a_symlinked_namespace_dir_is_refused_with_nothing_created_in_its_target() {
        let (dir, home) = home();
        let state = home.join(".local/state");
        fs::create_dir_all(&state).unwrap();
        let target = dir.path().join("someone-elses");
        fs::create_dir(&target).unwrap();
        symlink(&target, state.join(NS)).unwrap();
        let text = refusal(&state);
        assert!(text.contains("is a symlink"), "{text}");
        assert!(text.contains(SESSION_SOCKET_DIR_ENV), "{text}");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "nothing may be created through the link"
        );
    }

    #[test]
    fn a_symlinked_run_dir_is_refused() {
        let (dir, home) = home();
        let state = home.join(".local/state");
        fs::create_dir_all(state.join(NS)).unwrap();
        chmod(&state.join(NS), 0o700);
        let target = dir.path().join("elsewhere-run");
        fs::create_dir(&target).unwrap();
        chmod(&target, 0o700);
        symlink(&target, state.join(NS).join("run")).unwrap();
        let text = refusal(&state);
        assert!(text.contains("is a symlink"), "{text}");
    }

    /// The shape a `create_dir_all` at the default umask leaves when it
    /// runs before this does — refused, never repaired.
    #[test]
    fn a_run_dir_materialized_at_0755_is_refused() {
        let (_dir, home) = home();
        let state = home.join(".local/state");
        let run = state.join(NS).join("run");
        fs::create_dir_all(&run).unwrap();
        chmod(&state.join(NS), 0o700);
        chmod(&run, 0o755);
        let text = refusal(&state);
        assert!(text.contains("0755"), "{text}");
        assert!(text.contains(&run.display().to_string()), "{text}");
        assert!(text.contains(SESSION_SOCKET_DIR_ENV), "{text}");
        assert_eq!(mode_at(&run), 0o755, "refused, not repaired");
    }

    /// `<ns>` is also the log directory: a `chmod 750` there is the
    /// user's call. `run/` has no such excuse.
    #[test]
    fn the_namespace_dir_may_be_0750_and_the_run_dir_may_not() {
        let (_dir, home) = home();
        let state = home.join(".local/state");
        fs::create_dir_all(state.join(NS)).unwrap();
        chmod(&state.join(NS), 0o750);
        prepare_session_socket_dir(&state, NS).expect("a 0750 namespace dir");

        chmod(&state.join(NS).join("run"), 0o750);
        let text = refusal(&state);
        assert!(text.contains("0750"), "{text}");
    }

    #[test]
    fn a_group_writable_namespace_dir_is_refused() {
        let (_dir, home) = home();
        let state = home.join(".local/state");
        fs::create_dir_all(state.join(NS)).unwrap();
        chmod(&state.join(NS), 0o770);
        let text = refusal(&state);
        assert!(text.contains("group- or world-writable"), "{text}");
    }

    #[test]
    fn a_regular_file_where_the_state_dir_belongs_is_a_named_error() {
        let (_dir, home) = home();
        fs::create_dir(home.join(".local")).unwrap();
        let state = home.join(".local/state");
        fs::write(&state, b"not a directory").unwrap();
        let text = refusal(&state);
        assert!(text.contains(&state.display().to_string()), "{text}");
        assert!(text.contains("is not a directory"), "{text}");
        assert!(text.contains(SESSION_SOCKET_DIR_ENV), "{text}");
    }

    #[test]
    fn a_dangling_state_link_is_refused_rather_than_created_through() {
        let (dir, home) = home();
        fs::create_dir(home.join(".local")).unwrap();
        let gone = dir.path().join("gone");
        symlink(&gone, home.join(".local/state")).unwrap();
        refusal(&home.join(".local/state"));
        assert!(!gone.exists(), "nothing created at the link's target");
    }

    /// Plan 083 A2 review: `<ns>` swapped for a symlink after it was
    /// vouched for — what a member of a group-writable `.local/state`'s
    /// group can do — leaves `run/` in the directory that was checked,
    /// never in the link's target.
    #[test]
    fn run_is_made_in_the_namespace_dir_that_was_checked_not_one_swapped_in() {
        let (dir, home) = home();
        let state = home.join(".local/state");
        fs::create_dir_all(&state).unwrap();
        let namespace = state.join(NS);
        fs::create_dir(&namespace).unwrap();
        chmod(&namespace, 0o700);
        let checked = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&namespace)
            .map(OwnedFd::from)
            .unwrap();

        let moved = state.join("moved-aside");
        fs::rename(&namespace, &moved).unwrap();
        let target = dir.path().join("swapped-in");
        fs::create_dir(&target).unwrap();
        chmod(&target, 0o700);
        symlink(&target, &namespace).unwrap();

        open_run_dir(&checked, &namespace.join("run")).expect("run/ in the checked dir");
        assert!(moved.join("run").is_dir(), "made where the check was");
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "nothing may be created through the swapped-in link"
        );
    }

    /// Two first starts race here before any lock serializes them: both
    /// must come through, whichever made each level.
    #[test]
    fn two_concurrent_first_starts_both_succeed() {
        for _ in 0..20 {
            let (_dir, home) = home();
            let state = home.join(".local/state");
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
            let racers: Vec<_> = (0..2)
                .map(|_| {
                    let state = state.clone();
                    let barrier = barrier.clone();
                    std::thread::spawn(move || {
                        barrier.wait();
                        prepare_session_socket_dir(&state, NS)
                    })
                })
                .collect();
            for racer in racers {
                racer.join().unwrap().expect("a lost race is not a failure");
            }
            assert_eq!(mode_at(&state.join(NS).join("run")), 0o700);
        }
    }
}
