//! The `UNUserNotificationCenter` backend behind [`crate::notifications`].
//!
//! Threading is why this module is the seam's documented exception (see
//! [`crate::macos`]). `show()` runs on the engine's tokio worker, and a
//! `Retained<UNUserNotificationCenter>` is `!Send` — so the center is
//! fetched per call from `currentNotificationCenter()` (which is
//! `AnyThread`) and never stored. Delegate callbacks arrive on an
//! unspecified queue, so the delegate class is deliberately *not*
//! `MainThreadOnly` and touches only [`PENDING`] and the atomics. What is
//! main-thread stays main-thread: [`init`] takes the marker, and the
//! retained delegate plus the init bookkeeping live in `thread_local!`s
//! that nothing off the main thread reads.
//!
//! Replace, not stack: the notification identifier is `roost-tab-{id}`,
//! stable for the life of the tab, and UN's documented behaviour for a
//! second `add()` under an identifier already delivered is to replace it.
//! That is a conscious divergence from the Swift app, which mints a
//! unique identifier per event and therefore stacks — and it is what
//! honours the seam's replace contract, including for a fire that lands
//! after a `Retire` (the lingering banner is replaced, not joined).
//!
//! Authorization is requested once at [`init`] and re-read from the
//! system by [`refresh_authorization`] on every focus gain; an
//! unauthorized center makes every `show()` a silent no-op, matching
//! `mac/Sources/Roost/DesktopNotifications.swift:94-95`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};

use block2::{DynBlock, RcBlock};
use objc2::rc::Retained;
use objc2::runtime::{Bool, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, AnyThread, MainThreadMarker};
use objc2_foundation::{NSArray, NSError, NSSet, NSString};
use objc2_user_notifications::{
    UNAuthorizationOptions, UNAuthorizationStatus, UNMutableNotificationContent, UNNotification,
    UNNotificationCategory, UNNotificationCategoryOptions, UNNotificationDefaultActionIdentifier,
    UNNotificationPresentationOptions, UNNotificationRequest, UNNotificationResponse,
    UNNotificationSettings, UNUserNotificationCenter, UNUserNotificationCenterDelegate,
};
use roost_ipc::messages::AppNotificationStatusResult;
use roost_ui_model::keys::TabKey;
use tokio::sync::oneshot;

use crate::notifications::{Payload, Shown};

/// The title a banner carries when the event supplied none. The seam
/// passes an empty title through on purpose; substituting is the
/// backend's job, exactly as in `DesktopNotifications.swift:97`.
const FALLBACK_TITLE: &str = "Roost";

/// Why a bare `cargo run` binary gets no notifications: UN identifies the
/// sender by its bundle, and calling into it without one can abort the
/// process — so this state is decided *before* any UN API is touched.
const NO_BUNDLE_REASON: &str = "not running from an app bundle";

/// Why [`status`] reports unavailable before [`init`] has ever run.
/// `window_opened` is what calls `init`, so a status read landing first
/// — an IPC request racing the first window, vanishingly rare but
/// possible — has genuinely nothing else to report.
const NOT_INITIALIZED_REASON: &str = "window not opened yet";

/// Registered with no actions, matching the Swift app's `roost-tab`
/// category. Apple's docs imply the default (banner-body) action needs no
/// category, but the Swift M8 spike found clicks were delivered as plain
/// dismissals until the category was registered
/// (`DesktopNotifications.swift:58`) — the in-repo evidence wins.
const CATEGORY: &str = "roost-tab";

/// Whether [`init`] reached a bundled center at all.
static ENABLED: AtomicBool = AtomicBool::new(false);
/// The user's answer to the authorization prompt, written from the
/// completion blocks of [`request_authorization`] and
/// [`refresh_authorization`] on whatever queue UN chose.
static AUTHORIZED: AtomicBool = AtomicBool::new(false);

/// Which query owns [`AUTHORIZED`]. UN completes these queries
/// asynchronously on unspecified queues and documents no ordering
/// between separately issued ones, so each carries the generation of the
/// query that issued it and only the newest may write. Without that, a
/// delayed `Denied` snapshot could land after a newer `Authorized` one
/// and overwrite it — muting notifications with no further focus-gain
/// edge to correct it, which is the very staleness this module refreshes
/// to avoid.
static QUERY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// The click channel for every banner currently believed to be on screen,
/// keyed by notification identifier. Not a `thread_local!`: `show()`
/// registers from the tokio worker and the delegate resolves from an
/// unspecified queue.
static PENDING: LazyLock<Mutex<PendingMap>> = LazyLock::new(Mutex::default);

