//! Roost Linux UI — Phase 7 commit 8 (sidebar + tab bar + WatchEvents).
//!
//! Multi-project, multi-tab gtk4-rs UI talking to roost-core over UDS.
//! The window is now `adw::ApplicationWindow → HeaderBar + Paned →
//! ListBox sidebar | Stack of AdwTabView`. WatchEvents drives the
//! cross-client convergence: any `roost-cli-rs project create / tab
//! open / project rename / project delete / tab delete / tab reorder`
//! reflects here within ~1s.
//!
//! Commits 9-11 add: keybind config, OSC + notifications, full theme
//! + config + visual polish (CSS + GResource icons).

mod app;
mod cell_metrics;
mod client;
mod config;
mod events;
mod key_encoder;
mod keybind;
mod tab_session;
mod terminal_view;
mod theme;

use libadwaita::prelude::*;
use libadwaita::Application;
use tracing_subscriber::EnvFilter;

use crate::app::App;

const APP_ID: &str = "com.charliek.roost.linux";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let rt_handle = rt.handle().clone();

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        // App is reference-counted via `Rc`; we leak the outer handle
        // into the activation closure so the bootstrap futures stay
        // alive for the lifetime of the application.
        let _ = App::new(app, rt_handle.clone());
    });

    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}
