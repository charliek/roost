//! Toolkit-neutral bounded child-process execution for UI adapters.
//!
//! Providers and other dynamic integrations build their domain-specific argv,
//! environment, stdin, and output contracts outside this module. This service
//! owns only the runtime hazards: process spawning, explicit environment
//! removal, cwd selection, stdin delivery, timeout, cancellation, exit-status
//! reporting, and owned stdout.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use tokio::io::AsyncWriteExt;

#[derive(Debug, Clone)]
pub struct ProcessRequest {
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub stdin: Vec<u8>,
    pub cwd: Option<PathBuf>,
    pub timeout: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessOutput {
    pub stdout: String,
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("process argv is empty")]
    EmptyArgv,
    #[error("spawn process: {0}")]
    Spawn(#[source] std::io::Error),
    #[error("write process stdin: {0}")]
    Stdin(#[source] std::io::Error),
    #[error("process timed out after {0:?}")]
    Timeout(Duration),
    #[error("process io error: {0}")]
    Io(#[source] std::io::Error),
    #[error("process exited with status {code}")]
    Exit { code: i32 },
    #[error("process exited {code}: {stderr_tail}")]
    ExitWithStderr { code: i32, stderr_tail: String },
}

/// Run one owned request without blocking a UI thread.
///
/// `kill_on_drop(true)` makes timeout and task cancellation terminate the
/// child. No callback or UI object crosses this boundary.
pub async fn run(request: ProcessRequest) -> Result<ProcessOutput, ProcessError> {
    let Some(program) = request.argv.first() else {
        return Err(ProcessError::EmptyArgv);
    };
    let mut command = tokio::process::Command::new(program);
    command.args(&request.argv[1..]);
    for key in request.env_remove {
        command.env_remove(key);
    }
    for (key, value) in request.env {
        command.env(key, value);
    }
    if let Some(cwd) = request.cwd.filter(|path| path.is_dir()) {
        command.current_dir(cwd);
    }
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = command.spawn().map_err(ProcessError::Spawn)?;
    let stdin = child.stdin.take();
    let stdin_bytes = request.stdin;
    let communicate = async move {
        // Drain stdout/stderr while delivering stdin. Sequential delivery can
        // deadlock when the child fills an output pipe before reading input.
        let write_stdin = async move {
            if let Some(mut stdin) = stdin {
                if let Err(error) = stdin.write_all(&stdin_bytes).await {
                    // A provider is allowed to rely entirely on env/argv and
                    // close stdin immediately. EPIPE therefore means "input
                    // declined", not that its stdout/exit status should be
                    // discarded. Other write failures remain explicit.
                    if error.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(ProcessError::Stdin(error));
                    }
                }
            }
            Ok(())
        };
        let wait_output = async move { child.wait_with_output().await.map_err(ProcessError::Io) };
        let (_, output) = tokio::try_join!(write_stdin, wait_output)?;
        Ok::<_, ProcessError>(output)
    };
    let output = tokio::time::timeout(request.timeout, communicate)
        .await
        .map_err(|_| ProcessError::Timeout(request.timeout))??;
    if !output.status.success() {
        let code = output.status.code().unwrap_or(-1);
        let stderr_tail = String::from_utf8_lossy(&output.stderr)
            .lines()
            .last()
            .unwrap_or("")
            .trim()
            .to_string();
        return Err(if stderr_tail.is_empty() {
            ProcessError::Exit { code }
        } else {
            ProcessError::ExitWithStderr { code, stderr_tail }
        });
    }
    Ok(ProcessOutput {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
    })
}

/// Resolve an executable shipped beside `executable` — rung 1 of
/// [`roostctl_path`]'s ladder. Symlinked launch paths resolve to the real
/// install directory before selecting the sibling.
pub fn sibling_executable_from(executable: &Path, name: &str) -> Option<PathBuf> {
    let executable = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    executable_file(executable.parent()?.join(name))
}

/// A real file at `path` that **this** user may execute, as its
/// canonical absolute path — or `None`.
///
/// Canonicalized on every rung rather than only on the ones that start
/// from `current_exe()`: the answer is exported into child processes
/// with cwds of their own, so a relative or symlink-spelled path is a
/// path they cannot run. `canonicalize` also settles absoluteness, which
/// [`resolve_roostctl`] then re-checks as a fence.
pub fn executable_file(path: impl AsRef<Path>) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path.as_ref()).ok()?;
    (path.is_absolute() && path.is_file() && is_executable_by_current_user(&path)).then_some(path)
}

