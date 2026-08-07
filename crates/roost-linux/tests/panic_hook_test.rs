//! End-to-end cover for the crash-report path: spawns the real binary with
//! `ROOST_TEST_PANIC` set and asserts the on-disk artifacts.
//!
//! The forced panic fires before the single-instance lock, the socket, and
//! any window creation, so these tests are hermetic (all path-deriving env
//! points at a tempdir) and need no display.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::TempDir;

/// Generous: this only has to cover process spawn + dynamic linking of a
/// GUI-toolkit-linked binary on a loaded CI box, not any real work.
const EXIT_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

struct Outcome {
    crash_report: String,
    log: String,
}

/// Run the binary with `ROOST_TEST_PANIC=<variant>`, wait for it to die, and
/// return the crash report plus the log file it wrote under `dir`.
fn run_forced_panic(variant: &str) -> (TempDir, Outcome) {
    let dir = TempDir::new().expect("create tempdir");
    let path = dir.path();

    let mut command = Command::new(env!("CARGO_BIN_EXE_roost"));
    // Selective rather than `env_clear()`: the child still needs the
    // inherited PATH / dyld environment to launch at all. These are the
    // only inherited vars that would redirect it off the tempdir.
    command.env_remove("ROOST_BUNDLE_PROFILE");
    command.env_remove("ROOST_STATE_DIR");
    // The path resolver derives the log dir from HOME on macOS and from
    // XDG_STATE_HOME elsewhere; set every input so either branch lands
    // inside the tempdir.
    command
        .env("HOME", path)
        .env("XDG_STATE_HOME", path)
        .env("XDG_DATA_HOME", path)
        .env("XDG_RUNTIME_DIR", path)
        .env("RUST_LOG", "info")
        .env("ROOST_TEST_PANIC", variant)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let mut child = command.spawn().expect("spawn roost");

    let started = Instant::now();
    let status = loop {
        match child.try_wait().expect("poll child") {
            Some(status) => break status,
            None if started.elapsed() >= EXIT_TIMEOUT => {
                let _ = child.kill();
                let output = child.wait_with_output().expect("collect output after kill");
                panic!(
                    "roost did not exit within {EXIT_TIMEOUT:?} for ROOST_TEST_PANIC={variant}\n{}",
                    describe(&output.stdout, &output.stderr)
                );
            }
            None => std::thread::sleep(POLL_INTERVAL),
        }
    };
    let output = child.wait_with_output().expect("collect output");
    let captured = describe(&output.stdout, &output.stderr);

    // The hook calls `std::process::abort()`, which raises SIGABRT — on unix
    // `code()` is `None`, so only the failure itself is assertable.
    assert!(
        !status.success(),
        "expected a failing exit for ROOST_TEST_PANIC={variant}, got {status:?}\n{captured}"
    );

    let files = find_files(path).expect("walk tempdir");
    let crash_files = matching(&files, |name| {
        name.starts_with("crash-") && name.ends_with(".txt")
    });
    assert_eq!(
        crash_files.len(),
        1,
        "expected exactly one crash file under {}, found {crash_files:?}\n{captured}",
        path.display()
    );
    let crash_report = std::fs::read_to_string(&crash_files[0]).expect("read crash file");

    let logs = matching(&files, |name| name == "roost.log");
    assert_eq!(
        logs.len(),
        1,
        "expected exactly one roost.log under {}, found {logs:?}\n{captured}",
        path.display()
    );
    let log = std::fs::read_to_string(&logs[0]).expect("read roost.log");

    assert!(
        log.contains("panic: crash report written"),
        "roost.log is missing the crash summary\nlog:\n{log}\n{captured}"
    );
    assert!(
        !log.contains("failed to write crash report"),
        "the hook reported a failed crash-file write\nlog:\n{log}\n{captured}"
    );

    (dir, Outcome { crash_report, log })
}

fn describe(stdout: &[u8], stderr: &[u8]) -> String {
    format!(
        "--- child stdout ---\n{}\n--- child stderr ---\n{}",
        String::from_utf8_lossy(stdout),
        String::from_utf8_lossy(stderr)
    )
}

/// Recursively collects every file under `root` in a single walk; the two
/// call sites in `run_forced_panic` each narrow the result with `matching`
/// rather than re-walking the tree per pattern.
fn find_files(root: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        if path.is_dir() {
            found.extend(find_files(&path)?);
        } else {
            found.push(path);
        }
    }
    Ok(found)
}

fn matching(files: &[PathBuf], matches: impl Fn(&str) -> bool) -> Vec<PathBuf> {
    files
        .iter()
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(&matches)
        })
        .cloned()
        .collect()
}

#[test]
fn forced_startup_panic_writes_crash_file_and_aborts() {
    let (_dir, outcome) = run_forced_panic("1");
    let report = &outcome.crash_report;
    assert!(report.contains("ROOST_TEST_PANIC"), "{report}");
    assert!(report.contains("main.rs"), "{report}");
    assert!(report.contains("Roost-gtk"), "{report}");
    assert!(report.contains("backtrace:"), "{report}");
    assert!(outcome.log.contains("ROOST_TEST_PANIC"), "{}", outcome.log);
}

#[test]
fn forced_thread_panic_reports_thread_name() {
    let (_dir, outcome) = run_forced_panic("thread");
    let report = &outcome.crash_report;
    assert!(report.contains("ROOST_TEST_PANIC"), "{report}");
    assert!(report.contains("roost-test-panic"), "{report}");
    assert!(report.contains("backtrace:"), "{report}");
}
