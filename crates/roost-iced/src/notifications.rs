//! Native desktop notifications.
//!
//! The engine already decides whether a notification is warranted — a
//! focused window on the active tab emits no `NotificationFired` at all —
//! so every event that reaches this adapter fires.
//!
//! Invariant: backend I/O never runs on the UI thread or under an engine
//! lock. [`DesktopNotifications::fire`] and [`DesktopNotifications::retire`]
//! are non-blocking channel sends from the feed drain, and a single worker
//! task on the engine runtime owns every await. That worker drains
//! sequentially, which is what makes a tab's replace see the id its own
//! previous notification returned.
//!
//! A click on the banner comes back the other way: the worker spawns one
//! listener task per shown notification, and the click it observes lands on
//! the engine feed as [`EngineFeed::NotificationActivated`] — the same
//! channel every other engine → UI item travels, so the drain applies it in
//! arrival order with everything else.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::task::AbortHandle;

use crate::engine_feed::{EngineFeed, EngineFeedSender};

/// A wedged notification server (or a D-Bus connect that never completes)
/// must not stall every later notification behind it.
const SHOW_TIMEOUT: Duration = Duration::from_secs(5);

/// What the UI thread hands the worker.
enum Request {
    Fire {
        tab_id: i64,
        title: String,
        body: String,
    },
    /// The tab closed or its notification was cleared. gio owns the map on
    /// the GTK side; here it is ours, so it needs telling.
    Retire { tab_id: i64 },
}

/// One tab's live desktop notification, as far as this adapter knows.
#[derive(Default)]
struct TabSlot {
    /// The id the server returned for this tab's last shown notification.
    /// Handing it back is what makes the next one replace rather than
    /// stack — GTK gets the same from its fixed per-tab gio id.
    server_id: Option<u32>,
    /// The task awaiting a click on that notification. At most one per tab,
    /// ever: a replace emits no `NotificationClosed`, and some servers emit
    /// neither that nor `ActionInvoked` at all, so a listener that is not
    /// aborted when its banner is replaced (or its tab retired) never ends.
    listener: Option<AbortHandle>,
}

impl TabSlot {
    fn abort_listener(&mut self) {
        if let Some(listener) = self.listener.take() {
            listener.abort();
        }
    }
}

/// The future that resolves once the user acts on a shown notification:
/// `true` for a click on the banner body, `false` for anything else (a
/// dismiss, or a server that closed it).
pub(crate) type Activation = Pin<Box<dyn Future<Output = bool> + Send>>;

/// What one show produced. The `activation` is `None` on backends that
/// cannot report a click at all, which is why the worker — not the
/// backend — owns the listener bookkeeping.
pub(crate) struct Shown {
    pub(crate) server_id: Option<u32>,
    pub(crate) activation: Option<Activation>,
}

impl Shown {
    /// Nothing reached the desktop: no id to replace, no banner to click.
    pub(crate) fn nothing() -> Self {
        Self {
            server_id: None,
            activation: None,
        }
    }
}

/// A show request in toolkit-neutral form, so the mapping is testable off
/// Linux and a second backend has one shape to implement.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Payload {
    /// The tab this fired for. Backends that key their notifications by
    /// tab rather than by server id build their identifier from it; the
    /// freedesktop one replaces via [`Payload::replaces`] and ignores it.
    pub(crate) tab_id: i64,
    pub(crate) title: String,
    /// `None` when the notification carried no body — an empty body line
    /// is worse than none, and GTK omits it the same way.
    pub(crate) body: Option<String>,
    pub(crate) replaces: Option<u32>,
}

pub(crate) struct DesktopNotifications {
    tx: mpsc::UnboundedSender<Request>,
}

impl DesktopNotifications {
    /// `app_id` is the resolved bundle profile's id, captured rather than
    /// threaded through `spawn_on`'s `Fn(Payload) -> Fut` bound so the
    /// backend shape stays the one the tests implement.
    pub(crate) fn new(
        runtime: &tokio::runtime::Handle,
        feed: EngineFeedSender,
        app_id: String,
    ) -> Self {
        Self::spawn_on(
            runtime,
            move |payload| backend::show(payload, app_id.clone()),
            feed,
        )
    }

