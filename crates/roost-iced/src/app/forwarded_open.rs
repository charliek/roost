//! A tab opened through the UI socket on the slot, until the window's
//! mirror lists it (plan 071 §D12).
//!
//! The forwarded `tab.open` is answered from the session's reply, and
//! the batch that puts the tab in the mirror is a separate message that
//! may land after it. A `tab.focus` naming the tab in that gap would find
//! no row to select, so it waits here for one instead of answering
//! `not-found`.

use std::time::Instant;

use roost_engine::ipc::HostReply;
use roost_engine::WorkspaceError;
use roost_ipc::messages::{ops, TabOpenResult};
use roost_ui_model::keys::{HostId, TabKey};
use serde::Deserialize;

use super::host_wait_expired;

/// Whether a forwarded op selects what it opens: a `tab.open` that did
/// not ask for `activate: false`.
pub(super) fn forward_selects(op: &str, params: &serde_json::Value) -> bool {
    op == ops::TAB_OPEN
        && params.get("activate").and_then(serde_json::Value::as_bool) != Some(false)
}

/// The tab a forwarded op opened, read off the session's successful
/// reply.
pub(super) fn opened_tab(op: &str, reply: &serde_json::Value) -> Option<i64> {
    if op != ops::TAB_OPEN {
        return None;
    }
    TabOpenResult::deserialize(reply)
        .ok()
        .map(|opened| opened.tab.id)
}

/// What one reconcile makes of an awaited tab.
#[derive(Debug, PartialEq, Eq)]
enum Step<R> {
    Wait,
    Listed(R),
    Lost,
}

/// Listed wins over the deadline: a row that is there is the answer,
/// however late the reconcile that saw it.
fn step<R>(armed: Instant, connected: bool, row: Option<R>, now: Instant) -> Step<R> {
    if !connected {
        Step::Lost
    } else if let Some(row) = row {
        Step::Listed(row)
    } else if host_wait_expired(armed, now) {
        Step::Lost
    } else {
        Step::Wait
    }
}

/// Every successful forwarded open, with or without `activate`, until
/// its row is listed, it closes, its connection is purged, or the
/// deadline passes — with the focuses parked on it.
#[derive(Default)]
pub(super) struct AwaitingListing {
    /// In open order, so focuses settled by one reconcile land in the
    /// order their tabs were opened.
    entries: Vec<(TabKey, Awaiting)>,
}

struct Awaiting {
    armed: Instant,
    focuses: Vec<HostReply<()>>,
}

impl Awaiting {
    fn lose(&mut self, tab: TabKey) {
        for reply in self.focuses.drain(..) {
            let _ = reply.send(Err(WorkspaceError::TabNotFound(tab.tab)));
        }
    }
}

impl AwaitingListing {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(super) fn track(&mut self, tab: TabKey, now: Instant) {
        self.entries.push((
            tab,
            Awaiting {
                armed: now,
                focuses: Vec::new(),
            },
        ));
    }

    /// Park a focus on an awaited tab. A tab that is not awaited hands
    /// the reply back, for the ordinary focus path to answer.
    pub(super) fn park(&mut self, tab: TabKey, reply: HostReply<()>) -> Option<HostReply<()>> {
        match self.entries.iter_mut().find(|(key, _)| *key == tab) {
            Some((_, awaiting)) => {
                awaiting.focuses.push(reply);
                None
            }
            None => Some(reply),
        }
    }

    /// The session closed `tab`: it will never be listed.
    pub(super) fn closed(&mut self, tab: TabKey) {
        self.lose_where(|key| key == tab);
    }

    /// `host`'s connection incarnation is over.
    pub(super) fn purge(&mut self, host: HostId) {
        self.lose_where(|key| key.host == host);
    }

    pub(super) fn clear(&mut self) {
        self.lose_where(|_| true);
    }

    fn lose_where(&mut self, gone: impl Fn(TabKey) -> bool) {
        self.entries.retain_mut(|(key, awaiting)| {
            if gone(*key) {
                awaiting.lose(*key);
                return false;
            }
            true
        });
    }

