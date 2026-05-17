//! Roost Linux UI — Phase 7 commit 4 (cell renderer).
//!
//! Single-window adw::ApplicationWindow hosting a [`TerminalView`]
//! that allocates a libghostty-vt terminal, writes a static "hello"
//! payload, and renders the resulting screen state via Cairo + Pango
//! cell-by-cell. The Identify handshake against `roost-core` still
//! runs in the background and reports through the window's
//! HeaderBar title.
//!
//! Subsequent Phase 7 commits replace the static `vt_write` with a
//! real `StreamPty` round-trip (commit 5), add the full key encoder
//! (6), scrollback + selection (7), sidebar + tab bar (8), keybinds
//! (9), OSC + notifications (10), themes + config + visual polish
//! (11).

mod cell_metrics;
mod terminal_view;
mod theme;

use std::sync::Arc;
use std::sync::Mutex;

use gtk4::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{Application, ApplicationWindow, HeaderBar};
use tracing_subscriber::EnvFilter;

use roost_common::{connect_uds, default_socket_path};
use roost_proto::v1::roost_client::RoostClient;
use roost_proto::v1::IdentifyRequest;

use crate::terminal_view::TerminalView;

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
#[allow(dead_code)] // pid/socket/proto/active_* re-surface once the chrome lands (commit 8).
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
        .default_width(1100)
        .default_height(700)
        .title("Roost (Linux)")
        .build();

    let header = HeaderBar::new();
    // Title binding goes through a `gtk::Label` so we can rewrite it
    // when the daemon Identify resolves. AdwWindowTitle replaces this
    // in commit 8 once the project list drives the chrome.
    let title_label = gtk4::Label::new(Some("Roost"));
    title_label.add_css_class("title");
    header.set_title_widget(Some(&title_label));

    // The cell renderer. Phase 7 commit 4 hard-codes a static
    // vt_write payload + a roost-dark palette so we can eyeball the
    // walk side-by-side with `./roost` (Go) and Roost.app (Swift).
    let terminal = TerminalView::new();
    terminal.vt_write(
        "Roost — Linux UI (gtk4-rs)\r\n\
          \r\n\
          Phase 7 commit 4: cell renderer walking libghostty-vt's\r\n\
          render state via Cairo + Pango.\r\n\
          \r\n\
          $ "
        .as_bytes(),
    );

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(terminal.widget());
    window.set_content(Some(&outer));

    // Surface the Identify outcome in the headerbar title. Same
    // 200ms poll bridge the M8 spike used — adequate for one-shot
    // results. WatchEvents (commit 8) replaces this with a proper
    // glib channel.
    let title_for_poll = title_label.clone();
    let outcome_clone = outcome.clone();
    glib::timeout_add_local(std::time::Duration::from_millis(200), move || {
        let snapshot = outcome_clone
            .lock()
            .expect("identify mutex poisoned")
            .clone();
        match snapshot {
            IdentifyOutcome::Connecting => glib::ControlFlow::Continue,
            IdentifyOutcome::Ok {
                pid: _,
                socket: _,
                version,
                proto: _,
                active_project: _,
                active_tab: _,
            } => {
                title_for_poll.set_text(&format!("Roost — daemon v{version}"));
                glib::ControlFlow::Break
            }
            IdentifyOutcome::Failed(_reason) => {
                title_for_poll.set_text("Roost — daemon offline");
                glib::ControlFlow::Break
            }
        }
    });

    window.present();
    terminal.widget().grab_focus();
}
