//! Roost Linux UI — daemon-removed (M3b).
//!
//! Thin entry point that:
//! 1. Resolves the GTK bundle profile (paths + app id).
//! 2. Acquires the single-instance flock so a second launch
//!    activates the existing window rather than racing for the
//!    socket.
//! 3. Constructs the in-process `Workspace` + `PtySupervisor`.
//! 4. Binds the JSON `IpcServer` on the profile's socket path so
//!    `roostctl` and Claude hooks have a target.
//! 5. Hands a `LocalClient` to the gtk4-rs `App`.

mod app;
mod cell_metrics;
mod clipboard;
mod config;
mod custom_command;
mod events;
mod key_encoder;
mod keybind;
mod notification_inbox;
mod palette;
mod palette_ui;
mod paste_image;
mod provider;
mod rollup;
mod sprite;
mod tab_session;
mod terminal_view;
mod theme;
mod url_launcher;

use std::sync::Arc;

use std::fs::OpenOptions;
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Context;
use gtk4::glib::{self, LogWriterOutput};
use libadwaita::prelude::*;
use libadwaita::Application;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{fmt, EnvFilter};

use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::{IpcClient, IpcServer};
use roost_linux::daemon::{PtySupervisor, Workspace};
use roost_linux::ipc::IpcHandler;
use roost_linux::local_client::LocalClient;
use roost_linux::single_instance;

use crate::app::App;

// Matches `BundleProfile::gtk().app_id` (roost-common).
const APP_ID: &str = "ai.stridelabs.Roost.gtk";

/// Rate-limit window for the glib log writer. Within each window at most
/// [`LOG_MAX_PER_WINDOW`] messages reach the default writer; the overflow is
/// counted and collapsed into one summary line. This bounds the blast radius
/// of a per-frame glib warning (issue #234): a widget that emits a
/// `g_warning`/`g_critical` every main-loop iteration can no longer peg a
/// core formatting timestamps in `log_writer_default`, nor flood a redirected
/// stderr to gigabytes. The first window's worth still prints, so the warning
/// text stays diagnosable.
const LOG_WINDOW: Duration = Duration::from_secs(1);
/// 20/s sits well above glib's steady-state chatter (near-zero), so normal
/// diagnostics are never throttled, yet it's low enough that a storm is
/// bounded almost immediately — and the first window still passes enough
/// lines to capture the offending warning's text. Storms here are
/// warnings/criticals, so the limit is applied to every level alike
/// (exempting criticals would just reopen the blast radius).
const LOG_MAX_PER_WINDOW: u32 = 20;

struct LogThrottle {
    window_start: Option<Instant>,
    emitted: u32,
    suppressed: u64,
}

static LOG_THROTTLE: Mutex<LogThrottle> = Mutex::new(LogThrottle {
    window_start: None,
    emitted: 0,
    suppressed: 0,
});

/// Admit (or drop) one glib message under the fixed-window rate limit,
/// rolling the window when it elapses. Pure over `t` + `now` (the global
/// `static` lock lives in the caller) so the window-roll and suppression
/// accounting are unit-testable. Returns `(allow, suppressed)`: when a window
/// rolls over, `suppressed` carries the count dropped in the prior window so
/// the caller emits a single summary line *outside* the lock (and off the
/// glib writer path, via `tracing`, so it can't re-enter here).
fn log_throttle_admit(t: &mut LogThrottle, now: Instant) -> (bool, u64) {
    let rolled = t
        .window_start
        .is_none_or(|start| now.duration_since(start) >= LOG_WINDOW);
    let report = if rolled {
        let dropped = t.suppressed;
        t.window_start = Some(now);
        t.emitted = 0;
        t.suppressed = 0;
        dropped
    } else {
        0
    };
    if t.emitted < LOG_MAX_PER_WINDOW {
        t.emitted += 1;
        (true, report)
    } else {
        t.suppressed += 1;
        (false, report)
    }
}

