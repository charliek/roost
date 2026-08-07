mod app;
mod chrome;
mod engine_feed;
mod font_registry;
mod input;
mod notifications;
mod palette_scroll;
mod paste_image;
mod perf;
mod png_encode;
mod screenshot;
mod sidebar_resize;
mod strip_reorder;
mod terminal_widget;
mod url_launcher;

use std::fs::OpenOptions;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Context;
use iced::keyboard::key::Named;
use iced::keyboard::Key;
use iced::{event, keyboard, time, window, Event, Size, Subscription, Task, Theme};
use roost_engine::single_instance;
use roost_ipc::messages::ops;
use roost_ipc::paths::{BundleProfile, BundleProfileKind};
use roost_ipc::IpcClient;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use crate::app::App;

/// `$HOME` for the cwd segment of the window title. Read once: the title fn
/// runs on every update batch, and the value cannot change under a live
/// process.
static HOME_DIR: LazyLock<String> = LazyLock::new(|| {
    std::env::var_os("HOME")
        .map(|home| home.to_string_lossy().into_owned())
        .unwrap_or_default()
});

#[derive(Debug, Clone)]
enum Message {
    /// The engine feed has (probably) something to drain — see
    /// `engine_feed::wake_subscription`. Spurious wakes are expected and
    /// free.
    EngineReady,
    /// A mutation dispatched to the engine runtime finished. Carries only
    /// `Send + Clone` data — errors arrive already stringified.
    EngineOp(app::EngineOpResult),
    /// A banner is up and its expiry is due. Armed only while one is.
    StatusTick,
    /// A palette visibility request is outstanding. Armed only while one is.
    PaletteRetryTick,
    /// A tab the workspace lists still has no terminal. Armed only while
    /// the pending-attach set is non-empty.
    AttachRetryTick,
    /// A file-drop debounce window elapsed — a one-shot, not a timer.
    FileDropDeadline,
    WindowOpened(window::Id),
    WindowResized(window::Id, Size),
    WindowFocus(window::Id, bool),
    ScreenshotCaptured(window::Screenshot),
    ClipboardReadCompleted {
        request_id: u64,
        value: Option<String>,
    },
    ClipboardWriteCompleted(u64),
    /// A clipboard image probe finished. `path` is the temp PNG to paste,
    /// or `None` when the clipboard held no usable image.
    PasteImageMaterialized {
        tab_id: i64,
        path: Option<String>,
    },
    UrlOpenCompleted(Result<(), String>),
    Keyboard(keyboard::Event),
    CapturedEscape,
    CapturedEnterRelease,
    TerminalPointer(terminal_widget::TerminalPointer),
    WindowFileDropped {
        window_id: window::Id,
        path: std::path::PathBuf,
    },
    ProjectSelected(i64),
    BeginRenameProject(i64),
    AgentSelected(i64),
    TabSelected(i64),
    BeginRenameTab(i64),
    TabStrip(strip_reorder::StripEvent),
    ProjectStrip(strip_reorder::StripEvent),
    StripPointerReleased,
    RenameDraftChanged(String),
    RenameSubmit,
    RenamePointerDismiss,
    CloseTab(i64),
    NewTab,
    NewProject,
    ConfirmDeleteCancel,
    ConfirmDeleteConfirm,
    ConfirmDeleteCardPressed,
    SidebarResizeDragged {
        width: f32,
    },
    SidebarResizeEnded,
    SidebarDragCollapsed,
    PaletteQueryChanged(String),
    PaletteActivate(String),
    PaletteConfirm,
    PaletteDismiss,
    PaletteCardPressed,
    PaletteScrolled,
    PaletteVisibilityMeasured {
        session: u64,
        revision: u64,
        measurement_generation: u64,
        reveal: bool,
        visibility: palette_scroll::Visibility,
    },
}

fn main() -> anyhow::Result<()> {
    let profile = BundleProfile::resolve(BundleProfileKind::Iced)?;
    init_logging(&profile)?;
    roost_engine::crash::install_panic_hook(
        profile.log_dir.clone(),
        profile.app_label,
        env!("CARGO_PKG_VERSION"),
    );
    forced_test_panic();

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

    iced::application(boot, update, view)
        .title(title)
        .theme(theme)
        .subscription(subscription)
        .font(include_bytes!("../../../third_party/inter/Inter-Regular.ttf").as_slice())
        .font(include_bytes!("../../../third_party/inter/Inter-Medium.ttf").as_slice())
        .font(include_bytes!("../../../third_party/inter/Inter-SemiBold.ttf").as_slice())
        .default_font(chrome::chrome_font(iced::font::Weight::Normal))
        .window(window_settings())
        .run()
        .context("run Iced application")
}

