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
type Activation = Pin<Box<dyn Future<Output = bool> + Send>>;

/// What one show produced. The `activation` is `None` on backends that
/// cannot report a click at all, which is why the worker — not the
/// backend — owns the listener bookkeeping.
struct Shown {
    server_id: Option<u32>,
    activation: Option<Activation>,
}

impl Shown {
    /// Nothing reached the desktop: no id to replace, no banner to click.
    fn nothing() -> Self {
        Self {
            server_id: None,
            activation: None,
        }
    }
}

/// A show request in toolkit-neutral form, so the mapping is testable off
/// Linux and a second backend has one shape to implement.
#[derive(Debug, PartialEq, Eq)]
struct Payload {
    title: String,
    /// `None` when the notification carried no body — an empty body line
    /// is worse than none, and GTK omits it the same way.
    body: Option<String>,
    replaces: Option<u32>,
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
        let shown =
            match tokio::time::timeout(SHOW_TIMEOUT, show(build_payload(title, body, previous)))
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
            feed.send(EngineFeed::NotificationActivated { tab_id });
        }
    })
}

fn build_payload(title: String, body: String, replaces: Option<u32>) -> Payload {
    Payload {
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
    use notify_rust::{Hint, Notification, NotificationHandle, NotificationResponse};

    use super::{Payload, Shown};

    /// The freedesktop key a server invokes for a click on the banner body
    /// rather than on a button. Declaring it is what makes the banner
    /// clickable at all — servers only invoke actions a notification lists.
    const DEFAULT_ACTION: &str = "default";

    /// `app_id` is the desktop-entry hint — best-effort shell grouping: a
    /// desktop file may not be installed for a dev build, and the hint is
    /// simply ignored then.
    pub(super) async fn show(payload: Payload, app_id: String) -> Result<Shown, String> {
        let Payload {
            title,
            body,
            replaces,
        } = payload;
        let mut notification = Notification::new();
        notification
            .appname("Roost")
            .summary(&title)
            .action(DEFAULT_ACTION, "Open")
            .hint(Hint::DesktopEntry(app_id));
        if let Some(body) = &body {
            notification.body(body);
        }
        if let Some(replaces) = replaces {
            notification.id(replaces);
        }
        let handle = notification
            .show_async()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Shown {
            server_id: Some(handle.id()),
            activation: Some(Box::pin(wait_for_click(handle))),
        })
    }

    /// `wait_for_action_async` borrows its handle, so the handle moves into
    /// this future and the borrow never escapes it — the whole thing is one
    /// owned, abortable unit. Dropping it (the worker aborting) drops the
    /// handle and its D-Bus signal stream with it.
    async fn wait_for_click(handle: NotificationHandle) -> bool {
        let mut clicked = false;
        handle
            .wait_for_action_async(|response| {
                clicked = matches!(response, NotificationResponse::Default);
            })
            .await;
        clicked
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    use super::{Payload, Shown};

    /// macOS is deferred, not designed out: a UNUserNotificationCenter
    /// backend drops in at this signature, activation included.
    pub(super) async fn show(payload: Payload, _app_id: String) -> Result<Shown, String> {
        let Payload {
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
            build_payload("Claude".into(), String::new(), None),
            Payload {
                title: "Claude".into(),
                body: None,
                replaces: None,
            }
        );
        assert_eq!(
            build_payload(String::new(), "  ".into(), Some(4)),
            Payload {
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