type PendingMap = HashMap<String, oneshot::Sender<bool>>;

/// The retained Objective-C side. `UNUserNotificationCenter` holds its
/// delegate **weakly**, so this field is the only thing keeping the
/// delegate alive for the life of the process.
struct Objects {
    _delegate: Retained<NotificationDelegate>,
}

thread_local! {
    static OBJECTS: RefCell<Option<Objects>> = const { RefCell::new(None) };
    /// Why notifications are unavailable, or `None` once the center is
    /// live. Separate from [`OBJECTS`] because the failure path has no
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
    // - No `#[thread_kind]`: UN delivers both callbacks on an
    //   unspecified queue, and they touch only `PENDING` — never a
    //   thread_local, never AppKit.
    #[unsafe(super(NSObject))]
    #[name = "RoostUserNotificationDelegate"]
    struct NotificationDelegate;

    unsafe impl NSObjectProtocol for NotificationDelegate {}

    /// `UNUserNotificationCenterDelegate` is a *formal* protocol whose
    /// members are all `@optional`, so conformance is declared here
    /// rather than probed with `respondsToSelector:` the way the Sparkle
    /// seam's informal delegate is.
    unsafe impl UNUserNotificationCenterDelegate for NotificationDelegate {
        /// Without this, macOS suppresses banners while Roost is
        /// frontmost. Policy B already dropped anything fired for the tab
        /// in view, so what this presents is a *background* tab's banner
        /// while the window is focused — Swift parity
        /// (`DesktopNotifications.swift:130-136`), and the reason the
        /// content itself carries no sound.
        #[unsafe(method(userNotificationCenter:willPresentNotification:withCompletionHandler:))]
        fn will_present(
            &self,
            _center: &UNUserNotificationCenter,
            _notification: &UNNotification,
            completion: &DynBlock<dyn Fn(UNNotificationPresentationOptions)>,
        ) {
            completion.call((UNNotificationPresentationOptions::Banner
                | UNNotificationPresentationOptions::Sound,));
        }

        #[unsafe(method(userNotificationCenter:didReceiveNotificationResponse:withCompletionHandler:))]
        fn did_receive_response(
            &self,
            _center: &UNUserNotificationCenter,
            response: &UNNotificationResponse,
            completion: &DynBlock<dyn Fn()>,
        ) {
            let identifier = response.notification().request().identifier().to_string();
            let clicked = is_default_action(response);
            with_pending(|pending| resolve(pending, &identifier, clicked));
            // UN documents this as mandatory before returning.
            completion.call(());
        }
    }
);

/// A click on the banner body, as opposed to a dismiss or a button.
fn is_default_action(response: &UNNotificationResponse) -> bool {
    // SAFETY: reading one of the framework's own `NSString` constants.
    let default = unsafe { UNNotificationDefaultActionIdentifier };
    *response.actionIdentifier() == *default
}

// ---------------------------------------------------------------------
// Init
// ---------------------------------------------------------------------

/// What [`init`] decided, off ObjC so the bare-binary refusal is
/// testable.
#[derive(Debug, PartialEq, Eq)]
enum Gate {
    Enable,
    Unavailable(&'static str),
}

fn gate(bundle_id: Option<&str>) -> Gate {
    match bundle_id {
        Some(_) => Gate::Enable,
        None => Gate::Unavailable(NO_BUNDLE_REASON),
    }
}

/// Install the delegate and request authorization, once.
///
/// Called from `window_opened` beside the other seam inits. The marker is
/// not handed to UN — nothing here is main-thread-only *to AppKit* — but
/// it is what proves the `thread_local!`s below are the ones every other
/// caller sees.
pub(crate) fn init(_mtm: MainThreadMarker) {
    if INITIALIZED.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), true)) {
        return;
    }
    match gate(crate::main_bundle_identifier().as_deref()) {
        Gate::Unavailable(reason) => {
            tracing::info!(reason, "notifications: backend unavailable");
            UNAVAILABLE.with(|cell| *cell.borrow_mut() = Some(reason.to_string()));
        }
        Gate::Enable => {
            // SAFETY: `init` on a freshly allocated instance of our own class.
            let delegate: Retained<NotificationDelegate> =
                unsafe { msg_send![NotificationDelegate::alloc(), init] };
            let center = UNUserNotificationCenter::currentNotificationCenter();
            center.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            let category =
                UNNotificationCategory::categoryWithIdentifier_actions_intentIdentifiers_options(
                    &NSString::from_str(CATEGORY),
                    &NSArray::new(),
                    &NSArray::new(),
                    UNNotificationCategoryOptions::empty(),
                );
            center.setNotificationCategories(&NSSet::from_retained_slice(&[category]));
            OBJECTS.with(|cell| {
                *cell.borrow_mut() = Some(Objects {
                    _delegate: delegate,
                })
            });
            ENABLED.store(true, Ordering::SeqCst);
            request_authorization(&center);
            tracing::info!("notifications: UN delegate installed, authorization requested");
        }
    }
}