/// Forces a panic when `ROOST_TEST_PANIC` is set, so the crash-report path
/// can be exercised end to end against a real binary. Env-gated the same way
/// as `ROOST_TEST_MODE`, and called before the single-instance lock so it can
/// never disturb a running instance.
fn forced_test_panic() {
    match std::env::var("ROOST_TEST_PANIC").as_deref() {
        Ok("1") => panic!("ROOST_TEST_PANIC: forced startup panic"),
        Ok("thread") => {
            // The join never returns — the hook aborts the process from the
            // spawned thread. It is here so main can't race ahead into app
            // init (and a real window) before the abort lands.
            // A spawn failure must not fall through into app init —
            // expect() panics on main, which the hook also catches.
            let handle = std::thread::Builder::new()
                .name("roost-test-panic".into())
                .spawn(|| panic!("ROOST_TEST_PANIC: forced thread panic"))
                .expect("ROOST_TEST_PANIC: failed to spawn panic thread");
            let _ = handle.join();
        }
        _ => {}
    }
}

/// macOS `titlebar_transparent` drops the standard titlebar material so the
/// bar takes the window's own background, landing much closer to the Swift
/// app's solid `#24292C` band than the stock dark chrome. `title_hidden` and
/// `fullsize_content_view` stay off deliberately: the latter would slide
/// content under the titlebar, and `servicing.rs` reports
/// `terminal_top = BAND_HEIGHT` (the macOS pixel lane scans those rows), so
/// it is not a window-settings change alone. Linux `PlatformSpecific` is a
/// disjoint struct: `application_id` fills WM_CLASS (X11) / app_id (Wayland),
/// which winit otherwise leaves empty — the dynamic window title made an
/// empty class unfindable for tooling, and the id matches the notification
/// adapter's `desktop-entry` hint so shells group both under one identity.
fn window_settings() -> window::Settings {
    window::Settings {
        size: Size::new(1100.0, 720.0),
        min_size: Some(Size::new(640.0, 360.0)),
        #[cfg(target_os = "macos")]
        platform_specific: window::settings::PlatformSpecific {
            titlebar_transparent: true,
            title_hidden: false,
            fullsize_content_view: false,
        },
        #[cfg(target_os = "linux")]
        platform_specific: window::settings::PlatformSpecific {
            application_id: "ai.stridelabs.Roost.iced".to_owned(),
            ..window::settings::PlatformSpecific::default()
        },
        ..window::Settings::default()
    }
}

