//! `roost-session agent-hook <agent>` — the host's copy of the one hook
//! entrypoint (plan 046 §3.2).
//!
//! The session binary carries the verb itself rather than shipping a
//! `roostctl` beside it: bootstrap installs exactly one file on a host,
//! and an adapter that travelled separately could disagree with the
//! session it reports to. Linking `roost-agent` here makes the mapping
//! version-locked to the daemon by construction.
//!
//! Two ordering facts hold this module together:
//!
//! * **[`run`] early-exits before `Readiness::Stdout`, the profile
//!   resolve and logging init** — exactly like `client-bridge` and
//!   `identify`. A hook fires many times per turn inside a *running*
//!   session's tabs; one printed readiness line would be read as a start
//!   verdict, and opening the log would put a second appender on a file
//!   the live daemon is already writing.
//! * **[`hook_binary`] is called once at `start`**, before a tab exists.
//!   Bootstrap replaces this binary by an atomic rename
//!   (`roost_ipc::bootstrap`), so a still-running old session's
//!   `/proc/self/exe` reads `… (deleted)` — resolving lazily at the
//!   first spawn would hand tabs a path that no longer exists.

use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use roost_agent::hook::{self, hook_payload, parse_tab_id, payload_event_name};
use roost_agent::Agent;
use roost_ipc::messages::ops;
use roost_ipc::session_launch::timeout_scale;
use roost_ipc::IpcClient;