/// Whether the calling user may `execve` `path`.
///
/// `access(2)` rather than a permission-bit test: mode `0o001` on a file
/// this user owns has an execute bit set and still fails with `EACCES`,
/// and the same goes for a file on a `noexec` mount or behind a
/// directory this user cannot traverse. The kernel is the only thing
/// that knows the answer, and exporting a path that cannot be run is
/// strictly worse than exporting none — the hook wrapper can only take
/// its inert branch when it sees the variable unset.
#[cfg(unix)]
pub fn is_executable_by_current_user(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: `c_path` is a NUL-terminated string that outlives the call,
    // and `access` only reads it.
    unsafe { libc::access(c_path.as_ptr(), libc::X_OK) == 0 }
}

#[cfg(not(unix))]
pub fn is_executable_by_current_user(_path: &Path) -> bool {
    true
}

const ROOSTCTL: &str = "roostctl";

/// Roost's own `roostctl`, as an absolute path — the value handed to
/// providers as `ROOST_ROOSTCTL` and to every spawned tab as
/// `ROOST_AGENT_HOOK` (plan 046 §3.2).
///
/// Three rungs, tried in order:
///
/// 1. a sibling of the running executable — `target/debug`, `/usr/bin`,
///    and `Roost.app/Contents/MacOS` all put the two binaries together;
/// 2. `<bundle>/Contents/Resources/bin/roostctl` relative to the
///    executable, which is where `mac/scripts/bundle-iced.sh` embeds it
///    — the rung [`sibling_executable_from`] alone misses, which is why
///    `ROOST_ROOSTCTL` was absent inside `Roost-Iced.app`;
/// 3. `PATH`.
///
/// `None` when no rung answers. Callers **omit** their variable in that
/// case rather than exporting a guess: a path that does not resolve is
/// worse than an absent one, because the hook wrapper can only fall back
/// to its inert branch when it can see the variable is unset.
pub fn roostctl_path() -> Option<String> {
    let executable = std::env::current_exe().ok();
    resolve_roostctl(executable.as_deref(), std::env::var_os("PATH").as_deref())
        .map(|path| path.to_string_lossy().into_owned())
}

fn resolve_roostctl(executable: Option<&Path>, path_var: Option<&OsStr>) -> Option<PathBuf> {
    let executable = executable
        .map(|exe| std::fs::canonicalize(exe).unwrap_or_else(|_| exe.to_path_buf()))
        // A relative `current_exe()` would make both of the first two
        // rungs relative too, and this value is exported into shells
        // with cwds of their own.
        .filter(|exe| exe.is_absolute());
    executable
        .as_deref()
        .and_then(|exe| sibling_executable_from(exe, ROOSTCTL))
        .or_else(|| executable.as_deref().and_then(bundled_roostctl))
        .or_else(|| path_var.and_then(|path| lookup_on_path(path, ROOSTCTL)))
        // The fence [`executable_file`]'s canonicalize already satisfies,
        // restated because every caller exports this into a shell with a
        // cwd of its own and a relative answer is unrunnable there.
        .filter(|path| path.is_absolute())
}

/// `Contents/MacOS/<exe>` → `Contents/Resources/bin/roostctl`. The
/// layout is asserted by the file being there and executable rather than
/// by matching directory names, so a bundle that grows a level does not
/// need a second rule here.
fn bundled_roostctl(executable: &Path) -> Option<PathBuf> {
    let contents = executable.parent()?.parent()?;
    executable_file(contents.join("Resources").join("bin").join(ROOSTCTL))
}

/// `PATH`, minus the entries that only mean something to a process with
/// this cwd. An empty entry is `PATH`'s own spelling of "the current
/// directory", and a relative entry is the same thing written out — both
/// would resolve here and then be wrong in the tab that inherits the
/// answer.
fn lookup_on_path(path_var: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path_var)
        .filter(|dir| dir.is_absolute())
        .find_map(|dir| executable_file(dir.join(name)))
}

/// The binary a tab's `ROOST_AGENT_HOOK` names, pinned once per process.
///
/// [`set_agent_hook_binary`] exists for `roost-session`, whose answer is
/// itself rather than a `roostctl`; every UI leaves it unset and gets
/// [`roostctl_path`] on first use.
static AGENT_HOOK_BINARY: OnceLock<Option<String>> = OnceLock::new();

/// Pin the hook binary before any tab exists. Returns whether this call
/// is the one that set it.
///
/// `roost-session` calls this from `start`, deliberately early:
/// bootstrap replaces the session binary by an atomic rename, so a
/// still-running old session's `/proc/self/exe` reads `… (deleted)` and
/// a lazy resolve at the first spawn would hand tabs a path that no
/// longer exists.
pub fn set_agent_hook_binary(path: Option<String>) -> bool {
    AGENT_HOOK_BINARY.set(path).is_ok()
}

