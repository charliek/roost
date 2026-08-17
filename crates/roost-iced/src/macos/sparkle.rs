//! The Sparkle updater — loaded at runtime, never linked.
//!
//! `dlopen` rather than `-framework Sparkle` (plan 028 § 3.8): the
//! framework only ever ships inside `Roost-Iced.app`, so link-time
//! coupling would tax every cargo build, every CI matrix cell and the
//! bare-binary dev flow for something none of them can use. Here the
//! absence of the framework is just a state — `updater: unavailable`,
//! a disabled "Check for Updates…" item, and an app that is otherwise
//! completely unaffected.
//!
//! Everything below talks to Sparkle through hand-written `msg_send!`,
//! so there is no compile-time signature checking. The surface is kept
//! deliberately tiny — one class lookup, one init, two check calls, one
//! delegate — and every selector here was read off the shipped headers
//! (`SPUStandardUpdaterController.h`, `SPUUpdater.h`,
//! `SPUUpdaterDelegate.h`) rather than from memory.
//!
//! Ownership, pinned by § 3.8: `SPUStandardUpdaterController` holds its
//! delegate **weakly**, so this module retains the dlopen handle (app
//! lifetime, never `dlclose`d — Objective-C classes cannot be
//! unregistered), the controller, the updater and the delegate in
//! main-thread `thread_local!` statics. A `Mutex<Retained<_>>` would be
//! wrong twice over: these objects are main-thread-only, and nothing
//! here is `Send`.
//!
//! The retained objects and the callback-written state live in
//! *separate* `thread_local!`s, because the delegate callbacks run on
//! the main runloop while a seam entry point may be on the stack:
//! [`OBJECTS`] is borrowed by the entry points, [`STATE`] by the
//! callbacks, and neither ever borrows the other.

use std::cell::RefCell;
use std::ffi::{c_void, CStr, CString};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::ptr;

use objc2::rc::{Allocated, Retained};
use objc2::runtime::{AnyClass, AnyObject, NSObject};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_foundation::NSString;
use roost_ipc::messages::{AppUpdateStatusResult, UpdateCheckDump};

/// The env var carrying the test appcast's URL. Read **only** when the
/// app booted with `ROOST_TEST_MODE=1` (both conditions, checked once at
/// [`init`]): a production bundle ignores it entirely, so the delegate
/// can never become a side-channel for injecting a feed into a user's
/// install (plan 028 § 3.9).
const TEST_FEED_URL_ENV: &str = "ROOST_SPARKLE_FEED_URL";

/// The framework's stable top-level symlink, relative to
/// `Contents/MacOS/`. `fetch.sh` validates that this layout exists at
/// staging time and CI asserts it on the assembled bundle, so a broken
/// symlink farm fails the build rather than degrading to "no updater"
/// at runtime.
const FRAMEWORK_REL_PATH: &str = "../Frameworks/Sparkle.framework/Sparkle";

/// One completed check, as plain data. Written by the delegate
/// callbacks, read by `app.update_status`.
#[derive(Debug, Clone)]
struct LastCheck {
    outcome: &'static str,
    version: Option<String>,
    detail: Option<String>,
}

#[derive(Debug, Default)]
struct State {
    /// The test-mode feed override, resolved once at [`init`]. `None`
    /// in production (and in test mode with the var unset), which makes
    /// `feedURLStringForUpdater:` return nil — Sparkle then falls back
    /// to `SUFeedURL`, or reports no feed.
    feed_url: Option<String>,
    /// Increments once per completed check, so a poll cannot pass on a
    /// stale `last_check` from an earlier one (§ 3.12).
    check_id: i64,
    last_check: Option<LastCheck>,
    /// Whether the in-flight check already recorded its outcome. Sparkle
    /// reports a check through whichever of several delegate callbacks
    /// fits; the first one to fire owns the outcome and the rest — up to
    /// and including the cycle-finished callback — are no-ops.
    cycle_recorded: bool,
}

/// The retained Objective-C side. `None` until [`init`] runs; `updater`
/// is `None` when the framework loaded but the updater refused to start.
struct Objects {
    /// The `dlopen` handle, held for the app's whole life and never
    /// passed to `dlclose`: unloading a framework whose Objective-C
    /// classes are registered — and whose objects are live — is not
    /// supported. Kept as a field only to make that lifetime explicit.
    _handle: *mut c_void,
    _controller: Retained<AnyObject>,
    /// The controller's `SPUUpdater`, the object every check goes
    /// through. `None` when `-startUpdater:` failed.
    updater: Option<Retained<AnyObject>>,
    /// The controller holds this **weakly**, so this field is the only
    /// thing keeping the delegate alive.
    _delegate: Retained<UpdaterDelegate>,
}

