//! The local session's tabs follow the window's grid, shown or not (plan
//! 072 D5, #563).
//!
//! A tab this window is attached to follows the grid through its attach,
//! as a RESIZE frame. Every other tab the local session lists gets the
//! session's own `tab.resize` op, which needs no attach, in one wave per
//! edge: the settled end of a window or sidebar resize, a font change,
//! the first listing after a connect, the listing of a tab this window
//! opened or forwarded, and the end of an attach.
//!
//! Only the local session. Every control op to a host shares one strictly
//! sequential queue and an attach waits its turn in it, so a wave to a
//! host across a network would hold the next attach for one round trip
//! per tab (#568). On the local session a wave costs well under a
//! millisecond per tab.
//!
//! `last_sent` is what keeps a wave from repeating itself: a stale
//! deadline, or a trigger that moved no grid, finds every tab already
//! given the grid and sends nothing.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::time::{Duration, Instant};

use roost_ipc::client::ServerCode;
use roost_ipc::messages::{ops, TabResizeParams};
use roost_ui_model::keys::{HostId, TabKey};
use tokio::sync::oneshot;

use super::host_wait_expired;
use crate::host_conn::{HostIntent, HostOpError, HostOps};

/// How long the grid must hold still after a resize before the wave goes,
/// so a drag sends one wave rather than one per frame.
const SETTLE: Duration = Duration::from_millis(150);
const FIRST_RETRY: Duration = Duration::from_millis(250);
const MAX_RETRY: Duration = Duration::from_secs(5);

/// The listed tabs one wave resizes, and the grid each gets: every tab with
/// no attach in this window whose last grid from this window is not `grid`.
pub(super) fn tabs_to_resize(
    listed: &[TabKey],
    attached: &HashSet<TabKey>,
    last_sent: &HashMap<TabKey, (u16, u16)>,
    grid: (u16, u16),
) -> Vec<(TabKey, (u16, u16))> {
    listed
        .iter()
        .copied()
        .filter(|tab| !attached.contains(tab) && last_sent.get(tab) != Some(&grid))
        .map(|tab| (tab, grid))
        .collect()
}

/// One tab's `tab.resize`, fenced at the connection its key names. Finding
/// the queue by incarnation checks ownership only as the wave goes; see
/// [`HostIntent::fence`] for why that is not enough.
fn resize_intent(tab: TabKey, (cols, rows): (u16, u16)) -> HostIntent {
    let params = serde_json::to_value(TabResizeParams {
        tab_id: tab.tab,
        cols: cols.into(),
        rows: rows.into(),
    })
    .expect("tab.resize params serialize");
    HostIntent::new(ops::TAB_RESIZE, params).fenced_at(tab.host)
}

/// What one tab's resize came back as.
#[derive(Debug, Clone)]
pub(crate) struct Outcome {
    pub(crate) tab: TabKey,
    pub(crate) grid: (u16, u16),
    pub(crate) result: Result<(), HostOpError>,
}

/// Enqueue one wave, and the future that collects its answers. Once the
/// queue refuses one tab the rest are refused the same way unsent: the
/// queue is not going to take the next one either.
pub(crate) fn send_wave(
    queue: &HostOps,
    targets: &[(TabKey, (u16, u16))],
) -> impl Future<Output = Vec<Outcome>> + Send + 'static {
    let mut refused: Option<HostOpError> = None;
    let mut replies = Vec::with_capacity(targets.len());
    for &(tab, grid) in targets {
        let (tx, rx) = oneshot::channel();
        let intent = resize_intent(tab, grid).answering(tx);
        match &refused {
            Some(error) => intent.answer(Err(error.clone())),
            None => refused = queue.send_quiet(intent).err(),
        }
        replies.push((tab, grid, rx));
    }
    async move {
        let mut outcomes = Vec::with_capacity(replies.len());
        for (tab, grid, rx) in replies {
            let result = rx.await.unwrap_or(Err(HostOpError::Disconnected)).map(drop);
            outcomes.push(Outcome { tab, grid, result });
        }
        outcomes
    }
}

/// Whether a refused resize is worth another try as it stands: a full
/// queue drains, and a switch in flight ends.
fn retryable(error: &HostOpError) -> bool {
    matches!(
        error,
        HostOpError::QueueFull
            | HostOpError::Rejected {
                code: ServerCode::Busy,
                ..
            }
    )
}

fn backoff(retries: u32) -> Duration {
    FIRST_RETRY
        .saturating_mul(2u32.saturating_pow(retries))
        .min(MAX_RETRY)
}