/// Claim the next [`QUERY_GENERATION`] ticket, from wherever the
/// caller's answer is at its freshest.
fn issue_query() -> u64 {
    QUERY_GENERATION.fetch_add(1, Ordering::SeqCst) + 1
}

/// Record a completion's snapshot, unless a later query has since been
/// issued: that one's answer is fresher by construction and writes when
/// it lands, so an older snapshot is dropped rather than allowed to
/// overwrite it. The transition log lives inside the guard — a discarded
/// snapshot must not report a change it did not make.
fn store_authorization(generation: u64, authorized: bool) {
    if QUERY_GENERATION.load(Ordering::SeqCst) != generation {
        return;
    }
    let previous = AUTHORIZED.swap(authorized, Ordering::SeqCst);
    if previous != authorized {
        tracing::info!(
            from = previous,
            to = authorized,
            "notifications: authorization changed"
        );
    }
}

/// The same three options the Swift app asks for
/// (`DesktopNotifications.swift:77-88`). macOS persists the user's answer
/// against the bundle id, so a repeat launch re-answers from that record
/// rather than re-prompting.
fn request_authorization(center: &UNUserNotificationCenter) {
    let handler = RcBlock::new(|granted: Bool, error: *mut NSError| {
        if !error.is_null() {
            // SAFETY: UN hands the block a borrowed, autoreleased NSError.
            let detail = unsafe { (*error).localizedDescription() };
            tracing::warn!(%detail, "notifications: authorization error");
        }
        // The ticket is claimed here, not at issue time as
        // `refresh_authorization` does — the asymmetry between the two
        // writers. A settings query is a snapshot and ages; an
        // authorization answer is an event, current the instant it
        // fires however long the prompt stood. Claiming now is what
        // stops a refresh issued while the prompt was up from
        // discarding the user's actual answer.
        store_authorization(issue_query(), granted.as_bool());
        tracing::info!(
            granted = granted.as_bool(),
            "notifications: authorization answered"
        );
    });
    center.requestAuthorizationWithOptions_completionHandler(
        UNAuthorizationOptions::Alert
            | UNAuthorizationOptions::Sound
            | UNAuthorizationOptions::Badge,
        &handler,
    );
}

/// Re-read the system's answer, because it can change under a running
/// app: the user denies at first launch, grants in System Settings, and
/// comes back. Called on the focus-gain edge, which is the moment that
/// return happens.
///
/// Gated on [`ENABLED`] rather than `INITIALIZED`: the latter is set even
/// when [`gate`] refused, and touching the center from a bare binary is
/// the abort that gate exists to prevent.
pub(crate) fn refresh_authorization(_mtm: MainThreadMarker) {
    if !ENABLED.load(Ordering::SeqCst) {
        return;
    }
    let generation = issue_query();
    let handler = RcBlock::new(move |settings: NonNull<UNNotificationSettings>| {
        // SAFETY: UN hands the block a borrowed, autoreleased settings object.
        let status = unsafe { settings.as_ref() }.authorizationStatus();
        store_authorization(generation, authorized_from_status(status));
        if status == UNAuthorizationStatus::NotDetermined {
            request_authorization(&UNUserNotificationCenter::currentNotificationCenter());
        }
    });
    UNUserNotificationCenter::currentNotificationCenter()
        .getNotificationSettingsWithCompletionHandler(&handler);
}

