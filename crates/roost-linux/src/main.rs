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

use gtk4::glib::{self, LogWriterOutput};
use libadwaita::prelude::*;
use libadwaita::Application;
use tracing_subscriber::EnvFilter;

use roost_common::{BundleProfile, BundleProfileKind};
use roost_ipc::IpcServer;
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
            eprintln!(
                "Roost (GTK) is already running (pid {pid}); exiting.\nLock: {}",
                lock_path.display()
            );
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

    // Bind the JSON IPC server before any UI surface exists so
    // `roostctl identify` immediately after launch can succeed.
    let socket_path = profile.socket_path.clone();
    {
        let workspace = workspace.clone();
        let supervisor = supervisor.clone();
        let socket_path = socket_path.clone();
        let app_label = profile.app_label.to_string();
        let app_id = profile.app_id.to_string();
        rt_handle.spawn(async move {
            let handler = IpcHandler::new(
                workspace,
                supervisor,
                socket_path.clone(),
                app_label,
                app_id,
            );
            match IpcServer::bind(&socket_path, handler).await {
                Ok(server) => {
                    if let Err(err) = server.run().await {
                        tracing::warn!(?err, "ipc server exited with error");
                    }
                }
                Err(err) => {
                    tracing::warn!(?err, "failed to bind ipc server");
                }
            }
        });
    }

    let client = LocalClient::new(workspace, supervisor, socket_path);

    let app = Application::builder().application_id(APP_ID).build();
    let client_for_activate = client.clone();
    app.connect_activate(move |app| {
        // The App handle is reference-counted via `Rc`; we hand the
        // outer LocalClient to it so the bootstrap futures stay
        // alive for the lifetime of the application.
        let _ = App::new(app, rt_handle.clone(), client_for_activate.clone());
    });

    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}