thread_local! {
    static OBJECTS: RefCell<Option<Objects>> = const { RefCell::new(None) };
    /// Split from [`OBJECTS`] so a delegate callback firing while a seam
    /// entry point is on the stack cannot deadlock on a `RefCell`.
    static STATE: RefCell<State> = RefCell::new(State::default());
    /// Why the updater is unavailable, or `None` once it started.
    /// Separate from [`OBJECTS`] because the failure paths have no
    /// objects to hang it off.
    static UNAVAILABLE: RefCell<Option<String>> = const { RefCell::new(None) };
    /// [`init`] is idempotent; `window_opened` runs again on every focus
    /// regain.
    static INITIALIZED: RefCell<bool> = const { RefCell::new(false) };
}

// ---------------------------------------------------------------------
// The delegate
// ---------------------------------------------------------------------

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - The class does not implement `Drop`.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RoostSparkleUpdaterDelegate"]
    struct UpdaterDelegate;

    /// `SPUUpdaterDelegate` is a formal protocol whose members are all
    /// `@optional`; Sparkle probes each with `respondsToSelector:`, so
    /// declaring the four this app cares about is the whole conformance
    /// requirement. Every selector below is copied from
    /// `SPUUpdaterDelegate.h` (Sparkle 2.9), not reconstructed.
    impl UpdaterDelegate {
        /// `- (nullable NSString *)feedURLStringForUpdater:(SPUUpdater *)updater`
        ///
        /// Returns a raw pointer, not a `Retained`: the selector is in
        /// no ARC method family, so the caller expects a +0 autoreleased
        /// object — `Retained::autorelease_return` is objc2's spelling
        /// of exactly that convention.
        #[unsafe(method(feedURLStringForUpdater:))]
        fn feed_url_string(&self, _updater: &AnyObject) -> *mut NSString {
            STATE.with(|cell| {
                cell.borrow()
                    .feed_url
                    .as_deref()
                    .map_or_else(ptr::null_mut, |url| {
                        Retained::autorelease_return(NSString::from_str(url))
                    })
            })
        }

        /// `- (void)updater:(SPUUpdater *)updater didFindValidUpdate:(SUAppcastItem *)item`
        #[unsafe(method(updater:didFindValidUpdate:))]
        fn did_find_valid_update(&self, _updater: &AnyObject, item: &AnyObject) {
            record("found", appcast_item_version(item), None);
        }

        /// `- (void)updaterDidNotFindUpdate:(SPUUpdater *)updater error:(NSError *)error`
        ///
        /// The error-carrying variant, not the bare
        /// `updaterDidNotFindUpdate:`: its `userInfo` is where Sparkle
        /// puts the *reason* no update was found, which is exactly what
        /// `app.update_status`'s `detail` reports.
        #[unsafe(method(updaterDidNotFindUpdate:error:))]
        fn did_not_find_update(&self, _updater: &AnyObject, error: &AnyObject) {
            record("none", None, Some(error_description(error)));
        }

        /// `- (void)updater:(SPUUpdater *)updater didAbortWithError:(NSError *)error`
        #[unsafe(method(updater:didAbortWithError:))]
        fn did_abort_with_error(&self, _updater: &AnyObject, error: &AnyObject) {
            record("error", None, Some(error_description(error)));
        }

        /// `- (void)updater:(SPUUpdater *)updater
        ///      didFinishUpdateCycleForUpdateCheck:(SPUUpdateCheck)updateCheck
        ///      error:(nullable NSError *)error`
        ///
        /// The backstop: it fires for every finished cycle, so a check
        /// that somehow reported through none of the three callbacks
        /// above still advances `check_id` instead of hanging a
        /// condition-wait forever. When one of them already recorded,
        /// [`record`] makes this a no-op.
        #[unsafe(method(updater:didFinishUpdateCycleForUpdateCheck:error:))]
        fn did_finish_update_cycle(
            &self,
            _updater: &AnyObject,
            _update_check: isize,
            error: Option<&AnyObject>,
        ) {
            match error {
                Some(error) => record("error", None, Some(error_description(error))),
                None => record("none", None, None),
            }
        }
    }
);