/// The absolute path every spawned tab is told to invoke as
/// `"$ROOST_AGENT_HOOK" agent-hook <agent>`, or `None` when this build
/// has nothing to point at (a `swift run`/`cargo run` tree with no CLI
/// built beside it).
pub fn agent_hook_binary() -> Option<&'static str> {
    AGENT_HOOK_BINARY.get_or_init(roostctl_path).as_deref()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(script: &str) -> ProcessRequest {
        ProcessRequest {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: Vec::new(),
            env_remove: Vec::new(),
            stdin: Vec::new(),
            cwd: None,
            timeout: Duration::from_secs(2),
        }
    }

    #[tokio::test]
    async fn carries_stdin_environment_and_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let mut request =
            shell("read value; printf '%s|%s|%s' \"$value\" \"$ROOST_VALUE\" \"$PWD\"");
        request.stdin = b"input\n".to_vec();
        request.env = vec![("ROOST_VALUE".into(), "environment".into())];
        request.cwd = Some(dir.path().to_path_buf());
        let output = run(request).await.unwrap();
        let canonical_dir = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            output.stdout,
            format!("input|environment|{}", canonical_dir.display())
        );
    }

    #[tokio::test]
    async fn empty_argv_and_nonzero_exit_are_explicit() {
        let mut empty = shell("");
        empty.argv.clear();
        assert!(matches!(run(empty).await, Err(ProcessError::EmptyArgv)));

        let error = run(shell("printf 'last detail\\n' >&2; exit 7"))
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            ProcessError::ExitWithStderr {
                code: 7,
                ref stderr_tail
            } if stderr_tail == "last detail"
        ));
    }

    #[tokio::test]
    async fn child_may_decline_stdin_without_losing_its_result() {
        let mut request = shell("exec 0<&-; printf done");
        request.stdin = vec![b'x'; 1024 * 1024];
        assert_eq!(run(request).await.unwrap().stdout, "done");
    }

    #[tokio::test]
    async fn drains_large_output_while_delivering_large_stdin() {
        let mut request = shell("head -c 1048576 /dev/zero; cat >/dev/null");
        request.stdin = vec![b'x'; 1024 * 1024];
        assert_eq!(run(request).await.unwrap().stdout.len(), 1024 * 1024);
    }

    #[tokio::test]
    async fn timeout_is_bounded() {
        let mut request = shell("sleep 2");
        request.timeout = Duration::from_millis(25);
        assert!(matches!(
            run(request).await,
            Err(ProcessError::Timeout(duration)) if duration == Duration::from_millis(25)
        ));
    }

    #[test]
    fn sibling_requires_an_executable_file() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let executable = dir.path().join("roost");
        let sibling = dir.path().join("roostctl");
        std::fs::write(&executable, "app").unwrap();
        std::fs::write(&sibling, "cli").unwrap();
        assert_eq!(sibling_executable_from(&executable, "roostctl"), None);
        let mut permissions = std::fs::metadata(&sibling).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&sibling, permissions).unwrap();
        assert_eq!(
            sibling_executable_from(&executable, "roostctl"),
            Some(std::fs::canonicalize(sibling).unwrap())
        );
    }

    /// Write an executable `roostctl` at `path`, creating its parents.
    fn stub_roostctl(path: PathBuf) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;

        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(&path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&path, permissions).unwrap();
        path
    }

    /// A temporary directory under its *real* path.
    ///
    /// macOS hands out `/var/folders/…`, a symlink to `/private/var`.
    /// Every rung canonicalizes its answer, so a test that built its
    /// expectations under the symlink would be comparing two spellings
    /// of one path rather than one answer.
    fn real_tempdir() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        (dir, root)
    }

    /// The ladder's four outcomes (plan 046 §3.2). Each rung is checked
    /// with the rungs above it deliberately empty, so a test cannot pass
    /// by falling through to the wrong answer.
    #[test]
    fn the_ladder_prefers_a_sibling() {
        let (_dir, root) = real_tempdir();
        let executable = root.join("Contents/MacOS/Roost-Iced");
        let sibling = stub_roostctl(root.join("Contents/MacOS/roostctl"));
        // Present but lower down: the sibling must win it.
        stub_roostctl(root.join("Contents/Resources/bin/roostctl"));
        std::fs::write(&executable, "app").unwrap();
        assert_eq!(
            resolve_roostctl(Some(&executable), None),
            Some(sibling),
            "rung 1 must win"
        );
    }

    #[test]
    fn the_ladder_finds_the_bundles_resources_copy() {
        let (_dir, root) = real_tempdir();
        let executable = root.join("Contents/MacOS/Roost-Iced");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "app").unwrap();
        let embedded = stub_roostctl(root.join("Contents/Resources/bin/roostctl"));
        assert_eq!(resolve_roostctl(Some(&executable), None), Some(embedded));
    }

    #[test]
    fn the_ladder_falls_through_to_path() {
        let (_dir, root) = real_tempdir();
        let executable = root.join("bin/roost-iced");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "app").unwrap();
        let on_path = stub_roostctl(root.join("elsewhere/roostctl"));
        // An empty entry is `PATH`'s spelling of "the current directory",
        // which is never an answer this value may carry.
        let path_var = format!(":{}", root.join("elsewhere").display());
        assert_eq!(
            resolve_roostctl(Some(&executable), Some(OsStr::new(&path_var))),
            Some(on_path)
        );
        // …and with no executable at all, PATH is still the answer.
        assert!(resolve_roostctl(None, Some(OsStr::new(&path_var))).is_some());
    }

    /// `PATH=bin` is legal and means "a `bin` under whatever cwd the
    /// process has". The answer is exported into tabs with cwds of their
    /// own, so a rung that resolved it would hand them a path they
    /// cannot execute.
    ///
    /// Non-vacuous by construction: the same directory is offered twice,
    /// once spelled relative to this process's cwd (where the file is
    /// really there and really executable) and once absolute.
    #[test]
    fn the_ladder_refuses_a_relative_path_entry() {
        let (_dir, root) = real_tempdir();
        let elsewhere = root.join("elsewhere");
        stub_roostctl(elsewhere.join("roostctl"));

        let relative = relative_to_cwd(&elsewhere);
        assert!(
            relative.join("roostctl").is_file(),
            "the relative spelling has to actually resolve, or this proves nothing"
        );
        assert_eq!(resolve_roostctl(None, Some(relative.as_os_str())), None);
        // …and the same directory named absolutely is still an answer,
        // so the refusal is about the spelling and nothing else.
        assert!(resolve_roostctl(None, Some(elsewhere.as_os_str())).is_some());
    }

    /// `mode & 0o111 != 0` is not "this user may run it": a file this
    /// user owns with mode `0o001` has an execute bit for *others* and
    /// exec-fails with `EACCES`. Only `access(2)` knows.
    #[test]
    fn the_ladder_refuses_a_file_this_user_cannot_execute() {
        use std::os::unix::fs::PermissionsExt;

        // SAFETY: a read of the caller's own effective uid.
        if unsafe { libc::geteuid() } == 0 {
            // root's `access(X_OK)` succeeds on any file with *some*
            // execute bit, so the distinction this test draws does not
            // exist for it.
            return;
        }

        let (_dir, root) = real_tempdir();
        let elsewhere = root.join("elsewhere");
        let stub = stub_roostctl(elsewhere.join("roostctl"));
        assert!(resolve_roostctl(None, Some(elsewhere.as_os_str())).is_some());

        std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o001)).unwrap();
        assert!(
            std::fs::metadata(&stub).unwrap().permissions().mode() & 0o111 != 0,
            "the mode test this replaces would still pass here"
        );
        assert_eq!(resolve_roostctl(None, Some(elsewhere.as_os_str())), None);
    }

    /// A `PATH` entry is the user's own spelling — `/var/…` on macOS
    /// where the real path is `/private/var/…`, or any directory
    /// symlink. Rungs 1 and 2 canonicalize; rung 3 has to as well, or
    /// the same install answers with two different strings depending on
    /// which rung found it.
    #[test]
    fn a_path_hit_is_canonicalized_like_every_other_rung() {
        let (_dir, root) = real_tempdir();
        let real = root.join("real");
        stub_roostctl(real.join("roostctl"));
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(
            resolve_roostctl(None, Some(link.as_os_str())),
            Some(real.join("roostctl"))
        );
    }

    /// `path`, spelled relative to this process's cwd — `../../..` back
    /// to the root, then down again.
    fn relative_to_cwd(path: &Path) -> PathBuf {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let up: PathBuf = cwd.components().skip(1).map(|_| "..").collect();
        up.join(path.strip_prefix("/").unwrap())
    }

    #[test]
    fn the_ladder_answers_nothing_when_no_rung_holds() {
        let (_dir, root) = real_tempdir();
        let executable = root.join("bin/roost-iced");
        std::fs::create_dir_all(executable.parent().unwrap()).unwrap();
        std::fs::write(&executable, "app").unwrap();
        let empty = root.join("nothing-here");
        std::fs::create_dir_all(&empty).unwrap();
        assert_eq!(
            resolve_roostctl(Some(&executable), Some(empty.as_os_str())),
            None
        );
        assert_eq!(resolve_roostctl(None, None), None);
    }
}
