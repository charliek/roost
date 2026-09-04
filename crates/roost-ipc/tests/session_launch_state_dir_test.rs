//! The state dir a launcher hands the `roost-session` it spawns
//! ([#397](https://github.com/charliek/roost/issues/397)), driven over a
//! real child process.
//!
//! # Why these live in their own binary, and must stay there
//!
//! **They fork.** `Command::spawn` forks, and between the fork and the
//! exec the child holds a copy of *every* descriptor the parent had open
//! — `FD_CLOEXEC` closes them at `exec`, not at `fork`. Rust's test
//! harness runs a binary's tests concurrently in one process, so a fork
//! here would briefly duplicate any listener another test in the same
//! binary happens to have bound. That is not hypothetical: while these
//! three were inline in `session_launch.rs`, `socket_state`'s
//! `a_bound_listener_is_live_and_its_corpse_is_stale` failed about 40% of
//! the time in the full `roost-ipc` lib run (0 in 8 with exactly these
//! three skipped, 0 in 6 with `socket_state` alone), because a forked
//! child was still holding the "corpse" listener and `connect` therefore
//! succeeded — `Live` where the test had proved `Stale`. A separate test
//! binary is a separate process, so a fork in this one can never reach a
//! descriptor in that one.
//!
//! Production is unaffected by that race, which is why it was only ever
//! a test flake: `socket_state::probe` treats only `Missing` and `Stale`
//! as proof that nothing is listening, so a spurious `Live` costs a
//! refusal to unlink, never a double-bind.
//!
//! **Not** for the reason the plan originally gave. These used to need
//! their own binary because the test had to set process-global
//! `ROOST_STATE_DIR`, which would have collapsed `paths.rs`'s
//! distinct-profile assertions in the same lib binary. That reason is
//! gone — the seam is a parameter now, so nothing here touches process
//! env — and it was replaced by the sharper one above. It also happens
//! to be what CLAUDE.md asks for (`tests/*_test.rs`).
//!
//! # What is proved here
//!
//! Only what reaches the child's environment; `paths.rs` table-tests the
//! derivation rule itself, and `paths::tests::the_state_dir_env_name_is_frozen`
//! is what keeps the shell script below spelling the variable the same
//! way the Rust does.

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::time::Duration;

use roost_ipc::paths::STATE_DIR_ENV;
use roost_ipc::session_launch::{spawn_and_read_verdict, Verdict, BIN_NAME};

/// Generous on purpose: the stand-in prints and exits at once, so
/// nothing here waits on the clock — the budget only bounds a hang, and
/// a tight one would import `roost-cli`'s readiness flake.
const SEAM_BUDGET: Duration = Duration::from_secs(10);

/// What a child sees when the launcher sets nothing: whatever this
/// process itself carries. Usually nothing — but a developer running the
/// suite under `ROOST_STATE_DIR` still gets a true assertion, because
/// the claim is "the launcher left it alone", not "the variable was
/// absent".
fn ambient_state_dir() -> OsString {
    std::env::var_os(STATE_DIR_ENV).unwrap_or_else(|| OsString::from("UNSET"))
}

/// Spawn a stand-in `roost-session` through the real launcher with
/// `seam` in hand, and hand back the `ROOST_STATE_DIR` it saw.
///
/// The script takes its record path out of `LAUNCH_CWD_ENV` rather than
/// an interpolated literal, so no tempdir path has to survive shell
/// quoting, and the answer is read as bytes, so no path has to be UTF-8.
/// `${VAR-UNSET}` (not `${VAR:-UNSET}`) tells an unset variable from an
/// empty one.
async fn state_dir_handed_to(seam: Option<&OsStr>) -> (tempfile::TempDir, OsString) {
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let bin = dir.path().join(BIN_NAME);
    std::fs::write(
        &bin,
        "#!/bin/sh\nPATH=/usr/bin:/bin\nexport PATH\n\
         printf '%s' \"${ROOST_STATE_DIR-UNSET}\" > \"$ROOST_SESSION_LAUNCH_CWD/seen\"\n\
         echo 'ready pid=4321'\n",
    )
    .expect("write the stand-in session");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let verdict = spawn_and_read_verdict(&bin, dir.path(), seam, SEAM_BUDGET)
        .await
        .expect("the stand-in reports a verdict");
    assert_eq!(verdict, Verdict::Ready(4321));

    let seen =
        std::fs::read(dir.path().join("seen")).expect("the stand-in records what it was handed");
    (dir, OsString::from_vec(seen))
}

#[tokio::test]
async fn a_seam_gives_the_session_a_state_dir_nested_in_the_launchers_own() {
    let isolated = tempfile::tempdir().expect("temp dir");
    let (_dir, seen) = state_dir_handed_to(Some(isolated.path().as_os_str())).await;
    assert_eq!(PathBuf::from(seen), isolated.path().join("session"));
}

#[tokio::test]
async fn no_seam_leaves_the_childs_environment_alone() {
    let (_dir, seen) = state_dir_handed_to(None).await;
    assert_eq!(seen, ambient_state_dir());
}

/// A value the resolver ignores derives nothing — and is not forwarded
/// either: the launcher sets no variable at all, so the child's
/// environment is the one `None` leaves it.
#[tokio::test]
async fn a_seam_the_resolver_ignores_sets_nothing_on_the_child() {
    for raw in ["", "relative/state"] {
        let (_dir, seen) = state_dir_handed_to(Some(OsStr::new(raw))).await;
        assert_eq!(seen, ambient_state_dir(), "{raw:?}");
    }
}
