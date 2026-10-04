//! roost-input-mac: the Mac real-input harness's helper (plan 074 §D7).
//!
//! One invocation runs one command and prints one JSON object. The lifecycle is
//! the same for every command, because the process may be posting into a
//! desktop someone else is using:
//!
//! * `<outdir>/pid` is written before anything else, so a wrapper that gives up
//!   on the process can find and kill it (its own process group);
//! * `<outdir>/held.json` journals what it holds, rewritten atomically before
//!   each press is posted and after each release, so a wrapper whose helper
//!   died holding something releases exactly that (`release-held --file`) and
//!   nothing the person at the desk is holding;
//! * a watchdog enforces `--deadline-ms`: on expiry it releases every
//!   modifier, key and button this process still holds, then exits 124;
//! * SIGTERM, SIGINT or SIGHUP release the same way, then exit 128 + signal;
//!   a panic releases before it unwinds (exit 101);
//! * every other exit path releases what is still held, and reports it as
//!   `released`.
//!
//! Exit codes: 0 done, 2 usage, 3 unavailable (a missing grant: the wrapper's
//! skip), 4 refused (the console locked or switched away, Secure Input on, the
//! target stopped being frontmost, another app holds a key window, or a click
//! at a point would reach another app), 5 failed, 124 deadline.

mod args;
// Off macOS this binary is a stub that exists to run the pure unit tests.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod input;
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
mod keys;
#[cfg(target_os = "macos")]
mod mac;

use serde_json::{json, Value};
use std::io::Write;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Mutex;
use std::time::Duration;

#[derive(Debug)]
pub enum Failure {
    Unavailable(String),
    Refused(String),
    Failed(String),
}

impl Failure {
    fn kind(&self) -> &'static str {
        match self {
            Failure::Unavailable(_) => "unavailable",
            Failure::Refused(_) => "refused",
            Failure::Failed(_) => "failed",
        }
    }

    fn message(&self) -> &str {
        match self {
            Failure::Unavailable(message)
            | Failure::Refused(message)
            | Failure::Failed(message) => message,
        }
    }

    fn exit_code(&self) -> u8 {
        match self {
            Failure::Unavailable(_) => 3,
            Failure::Refused(_) => 4,
            Failure::Failed(_) => 5,
        }
    }
}

pub type Outcome = Result<Value, Failure>;

const EXIT_USAGE: u8 = 2;
const EXIT_DEADLINE: u8 = 124;

#[cfg(target_os = "macos")]
use mac as platform;

#[cfg(not(target_os = "macos"))]
mod platform {
    use super::{Failure, Outcome};
    use crate::args::Invocation;
    use serde_json::{json, Value};

    pub fn own_process_group() {}

    pub fn catch_termination(_on_signal: fn(i32)) {}

    pub fn release_after_panic() -> Value {
        json!([])
    }

    pub fn run(_invocation: &Invocation) -> Outcome {
        Err(Failure::Unavailable(
            "roost-input-mac drives macOS only".into(),
        ))
    }

    pub fn release_held() -> Value {
        json!([])
    }

    pub fn expire() -> Value {
        json!([])
    }
}

/// Whether the one JSON line has been printed. The watchdog and the main
/// thread can both reach the end at once; only the first speaks.
static PRINTED: Mutex<bool> = Mutex::new(false);

fn emit_once(value: &Value) -> bool {
    let mut printed = PRINTED.lock().unwrap_or_else(|poison| poison.into_inner());
    if *printed {
        return false;
    }
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{value}");
    let _ = stdout.flush();
    *printed = true;
    true
}

/// Write through a sibling `.tmp`, so a reader polling for `path` never sees
/// it half-written.
pub fn write_atomically(path: &Path, contents: &str) -> std::io::Result<()> {
    let staged = path.with_extension("tmp");
    std::fs::write(&staged, contents)?;
    std::fs::rename(staged, path)
}

fn failure_body(command: Option<&str>, kind: &str, error: &str, released: Value) -> Value {
    json!({"ok": false, "command": command, "kind": kind, "error": error, "released": released})
}

/// The signal thread's answer to SIGTERM, SIGINT or SIGHUP.
fn terminated(signal: i32) {
    let error = format!("terminated by signal {signal}");
    emit_once(&failure_body(
        None,
        "terminated",
        &error,
        platform::expire(),
    ));
    std::process::exit(128 + signal);
}

fn start_watchdog(deadline: Duration, command: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(deadline);
        let error = format!("the {} ms deadline passed", deadline.as_millis());
        let body = failure_body(Some(command), "deadline", &error, platform::expire());
        if emit_once(&body) {
            std::process::exit(i32::from(EXIT_DEADLINE));
        }
    });
}

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    if let Some(outdir) = args::outdir_hint(&raw) {
        let pid = outdir.join("pid");
        if let Err(error) = write_atomically(&pid, &format!("{}\n", std::process::id())) {
            let failure = Failure::Failed(format!("cannot write {}: {error}", pid.display()));
            emit_once(&failure_body(
                None,
                failure.kind(),
                failure.message(),
                json!([]),
            ));
            return ExitCode::from(failure.exit_code());
        }
    }
    platform::own_process_group();
    platform::catch_termination(terminated);
    std::panic::set_hook(Box::new(|info| {
        let released = platform::release_after_panic();
        eprintln!("{info}");
        emit_once(&failure_body(None, "panic", &info.to_string(), released));
    }));
    let invocation = match args::parse(&raw) {
        Ok(invocation) => invocation,
        Err(error) => {
            emit_once(&failure_body(None, "usage", &error, json!([])));
            return ExitCode::from(EXIT_USAGE);
        }
    };
    let command = invocation.command.name();
    start_watchdog(invocation.deadline, command);

    let outcome = platform::run(&invocation);
    let released = platform::release_held();
    let (code, body) = match outcome {
        // Every command answers with a JSON object.
        Ok(mut body) => {
            body["ok"] = true.into();
            body["command"] = command.into();
            body["released"] = released;
            (0, body)
        }
        Err(failure) => (
            failure.exit_code(),
            failure_body(Some(command), failure.kind(), failure.message(), released),
        ),
    };
    if !emit_once(&body) {
        // The watchdog already reported the deadline and is exiting.
        std::thread::sleep(Duration::from_secs(1));
        return ExitCode::from(EXIT_DEADLINE);
    }
    ExitCode::from(code)
}
