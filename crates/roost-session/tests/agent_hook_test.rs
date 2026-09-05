//! `roost-session agent-hook`, driven as a real process — the black-box
//! half of plan 046's C5.
//!
//! The pure path resolution is unit-tested on `agent_hook::resolve`
//! (`src/agent_hook.rs`); what only a subprocess can prove is the
//! **ordering**: the verb has to exit before `Readiness::Stdout`, the
//! profile resolve and `logging::init`, exactly like `client-bridge`
//! and `identify`. It fires many times per turn inside a *running*
//! session's tabs, so a readiness line on stdout would be read as a
//! start verdict, and opening the log would put a second appender on a
//! file the live daemon already owns.

use std::path::Path;
use std::process::{Command, Stdio};

/// Run the verb with a `$HOME` and `$XDG_*` pointing into `home`, and no
/// `ROOST_SOCKET`, so nothing it could reach exists.
fn agent_hook(home: &Path, agent: &str, stdin: &[u8]) -> std::process::Output {
    use std::io::Write;

    let mut child = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("agent-hook")
        .arg(agent)
        .env("HOME", home)
        .env("XDG_RUNTIME_DIR", home.join("run"))
        .env("XDG_STATE_HOME", home.join("state"))
        .env("XDG_CACHE_HOME", home.join("cache"))
        .env("ROOST_TAB_ID", "7")
        .env_remove("ROOST_SOCKET")
        .env_remove("ROOST_TEST_MODE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run roost-session agent-hook");
    child
        .stdin
        .take()
        .expect("stdin is piped")
        .write_all(stdin)
        .expect("write the payload");
    child.wait_with_output().expect("collect the output")
}

/// Every path answers `{}` and exit 0 — a real agent, an agent with no
/// adapter, and a body that does not parse. Claude's and codex's
/// `PermissionRequest` are decision hooks whose dialog waits on this
/// process, and anything else may be read as a block.
#[test]
fn every_path_answers_an_empty_object_and_exits_clean() {
    let dir = tempfile::tempdir().unwrap();
    for (agent, stdin) in [
        (
            "claude",
            &br#"{"hook_event_name":"Stop","session_id":"s-1"}"#[..],
        ),
        ("amp", &b"{}"[..]),
        ("claude", &b"not json at all"[..]),
        ("claude", &b""[..]),
    ] {
        let output = agent_hook(dir.path(), agent, stdin);
        assert!(output.status.success(), "{agent}: {output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "{}",
            "{agent} must answer exactly one empty object"
        );
    }
}

/// The ordering assertion. `Readiness::Stdout` prints `ready pid=N` /
/// `already-running` / `error: …`; `logging::init` creates
/// `<log dir>/roost.log`; `prepare_directories` creates the state and
/// runtime dirs. None of it may happen here — so the jailed `$HOME`
/// stays completely untouched, and stdout carries the `{}` and nothing
/// else.
#[test]
fn the_verb_exits_before_readiness_the_profile_and_the_log() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("jail");
    std::fs::create_dir(&home).unwrap();

    let output = agent_hook(&home, "claude", br#"{"hook_event_name":"Stop"}"#);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "exactly one line: {stdout:?}");
    for verdict in ["ready", "already-running", "error:"] {
        assert!(
            !stdout.contains(verdict),
            "a readiness verdict escaped onto stdout: {stdout:?}"
        );
    }
    assert!(
        output.stderr.is_empty(),
        "the verb must be silent on stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let left_behind: Vec<_> = std::fs::read_dir(&home)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        left_behind.is_empty(),
        "the verb created state under $HOME: {left_behind:?}"
    );
}

/// A payload larger than the 1 MiB cap is *drained*, not abandoned.
///
/// `take(CAP).read_to_end(..)` stops at exactly the cap and leaves the
/// rest in the pipe, so the writer — the agent, mid-turn — gets an EPIPE
/// the moment this process exits. The cap is about how much is parsed;
/// it may never become how much is read.
#[test]
fn a_payload_over_the_cap_is_drained_rather_than_abandoned() {
    use std::io::Write;

    let dir = tempfile::tempdir().unwrap();
    // Valid JSON, and comfortably over the 1 MiB cap: what is asserted
    // is the writer's side, not what the adapter made of it.
    let mut payload = br#"{"hook_event_name":"Stop","session_id":"s-1","pad":""#.to_vec();
    payload.resize(payload.len() + 2 * 1024 * 1024, b'x');
    payload.extend_from_slice(br#""}"#);

    let mut child = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("agent-hook")
        .arg("claude")
        .env("HOME", dir.path())
        .env("ROOST_TAB_ID", "7")
        .env_remove("ROOST_SOCKET")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run roost-session agent-hook");

    let mut stdin = child.stdin.take().expect("stdin is piped");
    let written = stdin
        .write_all(&payload)
        .and_then(|()| stdin.flush())
        .map_err(|e| e.kind());
    drop(stdin);
    let output = child.wait_with_output().expect("collect the output");

    assert_eq!(written, Ok(()), "the verb stopped reading mid-payload");
    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "{}");
}

/// A profile that cannot resolve is the sharpest form of the same
/// assertion: `start` fails on it, and this verb never asks.
#[test]
fn a_broken_profile_environment_does_not_reach_the_verb() {
    let output = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("agent-hook")
        .arg("claude")
        .env_remove("HOME")
        .env("XDG_RUNTIME_DIR", "relative-and-invalid")
        .env_remove("ROOST_TAB_ID")
        .env_remove("ROOST_SOCKET")
        .stdin(Stdio::null())
        .output()
        .expect("run roost-session agent-hook");

    assert!(output.status.success(), "{output:?}");
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "{}");
}