fn update(app: &mut App, message: Message) -> Task<Message> {
    match message {
        Message::EngineReady => app.service_engine_ready().map_task(),
        Message::EngineOp(result) => {
            app.engine_op_completed(result);
            Task::none()
        }
        Message::StatusTick => {
            app.expire_status();
            Task::none()
        }
        Message::PaletteRetryTick => app.take_palette_visibility_task().map_task(),
        Message::AttachRetryTick => {
            app.retry_pending_attachments();
            Task::none()
        }
        Message::FileDropDeadline => {
            app.file_drop_deadline();
            Task::none()
        }
        Message::WindowOpened(id) => app.window_opened(id).map_task(),
        Message::WindowResized(id, size) => app.window_resized(id, size).map_task(),
        Message::WindowFocus(id, focused) => {
            let task = app.window_opened(id).map_task();
            app.set_window_focus(focused);
            task
        }
        Message::ScreenshotCaptured(capture) => app.screenshot_captured(&capture).map_task(),
        Message::ClipboardReadCompleted { request_id, value } => {
            app.clipboard_read_completed(request_id, value).map_task()
        }
        Message::ClipboardWriteCompleted(request_id) => {
            app.clipboard_write_completed(request_id).map_task()
        }
        Message::PasteImageMaterialized { tab_id, path } => {
            app.paste_image_materialized(tab_id, path.as_deref());
            Task::none()
        }
        Message::Keyboard(event) => app.keyboard(event).map_task(),
        Message::CapturedEscape => app.captured_escape().map_task(),
        Message::CapturedEnterRelease => {
            app.captured_enter_release();
            Task::none()
        }
        Message::UrlOpenCompleted(result) => {
            app.url_open_completed(result);
            Task::none()
        }
        Message::TerminalPointer(event) => match event {
            terminal_widget::TerminalPointer::Event(event) => app.pointer(event).map_task(),
            terminal_widget::TerminalPointer::Wheel(event) => app.wheel(event).map_task(),
            terminal_widget::TerminalPointer::Leave { tab_id } => {
                app.pointer_leave(tab_id);
                Task::none()
            }
        },
        Message::WindowFileDropped { window_id, path } => {
            app.file_dropped(window_id, path).map_task()
        }
        Message::RenameDraftChanged(draft) => {
            app.rename_draft_changed(draft);
            Task::none()
        }
        Message::TabStrip(event) => app.tab_strip_event(event).map_task(),
        Message::ProjectStrip(event) => app.project_strip_event(event).map_task(),
        Message::StripPointerReleased => app.strip_pointer_released().map_task(),
        Message::SidebarResizeDragged { width } => {
            app.sidebar_resize_dragged(width);
            Task::none()
        }
        Message::SidebarResizeEnded => {
            app.sidebar_resize_ended();
            Task::none()
        }
        Message::SidebarDragCollapsed => {
            app.sidebar_drag_collapsed();
            Task::none()
        }
        Message::RenameSubmit => app.submit_rename_editor().map_task(),
        Message::RenamePointerDismiss => {
            app.rename_pointer_dismiss();
            Task::none()
        }
        Message::PaletteQueryChanged(query) => {
            app.palette_query_changed(&query);
            Task::none()
        }
        Message::PaletteActivate(id) => app.palette_activate(&id).map_task(),
        Message::PaletteConfirm => app.palette_confirm().map_task(),
        Message::PaletteDismiss => app.palette_pointer_dismiss().map_task(),
        Message::PaletteCardPressed | Message::ConfirmDeleteCardPressed => Task::none(),
        Message::PaletteScrolled => {
            app.palette_scrolled();
            Task::none()
        }
        Message::PaletteVisibilityMeasured {
            session,
            revision,
            measurement_generation,
            reveal,
            visibility,
        } => {
            app.palette_visibility_measured(
                session,
                revision,
                measurement_generation,
                reveal,
                visibility,
            );
            Task::none()
        }
        message @ (Message::ProjectSelected(_)
        | Message::BeginRenameProject(_)
        | Message::AgentSelected(_)
        | Message::TabSelected(_)
        | Message::BeginRenameTab(_)
        | Message::CloseTab(_)
        | Message::NewTab
        | Message::NewProject
        | Message::ConfirmDeleteCancel
        | Message::ConfirmDeleteConfirm) => message.apply(app).map_task(),
    }
}

fn view(app: &App) -> iced::Element<'_, Message> {
    strip_reorder::ReleaseBoundary::new(app.view(), app.has_drag_preview()).into()
}

/// Which state-conditional timers the subscription set carries. Kept as a
/// plain value so the state → armed-members mapping is testable without a
/// window, a renderer, or a bootstrapped `App`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ArmedTimers {
    status: bool,
    palette_retry: bool,
    attach_retry: bool,
}

impl ArmedTimers {
    fn of(app: &App) -> Self {
        Self {
            status: app.status_active(),
            palette_retry: app.palette_retry_pending(),
            attach_retry: app.attach_retry_pending(),
        }
    }

    /// How many conditional members this state should contribute — the
    /// expected-length term the subscription test asserts against.
    #[cfg(test)]
    fn count(self) -> usize {
        usize::from(self.status) + usize::from(self.palette_retry) + usize::from(self.attach_retry)
    }
}

fn subscription(app: &App) -> Subscription<Message> {
    subscription_with(app.wake_handle(), ArmedTimers::of(app))
}

fn subscription_with(wake: Arc<tokio::sync::Notify>, armed: ArmedTimers) -> Subscription<Message> {
    let mut members = vec![
        // Unconditional, and identified by a constant hash: the wake
        // stream must survive every conditional member joining or leaving
        // this batch, because a restarted stream can drop a permit.
        engine_feed::wake_subscription(wake),
        window::open_events().map(Message::WindowOpened),
        window::resize_events().map(|(id, size)| Message::WindowResized(id, size)),
        window::events().filter_map(|(id, event)| window_event_message(id, event)),
        event::listen_with(|event, status, _window| {
            let Event::Keyboard(event) = event else {
                return None;
            };
            match status {
                event::Status::Ignored => Some(Message::Keyboard(event)),
                event::Status::Captured if is_escape_press(&event) => Some(Message::CapturedEscape),
                event::Status::Captured if is_enter_release(&event) => {
                    Some(Message::CapturedEnterRelease)
                }
                event::Status::Captured => None,
            }
        }),
    ];
    // Every remaining unconditional member above is event-driven: with
    // the 16 ms tick gone, an idle app schedules no timer at all. Each
    // conditional member's recipe is its interval plus the TypeId of its
    // mapping closure, so two of them sharing an interval is not a
    // collision — `conditional_timers_join_the_subscription_set_only_with_the_state_that_needs_them`
    // holds that.
    if armed.status {
        members.push(time::every(app::STATUS_TICK_INTERVAL).map(|_| Message::StatusTick));
    }
    if armed.palette_retry {
        members.push(time::every(app::PALETTE_RETRY_INTERVAL).map(|_| Message::PaletteRetryTick));
    }
    if armed.attach_retry {
        members.push(time::every(app::ATTACH_RETRY_INTERVAL).map(|_| Message::AttachRetryTick));
    }
    Subscription::batch(members)
}