/// The `app.notification_status` payload — the marker proves the
/// `thread_local!` read below is on the thread every other caller of
/// [`init`] sees, same shape as `sparkle::status`.
pub(crate) fn status(_mtm: MainThreadMarker) -> AppNotificationStatusResult {
    if !INITIALIZED.with(|cell| *cell.borrow()) {
        return AppNotificationStatusResult {
            backend: "unavailable".into(),
            reason: Some(NOT_INITIALIZED_REASON.into()),
            authorized: false,
        };
    }
    if ENABLED.load(Ordering::SeqCst) {
        AppNotificationStatusResult {
            backend: "available".into(),
            reason: None,
            authorized: AUTHORIZED.load(Ordering::SeqCst),
        }
    } else {
        AppNotificationStatusResult {
            backend: "unavailable".into(),
            reason: UNAVAILABLE.with(|cell| cell.borrow().clone()),
            authorized: false,
        }
    }
}

// ---------------------------------------------------------------------
// Showing
// ---------------------------------------------------------------------

/// One banner, awaited on the engine's tokio worker.
///
/// Nothing `Retained` is held across the `await`: [`submit`] does the
/// whole Objective-C leg synchronously and hands back a channel, which is
/// what keeps this future `Send`.
pub(crate) async fn show(payload: Payload) -> Result<Shown, String> {
    if !show_allowed(
        ENABLED.load(Ordering::SeqCst),
        AUTHORIZED.load(Ordering::SeqCst),
    ) {
        tracing::debug!(
            title = %payload.title,
            tab_id = payload.tab.tab,
            "notifications: backend not enabled; nothing shown"
        );
        return Ok(Shown::nothing());
    }

    let identifier = identifier_for(payload.tab);
    let activation = with_pending(|pending| {
        // Reclaims both the entry a worker-side SHOW_TIMEOUT abandoned
        // and the one a `Retire` left behind: either way the listener
        // dropped the receiver, which closes the sender.
        sweep_closed(pending);
        register(pending, &identifier)
    });

    let added = submit(&identifier, &payload);
    let failure = match added.await {
        Ok(detail) => detail,
        Err(_) => Some("the notification add completion never answered".to_string()),
    };
    if let Some(detail) = failure {
        with_pending(|pending| unregister(pending, &identifier));
        return Err(detail);
    }

    Ok(Shown {
        // The worker only ever hands this back as `replaces`; the
        // identifier is what actually makes the next banner replace this
        // one, so a truncating cast costs nothing.
        server_id: Some(payload.tab.tab as u32),
        // A closed channel is a dismissed banner, never a panic: a
        // replacing show drops the displaced sender while its listener is
        // still awaiting, and a panic there would kill that task.
        activation: Some(Box::pin(async move { activation.await.unwrap_or(false) })),
    })
}

/// Build the request and hand it to the center. The returned channel
/// carries the add's failure detail, or `None` when it was accepted.
///
/// The whole Objective-C leg runs under an explicit autorelease pool:
/// this executes on a tokio worker thread, which has none of its own, so
/// any `+0` return Foundation actually autoreleases (the class factories
/// here) would otherwise accumulate for the thread's lifetime.
fn submit(identifier: &str, payload: &Payload) -> oneshot::Receiver<Option<String>> {
    let (tx, rx) = oneshot::channel();
    // The completion is an `Fn` block that UN may hold beyond this call,
    // so the (single-use) sender lives behind a lock rather than being
    // moved into the closure.
    let sender = Mutex::new(Some(tx));
    objc2::rc::autoreleasepool(|_| {
        let completion = RcBlock::new(move |error: *mut NSError| {
            let detail = (!error.is_null()).then(|| {
                // SAFETY: UN hands the block a borrowed, autoreleased NSError.
                unsafe { (*error).localizedDescription() }.to_string()
            });
            if let Some(sender) = sender.lock().ok().and_then(|mut slot| slot.take()) {
                let _ = sender.send(detail);
            }
        });

        let content = UNMutableNotificationContent::new();
        content.setTitle(&NSString::from_str(display_title(&payload.title)));
        if let Some(body) = &payload.body {
            content.setBody(&NSString::from_str(body));
        }
        content.setCategoryIdentifier(&NSString::from_str(CATEGORY));
        // A nil trigger is UN's "deliver now".
        let request = UNNotificationRequest::requestWithIdentifier_content_trigger(
            &NSString::from_str(identifier),
            &content,
            None,
        );
        // UN copies the block, so it outlives this pool scope.
        UNUserNotificationCenter::currentNotificationCenter()
            .addNotificationRequest_withCompletionHandler(&request, Some(&completion));
    });
    rx
}

