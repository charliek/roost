//! A protocol request winit has no API for, made on winit's own Wayland
//! display (plan 074 §D6).
//!
//! winit 0.30's Wayland `focus_window` is empty, so `window::gain_focus`
//! never reaches the compositor. A notification click's spec 1.2
//! `ActivationToken` is the portable answer, spent here as
//! `xdg_activation_v1.activate(token, surface)` (#351).
//!
//! The connection behind it is made on the first click that carries a
//! token and is never destroyed — [`CACHE`] says why.

use std::cell::RefCell;
use std::ffi::c_void;
use std::io;
use std::mem::ManuallyDrop;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::ptr::NonNull;
use std::time::{Duration, Instant};

use iced::window::raw_window_handle::{RawDisplayHandle, RawWindowHandle};
use iced::window::Window;
use roost_ipc::messages::ActivationOutcome;
use wayland_client::backend::{Backend, ObjectId, WaylandError};
use wayland_client::protocol::wl_callback::{self, WlCallback};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{delegate_noop, Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;

/// The longest one click waits on the compositor's first registry answer.
/// The wait blocks the UI thread, which `registry_queue_init`'s unbounded
/// roundtrip would do for as long as a wedged compositor liked; a click
/// that runs out falls back to `gain_focus`, and the next one resumes the
/// wait where this one left it.
const INIT_BUDGET: Duration = Duration::from_millis(250);

/// What one click's raise came to, and what the compositor was found to
/// offer — read off the registry apart from the outcome, so a test can say
/// what the outcome should have been.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Attempt {
    pub(crate) outcome: ActivationOutcome,
    /// Whether the registry lists `xdg_activation_v1`; `None` until it has
    /// answered.
    pub(crate) activation_global: Option<bool>,
}

/// Spend `token` on raising `window`, from inside `iced::window::run`. The
/// window's surface is borrowed for this call only; nothing here manages or
/// destroys it.
pub(crate) fn activate(window: &dyn Window, token: &str) -> Attempt {
    let Some((display, surface)) = wayland_handles(window) else {
        return Attempt {
            outcome: ActivationOutcome::NotWayland,
            activation_global: None,
        };
    };
    CACHE.with(|cache| activate_on(&mut cache.borrow_mut(), display, surface, token))
}

/// The `wl_display` and `wl_surface` behind `window`, or `None` for anything
/// but a Wayland window on a Wayland display.
fn wayland_handles(window: &dyn Window) -> Option<(NonNull<c_void>, NonNull<c_void>)> {
    let display = window.display_handle().ok()?.as_raw();
    let surface = window.window_handle().ok()?.as_raw();
    match (display, surface) {
        (RawDisplayHandle::Wayland(display), RawWindowHandle::Wayland(surface)) => {
            Some((display.display, surface.surface))
        }
        _ => None,
    }
}

thread_local! {
    /// The connection the raise is made on, `None` until the first click
    /// with a token and again after one fails. A failed attempt holds
    /// nothing, and the next click starts over.
    ///
    /// `ManuallyDrop` because the connection is never destroyed. Dropping a
    /// foreign-display backend destroys every proxy it made, and its event
    /// queue, on winit's `wl_display` (`wayland-backend-0.3.16`
    /// `src/sys/client_impl/mod.rs:1125-1148`), and a `thread_local!`'s
    /// destructor runs after winit has disconnected that display: a
    /// use-after-free. So it is leaked for the life of the process, as is
    /// one that fails — Roost has one window, and so one display.
    static CACHE: RefCell<Option<ManuallyDrop<Live>>> = const { RefCell::new(None) };
}

fn activate_on(
    cache: &mut Option<ManuallyDrop<Live>>,
    display: NonNull<c_void>,
    surface: NonNull<c_void>,
    token: &str,
) -> Attempt {
    let failed = Attempt {
        outcome: ActivationOutcome::Failed,
        activation_global: None,
    };
    let live = cache.get_or_insert_with(|| {
        // SAFETY: `display` is winit's live `wl_display` — the process's only
        // one — and the backend made on it is never dropped (see `CACHE`), so
        // it never outlives the display.
        let backend = unsafe { Backend::from_foreign_display(display.as_ptr().cast()) };
        ManuallyDrop::new(Live::connect(backend))
    });
    match live.settle() {
        Ok(true) => {}
        Ok(false) => {
            tracing::info!(
                budget_ms = INIT_BUDGET.as_millis(),
                "notification raise: the compositor's registry did not answer in time"
            );
            return failed;
        }
        Err(error) => {
            tracing::warn!(%error, "notification raise: the Wayland connection failed");
            *cache = None;
            return failed;
        }
    }
    let activation_global = Some(live.globals.activation.is_some());
    let outcome = match &live.activation {
        Some(activation) => {
            let bound = Bound {
                conn: &live.conn,
                activation,
            };
            request(&bound, token, surface)
        }
        None => ActivationOutcome::NoGlobal,
    };
    Attempt {
        outcome,
        activation_global,
    }
}

