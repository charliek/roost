mod app;
mod input;
mod terminal_canvas;

use std::fs::OpenOptions;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use iced::{keyboard, time, window, Size, Subscription, Task, Theme};
use roost_engine::single_instance;
use roost_ipc::messages::ops;
use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::IpcClient;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::app::App;

const WINDOW_TITLE: &str = "Roost — Iced POC";

#[derive(Debug, Clone)]
enum Message {
    Tick,
    WindowOpened(window::Id),
    WindowResized(window::Id, Size),
    Keyboard(keyboard::Event),
    ProjectSelected(i64),
    TabSelected(i64),
    NewTab,
}

fn main() -> anyhow::Result<()> {
    let profile = BundleProfile::resolve(BundleProfileKind::Iced)?;
    init_logging(&profile)?;

    let lock = match single_instance::acquire(profile.lock_path()) {
        Ok(lock) => lock,
        Err(single_instance::AcquireError::AlreadyHeld(pid)) => {
            activate_existing(&profile, pid);
            return Ok(());
        }
        Err(error) => return Err(anyhow::anyhow!("single-instance lock failed: {error}")),
    };

    let initial = Arc::new(Mutex::new(Some(App::bootstrap(&profile, lock)?)));
    let boot = {
        let initial = Arc::clone(&initial);
        move || {
            initial
                .lock()
                .expect("Iced bootstrap state lock poisoned")
                .take()
                .expect("Iced boot closure called more than once")
        }
    };

    iced::application(boot, update, App::view)
        .title(WINDOW_TITLE)
        .theme(theme)
        .subscription(subscription)
        .window(window::Settings {
            size: Size::new(1100.0, 720.0),
            min_size: Some(Size::new(640.0, 360.0)),
            ..window::Settings::default()
        })
        .run()
        .context("run Iced application")
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::Tick => app.tick().map_task(),
        Message::WindowOpened(id) => {
            app.set_window_id(id);
            Task::none()
        }
        Message::WindowResized(id, size) => {
            app.set_window_id(id);
            app.resize(size);
            Task::none()
        }
        Message::Keyboard(event) => {
            app.keyboard(event);
            Task::none()
        }
        message @ (Message::ProjectSelected(_) | Message::TabSelected(_) | Message::NewTab) => {
            message.apply(app)
        }
    }
}

fn subscription(_app: &App) -> Subscription<Message> {
    Subscription::batch([
        time::every(Duration::from_millis(16)).map(|_| Message::Tick),
        window::open_events().map(Message::WindowOpened),
        window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        keyboard::listen().map(Message::Keyboard),
    ])
}

fn theme(_app: &App) -> Theme {
    Theme::Dark
}

fn init_logging(profile: &BundleProfile) -> anyhow::Result<()> {
    std::fs::create_dir_all(&profile.log_dir)
        .with_context(|| format!("create log directory {}", profile.log_dir.display()))?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(profile.log_path())
        .with_context(|| format!("open {}", profile.log_path().display()))?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into());
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer())
        .with(
            tracing_subscriber::fmt::layer()
                .with_ansi(false)
                .with_writer(Mutex::new(file)),
        )
        .try_init()
        .map_err(|error| anyhow::anyhow!("initialize tracing: {error}"))?;
    Ok(())
}

fn activate_existing(profile: &BundleProfile, pid: i32) {
    let result = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .and_then(|runtime| {
            runtime.block_on(async {
                let activate = async {
                    let mut client = IpcClient::connect(&profile.socket_path).await?;
                    client
                        .call_raw(ops::APP_ACTIVATE, serde_json::json!({}))
                        .await?;
                    anyhow::Ok(())
                };
                tokio::time::timeout(Duration::from_secs(2), activate)
                    .await
                    .map_err(std::io::Error::other)?
                    .map_err(std::io::Error::other)
            })
        });
    if let Err(error) = result {
        eprintln!("Roost (Iced) is already running (pid {pid}), but activation failed: {error}");
    }
}

trait UiTask {
    fn map_task(self) -> Task<Message>;
}

impl UiTask for app::UiTask {
    fn map_task(self) -> Task<Message> {
        match self {
            app::UiTask::None => Task::none(),
            app::UiTask::Focus(id) => window::gain_focus(id),
            app::UiTask::Resize(id, size) => window::resize(id, size),
        }
    }
}