    fn spawn_on<F, Fut>(runtime: &tokio::runtime::Handle, show: F, feed: EngineFeedSender) -> Self
    where
        F: Fn(Payload) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Shown, String>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        runtime.spawn(run(rx, show, feed));
        Self { tx }
    }

    pub(crate) fn fire(&self, tab_id: i64, title: String, body: String) {
        self.send(Request::Fire {
            tab_id,
            title,
            body,
        });
    }

    pub(crate) fn retire(&self, tab_id: i64) {
        self.send(Request::Retire { tab_id });
    }

    fn send(&self, request: Request) {
        if self.tx.send(request).is_err() {
            tracing::debug!("desktop notification worker is gone");
        }
    }
}

async fn run<F, Fut>(mut rx: mpsc::UnboundedReceiver<Request>, show: F, feed: EngineFeedSender)
where
    F: Fn(Payload) -> Fut,
    Fut: Future<Output = Result<Shown, String>>,
{
    let mut slots: HashMap<i64, TabSlot> = HashMap::new();
    while let Some(request) = rx.recv().await {
        let (tab_id, title, body) = match request {
            Request::Retire { tab_id } => {
                if let Some(mut slot) = slots.remove(&tab_id) {
                    slot.abort_listener();
                    #[cfg(target_os = "linux")]
                    if let Some(id) = slot.server_id {
                        if tokio::time::timeout(SHOW_TIMEOUT, backend::close_notification(id))
                            .await
                            .is_err()
                        {
                            tracing::warn!(id, "CloseNotification timed out on retire");
                        }
                    }
                }
                continue;
            }
            Request::Fire {
                tab_id,
                title,
                body,
            } => (tab_id, title, body),
        };
        let previous = slots.get(&tab_id).and_then(|slot| slot.server_id);
        let shown = match tokio::time::timeout(
            SHOW_TIMEOUT,
            show(build_payload(tab_id, title, body, previous)),
        )
        .await
        {
            Ok(Ok(shown)) => shown,
            Ok(Err(error)) => {
                tracing::warn!(tab_id, %error, "desktop notification failed");
                Shown::nothing()
            }
            Err(_) => {
                tracing::warn!(tab_id, "desktop notification timed out");
                Shown::nothing()
            }
        };
        let listener = shown
            .activation
            .map(|activation| spawn_listener(&feed, tab_id, activation).abort_handle());
        record_shown(&mut slots, tab_id, shown.server_id, listener);
    }
}

/// Await one banner click off the worker's own task, so a server that never
/// answers cannot stall the next notification. The task borrows no worker
/// state — only the tab id and a clone of the feed — which is what lets the
/// worker abort it at any moment.
fn spawn_listener(
    feed: &EngineFeedSender,
    tab_id: i64,
    activation: Activation,
) -> tokio::task::JoinHandle<()> {
    let feed = feed.clone();
    tokio::spawn(async move {
        if activation.await {
            tracing::info!(tab_id, "desktop notification clicked");
            feed.send(EngineFeed::NotificationActivated { tab_id });
        }
    })
}

fn build_payload(tab_id: i64, title: String, body: String, replaces: Option<u32>) -> Payload {
    Payload {
        tab_id,
        title,
        body: (!body.is_empty()).then_some(body),
        replaces,
    }
}