/// What the `update` drain does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Poll {
    Idle,
    /// Arm a one-shot for this long.
    Arm(Duration),
    Wave,
}

#[derive(Debug, Default)]
pub(super) struct BackgroundResize {
    /// The local session's connection everything below describes.
    connection: Option<HostId>,
    /// The grid each tab was last given, as far as this window knows. The
    /// session tab's grid moves with an attach, so an attach owns the
    /// entry while it lasts, and leaves none behind ([`Self::detached`]).
    last_sent: HashMap<TabKey, (u16, u16)>,
    /// Tabs whose resize the session could not take yet.
    pending: BTreeSet<TabKey>,
    /// Tabs this window opened or forwarded, and when, until the listing
    /// names them or a forwarded open's focus would give up on its row
    /// ([`host_wait_expired`]): a close that beat the open's reply here
    /// found nothing to remove, and that tab is never listed.
    opened: Vec<(TabKey, Instant)>,
    /// When the next wave is owed.
    due: Option<Instant>,
    /// When the earliest one-shot on its way fires.
    armed: Option<Instant>,
    retries: u32,
}

impl BackgroundResize {
    /// A window or sidebar resize, which a drag repeats every frame.
    pub(super) fn resized(&mut self, now: Instant) {
        self.trigger(now + SETTLE);
    }

    /// The wave is owed at `at`: with this update for a trigger whose grid
    /// is final.
    pub(super) fn trigger(&mut self, at: Instant) {
        self.retries = 0;
        self.due = Some(at);
    }

    /// The local session's connection as the latest reconcile sees it. A
    /// new one describes nothing the old one did, and its first listing
    /// is a trigger.
    pub(super) fn connected(&mut self, connection: Option<HostId>, now: Instant) {
        if self.connection == connection {
            return;
        }
        self.connection = connection;
        self.last_sent.clear();
        self.pending.clear();
        self.opened.clear();
        if connection.is_some() {
            self.trigger(now);
        }
    }

    pub(super) fn opened(&mut self, tab: TabKey, now: Instant) {
        if self.connection == Some(tab.host) {
            self.opened.push((tab, now));
        }
    }

    pub(super) fn awaits_listing(&self) -> bool {
        !self.opened.is_empty()
    }

    /// The local session's listing: an opened tab it names is a trigger.
    pub(super) fn listed(&mut self, listed: &[TabKey], now: Instant) {
        let mut named = false;
        self.opened.retain(|&(tab, at)| {
            let is_listed = listed.contains(&tab);
            named |= is_listed;
            !is_listed && !host_wait_expired(at, now)
        });
        if named {
            self.trigger(now);
        }
    }

    pub(super) fn attached(&mut self, tab: TabKey) {
        self.last_sent.remove(&tab);
        self.pending.remove(&tab);
    }

    /// An attach's last grid can't count as given: a RESIZE queued behind
    /// input may be aborted with its writer, and one withheld during
    /// hydration dropped with it. So the tab is owed the grid again, and
    /// the wave that sends it costs nothing when the session has it
    /// already: a same-size resize neither signals the child nor moves
    /// D4(b)'s marker.
    pub(super) fn detached(&mut self, tab: TabKey, now: Instant) {
        if self.connection != Some(tab.host) {
            return;
        }
        self.last_sent.remove(&tab);
        self.resized(now);
    }

    pub(super) fn closed(&mut self, tab: TabKey) {
        self.last_sent.remove(&tab);
        self.pending.remove(&tab);
        self.opened.retain(|(opened, _)| *opened != tab);
    }

    /// Whether a wave is due now, or a one-shot has to be armed for one.
    pub(super) fn poll(&mut self, now: Instant) -> Poll {
        let Some(due) = self.due else {
            return Poll::Idle;
        };
        if due <= now {
            self.due = None;
            return Poll::Wave;
        }
        if self.armed.is_some_and(|armed| armed <= due) {
            return Poll::Idle;
        }
        self.armed = Some(due);
        Poll::Arm(due - now)
    }

    /// A one-shot fired. It may be a stale one, which the next poll re-arms
    /// behind.
    pub(super) fn fired(&mut self) {
        self.armed = None;
    }

