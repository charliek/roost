//! Roost Linux UI — Phase 7 commit 5 (StreamPty round-trip).
//!
//! Single-window adw::ApplicationWindow hosting a [`TerminalView`]
//! that connects to `roost-core`, opens a tab (auto-creating the
//! default project if needed), drives a bidi `StreamPty` stream,
//! and pipes PTY output → renderer / keystrokes → daemon. Type bash
//! commands in the window; `ls`, `echo`, anything ASCII echoes.
//!
//! Phase 7 commit 6 replaces the bare-minimum ASCII key encoder
//! with the full `roost_vt::KeyEncoder` surface (arrows, function
//! keys, Shift+Tab, Kitty protocol). Commit 7 adds scrollback +
//! selection + clipboard. Commit 8 adds the sidebar + tab bar.

mod cell_metrics;
mod client;
mod tab_session;
mod terminal_view;
mod theme;

use std::rc::Rc;

use gtk4::glib;
use gtk4::prelude::*;
use libadwaita::prelude::*;
use libadwaita::{Application, ApplicationWindow, HeaderBar};
use tracing_subscriber::EnvFilter;

use roost_common::default_socket_path;

use crate::client::RoostClient;
use crate::tab_session::{TabOutput, TabSession};
use crate::terminal_view::TerminalView;

const APP_ID: &str = "com.charliek.roost.linux";

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    // Tokio runtime for tonic. gtk4-rs runs on its own main loop
    // (glib::MainContext). Tokio tasks push events into a tokio mpsc
    // channel that the GTK side drains via `glib::spawn_future_local`
    // (glib 0.21 retired `MainContext::channel`).
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    let rt_handle = rt.handle().clone();

    let app = Application::builder().application_id(APP_ID).build();
    app.connect_activate(move |app| {
        build_window(app, rt_handle.clone());
    });

    // GTK consumes argv itself; pass empty args so libadwaita doesn't
    // misinterpret cargo's environment.
    let exit_code = app.run_with_args::<&str>(&[]);
    rt.shutdown_background();
    std::process::exit(exit_code.into());
}

fn build_window(app: &Application, rt: tokio::runtime::Handle) {
    let window = ApplicationWindow::builder()
        .application(app)
        .default_width(1100)
        .default_height(700)
        .title("Roost (Linux)")
        .build();

    let header = HeaderBar::new();
    let title_label = gtk4::Label::new(Some("Roost — connecting…"));
    title_label.add_css_class("title");
    header.set_title_widget(Some(&title_label));

    let terminal = Rc::new(TerminalView::new());

    let outer = gtk4::Box::new(gtk4::Orientation::Vertical, 0);
    outer.append(&header);
    outer.append(terminal.widget());
    window.set_content(Some(&outer));

    window.present();
    terminal.widget().grab_focus();

    // Boot the daemon round-trip on the GTK main loop's async
    // executor. The body awaits tonic futures (which run on the
    // tokio runtime via `Handle::spawn` for the actual network IO)
    // and drives state updates through gtk's own future executor.
    let title_for_boot = title_label.clone();
    let terminal_for_boot = terminal.clone();
    glib::spawn_future_local(async move {
        bootstrap_session(rt, terminal_for_boot, title_for_boot).await;
    });
}

/// Connect to the daemon, surface basic status in the headerbar,
/// open a tab, and wire its StreamPty bidi stream into the
/// TerminalView. Failures surface as a single error line in the
/// headerbar title; the renderer stays on its initial blank state
/// so the user sees "Roost — daemon offline" with an empty window.
async fn bootstrap_session(
    rt: tokio::runtime::Handle,
    terminal: Rc<TerminalView>,
    title: gtk4::Label,
) {
    // Connect.
    let socket = match default_socket_path() {
        Ok(p) => p,
        Err(err) => {
            title.set_text(&format!("Roost — socket path: {err}"));
            return;
        }
    };
    let client = match rt
        .spawn(async move { RoostClient::connect(socket).await })
        .await
    {
        Ok(Ok(c)) => c,
        Ok(Err(err)) => {
            title.set_text(&format!("Roost — connect: {err}"));
            return;
        }
        Err(join_err) => {
            title.set_text(&format!("Roost — connect join: {join_err}"));
            return;
        }
    };

    // Identify, then pick / create a project, then open a tab.
    let mut client_for_setup = client.clone();
    let setup = rt
        .spawn(async move {
            let id = client_for_setup.identify().await?;
            let projects = client_for_setup.list_projects().await?;
            let project_id = match projects.first() {
                Some(p) => p.id,
                None => {
                    let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                    client_for_setup
                        .create_project("roost-linux", &cwd)
                        .await?
                        .id
                }
            };
            let cwd = std::env::var("HOME").unwrap_or_else(|_| "/".into());
            let tab = client_for_setup.open_tab(project_id, &cwd, 80, 24).await?;
            anyhow::Ok((id, tab))
        })
        .await;
    let (identity, tab) = match setup {
        Ok(Ok(pair)) => pair,
        Ok(Err(err)) => {
            title.set_text(&format!("Roost — setup: {err}"));
            return;
        }
        Err(join_err) => {
            title.set_text(&format!("Roost — setup join: {join_err}"));
            return;
        }
    };
    title.set_text(&format!(
        "Roost — daemon v{} · tab {}",
        identity.daemon_version, tab.id
    ));

    // Spawn the StreamPty session on the tokio runtime; the output
    // receiver is drained on the GTK main loop so libghostty stays
    // single-threaded.
    let (output_tx, mut output_rx) = tokio::sync::mpsc::unbounded_channel::<TabOutput>();
    let tab_id = tab.id;
    let session = {
        let mut client_for_session = client.clone();
        let spawn_result = rt
            .spawn(async move {
                TabSession::spawn(&mut client_for_session, tab_id, 80, 24, output_tx).await
            })
            .await;
        match spawn_result {
            Ok(Ok(s)) => s,
            Ok(Err(err)) => {
                title.set_text(&format!("Roost — StreamPty: {err}"));
                return;
            }
            Err(join_err) => {
                title.set_text(&format!("Roost — StreamPty join: {join_err}"));
                return;
            }
        }
    };
    let session = Rc::new(session);

    // Wire keystrokes into the session.
    terminal.set_on_input({
        let session = session.clone();
        move |bytes| session.send_input(bytes)
    });

    // Drain output bytes on the GTK main loop and route to the renderer.
    let terminal_for_drain = terminal.clone();
    glib::spawn_future_local(async move {
        while let Some(msg) = output_rx.recv().await {
            match msg {
                TabOutput::Bytes(data) => terminal_for_drain.vt_write(&data),
                TabOutput::Exit { status, reason } => {
                    tracing::info!(status, %reason, "PTY exited");
                    break;
                }
                TabOutput::Error(reason) => {
                    tracing::warn!(reason, "PTY stream error");
                    break;
                }
            }
        }
    });
}