/// A guest connection onto winit's display, with a private queue, so
/// nothing of ours is ever dispatched on winit's and nothing of winit's on
/// ours.
struct Live {
    conn: Connection,
    queue: EventQueue<Globals>,
    registry: WlRegistry,
    globals: Globals,
    activation: Option<XdgActivationV1>,
}

impl Live {
    /// Ask, on a private queue, for the registry and a `wl_display.sync`
    /// behind it. Nothing is read here.
    fn connect(backend: Backend) -> Self {
        let conn = Connection::from_backend(backend);
        let queue = conn.new_event_queue();
        let handle = queue.handle();
        let registry = conn.display().get_registry(&handle, ());
        conn.display().sync(&handle, ());
        Self {
            conn,
            queue,
            registry,
            globals: Globals::default(),
            activation: None,
        }
    }

    /// Dispatch whatever the private queue holds and, until the registry's
    /// first burst has ended, wait for it — for at most [`INIT_BUDGET`].
    /// `Ok(false)` is a wait that ran out. Bind `xdg_activation_v1` once it
    /// is listed.
    ///
    /// Reading winit's display from here is sound. This runs inside an iced
    /// update, on the thread winit's event loop runs on, so winit is not
    /// between its own `prepare_read` and `read_events`: calloop takes its
    /// read guard in `before_sleep` and spends it in `before_handle_events`,
    /// before any handler — this one included — runs. Whatever our read
    /// pulls in for winit's queue stays queued there; the next
    /// `before_sleep` finds the queue non-empty and dispatches it
    /// (`calloop-wayland-source-0.3.0` `src/lib.rs:183-197`).
    fn settle(&mut self) -> Result<bool, String> {
        self.dispatch()?;
        let deadline = Instant::now() + INIT_BUDGET;
        while !self.globals.synced {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(false);
            }
            tolerate_would_block(self.conn.flush())?;
            // `None` is a queue that already holds events: dispatch first.
            if let Some(guard) = self.queue.prepare_read() {
                if readable(guard.connection_fd(), remaining)? {
                    tolerate_would_block(guard.read())?;
                }
                // A guard dropped unread cancels its read.
            }
            self.dispatch()?;
        }
        if self.activation.is_none() {
            if let Some((name, version)) = self.globals.activation {
                let version = version.min(XdgActivationV1::interface().version);
                self.activation = Some(self.registry.bind(name, version, &self.queue.handle(), ()));
            }
        }
        Ok(true)
    }

    fn dispatch(&mut self) -> Result<(), String> {
        self.queue
            .dispatch_pending(&mut self.globals)
            .map(drop)
            .map_err(|error| error.to_string())
    }
}

/// What the private queue's handlers collect.
#[derive(Default)]
struct Globals {
    /// `xdg_activation_v1`'s registry name and version, while it is listed.
    activation: Option<(u32, u32)>,
    /// The sync sent behind the registry request has answered, so the
    /// first burst of globals is all in.
    synced: bool,
}

impl Dispatch<WlRegistry, ()> for Globals {
    fn event(
        globals: &mut Self,
        _: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } if interface == XdgActivationV1::interface().name => {
                globals.activation = Some((name, version));
            }
            wl_registry::Event::GlobalRemove { name }
                if globals.activation.is_some_and(|(listed, _)| listed == name) =>
            {
                globals.activation = None;
            }
            _ => {}
        }
    }
}

impl Dispatch<WlCallback, ()> for Globals {
    fn event(
        globals: &mut Self,
        _: &WlCallback,
        event: wl_callback::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_callback::Event::Done { .. } = event {
            globals.synced = true;
        }
    }
}

delegate_noop!(Globals: XdgActivationV1);

/// The one request a click makes, behind a seam a unit test can observe:
/// no CI compositor offers `xdg_activation_v1`, so a recorder standing in
/// for it is the only proof the request is issued at all.
trait ActivationRequest {
    fn activate(&self, token: &str, surface: NonNull<c_void>) -> Result<(), String>;
}

