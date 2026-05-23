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
mod config;
mod events;
mod key_encoder;
mod keybind;
mod rollup;
mod tab_session;
mod terminal_view;
mod theme;

use std::sync::Arc;

use anyhow::Context;
use gtk4::glib::{self, LogWriterOutput};
use libadwaita::prelude::*;
use libadwaita::Application;
use tracing_subscriber::EnvFilter;

use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::{IpcClient, IpcServer};
use roost_linux::daemon::{PtySupervisor, Workspace};
use roost_linux::ipc::IpcHandler;
use roost_linux::local_client::LocalClient;
use roost_linux::single_instance;

use crate::app::App;

// Matches `BundleProfile::gtk().app_id` (roost-common).
const APP_ID: &str = "ai.stridelabs.Roost.gtk";

/// Drop the cosmetic `g_settings_schema_source_lookup: assertion
/// 'source != NULL' failed` GLib warning that fires on macOS
/// Homebrew GTK4 when libadwaita queries a missing GSettings schema
/// at startup. Harmless — the schema is only used by the system
/// dark-mode preference — but the line crowds out real diagnostics.
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
        glib::log_writer_default(level, fields)
    });
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();
    install_log_filter();

    let profile = BundleProfile::resolve(BundleProfileKind::Gtk)?;
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
            // current-thread one just for this call. If the dial or
            // send fails (socket missing, instance shutting down),
            // fall back to the diagnostic message. (#6)
            let socket_path = profile.socket_path.clone();
            let activated = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .ok()
                .map(|rt| {
                    rt.block_on(async move {
                        match IpcClient::connect(&socket_path).await {
                            Ok(mut client) => client
                                .call_raw(
                                    roost_ipc::messages::ops::APP_ACTIVATE,
                                    serde_json::json!({}),
                                )
                                .await
                                .is_ok(),
                            Err(_) => false,
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

    // Activation bridge: a second launch dials `app.activate`; the
    // handler forwards a unit here for the GTK thread to raise the
    // window (#6).
    let (activate_tx, activate_rx) = tokio::sync::mpsc::unbounded_channel::<()>();

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
        .with_activate(activate_tx);
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
    // `connect_activate` is `Fn`, but the activate receiver isn't
    // Clone and is consumed once. Wrap it so the first (only) GTK
    // activation hands it to the App; any later activation gets None.
    let activate_rx = std::cell::RefCell::new(Some(activate_rx));
    app.connect_activate(move |app| {
        // The App handle is reference-counted via `Rc`; we hand the
        // outer LocalClient to it so the bootstrap futures stay
        // alive for the lifetime of the application.
        let _ = App::new(
            app,
            rt_handle.clone(),
            client_for_activate.clone(),
            activate_rx.borrow_mut().take(),
        );
    });

    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}