/// Commit the outcome of the in-flight check, first writer wins, and
/// advance `check_id`.
///
/// First-writer-wins matters: `didFindValidUpdate:` fires before the
/// cycle-finished callback, and an information-only check ends its cycle
/// with a `SUNoUpdateError`-shaped error even when it *did* find one —
/// last-writer-wins would turn every found update into an error.
fn record(outcome: &'static str, version: Option<String>, detail: Option<String>) {
    STATE.with(|cell| {
        let Ok(mut state) = cell.try_borrow_mut() else {
            tracing::error!(outcome, "sparkle callback re-entered the state borrow");
            return;
        };
        if state.cycle_recorded {
            return;
        }
        state.cycle_recorded = true;
        state.check_id += 1;
        tracing::info!(outcome, ?version, ?detail, "sparkle update check finished");
        state.last_check = Some(LastCheck {
            outcome,
            version,
            detail,
        });
    });
}

// ---------------------------------------------------------------------
// Public seam surface — plain data in, plain data out
// ---------------------------------------------------------------------

/// Load the framework and start the updater, once.
///
/// Called from `window_opened` (after the menu install) for the same
/// reason every other AppKit touch is: winit documents
/// `NSApplication::sharedApplication` before the event loop exists as
/// unsupported, and Sparkle's user driver is an AppKit consumer.
///
/// `test_mode` is the app's own `ROOST_TEST_MODE` boot flag — the *only*
/// key that unlocks [`TEST_FEED_URL_ENV`].
pub(crate) fn init(mtm: MainThreadMarker, test_mode: bool) {
    if INITIALIZED.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), true)) {
        return;
    }
    let feed_url = test_mode
        .then(|| std::env::var(TEST_FEED_URL_ENV).ok())
        .flatten();
    if let Some(url) = feed_url.as_deref() {
        tracing::info!(url, "sparkle: test-mode feed override active");
    }
    STATE.with(|cell| cell.borrow_mut().feed_url = feed_url);

    match load(mtm) {
        Ok(objects) => {
            let started = objects.updater.is_some();
            OBJECTS.with(|cell| *cell.borrow_mut() = Some(objects));
            if started {
                tracing::info!("sparkle: updater started");
            }
        }
        Err(reason) => {
            tracing::info!(%reason, "sparkle: updater unavailable");
            UNAVAILABLE.with(|cell| *cell.borrow_mut() = Some(reason));
        }
    }
}

/// The `app.update_status` payload.
pub(crate) fn status(_mtm: MainThreadMarker) -> AppUpdateStatusResult {
    let (framework_loaded, started) = OBJECTS.with(|cell| {
        cell.borrow()
            .as_ref()
            .map_or((false, false), |objects| (true, objects.updater.is_some()))
    });
    let reason = UNAVAILABLE.with(|cell| cell.borrow().clone());
    STATE.with(|cell| {
        let state = cell.borrow();
        AppUpdateStatusResult {
            framework_loaded,
            updater: if started { "started" } else { "unavailable" }.into(),
            reason,
            check_id: state.check_id,
            last_check: state.last_check.as_ref().map(|check| UpdateCheckDump {
                outcome: check.outcome.into(),
                version: check.version.clone(),
                detail: check.detail.clone(),
            }),
        }
    })
}

/// Whether the "Check for Updates…" item should be live: the updater
/// started AND `SPUUpdater.canCheckForUpdates` — the property Sparkle
/// documents for exactly this (`SPUUpdater.h:171`), which also goes
/// false while a check is already in flight.
pub(crate) fn can_check(_mtm: MainThreadMarker) -> bool {
    with_updater(|updater| {
        // SAFETY: `canCheckForUpdates` is a readonly BOOL property on
        // SPUUpdater (SPUUpdater.h:171).
        let can: bool = unsafe { msg_send![updater, canCheckForUpdates] };
        can
    })
    .unwrap_or(false)
}

/// The interactive check the menu item drives: Sparkle's own UI, panels
/// and all. Errors are Sparkle's to present (this is the path a user
/// takes), so nothing is reported back here.
pub(crate) fn check_for_updates(_mtm: MainThreadMarker) {
    let dispatched = with_updater(|updater| {
        // A check dispatched while a session is in progress is a silent
        // Sparkle no-op (SPUUpdater.h:150) — re-arming the cycle for one
        // would let the IN-FLIGHT cycle's tail record itself as this
        // check's outcome. Skip arming (and the call) instead.
        if session_in_progress(updater) {
            tracing::debug!("sparkle: interactive check requested mid-session; ignored");
            return;
        }
        arm_cycle();
        // SAFETY: `-checkForUpdates` takes no arguments and returns void
        // (SPUUpdater.h:109). Deliberately the updater's own method
        // rather than the controller's `-checkForUpdates:` IBAction —
        // same UI, one less sender argument to get wrong.
        let () = unsafe { msg_send![updater, checkForUpdates] };
    });
    if dispatched.is_none() {
        tracing::warn!("sparkle: interactive check requested with no updater");
    }
}

