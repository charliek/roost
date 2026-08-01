//! Toolkit-neutral bounded child-process execution for UI adapters.
//!
//! Providers and other dynamic integrations build their domain-specific argv,
//! environment, stdin, and output contracts outside this module. This service
//! owns only the runtime hazards: process spawning, explicit environment
//! removal, cwd selection, stdin delivery, timeout, cancellation, exit-status
//! reporting, and owned stdout.

use std::path::{Path, PathBuf};
use std::process::Stdio;
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

/// Resolve an executable shipped beside the current process.
pub fn sibling_executable(name: &str) -> Option<String> {
    let executable = std::env::current_exe().ok()?;
    sibling_executable_from(&executable, name).map(|path| path.to_string_lossy().into_owned())
}

/// Testable path-level form of [`sibling_executable`]. Symlinked launch paths
/// resolve to the real install directory before selecting the sibling.
pub fn sibling_executable_from(executable: &Path, name: &str) -> Option<PathBuf> {
    let executable = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    let sibling = executable.parent()?.join(name);
    let metadata = std::fs::metadata(&sibling).ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0).then_some(sibling)
    }
    #[cfg(not(unix))]
    {
        metadata.is_file().then_some(sibling)
    }
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
}