// ---------------------------------------------------------------------
// Bookkeeping — plain Rust, so every rule above is testable off ObjC
// ---------------------------------------------------------------------

fn show_allowed(enabled: bool, authorized: bool) -> bool {
    enabled && authorized
}

/// Only an explicit yes counts: `Denied`, `NotDetermined`, `Ephemeral`
/// and any raw value this build does not know about all fall closed.
fn authorized_from_status(status: UNAuthorizationStatus) -> bool {
    status == UNAuthorizationStatus::Authorized || status == UNAuthorizationStatus::Provisional
}

/// Stable for the life of the tab: that is the whole replace mechanism.
///
/// The identifier space is the whole process's, so it is keyed on the
/// host-qualified tab rather than the bare id — two instances' tab 7 are
/// two tabs and must not share (and silently replace) one banner. A local
/// tab still renders `roost-tab-7`, so nothing an existing banner is
/// registered under moves.
fn identifier_for(tab: TabKey) -> String {
    format!("roost-tab-{tab}")
}

fn display_title(title: &str) -> &str {
    if title.is_empty() {
        FALLBACK_TITLE
    } else {
        title
    }
}

/// Drop the entries whose listener is gone. O(live banners), and the only
/// thing that keeps the map from growing across a long session.
fn sweep_closed(pending: &mut PendingMap) {
    pending.retain(|_, sender| !sender.is_closed());
}

/// Registered *before* the add, so a click that lands the instant the
/// banner appears still finds its channel. Re-registering under the same
/// identifier drops the sender it displaces — that banner is gone, and
/// the worker aborts its listener anyway.
fn register(pending: &mut PendingMap, identifier: &str) -> oneshot::Receiver<bool> {
    let (tx, rx) = oneshot::channel();
    pending.insert(identifier.to_string(), tx);
    rx
}

fn unregister(pending: &mut PendingMap, identifier: &str) {
    pending.remove(identifier);
}

fn resolve(pending: &mut PendingMap, identifier: &str, clicked: bool) {
    if let Some(sender) = pending.remove(identifier) {
        let _ = sender.send(clicked);
    }
}

/// A poisoned lock is recovered rather than propagated: the map holds
/// nothing but channel ends, and refusing to unlock it would wedge every
/// later notification for the life of the process.
fn with_pending<T>(f: impl FnOnce(&mut PendingMap) -> T) -> T {
    let mut pending = PENDING
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f(&mut pending)
}

#[cfg(test)]
mod tests {
    use roost_ui_model::keys::HostId;

    use super::*;

    #[test]
    fn a_tabs_identifier_is_stable_so_the_next_banner_replaces_the_last() {
        assert_eq!(identifier_for(TabKey::local(7)), "roost-tab-7");
        assert_eq!(
            identifier_for(TabKey::local(7)),
            identifier_for(TabKey::local(7))
        );
        assert_ne!(
            identifier_for(TabKey::local(7)),
            identifier_for(TabKey::local(8))
        );
        assert_ne!(
            identifier_for(TabKey::local(7)),
            identifier_for(TabKey::new(HostId::new(1), 7)),
            "two instances' tab 7 are two banners, not one that replaces the other"
        );
    }

    #[test]
    fn an_empty_title_falls_back_to_the_app_name() {
        assert_eq!(display_title(""), "Roost");
        assert_eq!(display_title("Claude"), "Claude");
    }

    #[test]
    fn a_bare_binary_never_reaches_the_center() {
        assert_eq!(gate(None), Gate::Unavailable(NO_BUNDLE_REASON));
        assert_eq!(gate(Some("ai.stridelabs.Roost-Iced")), Gate::Enable);
    }

    #[test]
    fn a_show_needs_both_a_center_and_the_users_consent() {
        assert!(show_allowed(true, true));
        assert!(!show_allowed(true, false));
        assert!(!show_allowed(false, true));
        assert!(!show_allowed(false, false));
    }