    /// The tabs a wave resizes, recorded as given.
    pub(super) fn wave(
        &mut self,
        connection: HostId,
        listed: &[TabKey],
        attached: &HashSet<TabKey>,
        grid: (u16, u16),
    ) -> Vec<(TabKey, (u16, u16))> {
        if self.connection != Some(connection) {
            return Vec::new();
        }
        let known: HashSet<TabKey> = listed.iter().copied().collect();
        self.last_sent.retain(|tab, _| known.contains(tab));
        self.pending.clear();
        let targets = tabs_to_resize(listed, attached, &self.last_sent, grid);
        self.last_sent.extend(targets.iter().copied());
        targets
    }

    /// Fold a wave's answers in, and owe a retry while a tab waits on one.
    pub(super) fn settle(&mut self, connection: HostId, outcomes: Vec<Outcome>, now: Instant) {
        if self.connection != Some(connection) {
            return;
        }
        for Outcome { tab, grid, result } in outcomes {
            let Err(error) = result else {
                continue;
            };
            // A later wave, or an attach, has had its say about this tab.
            if self.last_sent.get(&tab) != Some(&grid) {
                continue;
            }
            self.last_sent.remove(&tab);
            if retryable(&error) {
                tracing::debug!(%tab, %error, "a background resize waits for a retry");
                self.pending.insert(tab);
            } else {
                tracing::debug!(%tab, %error, "a background resize was refused");
                self.pending.remove(&tab);
            }
        }
        if self.pending.is_empty() {
            self.retries = 0;
            return;
        }
        let at = now + backoff(self.retries);
        self.retries = self.retries.saturating_add(1);
        self.due = Some(self.due.map_or(at, |due| due.min(at)));
    }

