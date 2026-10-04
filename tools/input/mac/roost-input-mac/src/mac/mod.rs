//! The macOS commands.

mod ax;
mod cf;
mod ffi;
mod post;
mod session;
mod tap;

use crate::args::{Command, Invocation, MouseAction, Rect};
use crate::input::{self, point_json, Policy};
use crate::keys::{self, Modifier};
use crate::{Failure, Outcome};
use post::MacDesktop;
use serde_json::{json, Value};
use std::path::Path;

/// Its own process group, so a wrapper that gives up on this process can kill
/// it and anything it started (`screencapture`) without reaching its parent.
pub fn own_process_group() {
    // SAFETY: setpgid on this process; a session leader (a direct-mode child
    // started in a new session) refuses, and is already its own group.
    unsafe { ffi::setpgid(0, 0) };
}

/// Route SIGTERM, SIGINT and SIGHUP to one thread that calls `on_signal`, so a
/// helper told to stop releases what it holds (in ordinary thread context:
/// posting events is not async-signal-safe). Called before any other thread
/// starts, so every thread inherits the blocked mask.
pub fn catch_termination(on_signal: fn(i32)) {
    let mut set: ffi::sigset_t = 0;
    // SAFETY: a local signal set, then this thread's mask; threads spawned
    // afterwards inherit it.
    unsafe {
        ffi::sigemptyset(&mut set);
        for signal in [ffi::SIGTERM, ffi::SIGINT, ffi::SIGHUP] {
            ffi::sigaddset(&mut set, signal);
        }
        ffi::pthread_sigmask(ffi::SIG_BLOCK, &set, std::ptr::null_mut());
    }
    std::thread::spawn(move || {
        let mut signal = 0;
        // SAFETY: waits on the set this process blocked above.
        if unsafe { ffi::sigwait(&set, &mut signal) } == 0 {
            on_signal(signal);
        }
    });
}

/// Release what this process still holds (every exit path).
pub fn release_held() -> Value {
    input::release(&MacDesktop, &mut input::held())
}

/// The deadline or a signal: stop pressing, release what is held.
pub fn expire() -> Value {
    let mut held = input::held();
    held.expire();
    input::release(&MacDesktop, &mut held)
}

/// From the panic hook (`input::release_after_panic`): mid-step, what this
/// process's own journal says it holds.
pub fn release_after_panic() -> Value {
    input::release_after_panic(&MacDesktop, &input::HELD, || {
        let file = post::JOURNAL
            .get()
            .ok_or("no --outdir, so no journal: nothing released")?;
        read_journal(file).map_err(|failure| failure.message().to_string())
    })
}

pub fn run(invocation: &Invocation) -> Outcome {
    if let Some(outdir) = &invocation.outdir {
        let _ = post::JOURNAL.set(outdir.join("held.json"));
    }
    let policy = Policy {
        allow_secure_input: invocation.allow_secure_input,
    };
    match &invocation.command {
        Command::Preflight { pid } => session::preflight(*pid),
        Command::Window { pid } => {
            ax_target(*pid)?;
            ax::window(*pid)
        }
        Command::WindowSet { pid, frame } => {
            ax_target(*pid)?;
            ax::window_set(*pid, *frame)
        }
        Command::Key {
            pid,
            codes,
            modifiers,
        } => key(*pid, codes, modifiers, policy),
        Command::ReleaseAll => {
            require_posting()?;
            post::release_all()
        }
        Command::Claimants { pid } => {
            target(*pid)?;
            ax::claimants(*pid)
        }
        Command::Mouse { pid, action } => mouse(*pid, action, policy),
        Command::MenuBar { pid } => {
            ax_target(*pid)?;
            ax::menu_bar(*pid)
        }
        Command::Popup { pid, at, wait } => {
            ax_target(*pid)?;
            ax::popup(*pid, *at, *wait)
        }
        Command::Press {
            pid,
            path,
            at,
            wait,
        } => {
            ax_target(*pid)?;
            ax::press(*pid, path, *at, *wait)
        }
        Command::EventTap { duration } => tap::listen(*duration, invocation.outdir.as_deref()),
        Command::Capture { rect, out } => {
            capture(*rect, out.as_deref(), invocation.outdir.as_deref())
        }
        Command::ReleaseHeld { file } => {
            require_posting()?;
            release_from_journal(file)
        }
    }
}