    /// Raw values rather than the constants, so the arms are enumerated
    /// exhaustively over what the type can hold rather than over what
    /// this build happens to name.
    #[test]
    fn only_an_explicit_yes_authorizes_and_an_unknown_status_falls_closed() {
        assert_eq!(
            UNAuthorizationStatus::Authorized,
            UNAuthorizationStatus(2),
            "the raw values below are the crate's constants"
        );
        assert!(
            !authorized_from_status(UNAuthorizationStatus(0)),
            "notDetermined"
        );
        assert!(!authorized_from_status(UNAuthorizationStatus(1)), "denied");
        assert!(
            authorized_from_status(UNAuthorizationStatus(2)),
            "authorized"
        );
        assert!(
            authorized_from_status(UNAuthorizationStatus(3)),
            "provisional"
        );
        assert!(
            !authorized_from_status(UNAuthorizationStatus(4)),
            "ephemeral"
        );

        for unknown in [5, -1, isize::MAX, isize::MIN] {
            assert!(
                !authorized_from_status(UNAuthorizationStatus(unknown)),
                "a status this build does not know about is not consent: {unknown}"
            );
        }
    }

    /// The one test that mutates [`AUTHORIZED`] and [`QUERY_GENERATION`],
    /// deliberately so: they are process-wide, and a sibling test
    /// touching them would race this one under the parallel runner. New
    /// cases for the guard belong inside here, not beside it.
    #[test]
    fn only_the_newest_answer_may_write_the_cached_authorization() {
        let overtaken = issue_query();
        let newest = issue_query();
        AUTHORIZED.store(false, Ordering::SeqCst);

        store_authorization(overtaken, true);
        assert!(
            !AUTHORIZED.load(Ordering::SeqCst),
            "a completion a later query overtook is dropped, not applied"
        );
        store_authorization(newest, true);
        assert!(
            AUTHORIZED.load(Ordering::SeqCst),
            "the newest query's answer is the one that lands"
        );

        // The first-launch shape: a refresh is issued while the prompt
        // still stands, then the user answers. The answer claims its
        // ticket as it fires, so it outranks that refresh — and the
        // refresh's older snapshot cannot undo it afterwards.
        let refresh_in_flight = issue_query();
        store_authorization(issue_query(), false);
        assert!(
            !AUTHORIZED.load(Ordering::SeqCst),
            "the user's answer outranks a refresh issued before they gave it"
        );
        store_authorization(refresh_in_flight, true);
        assert!(
            !AUTHORIZED.load(Ordering::SeqCst),
            "and that refresh's stale snapshot cannot undo the answer"
        );
    }

    #[test]
    fn registering_again_drops_the_sender_it_displaces() {
        let mut pending = PendingMap::new();
        let mut first = register(&mut pending, "roost-tab-7");
        let mut second = register(&mut pending, "roost-tab-7");
        assert_eq!(pending.len(), 1, "one live banner per tab");
        assert_eq!(
            first.try_recv(),
            Err(oneshot::error::TryRecvError::Closed),
            "the displaced listener sees a closed channel, not a click"
        );

        resolve(&mut pending, "roost-tab-7", true);
        assert_eq!(second.try_recv(), Ok(true));
        assert!(pending.is_empty());
    }

    #[test]
    fn a_sweep_reclaims_the_entries_whose_listener_is_gone() {
        let mut pending = PendingMap::new();
        let dropped = register(&mut pending, "roost-tab-7");
        let _live = register(&mut pending, "roost-tab-8");
        drop(dropped);

        sweep_closed(&mut pending);
        assert_eq!(pending.keys().collect::<Vec<_>>(), vec!["roost-tab-8"]);
    }

    #[test]
    fn a_dismiss_resolves_false_and_an_unknown_identifier_is_a_no_op() {
        let mut pending = PendingMap::new();
        let mut activation = register(&mut pending, "roost-tab-7");
        resolve(&mut pending, "roost-tab-9", true);
        assert_eq!(pending.len(), 1, "another banner's click resolves nothing");

        resolve(&mut pending, "roost-tab-7", false);
        assert_eq!(activation.try_recv(), Ok(false));
    }

    #[test]
    fn a_failed_add_takes_its_sender_back_out() {
        let mut pending = PendingMap::new();
        let mut activation = register(&mut pending, "roost-tab-7");
        unregister(&mut pending, "roost-tab-7");
        assert!(pending.is_empty());
        assert_eq!(
            activation.try_recv(),
            Err(oneshot::error::TryRecvError::Closed),
            "the listener for a banner that never appeared ends"
        );
    }
}