/// Filter glib's log output before the default writer. Two jobs:
///
/// 1. Drop the cosmetic `g_settings_schema_source_lookup: assertion
///    'source != NULL' failed` GLib warning that fires on macOS Homebrew
///    GTK4 when libadwaita queries a missing GSettings schema at startup.
///    Harmless — the schema is only used by the system dark-mode preference
///    — but the line crowds out real diagnostics.
/// 2. Rate-limit everything else (see [`log_throttle_admit`]) so no warning
///    storm can hang the UI or fill the disk.
fn install_log_filter() {
    glib::log_set_writer_func(|level, fields| {
        for field in fields {
            if field.key() == "MESSAGE" {
                if let Some(msg) = field.value_str() {
                    if msg.contains("g_settings_schema_source_lookup") {
                        return LogWriterOutput::Handled;
                    }
                }
            }
        }
        let (allow, suppressed) = {
            let mut t = LOG_THROTTLE.lock().unwrap_or_else(|e| e.into_inner());
            log_throttle_admit(&mut t, Instant::now())
        };
        if suppressed > 0 {
            tracing::warn!(suppressed, "glib log storm: rate-limited repeated messages");
        }
        if !allow {
            return LogWriterOutput::Handled;
        }
        glib::log_writer_default(level, fields)
    });
}

/// Initialize logging: always to stdout, and additionally tee to
/// `<log_dir>/roost.log` when that file can be opened (parity with the Mac
/// app's file log, so `tail -f` works on Linux too). Writes synchronously
/// (append mode) so entries hit disk immediately — important for tailing a
/// live session and for keeping logs after a crash. Best-effort: if the log
/// file can't be opened we fall back to stdout-only rather than refusing to
/// launch.
fn init_logging(log_dir: &Path) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());

    let file_layer = match std::fs::create_dir_all(log_dir).and_then(|()| {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_dir.join("roost.log"))
    }) {
        // ANSI stripped for the file; `Mutex<File>` serializes line writes.
        Ok(file) => Some(fmt::layer().with_ansi(false).with_writer(Mutex::new(file))),
        Err(e) => {
            eprintln!(
                "roost: file log disabled ({}: {e}); logging to stdout only",
                log_dir.join("roost.log").display()
            );
            None
        }
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer()) // stdout
        .with(file_layer) // Option<Layer> is a no-op when None
        .init();
}