fn request(
    target: &impl ActivationRequest,
    token: &str,
    surface: NonNull<c_void>,
) -> ActivationOutcome {
    // The generated request makes the token a C string and panics on an
    // interior NUL. No D-Bus string carries one, but the test op's JSON can.
    if token.contains('\0') {
        tracing::warn!("notification raise: the activation token holds a NUL; not sent");
        return ActivationOutcome::Failed;
    }
    match target.activate(token, surface) {
        Ok(()) => ActivationOutcome::Activated,
        Err(error) => {
            tracing::warn!(%error, "notification raise: xdg_activation_v1.activate was not sent");
            ActivationOutcome::Failed
        }
    }
}

/// The bound global, on the connection it was bound on.
struct Bound<'a> {
    conn: &'a Connection,
    activation: &'a XdgActivationV1,
}

impl ActivationRequest for Bound<'_> {
    fn activate(&self, token: &str, surface: NonNull<c_void>) -> Result<(), String> {
        // SAFETY: `surface` is winit's live `wl_surface`, borrowed for this
        // `window::run` callback and named only within it. It belongs to the
        // same `wl_display` this connection wraps, which is what lets a
        // request on our queue name it.
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface.as_ptr().cast()) }
            .map_err(|error| error.to_string())?;
        let surface = WlSurface::from_id(self.conn, id).map_err(|error| error.to_string())?;
        self.activation.activate(token.to_owned(), &surface);
        tolerate_would_block(self.conn.flush())
    }
}

/// `WouldBlock` is not a failure. A flush that would block leaves the
/// request queued for winit's next flush, and a read that would block was
/// beaten to the socket by another reader.
fn tolerate_would_block<T>(result: Result<T, WaylandError>) -> Result<(), String> {
    match result {
        Ok(_) => Ok(()),
        Err(WaylandError::Io(error)) if error.kind() == io::ErrorKind::WouldBlock => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

/// Whether `fd` turned readable within `budget`. An interrupted wait is "not
/// yet": the caller's loop works out what is left of its budget.
fn readable(fd: BorrowedFd<'_>, budget: Duration) -> Result<bool, String> {
    let mut poll = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout = libc::c_int::try_from(budget.as_millis().max(1)).unwrap_or(libc::c_int::MAX);
    // SAFETY: one valid `pollfd`, borrowed for the call.
    match unsafe { libc::poll(&mut poll, 1, timeout) } {
        0 => Ok(false),
        ready if ready > 0 => Ok(true),
        _ => {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                Ok(false)
            } else {
                Err(error.to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    /// Stands in for the bound global and keeps every request it is asked
    /// to send.
    #[derive(Default)]
    struct Recorder {
        sent: RefCell<Vec<(String, usize)>>,
        refuse: bool,
    }

    impl ActivationRequest for Recorder {
        fn activate(&self, token: &str, surface: NonNull<c_void>) -> Result<(), String> {
            if self.refuse {
                return Err("the connection is gone".into());
            }
            self.sent
                .borrow_mut()
                .push((token.to_owned(), surface.as_ptr() as usize));
            Ok(())
        }
    }

    /// An address only ever compared, never dereferenced.
    fn surface() -> NonNull<c_void> {
        NonNull::new(std::ptr::without_provenance_mut(0x5eed)).expect("non-null")
    }

    #[test]
    fn activated_means_the_request_went_out_with_the_clicks_token_and_this_surface() {
        let recorder = Recorder::default();
        assert_eq!(
            request(&recorder, "t-1", surface()),
            ActivationOutcome::Activated
        );
        assert_eq!(*recorder.sent.borrow(), vec![("t-1".to_owned(), 0x5eed)]);
    }

    #[test]
    fn a_request_that_could_not_be_sent_is_failed() {
        let recorder = Recorder {
            refuse: true,
            ..Recorder::default()
        };
        assert_eq!(
            request(&recorder, "t-1", surface()),
            ActivationOutcome::Failed
        );
        assert!(recorder.sent.borrow().is_empty());
    }

    #[test]
    fn a_token_with_a_nul_is_never_sent() {
        let recorder = Recorder::default();
        assert_eq!(
            request(&recorder, "t-\0-1", surface()),
            ActivationOutcome::Failed
        );
        assert!(recorder.sent.borrow().is_empty());
    }
}