/// The non-interactive check the `app.update_check` test op drives: no
/// UI, no download — just feed fetch, appcast parse and version
/// comparison, reported back through the delegate callbacks.
pub(crate) fn check_for_update_information(_mtm: MainThreadMarker) -> Result<(), String> {
    with_updater(|updater| {
        // Same mid-session hazard as the interactive path, but this op's
        // Ok must mean "a check actually started" — error instead of
        // silently no-oping so a caller can't condition-wait forever on
        // a check_id that belongs to someone else's cycle.
        if session_in_progress(updater) {
            return Err("a Sparkle update check is already in flight".to_string());
        }
        arm_cycle();
        // SAFETY: `-checkForUpdateInformation` takes no arguments and
        // returns void (SPUUpdater.h:154).
        let () = unsafe { msg_send![updater, checkForUpdateInformation] };
        Ok(())
    })
    .unwrap_or_else(|| {
        let reason = UNAVAILABLE.with(|cell| cell.borrow().clone());
        Err(format!(
            "the Sparkle updater is unavailable ({})",
            reason.as_deref().unwrap_or("not initialized")
        ))
    })
}

/// SAFETY wrapper: `sessionInProgress` is a readonly BOOL property on
/// SPUUpdater (SPUUpdater.h:189) — true while any update session,
/// including a permission request, is active.
fn session_in_progress(updater: &AnyObject) -> bool {
    unsafe { msg_send![updater, sessionInProgress] }
}

/// Mark the next callback as belonging to a fresh cycle. Called on the
/// way *into* a check so the first callback out of it records, even
/// though the previous cycle already did.
fn arm_cycle() {
    STATE.with(|cell| cell.borrow_mut().cycle_recorded = false);
}

fn with_updater<T>(f: impl FnOnce(&AnyObject) -> T) -> Option<T> {
    OBJECTS.with(|cell| {
        let slot = cell.borrow();
        let updater = slot.as_ref()?.updater.as_ref()?;
        Some(f(updater))
    })
}

// ---------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------

/// dlopen the framework, instantiate the controller, start the updater.
///
/// `Err` is the terminal "no updater, and here is why" state (no
/// framework beside the executable, a class that failed to register).
/// A controller that came up but whose `-startUpdater:` refused is `Ok`
/// with `updater: None` and the reason recorded — the objects still have
/// to be retained, because the delegate is weakly held and dropping the
/// controller mid-flight would leave Sparkle holding a dangling one.
fn load(mtm: MainThreadMarker) -> Result<Objects, String> {
    let path = framework_path()?;
    let handle = dlopen(&path)?;

    let class = AnyClass::get(c"SPUStandardUpdaterController").ok_or_else(|| {
        format!(
            "{} loaded but SPUStandardUpdaterController did not register",
            path.display()
        )
    })?;

    // SAFETY: `init` on a freshly allocated instance of our own class.
    let delegate: Retained<UpdaterDelegate> =
        unsafe { msg_send![UpdaterDelegate::alloc(mtm), init] };

    // SAFETY: `-initWithStartingUpdater:updaterDelegate:userDriverDelegate:`
    // (SPUStandardUpdaterController.h:97) — BOOL, then two nullable
    // object arguments.
    //
    // `startingUpdater: false` deviates from the plan's pinned `true`
    // for a reason the header spells out (`-startUpdater`, :105): the
    // controller's own start "logs an error and shows an alert to the
    // user (after a few seconds) to contact the developer" when the app
    // is misconfigured. A feedless production bundle is exactly that
    // configuration, and an unprompted modal alert is not an acceptable
    // failure mode — nor is one appearing mid-e2e. Starting the updater
    // directly below surfaces the same failure as a plain `NSError` we
    // can record and report through `app.update_status`.
    let controller: Retained<AnyObject> = unsafe {
        let allocated: Allocated<AnyObject> = msg_send![class, alloc];
        msg_send![
            allocated,
            initWithStartingUpdater: false,
            updaterDelegate: &*delegate as &AnyObject,
            userDriverDelegate: ptr::null::<AnyObject>(),
        ]
    };

    // SAFETY: `updater` is a readonly object property
    // (SPUStandardUpdaterController.h:64).
    let updater: Retained<AnyObject> = unsafe { msg_send![&*controller, updater] };

    let mut error: *mut AnyObject = ptr::null_mut();
    // SAFETY: `- (BOOL)startUpdater:(NSError **)error` (SPUUpdater.h:92).
    let started: bool = unsafe { msg_send![&*updater, startUpdater: &mut error] };
    let updater = if started {
        Some(updater)
    } else {
        let detail = if error.is_null() {
            "the updater refused to start and reported no error".to_string()
        } else {
            // The out-param NSError is autoreleased (+0); read it now.
            error_description(unsafe { &*error })
        };
        UNAVAILABLE.with(|cell| *cell.borrow_mut() = Some(detail));
        None
    };

    Ok(Objects {
        _handle: handle,
        _controller: controller,
        updater,
        _delegate: delegate,
    })
}

