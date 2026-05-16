//! Roost Linux UI — Phase 6a M8 initial spike.
//!
//! Single-window adw::ApplicationWindow that connects to `roost-core`
//! over a Unix domain socket and renders the daemon's Identify
//! response in a status label. Mirrors the Mac UI's Phase 5 step 2
//! Identify smoke (`mac/Sources/Roost/RoostClient.swift::runIdentify`)
//! so the two clients can be reasoned about side-by-side.
//!
//! This spike is intentionally narrow:
//!   * No terminal renderer (Cairo + Pango walk over libghostty-vt's
//!     render state — Phase 7 step 2).
//!   * No StreamPty round-trip (Phase 7 step 3).
//!   * No sidebar / tab bar (Phase 7 step 4).
//!
//! Its purpose is to prove that gtk4-rs + libadwaita-rs build end-to-end
//! against the existing `roost-common` UDS connector, on both Linux
//! (the eventual production target) and on macOS via Homebrew GTK4 so
//! the Mac UI maintainer can dogfood the cross-platform path without
//! provisioning a Linux box.

use std::sync::Arc;
use std::sync::Mutex;

use gtk4::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{Application, ApplicationWindow, HeaderBar};
use tracing_subscriber::EnvFilter;

use roost_common::{connect_uds, default_socket_path};
use roost_proto::v1::roost_client::RoostClient;
use roost_proto::v1::IdentifyRequest;

const APP_ID: &str = "com.charliek.roost.linux";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Tokio runtime for the gRPC call. gtk4-rs runs on its own main
    // loop (glib::MainContext); we run tonic on a separate tokio
    // runtime and bridge results back to the GTK thread via
    // `glib::idle_add_once` (or in this spike, an Arc<Mutex<…>> set
    // before the window appears, which is enough for the one-shot
    // Identify case).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");

    let identify_outcome: Arc<Mutex<IdentifyOutcome>> =
        Arc::new(Mutex::new(IdentifyOutcome::Connecting));
    let outcome_clone = identify_outcome.clone();
    rt.spawn(async move {
        let result = run_identify().await;
        let mut g = outcome_clone.lock().expect("identify mutex poisoned");
        *g = result;
    });

    let app = Application::builder().application_id(APP_ID).build();
    let outcome_for_activate = identify_outcome.clone();
    app.connect_activate(move |app| {
        build_window(app, outcome_for_activate.clone());
    });

    // GTK consumes argv itself; pass empty args so libadwaita doesn't
    // misinterpret cargo's environment.
    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}

#[derive(Clone, Debug)]
enum IdentifyOutcome {
    Connecting,
    Ok {
        pid: i32,
        socket: String,
        version: String,
        proto: u32,
        active_project: i64,
        active_tab: i64,
    },
    Failed(String),
}

async fn run_identify() -> IdentifyOutcome {
    let socket = match default_socket_path() {
        Ok(p) => p,
        Err(err) => return IdentifyOutcome::Failed(format!("socket path: {err}")),
    };
    let channel = match connect_uds(socket.clone()).await {
        Ok(c) => c,
        Err(err) => {
            return IdentifyOutcome::Failed(format!("connect uds at {}: {err}", socket.display()));
        }
    };
    let mut client = RoostClient::new(channel);
    match client
        .identify(IdentifyRequest {
            client_name: "roost-linux".into(),
            client_version: env!("CARGO_PKG_VERSION").into(),
        })
        .await
    {
        Ok(resp) => {
            let r = resp.into_inner();
            IdentifyOutcome::Ok {
                pid: r.pid,
                socket: r.socket_path,
                version: r.daemon_version,
                proto: r.protocol_version,
                active_project: r.active_project_id,
                active_tab: r.active_tab_id,
            }
        }
        Err(status) => IdentifyOutcome::Failed(format!("rpc: {status}")),
    }
}

fn build_window(app: &Application, outcome: Arc<Mutex<IdentifyOutcome>>) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(720)
        .default_height(420)
        .title("Roost (Linux spike)")
        .build();

    let header = HeaderBar::new();
    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);

    let title = gtk4::Label::new(Some("Roost — Linux spike"));
    title.add_css_class("title-1");
    title.set_halign(gtk4::Align::Start);

    let body = gtk4::Label::new(Some("connecting to roost-core…"));
    body.set_halign(gtk4::Align::Start);
    body.set_selectable(true);
    body.set_wrap(true);
    body.set_xalign(0.0);

    content.append(&title);
    content.append(&body);

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(&content);
    window.set_content(Some(&outer));

    // Poll the identify outcome on the GTK main thread every 200ms
    // until it resolves. Lightweight bridge between the tokio side
    // and GTK without bringing in a full async-gtk integration.
    let body_clone = body.clone();
    let outcome_clone = outcome.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let snapshot = outcome_clone
            .lock()
            .expect("identify mutex poisoned")
            .clone();
        match snapshot {
            IdentifyOutcome::Connecting => glib::ControlFlow::Continue,
            IdentifyOutcome::Ok {
                pid,
                socket,
                version,
                proto,
                active_project,
                active_tab,
            } => {
                body_clone.set_text(&format!(
                    "daemon: connected\n  socket: {socket}\n  pid: {pid}\n  version: {version}  (proto v{proto})\n  active project: {active_project}  active tab: {active_tab}"
                ));
                glib::ControlFlow::Break
            }
            IdentifyOutcome::Failed(reason) => {
                body_clone.set_text(&format!(
                    "daemon: not reachable\n  reason: {reason}\n  hint: start it with `cargo run -p roost-core`"
                ));
                glib::ControlFlow::Break
            }
        }
    });

    window.present();
}
