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
//!
//! A tab another client sizes refuses the op with `owned` (plan 084 D6),
//! and that refusal is final: the wave parks the tab and leaves it alone
//! until a `tab.resized` says nobody sizes it. Each send carries the
//! tab's owner epoch — how many `tab.resized` the mirror had folded for
//! it — and its refusal is judged against that, so one that crossed an
//! `owner: null` on its way back parks nothing. A rebuilt mirror counts
//! its epochs from 0 again, so each send also carries which mirror its
//! epoch was read from, and a refusal from an earlier one parks nothing
//! either.
//!
//! A `tab.resized` with no owner waves only for a tab this window parked.
//! Every window on the session hears every one, so waving at any grid
//! that was not this window's would have two windows resize a tab nobody
//! sizes back and forth for ever.

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

/// One tab a wave resizes: the grid it gets, and the tab's owner epoch
/// when the wave chose it, in the mirror the epoch was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Target {
    pub(crate) tab: TabKey,
    pub(crate) grid: (u16, u16),
    pub(crate) epoch: u64,
    pub(crate) mirror: u64,
}

/// What one tab's resize came back as.
#[derive(Debug, Clone)]
pub(crate) struct Outcome {
    pub(crate) tab: TabKey,
    pub(crate) grid: (u16, u16),
    /// The owner epoch the resize went out at, and its mirror.
    pub(crate) epoch: u64,
    pub(crate) mirror: u64,
    /// Whether the queue took it at all.
    pub(crate) sent: bool,
    pub(crate) result: Result<(), HostOpError>,
}

/// Enqueue one wave, and the future that collects its answers. Once the
/// queue refuses one tab the rest are refused the same way unsent: the
/// queue is not going to take the next one either.
pub(crate) fn send_wave(
    queue: &HostOps,
    targets: &[Target],
) -> impl Future<Output = Vec<Outcome>> + Send + 'static {
    let mut refused: Option<HostOpError> = None;
    let mut replies = Vec::with_capacity(targets.len());
    for &target in targets {
        let (tx, rx) = oneshot::channel();
        let intent = resize_intent(target.tab, target.grid).answering(tx);
        let sent = match &refused {
            Some(error) => {
                intent.answer(Err(error.clone()));
                false
            }
            None => {
                refused = queue.send_quiet(intent).err();
                refused.is_none()
            }
        };
        replies.push((target, sent, rx));
    }
    async move {
        let mut outcomes = Vec::with_capacity(replies.len());
        for (target, sent, rx) in replies {
            let result = rx.await.unwrap_or(Err(HostOpError::Disconnected)).map(drop);
            outcomes.push(Outcome {
                tab: target.tab,
                grid: target.grid,
                epoch: target.epoch,
                mirror: target.mirror,
                sent,
                result,
            });
        }
        outcomes
    }
}

