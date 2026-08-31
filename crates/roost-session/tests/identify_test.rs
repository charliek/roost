//! `roost-session identify`, driven as a real process — the black-box
//! half of plan 039's C1: the pure gate logic is unit-tested on
//! `identity::build_identity` directly (`src/identity.rs`), and this
//! file pins what a caller of the actual binary sees, matching the
//! spawn-and-assert shape `client_bridge_test.rs` uses for the other
//! subcommand.

use std::process::Command;

use roost_ipc::messages::SessionBinaryIdentity;

#[test]
fn identify_prints_one_json_line_and_exits_clean() {
    let output = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("identify")
        .env_remove("ROOST_TEST_MODE")
        .env_remove("ROOST_SESSION_FAKE_BUILD")
        .output()
        .expect("run roost-session identify");

    assert!(output.status.success(), "identify must exit 0: {output:?}");
    assert!(
        output.stderr.is_empty(),
        "identify must be silent on stderr: {:?}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "exactly one line: {stdout:?}");

    let identity: SessionBinaryIdentity =
        serde_json::from_str(lines[0]).expect("stdout parses as SessionBinaryIdentity");
    assert_eq!(identity.app_version, env!("CARGO_PKG_VERSION"));
    assert_eq!(
        identity.session_protocol,
        roost_ipc::messages::SESSION_PROTOCOL_VERSION
    );
    assert!(
        !identity.libghostty_build.is_empty(),
        "libghostty_build must be non-empty"
    );
}

#[test]
fn identify_needs_no_socket_or_profile_environment() {
    // Pointing HOME/XDG_RUNTIME_DIR nowhere real would break profile
    // resolution or socket dialing; identify must not touch either.
    let output = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("identify")
        .env("HOME", "/nonexistent-for-this-test")
        .env("XDG_RUNTIME_DIR", "/nonexistent-for-this-test")
        .env_remove("ROOST_TEST_MODE")
        .env_remove("ROOST_SESSION_FAKE_BUILD")
        .output()
        .expect("run roost-session identify");

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
    serde_json::from_str::<SessionBinaryIdentity>(stdout.trim())
        .expect("still a valid SessionBinaryIdentity with no real HOME");
}

/// Plan 039 §3.1's double gate has two halves: `build_identity`'s own
/// `test_mode` parameter (unit-tested directly in `src/identity.rs`) and
/// `test_mode_env`'s reading of `ROOST_TEST_MODE` from the process
/// environment, which only a real subprocess can exercise. CI e2e lanes
/// run with `ROOST_TEST_MODE=1` inherited, so these three cases pin the
/// environment half against a regression in that reader that the unit
/// tests alone would not catch.
mod env_gate {
    use super::*;

    const SENTINEL: &str = "identify-test-env-gate-sentinel";

    fn identify_with(test_mode: Option<&str>) -> SessionBinaryIdentity {
        let mut command = Command::new(env!("CARGO_BIN_EXE_roost-session"));
        command
            .arg("identify")
            .env("ROOST_SESSION_FAKE_BUILD", SENTINEL);
        match test_mode {
            Some(value) => {
                command.env("ROOST_TEST_MODE", value);
            }
            None => {
                command.env_remove("ROOST_TEST_MODE");
            }
        }
        let output = command.output().expect("run roost-session identify");
        assert!(output.status.success(), "{output:?}");
        let stdout = String::from_utf8(output.stdout).expect("stdout is utf-8");
        serde_json::from_str(stdout.trim()).expect("stdout parses as SessionBinaryIdentity")
    }

    #[test]
    fn fake_build_is_ignored_when_test_mode_is_absent() {
        let identity = identify_with(None);
        assert_ne!(identity.libghostty_build, SENTINEL);
    }

    #[test]
    fn fake_build_is_ignored_when_test_mode_is_zero() {
        let identity = identify_with(Some("0"));
        assert_ne!(identity.libghostty_build, SENTINEL);
    }

    #[test]
    fn fake_build_is_reported_when_test_mode_is_one() {
        let identity = identify_with(Some("1"));
        assert_eq!(identity.libghostty_build, SENTINEL);
    }
}

/// Plan 039 §3.1 relies on an old, pre-`identify` `roost-session` build
/// reading as "needs upgrade" during bootstrap: clap's default handling
/// of an unrecognized subcommand exits non-zero, which is what the
/// probe treats as "no identity, old build" rather than as a crash. This
/// pins that old-CLI behavior for the record — it is not new behavior
/// this commit adds, it is a fact about clap's `Subcommand` derive that
/// a future change to `Command` must not accidentally break.
#[test]
fn an_unknown_subcommand_exits_non_zero_the_way_an_old_binary_would() {
    let output = Command::new(env!("CARGO_BIN_EXE_roost-session"))
        .arg("definitely-not-a-subcommand")
        .output()
        .expect("run roost-session with a bogus subcommand");

    assert!(
        !output.status.success(),
        "an unknown subcommand must fail: {output:?}"
    );
}