/// Record what a show returned. A failed or timed-out show returns no id
/// and leaves the previous one in place on purpose: that notification may
/// still be on screen, and forgetting its id would stack a duplicate
/// beside it instead of replacing it. The inverse — a timed-out show that
/// actually landed on the bus under an id we never learned — leaves one
/// stray banner the next fire cannot replace; with a healthy session bus
/// the 5s timeout makes that pathological, so it is logged, not tracked.
///
/// The listener follows the same rule for the same reason: a new one
/// aborts the one it displaces (the banner behind it is gone, replaced),
/// while a show that produced none leaves the previous listener awaiting
/// the banner that is still up.
fn record_shown(
    slots: &mut HashMap<i64, TabSlot>,
    tab_id: i64,
    server_id: Option<u32>,
    listener: Option<AbortHandle>,
) {
    if server_id.is_none() && listener.is_none() {
        return;
    }
    let slot = slots.entry(tab_id).or_default();
    if server_id.is_some() {
        slot.server_id = server_id;
    }
    if listener.is_some() {
        slot.abort_listener();
        slot.listener = listener;
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use std::collections::HashMap;

    use iced::futures::StreamExt;
    use zbus::message::Type as MessageType;
    use zbus::zvariant::Value;
    use zbus::{MatchRule, MessageStream};

    use super::{Payload, Shown};

    /// The freedesktop key a server invokes for a click on the banner body
    /// rather than on a button. Declaring it is what makes the banner
    /// clickable at all — servers only invoke actions a notification lists.
    const DEFAULT_ACTION: &str = "default";
    const DEFAULT_ACTION_LABEL: &str = "Open";

    const NOTIFICATIONS_BUS: &str = "org.freedesktop.Notifications";
    const NOTIFICATIONS_PATH: &str = "/org/freedesktop/Notifications";
    const NOTIFICATIONS_INTERFACE: &str = "org.freedesktop.Notifications";

    /// Freedesktop Desktop Notifications spec: `expire_timeout` `0` means
    /// the notification never expires; `-1` defers to the server. We send
    /// `0` so a click-to-focus banner stays until the user acts or
    /// dismisses — servers that cap `-1` to a few seconds (several
    /// compositors do) otherwise hide the popup before it can be clicked.
    /// The tab badge remains the durable indicator either way.
    const EXPIRE_NEVER: i32 = 0;

    /// `app_id` is the desktop-entry hint — best-effort shell grouping: a
    /// desktop file may not be installed for a dev build, and the hint is
    /// simply ignored then.
    pub(super) async fn show(payload: Payload, app_id: String) -> Result<Shown, String> {
        let connection = zbus::Connection::session()
            .await
            .map_err(|error| error.to_string())?;
        // Subscribe before Notify so a fast click cannot land in the gap
        // between show and the listener task starting. Same connection as
        // Notify, so a server that unicasts ActionInvoked at the sender
        // still delivers it here.
        let mut stream = action_invoked_stream(&connection).await?;
        let id = send_notify(&connection, &app_id, &payload).await?;
        tracing::info!(id, "desktop notification shown");
        Ok(Shown {
            server_id: Some(id),
            activation: Some(Box::pin(async move {
                let clicked = drain_action_invoked(id, &mut stream).await;
                // `resident` + expire 0: the server will not withdraw the
                // banner for us after the action. Close it ourselves so a
                // click (or a later tab close via `Retire`) cannot leave a
                // permanent inert popup.
                if clicked
                    && tokio::time::timeout(super::SHOW_TIMEOUT, close_on(&connection, id))
                        .await
                        .is_err()
                {
                    tracing::warn!(id, "CloseNotification timed out after click");
                }
                clicked
            })),
        })
    }

    async fn send_notify(
        connection: &zbus::Connection,
        app_id: &str,
        payload: &Payload,
    ) -> Result<u32, String> {
        let mut hints: HashMap<String, Value<'static>> = HashMap::new();
        hints.insert("desktop-entry".into(), Value::from(app_id.to_string()));
        hints.insert("resident".into(), Value::from(true));
        hints.insert("urgency".into(), Value::from(1u8));
        let body = payload.body.as_deref().unwrap_or("");
        let replaces = payload.replaces.unwrap_or(0);
        let reply = connection
            .call_method(
                Some(NOTIFICATIONS_BUS),
                NOTIFICATIONS_PATH,
                Some(NOTIFICATIONS_INTERFACE),
                "Notify",
                &(
                    "Roost",
                    replaces,
                    "",
                    payload.title.as_str(),
                    body,
                    notify_actions(),
                    hints,
                    EXPIRE_NEVER,
                ),
            )
            .await
            .map_err(|error| error.to_string())?;
        reply
            .body()
            .deserialize()
            .map_err(|error| error.to_string())
    }

    pub(super) async fn close_notification(id: u32) {
        let Ok(connection) = zbus::Connection::session().await else {
            return;
        };
        close_on(&connection, id).await;
    }

    async fn close_on(connection: &zbus::Connection, id: u32) {
        if let Err(error) = connection
            .call_method(
                Some(NOTIFICATIONS_BUS),
                NOTIFICATIONS_PATH,
                Some(NOTIFICATIONS_INTERFACE),
                "CloseNotification",
                &(id,),
            )
            .await
        {
            tracing::debug!(id, %error, "CloseNotification failed");
        }
    }

    async fn action_invoked_stream(connection: &zbus::Connection) -> Result<MessageStream, String> {
        let rule = MatchRule::builder()
            .msg_type(MessageType::Signal)
            .interface(NOTIFICATIONS_INTERFACE)
            .expect("static Notifications interface")
            .member("ActionInvoked")
            .expect("static ActionInvoked member")
            .build();
        MessageStream::for_match_rule(rule, connection, Some(16))
            .await
            .map_err(|error| error.to_string())
    }

    /// Wait for a body-click on this banner.
    ///
    /// Spec 1.2 servers (GNOME, KDE, COSMIC, …) may emit `ActivationToken`
    /// immediately before `ActionInvoked`. We subscribe to `ActionInvoked`
    /// only, on one stream, on the same connection that sent `Notify`, so
    /// that extra signal cannot steal the click. The `xdg-activation`
    /// token is how Wayland compositors authorize a raise; iced 0.14 has
    /// no API to consume it on an existing window, so `window::gain_focus`
    /// is a no-op there — [#351](https://github.com/charliek/roost/issues/351).
    ///
    /// We only register the spec `default` action. Any `ActionInvoked` for
    /// this id is a click — some servers send the label instead of the
    /// key. Dismiss/timeout leave this future pending until the worker
    /// aborts it.
    async fn drain_action_invoked(id: u32, stream: &mut MessageStream) -> bool {
        while let Some(msg) = stream.next().await {
            let Ok(msg) = msg else {
                continue;
            };
            let Ok((nid, action)) = msg.body().deserialize::<(u32, String)>() else {
                continue;
            };
            if !action_invoked_is_ours(id, nid) {
                continue;
            }
            tracing::info!(id, %action, "desktop notification ActionInvoked");
            return true;
        }
        false
    }

    /// Spec `actions` is `as` (array of STRING): even keys, odd labels.
    /// A `[_; 2]` serializes as D-Bus `(ss)` and Notify rejects the call.
    fn notify_actions() -> Vec<&'static str> {
        vec![DEFAULT_ACTION, DEFAULT_ACTION_LABEL]
    }

    fn action_invoked_is_ours(our_id: u32, nid: u32) -> bool {
        nid == our_id
    }

    #[cfg(test)]
    mod linux_tests {
        use zbus::zvariant::DynamicType;

        use super::{
            action_invoked_is_ours, notify_actions, DEFAULT_ACTION, DEFAULT_ACTION_LABEL,
            EXPIRE_NEVER,
        };

        #[test]
        fn never_expire_is_the_spec_zero() {
            assert_eq!(EXPIRE_NEVER, 0);
        }

        #[test]
        fn default_action_pair_is_key_then_label() {
            assert_eq!(DEFAULT_ACTION, "default");
            assert_eq!(DEFAULT_ACTION_LABEL, "Open");
        }

        #[test]
        fn notify_actions_serialize_as_dbus_string_array_not_struct() {
            let actions = notify_actions();
            assert_eq!(actions.signature().to_string(), "as");
            let as_tuple = [DEFAULT_ACTION, DEFAULT_ACTION_LABEL];
            assert_eq!(
                as_tuple.signature().to_string(),
                "(ss)",
                "a 2-array would be the signature Notify rejected in development"
            );
        }

        #[test]
        fn any_action_invoked_for_our_id_is_a_click() {
            assert!(action_invoked_is_ours(7, 7));
            assert!(!action_invoked_is_ours(7, 8));
        }
    }
}