/// Whether a refused resize is worth another try as it stands: a full
/// queue drains, and a switch in flight ends. An `owned` tab is not —
/// see [`BackgroundResize::settle`].
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
    /// Tabs another client sizes, by their `owned` refusal.
    parked: HashSet<TabKey>,
    /// How many resizes the queue took for each tab, for
    /// `host.size_trace`.
    sends: HashMap<TabKey, u32>,
    /// Which mirror owner epochs are read from: bumped by every rebuilt
    /// mirror and every new connection.
    mirror: u64,
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
        self.mirror += 1;
        self.last_sent.clear();
        self.pending.clear();
        self.opened.clear();
        self.parked.clear();
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
        self.parked.remove(&tab);
        self.sends.remove(&tab);
    }

    /// The local session's mirror was rebuilt, and its owner epochs with
    /// it. Whatever happened in the gap — an owner that left, a grid
    /// another client installed — went unheard, and `tab.resized` is
    /// live-only, so it will not be heard later: the wave forgets what it
    /// gave and what it parked, and goes again to re-learn both.
    pub(super) fn mirror_reset(&mut self, connection: HostId, now: Instant) {
        if self.connection == Some(connection) {
            self.mirror += 1;
            self.parked.clear();
            self.last_sent.clear();
            self.trigger(now);
        }
    }

    /// A `tab.resized` on the local session. A grid this window did not
    /// give is not the tab's any more, so the next wave owes it this
    /// window's again. Only a tab this window parked waves at once, once
    /// nobody sizes it — see the module doc for why nothing else does.
    pub(super) fn tab_resized(
        &mut self,
        tab: TabKey,
        grid: (u16, u16),
        unowned: bool,
        now: Instant,
    ) {
        if self.connection != Some(tab.host) {
            return;
        }
        if self.last_sent.get(&tab) != Some(&grid) {
            self.last_sent.remove(&tab);
        }
        if unowned && self.parked.remove(&tab) {
            self.trigger(now);
        }
    }

    pub(super) fn sends(&self, tab: TabKey) -> u32 {
        self.sends.get(&tab).copied().unwrap_or(0)
    }

    pub(super) fn parked(&self, tab: TabKey) -> bool {
        self.parked.contains(&tab)
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

    /// The tabs a wave resizes, recorded as given, each with its owner
    /// `epoch`. A parked tab is left alone.
    pub(super) fn wave(
        &mut self,
        connection: HostId,
        listed: &[TabKey],
        attached: &HashSet<TabKey>,
        grid: (u16, u16),
        epoch: impl Fn(TabKey) -> u64,
    ) -> Vec<Target> {
        if self.connection != Some(connection) {
            return Vec::new();
        }
        let known: HashSet<TabKey> = listed.iter().copied().collect();
        self.last_sent.retain(|tab, _| known.contains(tab));
        self.pending.clear();
        let targets: Vec<Target> = tabs_to_resize(listed, attached, &self.last_sent, grid)
            .into_iter()
            .filter(|(tab, _)| !self.parked.contains(tab))
            .map(|(tab, grid)| Target {
                tab,
                grid,
                epoch: epoch(tab),
                mirror: self.mirror,
            })
            .collect();
        for target in &targets {
            self.last_sent.insert(target.tab, target.grid);
        }
        targets
    }

    /// Fold a wave's answers in, and owe a retry while a tab waits on one.
    ///
    /// `owned` is final: never retried, never recorded as given, and the
    /// tab is parked — unless its owner `epoch` has moved since that very
    /// send, or the mirror it was read from has been rebuilt, when the
    /// refusal describes an owner that may be gone and the tab is owed a
    /// wave instead.
    ///
    /// A retryable refusal is owed a retry even when an event or a later
    /// wave has moved `last_sent` since: the retry is a wave like any
    /// other, which sends nothing to a tab already given its grid.
    pub(super) fn settle(
        &mut self,
        connection: HostId,
        outcomes: Vec<Outcome>,
        now: Instant,
        epoch: impl Fn(TabKey) -> u64,
    ) {
        if self.connection != Some(connection) {
            return;
        }
        for Outcome {
            tab,
            grid,
            epoch: sent_at,
            mirror,
            sent,
            result,
        } in outcomes
        {
            if sent {
                *self.sends.entry(tab).or_default() += 1;
            }
            let error = match result {
                Ok(()) => {
                    self.parked.remove(&tab);
                    continue;
                }
                Err(error) => error,
            };
            if matches!(
                error,
                HostOpError::Rejected {
                    code: ServerCode::Owned,
                    ..
                }
            ) {
                if self.last_sent.get(&tab) == Some(&grid) {
                    self.last_sent.remove(&tab);
                }
                if mirror == self.mirror && sent_at == epoch(tab) {
                    tracing::debug!(%tab, %error, "a background resize parks on a sized tab");
                    self.parked.insert(tab);
                } else {
                    tracing::debug!(%tab, "a background resize was refused by an owner since gone");
                    self.due_by(now);
                }
                continue;
            }
            if self.last_sent.get(&tab) == Some(&grid) {
                self.last_sent.remove(&tab);
            }
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
        self.due_by(at);
    }

    /// Owe a wave no later than `at`.
    fn due_by(&mut self, at: Instant) {
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
        Outcome {
            tab,
            grid,
            epoch: 0,
            mirror: 0,
            sent: true,
            result,
        }
    }

    /// Each of a wave's targets answered `result`, at the epoch it went
    /// out at.
    fn answered(targets: &[Target], result: &Result<(), HostOpError>) -> Vec<Outcome> {
        targets
            .iter()
            .map(|target| Outcome {
                tab: target.tab,
                grid: target.grid,
                epoch: target.epoch,
                mirror: target.mirror,
                sent: true,
                result: result.clone(),
            })
            .collect()
    }

    impl PartialEq<(TabKey, (u16, u16))> for Target {
        fn eq(&self, other: &(TabKey, (u16, u16))) -> bool {
            (self.tab, self.grid) == *other
        }
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
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0),
            vec![(tab(1), GRID)]
        );
        assert!(resize
            .wave(SLOT, &[tab(1)], &none(), GRID, |_| 0)
            .is_empty());
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), WIDER, |_| 0),
            vec![(tab(1), WIDER)]
        );
    }

    #[test]
    fn what_was_sent_is_forgotten_with_the_connection() {
        let now = Instant::now();
        let mut resize = connected(now);
        resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);

        resize.connected(None, now);
        resize.connected(Some(SLOT), now);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0),
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
            .wave(HostId::new(4), &[tab(1)], &none(), GRID, |_| 0)
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
        resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.attached(tab(1));
        assert_eq!(
            resize.last_sent(tab(1)),
            None,
            "the attach resizes the tab now"
        );
        resize.trigger(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let attached = HashSet::from([tab(1)]);
        assert!(resize
            .wave(SLOT, &[tab(1)], &attached, WIDER, |_| 0)
            .is_empty());

        resize.detached(tab(1), now);
        assert_eq!(
            resize.poll(now),
            Poll::Arm(SETTLE),
            "a detach arms the wave"
        );
        resize.fired();
        assert_eq!(resize.poll(now + SETTLE), Poll::Wave);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), WIDER, |_| 0),
            vec![(tab(1), WIDER)]
        );
        assert!(resize
            .wave(SLOT, &[tab(1)], &none(), WIDER, |_| 0)
            .is_empty());
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
        resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0);
        resize.settle(
            SLOT,
            vec![
                outcome(tab(1), GRID, Err(rejected(ServerCode::NotFound))),
                outcome(tab(2), GRID, Ok(())),
            ],
            now,
            |_| 0,
        );
        assert_eq!(resize.poll(now), Poll::Idle, "a refusal is not retried");
        assert_eq!(
            resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0),
            vec![(tab(1), GRID)]
        );
    }

    #[test]
    fn an_answer_a_later_wave_overtook_changes_nothing() {
        let now = Instant::now();
        let mut resize = connected(now);
        resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.wave(SLOT, &[tab(1)], &none(), WIDER, |_| 0);
        resize.settle(
            SLOT,
            vec![outcome(tab(1), GRID, Err(HostOpError::QueueFull))],
            now,
            |_| 0,
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
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        let outcomes = targets
            .into_iter()
            .map(|target| outcome(target.tab, target.grid, Err(HostOpError::QueueFull)))
            .collect();
        resize.settle(SLOT, outcomes, now, |_| 0);
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
        assert!(
            !retryable(&rejected(ServerCode::Owned)),
            "an owned tab is parked, not retried"
        );
        assert!(!retryable(&rejected(ServerCode::NotFound)));
        assert!(!retryable(&rejected(ServerCode::InvalidParam)));
        assert!(!retryable(&HostOpError::Disconnected));
        assert!(!retryable(&HostOpError::WorkerGone));
    }

    fn owned() -> HostOpError {
        rejected(ServerCode::Owned)
    }

    /// Plan 084 D6: `owned` is final. The tab is parked, not retried and
    /// not recorded as given: a further window resize sends it nothing,
    /// and once a `tab.resized` says nobody sizes it, exactly one more
    /// wave resizes it — and the echo of that resize waves nothing.
    #[test]
    fn an_owned_tab_parks_until_nobody_sizes_it_then_gets_exactly_one_more_resize() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 4);
        assert_eq!(targets, vec![(tab(1), GRID)]);
        resize.settle(SLOT, answered(&targets, &Err(owned())), now, |_| 4);
        assert!(resize.parked(tab(1)));
        assert_eq!(resize.poll(now), Poll::Idle, "no retry is owed");
        assert_eq!(resize.last_sent(tab(1)), None, "never recorded as given");

        resize.resized(now);
        resize.fired();
        assert_eq!(resize.poll(now + SETTLE), Poll::Wave);
        assert!(
            resize
                .wave(SLOT, &[tab(1)], &none(), WIDER, |_| 4)
                .is_empty(),
            "a parked tab is left alone"
        );
        assert_eq!(resize.sends(tab(1)), 1);

        resize.tab_resized(tab(1), (100, 40), true, now);
        assert!(!resize.parked(tab(1)));
        assert_eq!(
            resize.poll(now),
            Poll::Wave,
            "nobody sizes it: a wave is owed"
        );
        let targets = resize.wave(SLOT, &[tab(1)], &none(), WIDER, |_| 5);
        assert_eq!(targets, vec![(tab(1), WIDER)]);
        resize.settle(SLOT, answered(&targets, &Ok(())), now, |_| 5);
        resize.tab_resized(tab(1), WIDER, true, now);
        assert_eq!(
            resize.poll(now),
            Poll::Idle,
            "the echo of the wave's own resize waves nothing"
        );
        assert_eq!(resize.sends(tab(1)), 2, "exactly one more send");
    }

    /// A wave over a tab nobody sizes resizes it once: the session's
    /// `tab.resized` for that resize is the wave's own grid coming back.
    #[test]
    fn a_wave_over_an_unowned_tab_sends_it_one_resize() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0);
        resize.tab_resized(tab(1), GRID, true, now);
        resize.tab_resized(tab(2), GRID, true, now);
        resize.settle(SLOT, answered(&targets, &Ok(())), now, |_| 1);
        assert_eq!(resize.poll(now), Poll::Idle);
        assert!(resize
            .wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 1)
            .is_empty());
        assert_eq!((resize.sends(tab(1)), resize.sends(tab(2))), (1, 1));
    }

    /// The session's other windows hear every `tab.resized` this one
    /// causes, and it hears theirs. Two windows at different grids, each
    /// waving a tab nobody sizes, with every resize either one applies
    /// announced to both: they settle, rather than resizing the tab back
    /// and forth for ever.
    #[test]
    fn two_windows_over_one_unowned_tab_reach_quiescence() {
        let now = Instant::now();
        let mut windows = [(connected(now), GRID), (connected(now), WIDER)];
        let mut applied = Vec::new();
        for _round in 0..20 {
            let mut announced = Vec::new();
            for (resize, grid) in &mut windows {
                if resize.poll(now) != Poll::Wave {
                    continue;
                }
                let targets = resize.wave(SLOT, &[tab(1)], &none(), *grid, |_| 0);
                for target in &targets {
                    announced.push(target.grid);
                    applied.push(target.grid);
                }
                resize.settle(SLOT, answered(&targets, &Ok(())), now, |_| 0);
            }
            if announced.is_empty() {
                assert!(applied.len() <= 2, "{applied:?}");
                return;
            }
            for grid in announced {
                for (resize, _) in &mut windows {
                    resize.tab_resized(tab(1), grid, true, now);
                }
            }
        }
        panic!("the two windows never stopped: {applied:?}");
    }

    /// A refusal that crossed an `owner: null` on its way back describes
    /// an owner already gone: it parks nothing, and the tab is owed a
    /// wave at what the session now says.
    #[test]
    fn a_refusal_from_an_owner_since_gone_parks_nothing() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 4);
        resize.settle(SLOT, answered(&targets, &Err(owned())), now, |_| 5);
        assert!(!resize.parked(tab(1)));
        assert_eq!(resize.poll(now), Poll::Wave);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 5),
            vec![(tab(1), GRID)]
        );
    }

    /// Each refusal is judged by the epoch its own send went out at, not
    /// by whatever a later wave stamped the tab with. W1 sends at epoch 4;
    /// the owner leaves (epoch 5) at another grid; W2 sends at 5 and finds
    /// the queue full; W1's `owned` lands last. It is stale and parks
    /// nothing, and the tab stays owed.
    #[test]
    fn a_refusal_is_judged_by_its_own_sends_epoch_not_a_later_waves() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let first = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 4);
        resize.tab_resized(tab(1), (100, 40), true, now);
        resize.resized(now);
        resize.fired();
        assert_eq!(resize.poll(now + SETTLE), Poll::Wave);
        let second = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 5);
        assert_eq!(second, vec![(tab(1), GRID)]);
        resize.settle(
            SLOT,
            answered(&second, &Err(HostOpError::QueueFull)),
            now,
            |_| 5,
        );
        resize.settle(SLOT, answered(&first, &Err(owned())), now, |_| 5);
        assert!(!resize.parked(tab(1)), "a stale refusal parks nothing");
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 5),
            vec![(tab(1), GRID)],
            "the tab is still owed"
        );
    }

    /// A full queue's refusal is retried even when an event cleared what
    /// the wave gave the tab while the answer was on its way: another
    /// window's `owner: null` at its own grid parks nothing here and waves
    /// nothing, so the retry is all that will ever send this window's.
    #[test]
    fn a_retry_survives_an_event_that_cleared_what_the_wave_gave() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let first = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.tab_resized(tab(1), WIDER, true, now);
        assert_eq!(resize.poll(now), Poll::Idle);
        resize.settle(
            SLOT,
            answered(&first, &Err(HostOpError::QueueFull)),
            now,
            |_| 1,
        );
        assert_eq!(resize.poll(now), Poll::Arm(FIRST_RETRY), "a retry is owed");
        resize.fired();
        assert_eq!(resize.poll(now + FIRST_RETRY), Poll::Wave);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 1),
            vec![(tab(1), GRID)]
        );
    }

    /// A rebuilt mirror counts owner epochs from 0 again. A refusal sent
    /// before the rebuild at the epoch the new mirror happens to show
    /// describes an owner that may have left in the gap: it parks
    /// nothing, behind a recovery wave the queue refused.
    #[test]
    fn a_refusal_from_before_a_rebuilt_mirror_parks_nothing() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let first = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.mirror_reset(SLOT, now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let recovery = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        assert_eq!(recovery, vec![(tab(1), GRID)]);
        resize.settle(
            SLOT,
            answered(&recovery, &Err(HostOpError::QueueFull)),
            now,
            |_| 0,
        );
        resize.settle(SLOT, answered(&first, &Err(owned())), now, |_| 0);
        assert!(!resize.parked(tab(1)));
        assert_eq!(resize.poll(now), Poll::Wave, "the tab is owed at once");
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0),
            vec![(tab(1), GRID)]
        );
    }

    /// A tab another client resized is not at this window's grid any
    /// more. Nobody sizing it is not, by itself, a reason to wave — every
    /// window hears that — but the next wave gives it this window's grid.
    #[test]
    fn a_tab_another_client_sized_gets_this_windows_grid_from_the_next_wave() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.tab_resized(tab(1), WIDER, false, now);
        assert_eq!(resize.poll(now), Poll::Idle, "owned: nothing to do yet");
        resize.tab_resized(tab(1), WIDER, true, now);
        assert_eq!(
            resize.poll(now),
            Poll::Idle,
            "never parked: no wave of its own"
        );
        resize.resized(now);
        resize.fired();
        assert_eq!(resize.poll(now + SETTLE), Poll::Wave);
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 2),
            vec![(tab(1), GRID)]
        );
    }

    /// What the wave learned from refusals goes with the mirror they were
    /// judged against, and with the connection.
    #[test]
    fn a_rebuilt_mirror_or_a_reconnect_unparks_every_tab() {
        let now = Instant::now();
        for clear in [
            (|resize: &mut BackgroundResize, now| resize.mirror_reset(SLOT, now))
                as fn(&mut BackgroundResize, Instant),
            |resize, now| {
                resize.connected(None, now);
                resize.connected(Some(SLOT), now);
            },
        ] {
            let mut resize = connected(now);
            let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 3);
            resize.settle(SLOT, answered(&targets, &Err(owned())), now, |_| 3);
            assert!(resize.parked(tab(1)));
            clear(&mut resize, now);
            assert!(!resize.parked(tab(1)));
            assert_eq!(
                resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0),
                vec![(tab(1), GRID)]
            );
        }
    }

    /// A resync means `tab.resized` went unheard in the gap, and will not
    /// be heard later: the wave forgets the grids it gave and goes again.
    #[test]
    fn a_rebuilt_mirror_forgets_what_it_gave_and_waves_again() {
        let now = Instant::now();
        let mut resize = connected(now);
        assert_eq!(resize.poll(now), Poll::Wave);
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        resize.settle(SLOT, answered(&targets, &Ok(())), now, |_| 0);
        assert_eq!(resize.poll(now), Poll::Idle);

        resize.mirror_reset(SLOT, now);
        assert_eq!(resize.poll(now), Poll::Wave, "the reset owes a wave");
        assert_eq!(
            resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0),
            vec![(tab(1), GRID)],
            "at a grid it had given before the gap"
        );
    }

    /// `wave_sends` counts what the queue took: a full queue refuses the
    /// whole wave unsent, and none of it is a send.
    #[tokio::test]
    async fn only_requests_the_queue_took_are_counted_as_sends() {
        let (queue, _worker) = HostOps::channel();
        while queue
            .send_quiet(HostIntent::new("filler", serde_json::Value::Null))
            .is_ok()
        {}
        let now = Instant::now();
        let mut resize = connected(now);
        let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0);
        let outcomes = send_wave(&queue, &targets).await;
        assert!(outcomes.iter().all(|outcome| !outcome.sent));
        resize.settle(SLOT, outcomes, now, |_| 0);
        assert_eq!((resize.sends(tab(1)), resize.sends(tab(2))), (0, 0));

        let (queue, mut worker) = HostOps::channel();
        let targets = resize.wave(SLOT, &[tab(1)], &none(), GRID, |_| 0);
        let answered = send_wave(&queue, &targets);
        worker
            .recv()
            .await
            .expect("the resize is on the queue")
            .answer(Ok(serde_json::json!({})));
        resize.settle(SLOT, answered.await, now, |_| 0);
        assert_eq!(resize.sends(tab(1)), 1);
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
            let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0);
            assert_eq!(targets.len(), 2, "every refused tab goes again");
            let outcomes = send_wave(&queue, &targets).await;
            assert!(outcomes
                .iter()
                .all(|outcome| outcome.result == Err(HostOpError::QueueFull)));
            resize.settle(SLOT, outcomes, now, |_| 0);
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
        let targets = resize.wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0);
        let answered = send_wave(&queue, &targets);
        for _ in &targets {
            let intent = worker.recv().await.expect("the retry reaches the queue");
            assert_eq!(intent.op, ops::TAB_RESIZE);
            assert_eq!(intent.fence, Some(SLOT));
            intent.answer(Ok(serde_json::json!({})));
        }
        resize.settle(SLOT, answered.await, now, |_| 0);
        assert_eq!(resize.poll(now), Poll::Idle, "the pending set drained");
        assert!(resize
            .wave(SLOT, &[tab(1), tab(2)], &none(), GRID, |_| 0)
            .is_empty());
    }
}