    /// Settle every awaited tab against `facts(tab) = (connected, the
    /// row listing it)`. A lost tab's focuses are answered `not-found`
    /// here; a listed one's are handed back with its row, for the caller
    /// to answer once it has selected the tab.
    pub(super) fn settle<R>(
        &mut self,
        now: Instant,
        facts: impl Fn(TabKey) -> (bool, Option<R>),
    ) -> Vec<(TabKey, R, Vec<HostReply<()>>)> {
        let mut listed = Vec::new();
        self.entries.retain_mut(|(key, awaiting)| {
            let (connected, row) = facts(*key);
            match step(awaiting.armed, connected, row, now) {
                Step::Wait => true,
                Step::Listed(row) => {
                    listed.push((*key, row, std::mem::take(&mut awaiting.focuses)));
                    false
                }
                Step::Lost => {
                    awaiting.lose(*key);
                    false
                }
            }
        });
        listed
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use serde_json::json;
    use tokio::sync::oneshot;

    use super::super::tab_memory::fixtures::project;
    use super::super::PENDING_HOST_SELECTION_DEADLINE;
    use super::*;

    type Answer = oneshot::Receiver<Result<(), WorkspaceError>>;

    fn slot() -> HostId {
        HostId::new(3)
    }

    fn key(tab: i64) -> TabKey {
        TabKey::new(slot(), tab)
    }

    fn focus() -> (HostReply<()>, Answer) {
        oneshot::channel()
    }

    fn unanswered(answer: &mut Answer) -> bool {
        matches!(answer.try_recv(), Err(oneshot::error::TryRecvError::Empty))
    }

    fn not_found(answer: &mut Answer, tab: i64) -> bool {
        matches!(answer.try_recv(), Ok(Err(WorkspaceError::TabNotFound(id))) if id == tab)
    }

    #[test]
    fn a_forwarded_tab_open_selects_unless_it_asked_not_to() {
        let open = |params| forward_selects(ops::TAB_OPEN, &params);
        assert!(open(json!({"project_id": "1"})));
        assert!(open(json!({"project_id": "1", "activate": true})));
        assert!(open(json!({"project_id": "1", "activate": null})));
        assert!(!open(json!({"project_id": "1", "activate": false})));
    }

    #[test]
    fn no_other_forwarded_op_selects() {
        for op in [
            ops::PROJECT_CREATE,
            ops::PROJECT_ENSURE,
            ops::TAB_CLOSE,
            ops::TAB_WRITE,
        ] {
            assert!(!forward_selects(op, &json!({"project_id": "1"})), "{op}");
        }
    }

    #[test]
    fn the_opened_tab_is_read_off_a_tab_open_reply_only() {
        let tab = project(1, &[42]).tabs.remove(0);
        let reply = serde_json::to_value(TabOpenResult { tab }).unwrap();
        assert_eq!(opened_tab(ops::TAB_OPEN, &reply), Some(42));
        assert_eq!(opened_tab(ops::PROJECT_CREATE, &reply), None);
        assert_eq!(opened_tab(ops::TAB_OPEN, &json!({})), None);
    }

    #[test]
    fn the_step_table() {
        let armed = Instant::now();
        let late = armed + PENDING_HOST_SELECTION_DEADLINE;
        let early = late - Duration::from_millis(1);
        assert_eq!(step(armed, true, None::<()>, early), Step::Wait);
        assert_eq!(step(armed, true, Some(()), early), Step::Listed(()));
        assert_eq!(step(armed, true, None::<()>, late), Step::Lost);
        assert_eq!(step(armed, true, Some(()), late), Step::Listed(()));
        assert_eq!(step(armed, false, None::<()>, early), Step::Lost);
    }

    #[test]
    fn a_focus_on_an_awaited_tab_is_parked_until_the_row_is_listed() {
        let armed = Instant::now();
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), armed);
        let (reply, mut answer) = focus();

        assert!(
            awaiting.park(key(7), reply).is_none(),
            "parked, not answered"
        );
        assert!(unanswered(&mut answer));

        assert!(awaiting.settle(armed, |_| (true, None::<()>)).is_empty());
        assert!(unanswered(&mut answer), "still waiting for the row");