    #[cfg(test)]
    fn last_sent(&self, tab: TabKey) -> Option<(u16, u16)> {
        self.last_sent.get(&tab).copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SLOT: HostId = HostId::new(3);
    const GRID: (u16, u16) = (88, 34);
    const WIDER: (u16, u16) = (120, 34);

    fn tab(id: i64) -> TabKey {
        TabKey::new(SLOT, id)
    }

    fn none() -> HashSet<TabKey> {
        HashSet::new()
    }

    fn connected(now: Instant) -> BackgroundResize {
        let mut resize = BackgroundResize::default();
        resize.connected(Some(SLOT), now);
        resize
    }

    fn outcome(tab: TabKey, grid: (u16, u16), result: Result<(), HostOpError>) -> Outcome {
        Outcome { tab, grid, result }
    }

    fn rejected(code: ServerCode) -> HostOpError {
        HostOpError::Rejected {
            code,
            message: String::new(),
        }
    }

    #[test]
    fn a_wave_resizes_every_listed_tab_with_no_attach_that_lacks_the_grid() {
        let listed = [tab(1), tab(2), tab(3), tab(4)];
        let attached = HashSet::from([tab(2)]);
        let last_sent = HashMap::from([(tab(3), GRID), (tab(4), WIDER)]);
        assert_eq!(
            tabs_to_resize(&listed, &attached, &last_sent, GRID),
            vec![(tab(1), GRID), (tab(4), GRID)]
        );
        assert!(tabs_to_resize(&[], &attached, &last_sent, GRID).is_empty());
    }

    #[test]
    fn a_wave_sends_a_grid_once() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID),
            vec![(tab(1), GRID)]
        );
        assert!(resize.wave(SLOT, &[tab(1)], &none(), GRID).is_empty());
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), WIDER),
            vec![(tab(1), WIDER)]
        );
    }

    #[test]
    fn what_was_sent_is_forgotten_with_the_connection() {
        let now = Instant::now();
        let mut resize = connected(now);
        resize.wave(SLOT, &[tab(1)], &none(), GRID);

        resize.connected(None, now);
        resize.connected(Some(SLOT), now);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID),
            vec![(tab(1), GRID)],
            "a session reached again may hold the tab at any grid"
        );
    }

    #[test]
    fn a_connect_is_a_trigger_and_a_wave_for_another_connection_sends_nothing() {
        let now = Instant::now();
        let mut resize = BackgroundResize::default();
        assert_eq!(resize.poll(now), Poll::Idle);
        resize.connected(Some(SLOT), now);
        assert_eq!(
            resize.poll(now),
            Poll::Wave,
            "the first listing after a connect"
        );
        resize.connected(Some(SLOT), now);
        assert_eq!(resize.poll(now), Poll::Idle, "the same connection again");
        assert!(resize
            .wave(HostId::new(4), &[tab(1)], &none(), GRID)
            .is_empty());
    }

    /// The tab attached at GRID, then the grid went to WIDER: onto the live
    /// attach's queue behind input, where the detach aborted it unwritten,
    /// or withheld while it hydrated, where the detach dropped it. The
    /// wave at WIDER skipped the attached tab, so the one the detach arms
    /// has to send it.
    #[test]
    fn a_tab_detached_before_its_attach_gave_it_the_grid_gets_it_from_the_next_wave() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.wave(SLOT, &[tab(1)], &none(), GRID);
        resize.attached(tab(1));
        assert_eq!(
            resize.last_sent(tab(1)),
            None,
            "the attach resizes the tab now"
        );
        resize.trigger(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let attached = HashSet::from([tab(1)]);
        assert!(resize.wave(SLOT, &[tab(1)], &attached, WIDER).is_empty());

        resize.detached(tab(1), now);
        assert_eq!(
            resize.poll(now),
            Poll::Arm(SETTLE),
            "a detach arms the wave"
        );
        resize.fired();
        assert_eq!(resize.poll(now + SETTLE), Poll::Wave);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), WIDER),
            vec![(tab(1), WIDER)]
        );
        assert!(resize.wave(SLOT, &[tab(1)], &none(), WIDER).is_empty());
    }

    #[test]
    fn a_detach_on_another_connection_arms_nothing() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.detached(TabKey::new(HostId::new(4), 1), now);
        assert_eq!(resize.poll(now), Poll::Idle);
    }

    #[test]
    fn a_failed_resize_is_sent_again_by_the_next_wave() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID);
        resize.settle(
            SLOT,
            vec![
                outcome(tab(1), GRID, Err(rejected(ServerCode::NotFound))),
                outcome(tab(2), GRID, Ok(())),
            ],
            now,
        );
        assert_eq!(resize.poll(now), Poll::Idle, "a refusal is not retried");
        assert_eq!(
            resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID),
            vec![(tab(1), GRID)]
        );
    }

    #[test]
    fn an_answer_a_later_wave_overtook_changes_nothing() {
        let now = Instant::now();
        let mut resize = connected(now);
        resize.wave(SLOT, &[tab(1)], &none(), GRID);
        resize.wave(SLOT, &[tab(1)], &none(), WIDER);
        resize.settle(
            SLOT,
            vec![outcome(tab(1), GRID, Err(HostOpError::QueueFull))],
            now,
        );
        assert_eq!(resize.last_sent(tab(1)), Some(WIDER));
    }

    /// A drag re-arms the settle every frame; one wave goes, 150 ms after
    /// the last resize, and a single one-shot is ever in flight for it.
    #[test]
    fn a_drags_resizes_coalesce_into_one_wave_after_the_last() {
        let t0 = Instant::now();
        let mut resize = BackgroundResize::default();
        resize.resized(t0);
        assert_eq!(resize.poll(t0), Poll::Arm(SETTLE));
        for frame in 1..=6 {
            let now = t0 + Duration::from_millis(16 * frame);
            resize.resized(now);
            assert_eq!(resize.poll(now), Poll::Idle, "frame {frame}");
        }
        let last = t0 + Duration::from_millis(96);

        resize.fired();
        let first_shot = t0 + SETTLE;
        assert_eq!(
            resize.poll(first_shot),
            Poll::Arm(last + SETTLE - first_shot)
        );
        resize.fired();
        assert_eq!(resize.poll(last + SETTLE), Poll::Wave);
        assert_eq!(resize.poll(last + SETTLE), Poll::Idle);
    }

    /// Each refusal of a full queue at `now`, and the delay the retry is
    /// armed for.
    fn refused(resize: &mut BackgroundResize, now: Instant) -> Duration {
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID);
        let outcomes = targets
            .into_iter()
            .map(|(tab, grid)| outcome(tab, grid, Err(HostOpError::QueueFull)))
            .collect();
        resize.settle(SLOT, outcomes, now);
        let Poll::Arm(delay) = resize.poll(now) else {
            panic!("a refused tab owes a retry");
        };
        delay
    }

    #[test]
    fn a_new_trigger_goes_ahead_of_a_retry_and_starts_the_backoff_over() {
        let mut now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        for expected in [250, 500] {
            assert_eq!(refused(&mut resize, now), Duration::from_millis(expected));
            now += Duration::from_millis(expected);
            resize.fired();
            assert_eq!(resize.poll(now), Poll::Wave);
        }
        assert_eq!(refused(&mut resize, now), Duration::from_millis(1000));

        resize.resized(now);
        assert_eq!(
            resize.poll(now),
            Poll::Arm(SETTLE),
            "ahead of the armed retry"
        );
        now += SETTLE;
        resize.fired();
        assert_eq!(resize.poll(now), Poll::Wave);
        assert_eq!(refused(&mut resize, now), Duration::from_millis(250));
    }

    #[test]
    fn a_retry_backs_off_from_250_ms_to_5_s() {
        let delays: Vec<u64> = (0..8).map(|n| backoff(n).as_millis() as u64).collect();
        assert_eq!(delays, [250, 500, 1000, 2000, 4000, 5000, 5000, 5000]);
        assert_eq!(backoff(u32::MAX), MAX_RETRY);
    }

    #[test]
    fn a_busy_session_is_retried_and_any_other_refusal_is_not() {
        assert!(retryable(&HostOpError::QueueFull));
        assert!(retryable(&rejected(ServerCode::Busy)));
        assert!(!retryable(&rejected(ServerCode::NotFound)));
        assert!(!retryable(&rejected(ServerCode::InvalidParam)));
        assert!(!retryable(&HostOpError::Disconnected));
        assert!(!retryable(&HostOpError::WorkerGone));
    }

    #[test]
    fn an_opened_tab_is_a_trigger_once_the_listing_names_it() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.opened(tab(7), now);
        resize.opened(TabKey::new(HostId::LOCAL, 7), now);
        assert!(resize.awaits_listing());

        resize.listed(&[tab(1)], now);
        assert_eq!(resize.poll(now), Poll::Idle, "not listed yet");
        resize.listed(&[tab(1), tab(7)], now);
        assert_eq!(resize.poll(now), Poll::Wave);
        assert!(!resize.awaits_listing());

        resize.opened(tab(8), now);
        resize.closed(tab(8));
        assert!(
            !resize.awaits_listing(),
            "a tab closed before it was listed"
        );
    }

    /// Its close reached the window before the open's reply did.
    #[test]
    fn a_tab_closed_before_its_open_was_answered_is_awaited_only_until_the_deadline() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.closed(tab(9));
        resize.opened(tab(9), now);

        let deadline = now + super::super::PENDING_HOST_SELECTION_DEADLINE;
        resize.listed(&[tab(1)], deadline - Duration::from_millis(1));
        assert!(resize.awaits_listing(), "a listing may still be on its way");
        resize.listed(&[tab(1)], deadline);
        assert!(!resize.awaits_listing());
        assert_eq!(resize.poll(deadline), Poll::Idle, "and it is no trigger");
    }

    /// The panel's convergence case: a wave the queue cannot take keeps its
    /// tabs pending, and the one-shot keeps coming back, backing off, until
    /// the queue drains and the resize lands.
    #[tokio::test]
    async fn a_resize_refused_by_a_full_queue_is_retried_until_it_lands() {
        let (queue, mut worker) = HostOps::channel();
        let mut filler = 0;
        while queue
            .send_quiet(HostIntent::new("filler", serde_json::Value::Null))
            .is_ok()
        {
            filler += 1;
        }
        assert!(filler > 0);

        let t0 = Instant::now();
        let mut resize = connected(t0);
        assert_eq!(resize.poll(t0), Poll::Wave);
        let mut now = t0;
        for expected in [250, 500, 1000] {
            let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID);
            assert_eq!(targets.len(), 2, "every refused tab goes again");
            let outcomes = send_wave(&queue, &targets).await;
            assert!(outcomes
                .iter()
                .all(|outcome| outcome.result == Err(HostOpError::QueueFull)));
            resize.settle(SLOT, outcomes, now);
            assert_eq!(
                resize.poll(now),
                Poll::Arm(Duration::from_millis(expected)),
                "the retry backs off"
            );
            now += Duration::from_millis(expected);
            resize.fired();
            assert_eq!(resize.poll(now), Poll::Wave);
        }

        while worker.try_recv().is_ok() {}
        let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID);
        let answered = send_wave(&queue, &targets);
        for _ in &targets {
            let intent = worker.recv().await.expect("the retry reaches the queue");
            assert_eq!(intent.op, ops::TAB_RESIZE);
            assert_eq!(intent.fence, Some(SLOT));
            intent.answer(Ok(serde_json::json!({})));
        }
        resize.settle(SLOT, answered.await, now);
        assert_eq!(resize.poll(now), Poll::Idle, "the pending set drained");
        assert!(resize
            .wave(SLOT, &[tab(1), tab(2)], &none(), GRID)
            .is_empty());
    }
}
