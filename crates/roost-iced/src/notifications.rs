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

use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use tokio::sync::mpsc;

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
    pub(crate) fn new(runtime: &tokio::runtime::Handle) -> Self {
        Self::spawn_on(runtime, backend::show)
    }

    fn spawn_on<F, Fut>(runtime: &tokio::runtime::Handle, show: F) -> Self
    where
        F: Fn(Payload) -> Fut + Send + 'static,
        Fut: Future<Output = Result<Option<u32>, String>> + Send + 'static,
    {
        let (tx, rx) = mpsc::unbounded_channel();
        runtime.spawn(run(rx, show));
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

async fn run<F, Fut>(mut rx: mpsc::UnboundedReceiver<Request>, show: F)
where
    F: Fn(Payload) -> Fut,
    Fut: Future<Output = Result<Option<u32>, String>>,
{
    let mut slots: HashMap<i64, TabSlot> = HashMap::new();
    while let Some(request) = rx.recv().await {
        let (tab_id, title, body) = match request {
            Request::Retire { tab_id } => {
                slots.remove(&tab_id);
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
                Ok(Ok(server_id)) => server_id,
                Ok(Err(error)) => {
                    tracing::warn!(tab_id, %error, "desktop notification failed");
                    None
                }
                Err(_) => {
                    tracing::warn!(tab_id, "desktop notification timed out");
                    None
                }
            };
        record_shown(&mut slots, tab_id, shown);
    }
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
fn record_shown(slots: &mut HashMap<i64, TabSlot>, tab_id: i64, server_id: Option<u32>) {
    if server_id.is_some() {
        slots.entry(tab_id).or_default().server_id = server_id;
    }
}

#[cfg(target_os = "linux")]
mod backend {
    use notify_rust::{Hint, Notification};

    use super::Payload;

    /// Best-effort shell grouping: a desktop file may not be installed for
    /// a dev build, and the hint is simply ignored then.
    const DESKTOP_ENTRY: &str = "ai.stridelabs.Roost.iced";

    pub(super) async fn show(payload: Payload) -> Result<Option<u32>, String> {
        let Payload {
            title,
            body,
            replaces,
        } = payload;
        let mut notification = Notification::new();
        notification
            .appname("Roost")
            .summary(&title)
            .hint(Hint::DesktopEntry(DESKTOP_ENTRY.to_string()));
        if let Some(body) = &body {
            notification.body(body);
        }
        if let Some(replaces) = replaces {
            notification.id(replaces);
        }
        notification
            .show_async()
            .await
            .map(|handle| Some(handle.id()))
            .map_err(|error| error.to_string())
    }
}

#[cfg(not(target_os = "linux"))]
mod backend {
    use super::Payload;

    /// macOS is deferred, not designed out: a UNUserNotificationCenter
    /// backend drops in at this signature.
    pub(super) async fn show(payload: Payload) -> Result<Option<u32>, String> {
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
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use super::*;

    /// A backend that reports each payload it was handed and answers from
    /// a script, so the worker's own bookkeeping is what the assertions
    /// see. Reporting through a channel keeps the tests wait-free: the
    /// worker drains sequentially, so payload N+1 carries the bookkeeping
    /// that show N's recorded result produced.
    type ShowResult = Result<Option<u32>, String>;

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
                .unwrap_or(Ok(None));
            std::future::ready(result)
        };
        (show, rx)
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
        record_shown(&mut slots, 7, Some(11));
        record_shown(&mut slots, 7, None);
        assert_eq!(
            slots.get(&7).and_then(|slot| slot.server_id),
            Some(11),
            "a failed show must not orphan the notification still on screen"
        );
        record_shown(&mut slots, 8, None);
        assert!(
            !slots.contains_key(&8),
            "a tab that never showed anything gets no slot"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tabs_next_notification_replaces_the_one_it_last_showed() {
        let (show, mut shown) = recording_backend(vec![
            Ok(Some(11)),
            Err("no notification server".into()),
            Ok(Some(12)),
        ]);
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show);

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
        let (show, mut shown) = recording_backend(vec![Ok(Some(11)), Ok(Some(21)), Ok(Some(31))]);
        let notifications =
            DesktopNotifications::spawn_on(&tokio::runtime::Handle::current(), show);

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
}
