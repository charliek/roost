//! Panic hook that turns a Rust panic into a crash report on disk.
//!
//! A panic hook runs while the process is already coming apart —
//! possibly on a thread with corrupted invariants, possibly during
//! shutdown. It must never itself panic (a panic inside a panic
//! hook aborts immediately with no report written), so every step
//! here is best-effort and errors are swallowed rather than
//! returned. This is the sanctioned exception to the repo's
//! "errors are returned, not logged-and-swallowed" rule.

use std::backtrace::Backtrace;
use std::fs::OpenOptions;
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Number of numeric-suffix retries attempted on a crash-file name
/// collision (`crash-<secs>-<pid>-1.txt`, `-2.txt`, ...) before giving
/// up on writing the file. Collisions require two panics landing in
/// the same process within the same wall-clock second, which is rare
/// enough that a small cap is plenty.
const MAX_COLLISION_RETRIES: u32 = 9;

/// Install a process-global panic hook that writes a crash report to
/// `log_dir`, echoes it to stderr, logs a one-line summary, then
/// aborts.
///
/// This REPLACES the default hook (no chaining to the prior hook) —
/// the crash file + stderr copy are the durable record; the default
/// hook's stderr-only output would be redundant.
pub fn install_panic_hook(log_dir: PathBuf, app_label: &'static str, version: &'static str) {
    std::panic::set_hook(Box::new(move |info| {
        let backtrace = Backtrace::force_capture();
        let report = format_report(app_label, version, info, &backtrace);

        // Written first: this is the artifact a user attaches to a
        // bug report, so it must exist even if everything after
        // this line fails.
        let now_secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let pid = std::process::id();
        let crash_file = write_crash_file(&log_dir, &report, now_secs, pid);

        // Not `eprintln!` — it panics if stderr is gone (EPIPE, closed
        // fd 2), which would double-panic the hook.
        {
            use std::io::Write;
            let _ = writeln!(std::io::stderr().lock(), "{report}");
        }

        // `tracing::error!` goes LAST, right before abort. If the
        // panic originated inside a tracing writer (e.g. a poisoned
        // subscriber-internal lock), calling back into tracing could
        // deadlock the process instead of aborting it. By this point
        // the crash file and stderr copy already exist, so a hung
        // tracing call here loses only the one-line log summary, not
        // the report itself.
        let payload = panic_payload(info);
        let first_line = payload.lines().next().unwrap_or("");
        let location = panic_location(info);
        match &crash_file {
            Some(path) => {
                tracing::error!(
                    location = %location,
                    payload = %first_line,
                    crash_file = %path.display(),
                    "panic: crash report written"
                );
            }
            None => {
                tracing::error!(
                    location = %location,
                    payload = %first_line,
                    "panic: failed to write crash report"
                );
            }
        }

        std::process::abort();
    }));
}

fn panic_payload(info: &PanicHookInfo<'_>) -> String {
    info.payload_as_str()
        .unwrap_or("<non-string payload>")
        .to_string()
}