fn main() -> anyhow::Result<()> {
    let profile = BundleProfile::resolve(BundleProfileKind::Gtk)?;
    init_logging(&profile.log_dir);
    install_log_filter();

    let lock_path = profile.lock_path();

    // M3b: single-instance via flock-on-pidfile. The Mac side will
    // pick up the same primitive in M4. Second launch falls
    // through with a clear error rather than racing on the socket.
    let _lock = match single_instance::acquire(&lock_path) {
        Ok(lock) => lock,
        Err(single_instance::AcquireError::AlreadyHeld(pid)) => {
            // Another instance holds the lock. Ask it to raise its
            // window via `app.activate` over IPC, then exit. The
            // tokio runtime isn't built yet here, so spin up a tiny
            // current-thread one just for this call. Bound the whole
            // round-trip with a 2 s timeout so a wedged IPC server
            // can't block the second launch indefinitely, and `warn`
            // on every failure branch (timeout or connect/send error)
            // so a recurring failure is visible in roost.log rather
            // than silently swallowed. On any failure, fall through to
            // the diagnostic message. (#6)
            let socket_path = profile.socket_path.clone();
            let activated = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()
                .map(|rt| {
                    rt.block_on(async {
                        let activate = async {
                            let mut client = IpcClient::connect(&socket_path).await?;
                            client
                                .call_raw(
                                    roost_ipc::messages::ops::APP_ACTIVATE,
                                    serde_json::json!({}),
                                )
                                .await?;
                            anyhow::Ok(())
                        };
                        match tokio::time::timeout(Duration::from_secs(2), activate).await {
                            Ok(Ok(())) => true,
                            Ok(Err(err)) => {
                                tracing::warn!(
                                    ?err,
                                    socket = %socket_path.display(),
                                    "failed to activate running instance"
                                );
                                false
                            }
                            Err(_elapsed) => {
                                tracing::warn!(
                                    socket = %socket_path.display(),
                                    "timed out activating running instance after 2s"
                                );
                                false
                            }
                        }
                    })
                })
                .unwrap_or(false);
            if !activated {
                eprintln!(
                    "Roost (GTK) is already running (pid {pid}); exiting.\nLock: {}",
                    lock_path.display()
                );
            }
            return Ok(());
        }
        Err(other) => return Err(anyhow::anyhow!("single_instance lock failed: {other}")),
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let rt_handle = rt.handle().clone();

    // In-process daemon: workspace persisted to state.json, PTY
    // supervisor owned by us. No external process.
    let workspace = Arc::new(Workspace::open(profile.state_json_path()));
    let supervisor = Arc::new(PtySupervisor::new());

    // UI bridge: ops that must touch GTK / libghostty (activate,
    // screenshot, dump) forward a `UiRequest` here; the GTK main thread
    // drains it and services each (replying over the request's oneshot
    // for the request-reply variants). One channel for all such ops.
    let (ui_tx, ui_rx) = tokio::sync::mpsc::unbounded_channel::<roost_linux::ipc::UiRequest>();

    // Bind the JSON IPC server *synchronously* before any UI surface
    // exists, so `roostctl identify` right after launch succeeds. The
    // single-instance flock is already held, so the stale socket is
    // safe to remove and the bind should succeed; if it fails,
    // `roostctl` and Claude hooks would have no socket to reach —
    // fail startup rather than run half-wired (#7).
    let socket_path = profile.socket_path.clone();
    let server = {
        let handler = IpcHandler::new(
            workspace.clone(),
            supervisor.clone(),
            socket_path.clone(),
            profile.app_label.to_string(),
            profile.app_id.to_string(),
        )
        .with_ui(ui_tx);
        rt_handle
            .block_on(IpcServer::bind(&socket_path, handler))
            .context("bind IPC server")?
    };
    rt_handle.spawn(async move {
        if let Err(err) = server.run().await {
            tracing::warn!(?err, "ipc server exited with error");
        }
    });

    let client = LocalClient::new(workspace, supervisor, socket_path);

    let app = Application::builder().application_id(APP_ID).build();
    let client_for_activate = client.clone();
    // `connect_activate` is `Fn`, but the UI receiver isn't Clone and is
    // consumed once. Wrap it so the first (only) GTK activation hands it
    // to the App; any later activation gets None.
    let ui_rx = std::cell::RefCell::new(Some(ui_rx));
    app.connect_activate(move |app| {
        // The App handle is reference-counted via `Rc`; we hand the
        // outer LocalClient to it so the bootstrap futures stay
        // alive for the lifetime of the application.
        let _ = App::new(
            app,
            rt_handle.clone(),
            client_for_activate.clone(),
            ui_rx.borrow_mut().take(),
        );
    });

    // Persist + fsync the tab layout on clean exit. `connect_shutdown`
    // fires during the GApplication shutdown sequence (after the main
    // loop terminates, once the last window is closing) — covering the
    // window X button, Cmd+Q, AND the empty-workspace internal
    // `window.close()` (ProjectDeleted arm in app.rs). flush() captures
    // the layout, then sets `shutting_down`; that flag is the real
    // guard — it makes any PTY-exit persistence racing in during the
    // shutdown sequence a no-op, so the teardown cascade can't
    // overwrite the flushed layout (rather than relying on flush
    // strictly preceding all window-close activity). Missed only on a
    // hard kill / crash, where best-effort staleness is acceptable.
    let client_for_shutdown = client.clone();
    app.connect_shutdown(move |_| {
        client_for_shutdown.workspace.flush();
    });

    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn throttle_caps_window_drops_overflow_and_reports_once() {
        let mut t = LogThrottle {
            window_start: None,
            emitted: 0,
            suppressed: 0,
        };
        let base = Instant::now();

        // The window's budget passes through untouched (no suppression yet).
        for _ in 0..LOG_MAX_PER_WINDOW {
            assert_eq!(log_throttle_admit(&mut t, base), (true, 0));
        }
        // Overflow in the same window is dropped and counted, not reported.
        for _ in 0..5 {
            assert_eq!(log_throttle_admit(&mut t, base), (false, 0));
        }
        assert_eq!(t.suppressed, 5);

        // Rolling into the next window admits again and reports the prior
        // window's drop count exactly once.
        assert_eq!(log_throttle_admit(&mut t, base + LOG_WINDOW), (true, 5));
        // The roll reset the counter: a later quiet roll reports zero.
        assert_eq!(
            log_throttle_admit(&mut t, base + LOG_WINDOW + LOG_WINDOW),
            (true, 0)
        );
    }
}