/// `Contents/MacOS/Roost-Iced` → `Contents/Frameworks/Sparkle.framework/Sparkle`.
///
/// A bare `cargo build` binary has no `Contents/Frameworks` above it, so
/// this resolves to a path that does not exist and the dev flow lands in
/// the `unavailable` state by construction — no bundle detection needed.
fn framework_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe()
        .map_err(|error| format!("could not resolve the executable path: {error}"))?;
    // Canonicalize so a symlinked launch of the bundle binary still
    // anchors ../Frameworks at the real Contents/MacOS, not at the
    // symlink's parent (review finding, plan 028 C5).
    let exe = exe.canonicalize().unwrap_or(exe);
    let dir = exe
        .parent()
        .ok_or_else(|| format!("executable {} has no parent directory", exe.display()))?;
    Ok(dir.join(FRAMEWORK_REL_PATH))
}

fn dlopen(path: &std::path::Path) -> Result<*mut c_void, String> {
    // dyld consults DYLD_FALLBACK_LIBRARY_PATH for a leaf name when the
    // given path is missing — a stray "Sparkle" dylib on that path could
    // load in a dev environment. One stat keeps unavailability
    // deterministic.
    if !path.exists() {
        return Err(format!("{} does not exist", path.display()));
    }
    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| format!("framework path {} contains a NUL byte", path.display()))?;
    // SAFETY: a valid NUL-terminated path and a documented flag. The
    // handle is never passed to `dlclose`.
    let handle = unsafe { libc::dlopen(c_path.as_ptr(), libc::RTLD_LAZY) };
    if handle.is_null() {
        // SAFETY: `dlerror` returns a pointer to a static, thread-local
        // C string owned by the loader, valid until the next dl* call.
        let detail = unsafe {
            let raw = libc::dlerror();
            if raw.is_null() {
                "no dlerror detail".to_string()
            } else {
                CStr::from_ptr(raw).to_string_lossy().into_owned()
            }
        };
        return Err(format!("dlopen({}) failed: {detail}", path.display()));
    }
    Ok(handle)
}

// ---------------------------------------------------------------------
// Small ObjC reads
// ---------------------------------------------------------------------

/// The version string to report for a found update.
///
/// `displayVersionString` falls back to `versionString` when the item
/// carries no short version (`SUAppcastItem.h:61`), so it is the one
/// read that always answers. Guarded by `respondsToSelector:` anyway: an
/// appcast item from a Sparkle this seam was not written against is data
/// to report around, not a reason to crash a check.
fn appcast_item_version(item: &AnyObject) -> Option<String> {
    // SAFETY: `respondsToSelector:` is NSObject's own; the guarded
    // selector names a readonly `NSString` property on SUAppcastItem.
    unsafe {
        let responds: bool = msg_send![item, respondsToSelector: objc2::sel!(displayVersionString)];
        if !responds {
            return None;
        }
        let value: Option<Retained<NSString>> = msg_send![item, displayVersionString];
        value.map(|string| string.to_string())
    }
}

/// `NSError.localizedDescription`, or a placeholder — the seam reports
/// *something* for every failure rather than an empty detail.
fn error_description(error: &AnyObject) -> String {
    // SAFETY: `localizedDescription` is a readonly `NSString` property
    // on NSError, which is the only type this is ever handed.
    let description: Option<Retained<NSString>> = unsafe { msg_send![error, localizedDescription] };
    description.map_or_else(|| "unknown error".to_string(), |string| string.to_string())
}