        let listed = awaiting.settle(armed, |_| (true, Some(())));
        assert_eq!(listed.len(), 1);
        let (tab, (), focuses) = listed.into_iter().next().unwrap();
        assert_eq!(tab, key(7));
        assert_eq!(focuses.len(), 1, "handed back for the caller to answer");
        assert!(awaiting.is_empty(), "a listed tab leaves the set");
    }

    #[test]
    fn a_focus_on_a_tab_nobody_is_awaiting_is_handed_back() {
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), Instant::now());
        let (reply, _answer) = focus();
        assert!(awaiting.park(key(8), reply).is_some());
        let (reply, _answer) = focus();
        assert!(
            awaiting
                .park(TabKey::new(HostId::new(4), 7), reply)
                .is_some(),
            "the same id on another incarnation is another tab"
        );
    }

    #[test]
    fn a_parked_focus_answers_not_found_at_the_deadline() {
        let armed = Instant::now();
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), armed);
        let (reply, mut answer) = focus();
        assert!(awaiting.park(key(7), reply).is_none());

        let early = armed + PENDING_HOST_SELECTION_DEADLINE - Duration::from_millis(1);
        assert!(awaiting.settle(early, |_| (true, None::<()>)).is_empty());
        assert!(unanswered(&mut answer));

        let late = armed + PENDING_HOST_SELECTION_DEADLINE;
        assert!(awaiting.settle(late, |_| (true, None::<()>)).is_empty());
        assert!(not_found(&mut answer, 7));
        assert!(awaiting.is_empty());
    }

    #[test]
    fn a_parked_focus_answers_not_found_when_its_tab_closes() {
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), Instant::now());
        awaiting.track(key(8), Instant::now());
        let (reply, mut answer) = focus();
        assert!(awaiting.park(key(7), reply).is_none());

        awaiting.closed(key(7));
        assert!(not_found(&mut answer, 7));
        let (reply, _answer) = focus();
        assert!(
            awaiting.park(key(7), reply).is_some(),
            "the closed tab left the set"
        );
        let (reply, _answer) = focus();
        assert!(
            awaiting.park(key(8), reply).is_none(),
            "and nothing else did"
        );
    }

    #[test]
    fn a_purge_answers_every_parked_focus_on_that_incarnation() {
        let other = TabKey::new(HostId::new(4), 7);
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), Instant::now());
        awaiting.track(other, Instant::now());
        let (reply, mut answer) = focus();
        assert!(awaiting.park(key(7), reply).is_none());
        let (reply, mut kept) = focus();
        assert!(awaiting.park(other, reply).is_none());

        awaiting.purge(slot());
        assert!(not_found(&mut answer, 7));
        assert!(unanswered(&mut kept), "another incarnation's wait goes on");
    }

    #[test]
    fn a_clear_answers_every_parked_focus_and_empties_the_set() {
        let other = TabKey::new(HostId::new(4), 7);
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), Instant::now());
        awaiting.track(other, Instant::now());
        let (reply, mut answer) = focus();
        assert!(awaiting.park(key(7), reply).is_none());
        let (reply, mut also) = focus();
        assert!(awaiting.park(other, reply).is_none());

        awaiting.clear();
        assert!(not_found(&mut answer, 7));
        assert!(not_found(&mut also, 7));
        assert!(awaiting.is_empty());
    }

    #[test]
    fn a_lost_connection_answers_not_found_before_the_deadline() {
        let armed = Instant::now();
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), armed);
        let (reply, mut answer) = focus();
        assert!(awaiting.park(key(7), reply).is_none());
        assert!(awaiting.settle(armed, |_| (false, None::<()>)).is_empty());
        assert!(not_found(&mut answer, 7));
    }

    #[test]
    fn an_open_nobody_focused_leaves_the_set_on_listing_or_deadline() {
        let armed = Instant::now();
        let mut awaiting = AwaitingListing::default();
        awaiting.track(key(7), armed);
        awaiting.track(key(8), armed);
        let listed = awaiting.settle(armed, |tab| (true, (tab == key(7)).then_some(())));
        assert_eq!(listed.len(), 1);
        assert!(listed[0].2.is_empty(), "no focus was parked on it");
        assert!(!awaiting.is_empty());
        awaiting.settle(armed + PENDING_HOST_SELECTION_DEADLINE, |_| {
            (true, None::<()>)
        });
        assert!(awaiting.is_empty());
    }

    #[test]
    fn listed_tabs_settle_in_open_order() {
        let armed = Instant::now();
        let mut awaiting = AwaitingListing::default();
        for tab in [9, 4, 6] {
            awaiting.track(key(tab), armed);
        }
        let order: Vec<i64> = awaiting
            .settle(armed, |_| (true, Some(())))
            .into_iter()
            .map(|(tab, (), _)| tab.tab)
            .collect();
        assert_eq!(order, [9, 4, 6]);
    }
}