/// The path this session hands every tab as `ROOST_AGENT_HOOK`.
///
/// This binary, canonicalized. The fallback is the one place bootstrap
/// installs into (`$HOME` + [`roost_ipc::bootstrap::INSTALL_DEST_SUFFIX`]),
/// which is where a replacement lands when the running binary's own path
/// has been renamed out from under it. `None` when neither resolves —
/// the caller then omits the variable rather than exporting a path that
/// would exec-fail.
pub fn hook_binary() -> Option<String> {
    resolve(
        std::env::current_exe().ok().as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn resolve(current_exe: Option<&Path>, home: Option<&OsStr>) -> Option<String> {
    let installed = || {
        let home = PathBuf::from(home?);
        home.is_absolute()
            .then(|| {
                let mut path = home.into_os_string();
                path.push(roost_ipc::bootstrap::INSTALL_DEST_SUFFIX);
                PathBuf::from(path)
            })
            // `is_file()` is not enough on this rung: it is reached
            // precisely when `current_exe()` stopped resolving, i.e.
            // while bootstrap is replacing the binary, so a regular file
            // that is not yet (or no longer) executable is exactly what
            // may be sitting there. Exporting it would exec-fail in
            // every tab, where omitting the variable makes the hook
            // wrapper take its inert branch instead.
            .and_then(roost_engine::process::executable_file)
    };
    current_exe
        .and_then(|exe| std::fs::canonicalize(exe).ok())
        .and_then(roost_engine::process::executable_file)
        .or_else(installed)
        .map(|path| path.to_string_lossy().into_owned())
}

/// Run one hook invocation. Always prints `{}` and always returns 0:
/// the whole contract of this path is that a Roost problem can never
/// break the turn it fired from, so every failure below is deliberately
/// swallowed rather than returned.
pub fn run(agent: &str) -> i32 {
    // Drained first and unconditionally — the agent is writing into this
    // pipe right now, and every early return below would otherwise leave
    // it with an EPIPE from a hook that is supposed to be invisible.
    let stdin_buf = drain_stdin();
    dispatch(agent, &stdin_buf);
    answer()
}

/// Read stdin to the shared cap **and keep reading past it**.
///
/// `take(CAP).read_to_end(..)` alone declares EOF at exactly the cap and
/// leaves the rest in the pipe, so a payload one byte over the line
/// hands the writing agent an EPIPE the moment this process exits — the
/// one outcome a hook must never produce. Everything past the cap is
/// discarded (the truncated head no longer parses anyway); what matters
/// is that the writer's `write` returns.
///
/// Duplicated in `roostctl` rather than shared through `roost-agent`:
/// the shared crate's charter is pure functions over bytes and JSON, and
/// each entrypoint keeps its own I/O.
fn drain_stdin() -> Vec<u8> {
    let mut stdin = std::io::stdin().lock();
    let mut buf = Vec::with_capacity(4096);
    let _ = (&mut stdin).take(hook::STDIN_CAP).read_to_end(&mut buf);
    let _ = std::io::copy(&mut stdin, &mut std::io::sink());
    buf
}

fn dispatch(agent: &str, stdin_buf: &[u8]) {
    let Some(adapter) = Agent::parse(agent) else {
        return;
    };
    let Some(tab_id) = std::env::var("ROOST_TAB_ID")
        .ok()
        .as_deref()
        .and_then(parse_tab_id)
    else {
        return;
    };
    // `ROOST_SOCKET` only: resolving a bundle profile is exactly the
    // work this verb exits before doing, and every tab a session spawns
    // carries the variable already.
    let Some(socket) = std::env::var_os("ROOST_SOCKET").filter(|value| !value.is_empty()) else {
        return;
    };
    let Some(payload) = hook_payload(stdin_buf, tab_id) else {
        return;
    };
    let event = payload_event_name(&payload);
    let reports = adapter.event_to_reports(event, &payload, tab_id);
    if reports.is_empty() {
        return;
    }

    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async move {
        let _ = tokio::time::timeout(hook::TOTAL_BUDGET.mul_f64(timeout_scale()), async move {
            // Over SSH the socket at the other end of `ROOST_SOCKET` is
            // local to this machine, so the budget is if anything
            // generous: a dial that does not answer at once is a dead
            // session, not a slow one.
            let connect = tokio::time::timeout(
                hook::CONNECT_TIMEOUT.mul_f64(timeout_scale()),
                IpcClient::connect(Path::new(&socket)),
            )
            .await;
            let Ok(Ok(mut client)) = connect else {
                return;
            };
            for report in reports {
                let _ = client
                    .call::<_, serde_json::Value>(ops::TAB_AGENT_REPORT, report)
                    .await;
            }
        })
        .await;
    });
}

/// `{}` on stdout, whatever happened. A locked, fallible writer rather
/// than `println!` for the same reason `identify` uses one: Rust ignores
/// SIGPIPE, so `println!` would panic when the agent has already gone.
fn answer() -> i32 {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout.write_all(b"{}\n").and_then(|()| stdout.flush());
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn touch_executable(path: &Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[test]
    fn the_running_binary_is_the_answer_when_it_still_exists() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("roost-session");
        touch_executable(&exe);
        assert_eq!(
            resolve(Some(&exe), None),
            Some(
                std::fs::canonicalize(&exe)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    /// `$HOME` + the bootstrap suffix, and the canonical spelling the
    /// resolver answers with (macOS hands out `/var/folders/…`, a
    /// symlink to `/private/var`).
    fn install_dest(home: &Path) -> PathBuf {
        PathBuf::from(format!(
            "{}{}",
            home.display(),
            roost_ipc::bootstrap::INSTALL_DEST_SUFFIX
        ))
    }

    /// The rename case: bootstrap replaced the binary, so this process's
    /// own path no longer resolves. The install destination does, and it
    /// holds the successor.
    #[test]
    fn a_replaced_binary_falls_back_to_the_install_destination() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let installed = install_dest(&home);
        touch_executable(&installed);

        let deleted = dir.path().join("roost-session (deleted)");
        assert_eq!(
            resolve(Some(&deleted), Some(home.as_os_str())),
            Some(
                std::fs::canonicalize(&installed)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    /// The same rung, mid-replacement: a regular file is sitting at the
    /// destination that this user cannot execute. `is_file()` blesses
    /// it, `access(2)` does not — and a `ROOST_AGENT_HOOK` that
    /// exec-fails is worse than an absent one, because only an absent
    /// one lets the hook wrapper take its inert branch.
    #[test]
    fn a_destination_that_cannot_be_executed_is_not_an_answer() {
        // SAFETY: a read of the caller's own effective uid.
        if unsafe { libc::geteuid() } == 0 {
            // root may execute anything with some execute bit, so the
            // distinction does not exist for it.
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let installed = install_dest(&home);
        touch_executable(&installed);
        assert!(resolve(None, Some(home.as_os_str())).is_some());

        std::fs::set_permissions(&installed, std::fs::Permissions::from_mode(0o644)).unwrap();
        let deleted = dir.path().join("roost-session (deleted)");
        assert_eq!(resolve(Some(&deleted), Some(home.as_os_str())), None);
    }

    #[test]
    fn nothing_resolvable_is_answered_with_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let deleted = dir.path().join("roost-session (deleted)");
        assert_eq!(resolve(Some(&deleted), Some(dir.path().as_os_str())), None);
        assert_eq!(resolve(None, None), None);
        // A relative `$HOME` would put the install somewhere that
        // depends on this process's cwd.
        assert_eq!(resolve(None, Some(OsStr::new("relative/home"))), None);
    }
}