#[cfg(target_os = "macos")]
mod backend {
    use super::{Payload, Shown};

    /// `app_id` has no counterpart here: UN identifies the sender by the
    /// bundle it is running out of, which is also what gates the backend
    /// on (`crate::macos::notifications::init`).
    pub(super) async fn show(payload: Payload, _app_id: String) -> Result<Shown, String> {
        crate::macos::notifications::show(payload).await
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod backend {
    use super::{Payload, Shown};

    /// Every remaining host: no backend, and the seam says so rather than
    /// pretending a banner reached anyone.
    pub(super) async fn show(payload: Payload, _app_id: String) -> Result<Shown, String> {
        let Payload {
            tab_id: _,
            title,
            body,
            replaces,
        } = payload;
        tracing::debug!(
            %title,
            ?body,
            ?replaces,
            "no desktop notification backend on this platform"
        );
        Ok(Shown::nothing())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::engine_feed;

    /// A backend that reports each payload it was handed and answers from
    /// a script, so the worker's own bookkeeping is what the assertions
    /// see. Reporting through a channel keeps the tests wait-free: the
    /// worker drains sequentially, so payload N+1 carries the bookkeeping
    /// that show N's recorded result produced.
    type ShowResult = Result<Shown, String>;

    fn recording_backend(
        results: Vec<ShowResult>,
    ) -> (
        impl Fn(Payload) -> std::future::Ready<ShowResult> + Send + 'static,
        mpsc::UnboundedReceiver<Payload>,
    ) {
        let (tx, rx) = mpsc::unbounded_channel();
        let results = Arc::new(Mutex::new(VecDeque::from(results)));
        let show = move |payload: Payload| {
            let _ = tx.send(payload);
            let result = results
                .lock()
                .expect("recording backend script lock poisoned")
                .pop_front()
                .unwrap_or_else(|| Ok(Shown::nothing()));
            std::future::ready(result)
        };
        (show, rx)
    }

    fn banner(server_id: u32) -> ShowResult {
        Ok(Shown {
            server_id: Some(server_id),
            activation: None,
        })
    }

    /// Reports its own death. A listener the worker aborts is dropped, and
    /// this is how a test observes that without reaching into the runtime.
    struct DropSignal(mpsc::UnboundedSender<()>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            let _ = self.0.send(());
        }
    }

    /// A banner nobody ever clicks: the future only ends when the worker
    /// aborts the task holding it, which the returned receiver reports. The
    /// signal is captured at construction, not on first poll — an abort can
    /// land before the task ever runs, and that still ends the listener.
    fn watched_listener() -> (Activation, mpsc::UnboundedReceiver<()>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let signal = DropSignal(tx);
        let activation: Activation = Box::pin(async move {
            let _signal = signal;
            std::future::pending::<()>().await;
            false
        });
        (activation, rx)
    }

    fn clickable_banner(server_id: u32) -> (ShowResult, mpsc::UnboundedReceiver<()>) {
        let (activation, dropped) = watched_listener();
        (
            Ok(Shown {
                server_id: Some(server_id),
                activation: Some(activation),
            }),
            dropped,
        )
    }

    #[test]
    fn an_empty_body_is_omitted_and_the_title_travels_verbatim() {
        assert_eq!(
            build_payload(7, "Claude".into(), String::new(), None),
            Payload {
                tab_id: 7,
                title: "Claude".into(),
                body: None,
                replaces: None,
            }
        );
        assert_eq!(
            build_payload(7, String::new(), "  ".into(), Some(4)),
            Payload {
                tab_id: 7,
                title: String::new(),
                body: Some("  ".into()),
                replaces: Some(4),
            },
            "an empty title still fires; only an empty body is dropped"
        );
    }

    #[test]
    fn a_show_that_returned_no_id_keeps_the_previous_one() {
        let mut slots = HashMap::new();
        record_shown(&mut slots, 7, Some(11), None);
        record_shown(&mut slots, 7, None, None);
        assert_eq!(
            slots.get(&7).and_then(|slot| slot.server_id),
            Some(11),
            "a failed show must not orphan the notification still on screen"
        );
        record_shown(&mut slots, 8, None, None);
        assert!(
            !slots.contains_key(&8),
            "a tab that never showed anything gets no slot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tabs_next_notification_replaces_the_one_it_last_showed() {
        let (show, mut shown) = recording_backend(vec![
            banner(11),
            Err("no notification server".into()),
            banner(12),
        ]);
        let (feed, _feed_rx) = engine_feed::channel();
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show, feed);

        notifications.fire(7, "Claude".into(), "needs input".into());
        let first = shown.recv().await.expect("the worker showed the first");
        assert_eq!(first.replaces, None, "nothing to replace yet");
        assert_eq!(first.body.as_deref(), Some("needs input"));

        notifications.fire(7, "Claude".into(), "done".into());
        let second = shown.recv().await.expect("the worker showed the second");
        assert_eq!(second.replaces, Some(11));

        // The second show failed, so the third still targets id 11.
        notifications.fire(7, "Claude".into(), String::new());
        let third = shown.recv().await.expect("the worker showed the third");
        assert_eq!(third.replaces, Some(11));
        assert_eq!(third.body, None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_retired_tab_starts_over_and_tabs_do_not_share_ids() {
        let (show, mut shown) = recording_backend(vec![banner(11), banner(21), banner(31)]);
        let (feed, _feed_rx) = engine_feed::channel();
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show, feed);

        notifications.fire(7, "seven".into(), "one".into());
        assert_eq!(shown.recv().await.expect("first show").replaces, None);

        notifications.fire(8, "eight".into(), "one".into());
        assert_eq!(
            shown.recv().await.expect("second show").replaces,
            None,
            "a different tab has its own slot"
        );

        notifications.retire(7);
        notifications.fire(7, "seven".into(), "two".into());
        assert_eq!(
            shown.recv().await.expect("third show").replaces,
            None,
            "a retired tab's id is dropped, so its next notification is new"
        );
    }

    /// The reason the slot carries an abort handle at all: a replace emits
    /// no close signal, so the listener for the banner that was just
    /// replaced would wait forever. One live listener per tab, always.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn refiring_a_tab_aborts_the_listener_it_replaces() {
        let (first, mut first_ended) = clickable_banner(11);
        let (second, mut second_ended) = clickable_banner(12);
        let (third, mut third_ended) = clickable_banner(13);
        let (show, mut shown) = recording_backend(vec![first, second, third]);
        let (feed, _feed_rx) = engine_feed::channel();
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show, feed);

        notifications.fire(7, "Claude".into(), "one".into());
        shown.recv().await.expect("first show");
        notifications.fire(7, "Claude".into(), "two".into());
        shown.recv().await.expect("second show");
        first_ended.recv().await.expect("the first listener ended");
        assert!(
            second_ended.try_recv().is_err(),
            "the listener for the banner now on screen stays live"
        );

        notifications.fire(7, "Claude".into(), "three".into());
        shown.recv().await.expect("third show");
        second_ended
            .recv()
            .await
            .expect("the second listener ended");
        assert!(third_ended.try_recv().is_err(), "still exactly one live");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn retiring_a_tab_aborts_its_listener() {
        let (first, mut first_ended) = clickable_banner(11);
        let (show, mut shown) = recording_backend(vec![first]);
        let (feed, _feed_rx) = engine_feed::channel();
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show, feed);

        notifications.fire(7, "Claude".into(), "one".into());
        shown.recv().await.expect("first show");
        notifications.retire(7);
        first_ended
            .recv()
            .await
            .expect("a closed tab leaves no listener behind");
    }

    /// The click's whole path through this adapter: the listener resolves,
    /// and the tab it was fired for lands on the engine feed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_clicked_banner_reaches_the_feed_as_an_activation() {
        let (show, mut shown) = recording_backend(vec![Ok(Shown {
            server_id: Some(11),
            activation: Some(Box::pin(std::future::ready(true))),
        })]);
        let (feed, mut feed_rx) = engine_feed::channel();
        let wake = feed_rx.wake_handle();
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show, feed);

        notifications.fire(7, "Claude".into(), "needs input".into());
        shown.recv().await.expect("first show");
        wake.notified().await;
        let mut batch = engine_feed::EngineBatch::default();
        assert!(matches!(
            feed_rx.try_next(&mut batch),
            Some(EngineFeed::NotificationActivated { tab_id: 7 })
        ));
    }

    /// The listener's own verdict, awaited rather than raced: a dismiss (or
    /// a server closing the banner) resolves the same future with `false`
    /// and must put nothing on the feed.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn only_a_click_on_the_banner_body_is_an_activation() {
        let (feed, mut feed_rx) = engine_feed::channel();
        let mut batch = engine_feed::EngineBatch::default();

        spawn_listener(&feed, 7, Box::pin(std::future::ready(false)))
            .await
            .expect("the listener ended");
        assert!(
            feed_rx.try_next(&mut batch).is_none(),
            "a dismissed banner is not a jump"
        );

        spawn_listener(&feed, 7, Box::pin(std::future::ready(true)))
            .await
            .expect("the listener ended");
        assert!(matches!(
            feed_rx.try_next(&mut batch),
            Some(EngineFeed::NotificationActivated { tab_id: 7 })
        ));
    }
}