/// Release exactly what another helper's journal says it still holds — its
/// own presses, posted unchecked because leaving them down is worse — and
/// leave in that journal only what failed to post (`input::release_journaled`).
fn release_from_journal(file: &Path) -> Outcome {
    let held = read_journal(file)?;
    let released = input::release_journaled(&MacDesktop, &post::FileJournal(file), held)?;
    Ok(json!({"file": file, "released_from_journal": released}))
}

fn read_journal(file: &Path) -> Result<input::Held, Failure> {
    let text = std::fs::read_to_string(file)
        .map_err(|error| Failure::Failed(format!("{}: {error}", file.display())))?;
    let journal: Value = serde_json::from_str(text.trim())
        .map_err(|error| Failure::Failed(format!("{}: {error}", file.display())))?;
    input::Held::from_json(&journal).map_err(Failure::Failed)
}

fn target(pid: i32) -> Result<(), Failure> {
    if session::alive(pid) {
        Ok(())
    } else {
        Err(Failure::Failed(format!("pid {pid} is not running")))
    }
}

fn ax_target(pid: i32) -> Result<(), Failure> {
    target(pid)?;
    ax::require_trusted()
}

fn require_posting() -> Result<(), Failure> {
    // SAFETY: a read-only check (never the requesting variant).
    if unsafe { ffi::CGPreflightPostEventAccess() } {
        Ok(())
    } else {
        Err(Failure::Unavailable(
            "posting events is not granted to this process (Accessibility)".into(),
        ))
    }
}

fn key(pid: i32, codes: &[u16], modifiers: &[Modifier], policy: Policy) -> Outcome {
    require_posting()?;
    target(pid)?;
    input::perform(
        &MacDesktop,
        &input::HELD,
        &input::chord(pid, codes, modifiers),
        policy,
    )?;
    Ok(json!({
        "pid": pid,
        "codes": codes,
        "flags": modifiers.iter().map(|modifier| modifier.name).collect::<Vec<_>>(),
        "event_flags": keys::flags_for(modifiers),
    }))
}

fn mouse(pid: i32, action: &MouseAction, policy: Policy) -> Outcome {
    require_posting()?;
    target(pid)?;
    input::perform(
        &MacDesktop,
        &input::HELD,
        &input::mouse(pid, action),
        policy,
    )?;
    let (name, points) = match action {
        MouseAction::Move(at) => ("move", vec![*at]),
        MouseAction::Down(at, _) => ("down", vec![*at]),
        MouseAction::Up(at, _) => ("up", vec![*at]),
        MouseAction::Click(at) => ("click", vec![*at]),
        MouseAction::RightClick(at) => ("right-click", vec![*at]),
        MouseAction::CtrlClick(at) => ("ctrl-click", vec![*at]),
        MouseAction::Drag { path, .. } => ("drag", path.clone()),
    };
    Ok(json!({
        "pid": pid,
        "action": name,
        "points": points.into_iter().map(point_json).collect::<Vec<_>>(),
    }))
}

fn capture(rect: Rect, out: Option<&Path>, outdir: Option<&Path>) -> Outcome {
    // SAFETY: a read-only check (never the requesting variant).
    if !unsafe { ffi::CGPreflightScreenCaptureAccess() } {
        return Err(Failure::Unavailable(
            "screen capture is not granted (Screen Recording)".into(),
        ));
    }
    let path = match (out, outdir) {
        (Some(out), _) => out.to_path_buf(),
        (None, Some(dir)) => dir.join("capture.png"),
        (None, None) => return Err(Failure::Failed("capture needs --out or --outdir".into())),
    };
    let region = format!(
        "{},{},{},{}",
        rect.x.round(),
        rect.y.round(),
        rect.width.round(),
        rect.height.round()
    );
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-R", &region])
        .arg(&path)
        .status()
        .map_err(|error| Failure::Failed(format!("screencapture: {error}")))?;
    if !status.success() {
        return Err(Failure::Failed(format!("screencapture exited {status}")));
    }
    let bytes = std::fs::metadata(&path)
        .map_err(|error| Failure::Failed(format!("{}: {error}", path.display())))?
        .len();
    if bytes == 0 {
        return Err(Failure::Failed(format!("{} is empty", path.display())));
    }
    Ok(json!({"path": path, "bytes": bytes, "rect": region}))
}