fn window_event_message(id: window::Id, event: window::Event) -> Option<Message> {
    match event {
        window::Event::Focused => Some(Message::WindowFocus(id, true)),
        window::Event::Unfocused => Some(Message::WindowFocus(id, false)),
        window::Event::FileDropped(path) => Some(Message::WindowFileDropped {
            window_id: id,
            path,
        }),
        _ => None,
    }
}

/// Text inputs capture Escape before `keyboard::listen`, but Escape is an
/// application-level cancel for inline rename and palette frames. Forward that
/// one captured press while keeping captured Enter/printable input widget-owned
/// so submit and activation cannot dispatch twice.
fn is_escape_press(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyPressed {
            key: Key::Named(Named::Escape),
            repeat: false,
            ..
        }
    )
}

fn is_enter_release(event: &keyboard::Event) -> bool {
    matches!(
        event,
        keyboard::Event::KeyReleased {
            key: Key::Named(Named::Enter),
            ..
        }
    )
}

fn title(app: &App) -> String {
    app.window_title(&HOME_DIR)
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
            app::UiTask::Then(first, second) => first.map_task().chain(second.map_task()),
            app::UiTask::EngineOp(future) => Task::future(future).map(Message::EngineOp),
            app::UiTask::Focus(id) => window::gain_focus(id),
            app::UiTask::FocusWidget(id) => iced::widget::operation::focus(id),
            app::UiTask::SelectAllWidget(id) => iced::widget::operation::select_all(id),
            app::UiTask::Resize(id, size) => window::resize(id, size),
            app::UiTask::Screenshot(id) => window::screenshot(id).map(Message::ScreenshotCaptured),
            app::UiTask::ClipboardRead { request_id, target } => {
                let message = move |value| Message::ClipboardReadCompleted { request_id, value };
                match target {
                    roost_engine::ipc::ClipboardOp::System => iced::clipboard::read().map(message),
                    roost_engine::ipc::ClipboardOp::Selection => {
                        iced::clipboard::read_primary().map(message)
                    }
                }
            }
            app::UiTask::ClipboardWrite {
                request_id,
                target,
                text,
            } => {
                let write = match target {
                    roost_engine::ipc::ClipboardOp::System => iced::clipboard::write(text),
                    roost_engine::ipc::ClipboardOp::Selection => {
                        iced::clipboard::write_primary(text)
                    }
                };
                write.chain(Task::done(Message::ClipboardWriteCompleted(request_id)))
            }
            app::UiTask::OpenUrl { url } => {
                Task::perform(url_launcher::open(url), Message::UrlOpenCompleted)
            }
            // `spawn_blocking` is legal here because iced_winit wraps every
            // `update` in `Executor::enter`, i.e. this runs inside the
            // application's tokio runtime. The blocking pool is what keeps
            // the clipboard round-trip and the PNG encode off the UI thread.
            app::UiTask::PasteImageProbe { tab_id } => Task::perform(
                tokio::task::spawn_blocking(paste_image::materialize),
                move |joined| {
                    let path = match joined {
                        Ok(Ok(path)) => Some(path.to_string_lossy().into_owned()),
                        Ok(Err(error)) => {
                            tracing::debug!(tab_id, %error, "clipboard image paste found nothing");
                            None
                        }
                        Err(error) => {
                            tracing::debug!(tab_id, %error, "clipboard image probe did not join");
                            None
                        }
                    };
                    Message::PasteImageMaterialized { tab_id, path }
                },
            ),
            app::UiTask::FileDropDeadline(delay) => {
                Task::perform(tokio::time::sleep(delay), |()| Message::FileDropDeadline)
            }
            app::UiTask::PaletteVisibility {
                scroll_id,
                row_id,
                session,
                revision,
                measurement_generation,
                reveal,
            } => {
                let message = move |visibility| Message::PaletteVisibilityMeasured {
                    session,
                    revision,
                    measurement_generation,
                    reveal,
                    visibility,
                };
                if reveal {
                    iced::advanced::widget::operate(palette_scroll::ensure_visible(
                        scroll_id, row_id,
                    ))
                    .map(message)
                } else {
                    iced::advanced::widget::operate(palette_scroll::measure_visible(
                        scroll_id, row_id,
                    ))
                    .map(message)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::hash::Hasher as _;

    use super::*;
    use iced::advanced::subscription;
    use iced::keyboard::key::{Code, Physical};
    use iced::keyboard::Location;

    fn press(key: Key) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(Code::Escape),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    /// The ids Iced's tracker keys this subscription set on.
    fn recipe_ids(subscription: Subscription<Message>) -> Vec<u64> {
        subscription::into_recipes(subscription)
            .iter()
            .map(|recipe| {
                let mut hasher = subscription::Hasher::default();
                recipe.hash(&mut hasher);
                hasher.finish()
            })
            .collect()
    }

    fn armed(status: bool, palette_retry: bool, attach_retry: bool) -> ArmedTimers {
        ArmedTimers {
            status,
            palette_retry,
            attach_retry,
        }
    }

    /// The five members an idle app always carries: the engine wake plus
    /// the window-open, window-resize, window-event and keyboard
    /// listeners. All four listeners are event-driven and the wake only
    /// fires when the feed is fed, so this count is also the "no periodic
    /// wakeups when idle" guard — a timer that crept back into the
    /// unconditional set would move it.
    const UNCONDITIONAL_MEMBERS: usize = 5;

    /// The whole point of the conditional members: an idle app carries
    /// none of them, each piece of live state adds exactly its own, and
    /// none of them displaces an unconditional member — the wake stream
    /// least of all, whose identity must survive this churn.
    #[test]
    fn conditional_timers_join_the_subscription_set_only_with_the_state_that_needs_them() {
        let wake = Arc::new(tokio::sync::Notify::new());
        let idle = recipe_ids(subscription_with(Arc::clone(&wake), ArmedTimers::default()));
        assert_eq!(ArmedTimers::default().count(), 0, "idle arms no timer");
        assert_eq!(
            idle.len(),
            UNCONDITIONAL_MEMBERS,
            "an idle app subscribes to the wake and the window/keyboard events, nothing periodic"
        );

        for timers in [
            armed(false, false, false),
            armed(true, false, false),
            armed(false, true, false),
            armed(false, false, true),
            armed(true, true, false),
            armed(true, false, true),
            armed(false, true, true),
            armed(true, true, true),
        ] {
            let ids = recipe_ids(subscription_with(Arc::clone(&wake), timers));
            let unique: HashSet<u64> = ids.iter().copied().collect();
            assert_eq!(
                unique.len(),
                ids.len(),
                "{timers:?}: two members sharing a recipe id would silently drop one"
            );
            assert_eq!(
                ids.len(),
                idle.len() + timers.count(),
                "{timers:?}: armed member count"
            );
            assert!(
                idle.iter().all(|id| unique.contains(id)),
                "{timers:?}: the unconditional members keep their identity"
            );
        }
    }

    #[test]
    fn captured_keyboard_forwarding_recognizes_escape_only() {
        assert!(is_escape_press(&press(Key::Named(Named::Escape))));
        assert!(!is_escape_press(&press(Key::Named(Named::Enter))));
        assert!(!is_escape_press(&press(Key::Character("x".into()))));
    }

    #[test]
    fn captured_enter_release_is_forwarded_for_failed_submit_rearming() {
        let keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            modifiers,
            ..
        } = press(Key::Named(Named::Enter))
        else {
            unreachable!()
        };
        let release = keyboard::Event::KeyReleased {
            key,
            modified_key,
            physical_key,
            location,
            modifiers,
        };
        assert!(is_enter_release(&release));
        assert!(!is_enter_release(&press(Key::Named(Named::Enter))));
    }

    #[test]
    fn native_file_drop_mapping_preserves_window_and_raw_path() {
        let id = window::Id::unique();
        let path = std::path::PathBuf::from("/tmp/My File.png");
        let message = window_event_message(id, window::Event::FileDropped(path.clone()))
            .expect("file drop maps to an application message");
        let Message::WindowFileDropped {
            window_id,
            path: mapped,
        } = message
        else {
            panic!("unexpected native file-drop message")
        };
        assert_eq!(window_id, id);
        assert_eq!(mapped, path);
        assert!(window_event_message(id, window::Event::FilesHoveredLeft).is_none());
    }
}