fn panic_location(info: &PanicHookInfo<'_>) -> String {
    info.location()
        .map(|loc| loc.to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Build the full crash report text. Pure and panic-free so it can
/// be unit tested without triggering a real panic.
fn format_report(
    app_label: &str,
    version: &str,
    info: &PanicHookInfo<'_>,
    backtrace: &Backtrace,
) -> String {
    let thread_name = std::thread::current()
        .name()
        .unwrap_or("<unnamed>")
        .to_string();
    let payload = panic_payload(info);
    let location = panic_location(info);

    format!(
        "app: {app_label}\n\
         version: {version}\n\
         os: {os}\n\
         arch: {arch}\n\
         thread: {thread_name}\n\
         location: {location}\n\
         panic: {payload}\n\
         \n\
         backtrace:\n\
         {backtrace}\n",
        os = std::env::consts::OS,
        arch = std::env::consts::ARCH,
    )
}

/// Write `report` to `<log_dir>/crash-<now_secs>-<pid>.txt`, retrying
/// with numeric suffixes on a same-second collision. Returns the path
/// written on success, `None` if every attempt failed (directory
/// creation failed, permissions, out of retries, ...) — the caller
/// still proceeds, since stderr already has the report.
fn write_crash_file(log_dir: &Path, report: &str, now_secs: u64, pid: u32) -> Option<PathBuf> {
    // Best-effort: if this fails, the `OpenOptions` call below will
    // also fail and we return `None` the same way.
    let _ = std::fs::create_dir_all(log_dir);

    let base_name = format!("crash-{now_secs}-{pid}");
    let mut candidate = log_dir.join(format!("{base_name}.txt"));
    let mut attempt = 0u32;
    loop {
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(mut file) => {
                // Only report success for a fully written file — a
                // partial artifact would make the "crash report
                // written" log line a lie; stderr still has the full
                // report either way.
                if file.write_all(report.as_bytes()).is_ok() && file.flush().is_ok() {
                    return Some(candidate);
                }
                let _ = std::fs::remove_file(&candidate);
                return None;
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                attempt += 1;
                if attempt > MAX_COLLISION_RETRIES {
                    return None;
                }
                candidate = log_dir.join(format!("{base_name}-{attempt}.txt"));
            }
            Err(_) => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tempfile::tempdir;

    // `std::panic::set_hook`/`take_hook` are process-global, and cargo
    // runs tests in parallel on separate threads within the same
    // process by default — two tests swapping the hook concurrently
    // would race. Serialize the tests that touch it.
    static PANIC_HOOK_LOCK: Mutex<()> = Mutex::new(());

    /// Install a temporary hook that runs `format_report` and captures
    /// its output, trigger `do_panic` under `catch_unwind`, then
    /// restore the prior hook and return the captured report. Callers
    /// must hold `PANIC_HOOK_LOCK`.
    fn capture_report_via_hook(
        app_label: &'static str,
        version: &'static str,
        do_panic: impl FnOnce() + std::panic::UnwindSafe,
    ) -> String {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let captured_clone = captured.clone();
        // While this temp hook is installed, an unrelated test failing
        // in parallel also lands here — filter to this test's thread so
        // its panic can't overwrite the captured report.
        let test_thread = std::thread::current().id();
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if std::thread::current().id() != test_thread {
                return;
            }
            let backtrace = Backtrace::force_capture();
            let report = format_report(app_label, version, info, &backtrace);
            *captured_clone.lock().unwrap() = Some(report);
        }));
        let result = std::panic::catch_unwind(do_panic);
        std::panic::set_hook(prev);
        assert!(result.is_err());
        let report = captured.lock().unwrap().take().expect("hook ran");
        report
    }

    #[test]
    fn format_report_contains_expected_fields() {
        let _guard = PANIC_HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let report = capture_report_via_hook("roost-test", "9.9.9", || {
            panic!("boom at the disco");
        });
        assert!(report.contains("boom at the disco"), "{report}");
        assert!(report.contains("roost-test"), "{report}");
        assert!(report.contains("9.9.9"), "{report}");
        assert!(report.contains(std::env::consts::OS), "{report}");
        assert!(report.contains(std::env::consts::ARCH), "{report}");
        assert!(report.contains("crash.rs"), "{report}");
        assert!(report.contains("thread:"), "{report}");
        assert!(report.contains("backtrace:"), "{report}");
    }

    #[test]
    fn write_crash_file_lands_exactly_one_file_with_matching_contents() {
        let dir = tempdir().unwrap();
        let report = "hello crash report";
        let path = write_crash_file(dir.path(), report, 1_700_000_000, 4242)
            .expect("write should succeed");

        let entries: Vec<_> = std::fs::read_dir(dir.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "expected exactly one crash file");

        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(contents, report);

        let file_name = path.file_name().unwrap().to_str().unwrap();
        assert_eq!(file_name, "crash-1700000000-4242.txt");
    }

    #[test]
    fn write_crash_file_suffixes_on_collision() {
        let dir = tempdir().unwrap();
        let secs = 1_700_000_000;
        let pid = 4242;

        let first = write_crash_file(dir.path(), "first", secs, pid).unwrap();
        let second = write_crash_file(dir.path(), "second", secs, pid).unwrap();

        assert_ne!(first, second);
        assert!(first.exists());
        assert!(second.exists());
        assert_eq!(std::fs::read_to_string(&first).unwrap(), "first");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "second");
        assert!(
            second.file_name().unwrap().to_str().unwrap().contains("-1"),
            "expected numeric suffix in {second:?}"
        );
    }

    #[test]
    fn write_crash_file_returns_none_for_unwritable_dir() {
        let dir = tempdir().unwrap();
        let regular_file = dir.path().join("not_a_dir");
        std::fs::write(&regular_file, b"x").unwrap();
        let unwritable = regular_file.join("sub");

        let result = write_crash_file(&unwritable, "report", 1_700_000_000, 1);
        assert!(result.is_none());
    }

    #[test]
    fn format_report_does_not_panic_on_degenerate_inputs() {
        let _guard = PANIC_HOOK_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        // Non-string payload and (in practice, always-present under
        // `catch_unwind`) location — exercises the downcast fallback
        // path via a payload type other than &str/String.
        let report = capture_report_via_hook("", "", || {
            std::panic::panic_any(42i32);
        });
        assert!(report.contains("<non-string payload>"), "{report}");
        assert!(!report.is_empty());
    }
}
