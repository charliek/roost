//! Tab size ownership (plan 084 D4; `discovery/protocol-7-release.md` § M3
//! rules 1–10): which attachment's grid is the PTY's.
//!
//! Decisions only. Everything that sizes the tab goes through a [`Sizer`]
//! the tab task hands in, and every change this module makes is read back
//! by [`SizeOwnership::announce`], so the task — the one serializer of a
//! tab's input and geometry — performs the applies and emits `GRID` and
//! `tab.resized`. Time is a millisecond count the caller supplies.
//!
//! An attachment that takes before it is committed (its accepted reply not
//! yet written) owns *provisionally*: it is the owner for every decision,
//! and a cancel rolls it back to what it replaced — only while nothing newer
//! has replaced it, and only to a predecessor that is still eligible.

use std::collections::{BTreeMap, BTreeSet};

use roost_ipc::messages::SizeOwner;
use tracing::warn;

use crate::pty::Geometry;

/// One data connection's id: its connection id.
pub type AttachmentId = u64;

/// How a decision sizes the tab.
pub trait Sizer {
    /// Size the tab to `grid`, both halves, or leave it exactly as it was
    /// and say so.
    fn apply(&mut self, grid: Geometry) -> bool;
    /// The tab's geometry right now.
    fn current(&self) -> Geometry;
}

#[derive(Debug, Clone)]
struct Attachment {
    client_id: String,
    label: String,
    /// The client's own grid, whether or not it owns the tab.
    grid: Geometry,
    /// Moved by the attach and by every `RESIZE`: the handoff's recency.
    updated_at: u64,
    attached_order: u64,
    committed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Owner {
    None,
    Live(AttachmentId),
    /// An attachment took the tab before its commit. It can do so only
    /// once — at its attach — so its id names the take, and a cancel
    /// rolls back only while this is still the owner.
    Provisional {
        id: AttachmentId,
        prev: Box<Owner>,
    },
    Grace(Grace),
}

/// An owner whose connection dropped without `DETACH`: the tab stays its
/// for a while, under the label it had, at the grid it had.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Grace {
    client_id: String,
    label: String,
    grid: Geometry,
    until: u64,
    incarnation: u64,
}

/// What a `tab.take_size` could not do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TakeRefusal {
    /// The client has no committed attachment to this tab.
    NoAttachment,
    /// The tab could not be sized to the attachment's grid.
    ApplyFailed,
}

/// One tab's size-ownership state.
#[derive(Debug)]
pub struct SizeOwnership {
    attachments: BTreeMap<AttachmentId, Attachment>,
    owner: Owner,
    /// Clients that released the tab and have not taken it since: none of
    /// their attachments, present or yet to come, is a candidate —
    /// otherwise a still-attached owner's release would hand the size
    /// straight back to it.
    released: BTreeSet<String>,
    next_order: u64,
    next_incarnation: u64,
    grace_ms: u64,
    /// Every `updated_at` reads the same instant, so the tie rule decides
    /// every handoff (a test knob).
    frozen_stamps: bool,
    /// Bumps on every announced change of grid or owner.
    grid_gen: u64,
    announced: ((u16, u16), Option<SizeOwner>),
}

impl Default for SizeOwnership {
    fn default() -> Self {
        Self::new((0, 0), 0, false)
    }
}

impl SizeOwnership {
    /// A tab at `grid` that nobody owns.
    pub fn new(grid: (u16, u16), grace_ms: u64, frozen_stamps: bool) -> Self {
        Self {
            attachments: BTreeMap::new(),
            owner: Owner::None,
            released: BTreeSet::new(),
            next_order: 0,
            next_incarnation: 0,
            grace_ms,
            frozen_stamps,
            grid_gen: 0,
            announced: (grid, None),
        }
    }

    /// The owner as the wire names it: the owning attachment's client and
    /// label, or a grace record's.
    pub fn owner(&self) -> Option<SizeOwner> {
        self.owner_names().map(|(client_id, label)| SizeOwner {
            client_id: client_id.to_string(),
            label: label.to_string(),
        })
    }

    /// The owner's client id and label, borrowed.
    fn owner_names(&self) -> Option<(&str, &str)> {
        match &self.owner {
            Owner::None => None,
            Owner::Live(id) | Owner::Provisional { id, .. } => self
                .attachments
                .get(id)
                .map(|att| (att.client_id.as_str(), att.label.as_str())),
            Owner::Grace(grace) => Some((&grace.client_id, &grace.label)),
        }
    }

    fn owner_client(&self) -> Option<&str> {
        self.owner_names().map(|(client_id, _)| client_id)
    }

    pub fn grid_gen(&self) -> u64 {
        self.grid_gen
    }

    /// Record the tab's state after a decision. `Some(gen)` when its grid
    /// or its wire owner differs from the last one announced — one `GRID`
    /// and one `tab.resized` are owed, at that generation.
    pub fn announce(&mut self, grid: (u16, u16)) -> Option<u64> {
        let (announced_grid, announced_owner) = &self.announced;
        let announced_names = announced_owner
            .as_ref()
            .map(|owner| (owner.client_id.as_str(), owner.label.as_str()));
        if grid == *announced_grid && self.owner_names() == announced_names {
            return None;
        }
        self.announced = (grid, self.owner());
        self.grid_gen += 1;
        Some(self.grid_gen)
    }

    /// When the grace record, if one is the owner, runs out — and the
    /// incarnation an expiry must name to act on it.
    pub fn grace_deadline(&self) -> Option<(u64, u64)> {
        match &self.owner {
            Owner::Grace(grace) => Some((grace.until, grace.incarnation)),
            _ => None,
        }
    }

    /// The client whose grace record this tab holds, current or waiting to
    /// be restored under a provisional reclaim, with the label the record
    /// keeps — what keeps that client's label alive in the session.
    pub fn graced_client(&self) -> Option<(&str, &str)> {
        let mut owner = &self.owner;
        loop {
            match owner {
                Owner::Grace(grace) => return Some((&grace.client_id, &grace.label)),
                Owner::Provisional { prev, .. } => owner = prev,
                Owner::None | Owner::Live(_) => return None,
            }
        }
    }

    /// Whether this attachment owns the tab, live or provisionally.
    pub fn is_owner(&self, id: AttachmentId) -> bool {
        matches!(self.owner, Owner::Live(owner) | Owner::Provisional { id: owner, .. } if owner == id)
    }

    /// Rule 4: the attachment an `INPUT`'s take flags act for — none from
    /// the owner, whose flags change nothing.
    pub fn input_taker(&self, from: Option<AttachmentId>) -> Option<AttachmentId> {
        from.filter(|id| !self.is_owner(*id))
    }

    /// Rule 2 (step 1). `true` when the attachment took the tab
    /// provisionally: it asked to, nobody owned the tab, its own client's
    /// grace record is the owner (a reclaim), or another attachment of its
    /// own client owns it (a reconnect whose attach beat the old EOF).
    #[allow(clippy::too_many_arguments)] // One attach's facts, named as the wire names them.
    pub fn attach(
        &mut self,
        id: AttachmentId,
        client_id: &str,
        label: &str,
        take: bool,
        grid: Geometry,
        now: u64,
        sizer: &mut impl Sizer,
    ) -> bool {
        let attached_order = self.next_order;
        self.next_order += 1;
        self.attachments.insert(
            id,
            Attachment {
                client_id: client_id.to_string(),
                label: label.to_string(),
                grid,
                updated_at: self.stamp(now),
                attached_order,
                committed: false,
            },
        );
        let claims = take
            || match &self.owner {
                Owner::None => true,
                Owner::Provisional { .. } => false,
                Owner::Live(_) | Owner::Grace(_) => self.owner_client() == Some(client_id),
            };
        if !claims {
            return false;
        }
        let prev = Box::new(self.owner.clone());
        if !self.take(id, sizer) {
            return false;
        }
        self.owner = Owner::Provisional { id, prev };
        true
    }

    /// Step 2: the attachment's accepted reply is on the wire.
    pub fn commit(&mut self, id: AttachmentId) {
        let Some(att) = self.attachments.get_mut(&id) else {
            return;
        };
        att.committed = true;
        if matches!(self.owner, Owner::Provisional { id: owner, .. } if owner == id) {
            self.owner = Owner::Live(id);
        }
    }

    /// The attachment ended: a cancel before commit (step 3), a deliberate
    /// detach (step 7) or a disconnect (step 8). Unknown ids are a no-op.
    pub fn detach(&mut self, id: AttachmentId, deliberate: bool, now: u64, sizer: &mut impl Sizer) {
        let Some(att) = self.attachments.remove(&id) else {
            return;
        };
        if !att.committed {
            return self.cancel(id, now, sizer);
        }
        if self.owner != Owner::Live(id) {
            return;
        }
        self.owner = Owner::None;
        let sibling = self
            .candidates()
            .any(|(_, other)| other.client_id == att.client_id);
        if deliberate || sibling {
            return self.handoff(Some(&att.client_id), sizer);
        }
        let incarnation = self.next_incarnation;
        self.next_incarnation += 1;
        self.owner = Owner::Grace(Grace {
            client_id: att.client_id,
            label: att.label,
            grid: sizer.current(),
            until: now.saturating_add(self.grace_ms),
            incarnation,
        });
    }

    /// Step 3, with the attachment already removed.
    fn cancel(&mut self, id: AttachmentId, now: u64, sizer: &mut impl Sizer) {
        let prev = match std::mem::replace(&mut self.owner, Owner::None) {
            Owner::Provisional { id: owner, prev } if owner == id => *prev,
            newer => {
                self.owner = newer;
                return;
            }
        };
        match self.restorable(prev, now) {
            Some(restored) => {
                let grid = self.grid_of(&restored);
                self.owner = restored;
                if let Some(grid) = grid {
                    if !sizer.apply(grid) {
                        warn!("a cancelled attach's predecessor could not be resized back");
                    }
                }
            }
            None => self.handoff(None, sizer),
        }
    }

    /// The predecessor a cancel returns to, if it is still valid — a grace
    /// keeps its own deadline and incarnation, a provisional predecessor
    /// that has committed since comes back as the live owner it now is,
    /// and one that has gone stands aside for its own.
    fn restorable(&self, prev: Owner, now: u64) -> Option<Owner> {
        match prev {
            Owner::None => None,
            Owner::Live(id) => self
                .attachments
                .get(&id)
                .is_some_and(|att| att.committed && self.eligible(att))
                .then_some(Owner::Live(id)),
            Owner::Grace(grace) => (grace.until > now).then_some(Owner::Grace(grace)),
            Owner::Provisional { id, prev } => match self.attachments.get(&id) {
                Some(att) if !att.committed => Some(Owner::Provisional { id, prev }),
                Some(att) if self.eligible(att) => Some(Owner::Live(id)),
                _ => self.restorable(*prev, now),
            },
        }
    }

    fn grid_of(&self, owner: &Owner) -> Option<Geometry> {
        match owner {
            Owner::None => None,
            Owner::Live(id) | Owner::Provisional { id, .. } => {
                self.attachments.get(id).map(|att| att.grid)
            }
            Owner::Grace(grace) => Some(grace.grid),
        }
    }

    /// Rule 3 / step 6: this attachment takes the tab at its own grid.
    /// `false` when it is unknown or the apply failed (nothing changes).
    pub fn take(&mut self, id: AttachmentId, sizer: &mut impl Sizer) -> bool {
        let Some(att) = self.attachments.get(&id) else {
            return false;
        };
        let (grid, client_id) = (att.grid, att.client_id.clone());
        if !sizer.apply(grid) {
            warn!(attachment = id, "a take could not size the tab");
            return false;
        }
        self.owner = Owner::Live(id);
        self.released.remove(&client_id);
        true
    }

    /// Step 5: a `RESIZE` frame. The grid becomes the attachment's own
    /// either way; the owner's applies, a focused non-owner's takes.
    pub fn resize(
        &mut self,
        id: AttachmentId,
        grid: Geometry,
        focused: bool,
        now: u64,
        sizer: &mut impl Sizer,
    ) {
        let stamp = self.stamp(now);
        let Some(att) = self.attachments.get_mut(&id) else {
            return;
        };
        att.grid = grid;
        att.updated_at = stamp;
        if self.is_owner(id) {
            if !sizer.apply(grid) {
                warn!(attachment = id, "the owner's resize could not be applied");
            }
        } else if focused {
            self.take(id, sizer);
        }
    }

    /// `tab.take_size` (rule 5): the client's newest attachment takes.
    /// The grid is the tab's afterwards.
    pub fn take_size(
        &mut self,
        client_id: &str,
        sizer: &mut impl Sizer,
    ) -> Result<(), TakeRefusal> {
        let id = self
            .attachments
            .iter()
            .filter(|(_, att)| att.committed && att.client_id == client_id)
            .max_by_key(|(_, att)| att.attached_order)
            .map(|(id, _)| *id)
            .ok_or(TakeRefusal::NoAttachment)?;
        if self.take(id, sizer) {
            Ok(())
        } else {
            Err(TakeRefusal::ApplyFailed)
        }
    }

    /// `tab.release_size` (rule 6): when this client owns the tab, live,
    /// provisionally or in grace, hand it on now — never back to it.
    pub fn release(&mut self, client_id: &str, sizer: &mut impl Sizer) {
        if self.owner_client() != Some(client_id) {
            return;
        }
        self.released.insert(client_id.to_string());
        self.owner = Owner::None;
        self.handoff(None, sizer);
    }

    /// Step 8's timer: a grace record ran out. Acts only on the incarnation
    /// that was armed, and only while it is still the owner.
    pub fn expire(&mut self, incarnation: u64, sizer: &mut impl Sizer) {
        let client = match &self.owner {
            Owner::Grace(grace) if grace.incarnation == incarnation => grace.client_id.clone(),
            _ => return,
        };
        self.owner = Owner::None;
        self.handoff(Some(&client), sizer);
    }

    /// Step 7: the next owner, with the leaving owner's client first
    /// (newest attachment first), then the latest advisory grid, ties to
    /// the earliest attachment. Each candidate is tried once; one whose
    /// grid cannot be applied is passed over, and none left leaves the tab
    /// unowned at the size it has.
    fn handoff(&mut self, leaving: Option<&str>, sizer: &mut impl Sizer) {
        let mut candidates: Vec<(AttachmentId, bool, u64, u64)> = self
            .candidates()
            .map(|(id, att)| {
                let own = leaving == Some(att.client_id.as_str());
                (id, own, att.updated_at, att.attached_order)
            })
            .collect();
        candidates.sort_by(|a, b| match (a.1, b.1) {
            (true, true) => b.3.cmp(&a.3),
            (false, false) => b.2.cmp(&a.2).then(a.3.cmp(&b.3)),
            _ => b.1.cmp(&a.1),
        });
        for (id, ..) in candidates {
            if self.take(id, sizer) {
                return;
            }
        }
    }

    fn candidates(&self) -> impl Iterator<Item = (AttachmentId, &Attachment)> {
        self.attachments
            .iter()
            .filter(|(_, att)| att.committed && self.eligible(att))
            .map(|(id, att)| (*id, att))
    }

    fn eligible(&self, att: &Attachment) -> bool {
        !self.released.contains(&att.client_id)
    }

    fn stamp(&self, now: u64) -> u64 {
        if self.frozen_stamps {
            0
        } else {
            now
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo(cols: u16, rows: u16) -> Geometry {
        Geometry {
            cols,
            rows,
            cell_w: 8,
            cell_h: 16,
        }
    }

    /// A tab that records every apply and fails the ones it was told to:
    /// every apply of a `refuse` grid, and the next apply after
    /// `fail_next` is set, as `tab.fail_next_resize` does.
    struct Tab {
        at: Geometry,
        applied: Vec<(u16, u16)>,
        refuse: Vec<(u16, u16)>,
        fail_next: bool,
    }

    impl Tab {
        fn at(cols: u16, rows: u16) -> Self {
            Self {
                at: geo(cols, rows),
                applied: Vec::new(),
                refuse: Vec::new(),
                fail_next: false,
            }
        }
        fn grid(&self) -> (u16, u16) {
            (self.at.cols, self.at.rows)
        }
    }

    impl Sizer for Tab {
        fn apply(&mut self, grid: Geometry) -> bool {
            if std::mem::take(&mut self.fail_next) || self.refuse.contains(&(grid.cols, grid.rows))
            {
                return false;
            }
            self.at = grid;
            self.applied.push((grid.cols, grid.rows));
            true
        }
        fn current(&self) -> Geometry {
            self.at
        }
    }

    const GRACE: u64 = 5_000;

    struct Fixture {
        owner: SizeOwnership,
        tab: Tab,
        now: u64,
    }

    impl Fixture {
        fn new() -> Self {
            Self::with(false)
        }
        fn with(frozen: bool) -> Self {
            Self {
                owner: SizeOwnership::new((80, 24), GRACE, frozen),
                tab: Tab::at(80, 24),
                now: 1_000,
            }
        }
        fn tick(&mut self, ms: u64) {
            self.now += ms;
        }
        /// Attach and commit: an attachment whose reply is on the wire.
        fn attached(&mut self, id: u64, client: &str, take: bool, grid: (u16, u16)) -> bool {
            let took = self.attach(id, client, take, grid);
            self.owner.commit(id);
            took
        }
        fn attach(&mut self, id: u64, client: &str, take: bool, grid: (u16, u16)) -> bool {
            self.tick(1);
            self.owner.attach(
                id,
                client,
                &format!("{client}-label"),
                take,
                geo(grid.0, grid.1),
                self.now,
                &mut self.tab,
            )
        }
        fn detach(&mut self, id: u64, deliberate: bool) {
            self.owner.detach(id, deliberate, self.now, &mut self.tab);
        }
        fn resize(&mut self, id: u64, grid: (u16, u16), focused: bool) {
            self.tick(1);
            self.owner
                .resize(id, geo(grid.0, grid.1), focused, self.now, &mut self.tab);
        }
        fn take(&mut self, id: u64) -> bool {
            self.owner.take(id, &mut self.tab)
        }
        fn expire(&mut self) {
            let (until, incarnation) = self.owner.grace_deadline().expect("a grace record");
            self.now = self.now.max(until);
            self.owner.expire(incarnation, &mut self.tab);
        }
        fn owner(&self) -> Option<String> {
            self.owner.owner().map(|owner| owner.client_id)
        }
    }

    // -- step 1: attach --------------------------------------------------

    #[test]
    fn a_take_attach_owns_provisionally_and_resizes() {
        let mut f = Fixture::new();
        f.attached(1, "a", false, (80, 24));
        assert!(f.attach(2, "b", true, (100, 30)));
        assert!(f.owner.is_owner(2), "a provisional owner is the owner");
        assert_eq!(f.owner(), Some("b".into()));
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn an_unowned_tab_is_taken_by_any_attach() {
        let mut f = Fixture::new();
        assert!(f.attached(1, "a", false, (100, 30)));
        assert_eq!(f.owner(), Some("a".into()));
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn a_watching_attach_on_an_owned_tab_changes_nothing() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.tab.applied.clear();
        assert!(!f.attached(2, "b", false, (100, 30)));
        assert_eq!(f.owner(), Some("a".into()));
        assert!(f.tab.applied.is_empty(), "watching resizes nothing");
    }

    #[test]
    fn a_same_client_attach_moves_ownership_and_resizes_only_on_a_new_grid() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.tab.applied.clear();
        assert!(f.attached(2, "a", false, (100, 30)));
        assert!(f.owner.is_owner(2));
        f.detach(1, false);
        assert!(f.owner.is_owner(2), "the old connection's EOF arms nothing");
        assert_eq!(f.owner.grace_deadline(), None);
        assert!(f.tab.applied.iter().all(|grid| *grid == (100, 30)));
    }

    #[test]
    fn an_attach_whose_take_cannot_apply_is_advisory() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.tab.refuse.push((100, 30));
        assert!(!f.attached(2, "b", true, (100, 30)));
        assert_eq!(f.owner(), Some("a".into()));
        assert_eq!(f.tab.grid(), (80, 24));
    }

    // -- step 2 and 3: commit and cancel -------------------------------

    #[test]
    fn commit_makes_a_provisional_owner_live() {
        let mut f = Fixture::new();
        f.attach(1, "a", true, (100, 30));
        f.owner.commit(1);
        f.attached(2, "b", false, (80, 24));
        f.detach(2, true);
        assert_eq!(f.owner(), Some("a".into()));
        assert_eq!(f.owner.owner, Owner::Live(1));
    }

    #[test]
    fn a_cancelled_take_returns_to_its_predecessor_at_its_grid() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attach(2, "b", true, (100, 30));
        f.detach(2, false);
        assert_eq!(f.owner(), Some("a".into()));
        assert_eq!(
            f.tab.grid(),
            (80, 24),
            "the predecessor's grid applies again"
        );
        assert_eq!(f.owner.owner, Owner::Live(1));
    }

    #[test]
    fn a_cancelled_take_with_no_valid_predecessor_hands_off() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(3, "c", false, (90, 25));
        f.attach(2, "b", true, (100, 30));
        f.detach(1, true);
        assert_eq!(f.owner(), Some("b".into()), "a superseded owner's detach");
        f.detach(2, false);
        assert_eq!(
            f.owner(),
            Some("c".into()),
            "the predecessor left, so a handoff"
        );
        assert_eq!(f.tab.grid(), (90, 25));
    }

    #[test]
    fn a_cancelled_attach_never_appears_as_a_candidate() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attach(2, "b", false, (100, 30));
        f.detach(1, true);
        assert_eq!(f.owner(), None, "an uncommitted watcher is no candidate");
        f.detach(2, false);
        assert_eq!(f.owner(), None);
    }

    #[test]
    fn overlapping_takes_commit_in_order() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attach(1, "a", true, (100, 30));
        f.attach(2, "b", true, (120, 40));
        f.owner.commit(1);
        f.owner.commit(2);
        assert_eq!(f.owner.owner, Owner::Live(2));
        assert_eq!(f.tab.grid(), (120, 40));
    }

    #[test]
    fn overlapping_takes_commit_in_reverse_order() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attach(1, "a", true, (100, 30));
        f.attach(2, "b", true, (120, 40));
        f.owner.commit(2);
        f.owner.commit(1);
        assert_eq!(f.owner.owner, Owner::Live(2));
    }

    #[test]
    fn overlapping_takes_cancel_in_order_back_to_the_first_owner() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attach(1, "a", true, (100, 30));
        f.attach(2, "b", true, (120, 40));
        f.detach(1, false);
        assert!(f.owner.is_owner(2), "the newer take is untouched");
        f.detach(2, false);
        assert_eq!(f.owner.owner, Owner::Live(9));
        assert_eq!(f.tab.grid(), (80, 24));
    }

    #[test]
    fn overlapping_takes_cancel_in_reverse_order_back_to_the_first_owner() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attach(1, "a", true, (100, 30));
        f.attach(2, "b", true, (120, 40));
        f.detach(2, false);
        assert!(f.owner.is_owner(1), "back to the take it replaced");
        assert_eq!(f.tab.grid(), (100, 30));
        f.detach(1, false);
        assert_eq!(f.owner.owner, Owner::Live(9));
        assert_eq!(f.tab.grid(), (80, 24));
    }

    #[test]
    fn a_committed_first_take_is_what_a_cancelled_second_returns_to() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attach(1, "a", true, (100, 30));
        f.attach(2, "b", true, (120, 40));
        f.owner.commit(1);
        f.detach(2, false);
        assert_eq!(f.owner.owner, Owner::Live(1));
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn a_newer_take_is_never_clobbered_by_an_older_cancel() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attached(3, "c", false, (90, 25));
        f.attach(1, "a", true, (100, 30));
        assert!(
            f.take(3),
            "a deliberate input takes from the provisional owner"
        );
        f.detach(1, false);
        assert_eq!(f.owner.owner, Owner::Live(3));
        assert_eq!(f.tab.grid(), (90, 25));
    }

    #[test]
    fn interactions_during_a_provisional_window_follow_the_rules() {
        let mut f = Fixture::new();
        f.attached(9, "p", true, (80, 24));
        f.attached(3, "c", false, (90, 25));
        f.attach(1, "a", true, (100, 30));
        f.resize(3, (91, 26), false);
        assert!(
            f.owner.is_owner(1),
            "an unfocused resize only records the grid"
        );
        assert_eq!(f.tab.grid(), (100, 30));
        f.resize(3, (92, 27), true);
        assert_eq!(f.owner.owner, Owner::Live(3), "a focused one takes");
        assert_eq!(f.tab.grid(), (92, 27));
        f.owner.commit(1);
        assert_eq!(
            f.owner.owner,
            Owner::Live(3),
            "a late commit is only a commit"
        );
    }

    // -- step 4-6: input, resize, take -----------------------------------

    #[test]
    fn the_owners_resize_applies_and_a_failure_keeps_the_old_grid() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.resize(1, (100, 30), false);
        assert_eq!(f.tab.grid(), (100, 30));
        f.tab.refuse.push((120, 40));
        f.resize(1, (120, 40), false);
        assert_eq!(f.tab.grid(), (100, 30));
        assert_eq!(f.owner(), Some("a".into()));
    }

    #[test]
    fn a_take_whose_apply_fails_changes_nothing() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.tab.refuse.push((100, 30));
        assert!(!f.take(2));
        assert_eq!(f.owner(), Some("a".into()));
        assert_eq!(f.tab.grid(), (80, 24));
    }

    #[test]
    fn take_size_takes_with_the_clients_newest_attachment() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.attached(3, "b", false, (110, 35));
        assert_eq!(f.owner.take_size("b", &mut f.tab), Ok(()));
        assert_eq!(f.owner.owner, Owner::Live(3));
        assert_eq!(f.tab.grid(), (110, 35));
        assert_eq!(
            f.owner.take_size("z", &mut f.tab),
            Err(TakeRefusal::NoAttachment)
        );
        f.tab.refuse.push((80, 24));
        assert_eq!(
            f.owner.take_size("a", &mut f.tab),
            Err(TakeRefusal::ApplyFailed)
        );
    }

    // -- step 7: handoff -----------------------------------------------

    #[test]
    fn a_deliberate_detach_hands_off_to_the_latest_advisory_grid() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (90, 25));
        f.attached(3, "c", false, (100, 30));
        f.resize(2, (95, 26), false);
        f.detach(1, true);
        assert_eq!(f.owner(), Some("b".into()), "b's resize is the latest");
        assert_eq!(f.tab.grid(), (95, 26));
    }

    #[test]
    fn equal_timestamps_go_to_the_earliest_attachment() {
        let mut f = Fixture::with(true);
        f.attached(1, "a", true, (80, 24));
        f.attached(3, "c", false, (100, 30));
        f.attached(2, "b", false, (90, 25));
        f.resize(2, (95, 26), false);
        f.detach(1, true);
        assert_eq!(
            f.owner(),
            Some("c".into()),
            "frozen stamps: earliest attach wins"
        );
    }

    #[test]
    fn the_leaving_owners_own_client_comes_first() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(3, "b", false, (100, 30));
        f.attached(2, "a", false, (85, 24));
        assert_eq!(f.owner.owner, Owner::Live(2), "a's newer attachment owns");
        f.resize(3, (101, 31), false);
        f.detach(2, true);
        assert_eq!(
            f.owner.owner,
            Owner::Live(1),
            "a's other attachment, though b's grid is the latest"
        );
        assert_eq!(f.tab.grid(), (80, 24));
    }

    #[test]
    fn a_handoff_passes_over_a_candidate_whose_grid_will_not_apply() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (90, 25));
        f.attached(3, "c", false, (100, 30));
        f.tab.refuse.push((100, 30));
        f.detach(1, true);
        assert_eq!(f.owner(), Some("b".into()));
        f.tab.refuse.push((90, 25));
        f.detach(2, true);
        assert_eq!(f.owner(), None, "nobody left whose grid applies");
        assert_eq!(f.tab.grid(), (90, 25), "the size is kept");
    }

    #[test]
    fn no_candidate_leaves_the_tab_unowned_at_its_size() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.detach(1, true);
        assert_eq!(f.owner(), None);
        assert_eq!(f.tab.grid(), (100, 30));
    }

    // -- step 8: disconnect and grace ------------------------------------

    #[test]
    fn an_owners_disconnect_keeps_the_tab_owned_through_its_grace() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.tab.applied.clear();
        f.detach(1, false);
        assert_eq!(f.owner(), Some("a".into()), "owned under its label");
        assert!(f.tab.applied.is_empty(), "nothing resizes");
        assert_eq!(f.owner.graced_client(), Some(("a", "a-label")));
        f.expire();
        assert_eq!(f.owner(), Some("b".into()));
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn a_non_owners_disconnect_just_removes_it() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.detach(2, false);
        assert_eq!(f.owner.grace_deadline(), None);
        f.detach(1, true);
        assert_eq!(f.owner(), None);
    }

    #[test]
    fn a_superseded_owners_late_disconnect_arms_nothing() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        assert!(f.take(2));
        f.detach(1, false);
        assert_eq!(f.owner.grace_deadline(), None);
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_reclaim_before_expiry_keeps_ownership_without_a_reflow() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        f.tab.applied.clear();
        assert!(f.attached(3, "a", false, (100, 30)));
        assert_eq!(f.owner.owner, Owner::Live(3));
        assert!(
            f.tab.applied.iter().all(|grid| *grid == (100, 30)),
            "no reflow"
        );
        assert_eq!(f.owner.grace_deadline(), None);
    }

    #[test]
    fn an_expiry_before_the_reclaim_hands_off_and_the_reclaim_watches() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        f.expire();
        assert_eq!(f.owner(), Some("b".into()));
        assert!(!f.attached(3, "a", false, (100, 30)));
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_stale_expiry_acts_on_nothing() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        let (_, first) = f.owner.grace_deadline().unwrap();
        f.attached(3, "a", false, (100, 30));
        f.detach(3, false);
        let (_, second) = f.owner.grace_deadline().unwrap();
        assert_ne!(first, second, "a new grace is a new incarnation");
        f.owner.expire(first, &mut f.tab);
        assert_eq!(f.owner(), Some("a".into()), "the old incarnation's timer");
        f.owner.expire(second, &mut f.tab);
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_take_during_grace_replaces_it_and_the_old_client_comes_back_watching() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        assert!(f.take(2));
        assert_eq!(f.owner.grace_deadline(), None);
        assert_eq!(f.owner.graced_client(), None);
        assert!(!f.attached(3, "a", false, (100, 30)));
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_failed_provisional_reclaim_restores_the_grace_as_it_was() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        let armed = f.owner.grace_deadline();
        f.attach(3, "a", false, (110, 35));
        assert_eq!(
            f.owner.grace_deadline(),
            None,
            "the reclaim owns provisionally"
        );
        assert_eq!(
            f.owner.graced_client(),
            Some(("a", "a-label")),
            "and the record is kept"
        );
        f.detach(3, false);
        assert_eq!(
            f.owner.grace_deadline(),
            armed,
            "same deadline, same incarnation"
        );
        assert_eq!(f.tab.grid(), (100, 30), "the grace's grid applies again");
        f.expire();
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_cancelled_reclaim_after_the_deadline_hands_off() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "b", false, (80, 24));
        f.detach(1, false);
        f.attach(3, "a", false, (110, 35));
        f.tick(GRACE + 1);
        f.detach(3, false);
        assert_eq!(f.owner(), Some("b".into()));
    }

    #[test]
    fn a_disconnecting_owner_with_another_attachment_hands_to_it_at_once() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "a", false, (90, 25));
        f.attached(3, "b", false, (80, 24));
        assert_eq!(f.owner.owner, Owner::Live(2));
        f.detach(2, false);
        assert_eq!(f.owner.owner, Owner::Live(1));
        assert_eq!(
            f.owner.grace_deadline(),
            None,
            "no grace with a sibling left"
        );
    }

    #[test]
    fn a_disconnecting_owner_whose_sibling_cannot_apply_hands_to_another_client() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (90, 25));
        f.attached(3, "a", false, (100, 30));
        assert_eq!(f.owner.owner, Owner::Live(3));
        f.tab.refuse.push((80, 24));
        f.detach(3, false);
        assert_eq!(f.owner(), Some("b".into()), "the handoff goes on to b");
        assert_eq!(f.tab.grid(), (90, 25));
        assert_eq!(
            f.owner.grace_deadline(),
            None,
            "a client with another candidate gets no grace"
        );
    }

    #[test]
    fn a_handoff_tries_each_candidate_once() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (90, 25));
        f.attached(3, "a", false, (100, 30));
        f.resize(1, (81, 24), false);
        f.tab.fail_next = true;
        f.detach(3, true);
        assert_eq!(
            f.owner(),
            Some("b".into()),
            "a's sibling failed first and is not tried again for its newer grid"
        );
        assert_eq!(f.tab.grid(), (90, 25));
    }

    // -- rule 6: release -------------------------------------------------

    #[test]
    fn release_hands_off_to_another_clients_watcher() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.owner.release("a", &mut f.tab);
        assert_eq!(f.owner(), Some("b".into()));
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn release_with_only_the_owner_attached_leaves_the_tab_unowned() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.owner.release("a", &mut f.tab);
        assert_eq!(f.owner(), None);
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn release_never_hands_back_to_the_releasing_client() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (100, 30));
        f.attached(2, "a", false, (90, 25));
        f.owner.release("a", &mut f.tab);
        assert_eq!(f.owner(), None, "both of a's attachments are excluded");
        f.attached(3, "b", false, (80, 24));
        assert_eq!(f.owner(), Some("b".into()), "an unowned tab is taken");
        f.detach(3, true);
        assert_eq!(f.owner(), None, "still excluded until a takes again");
        assert!(f.take(2));
        f.attached(4, "c", false, (70, 20));
        f.detach(2, true);
        assert!(
            f.owner.is_owner(1),
            "a took again, so its other attachment is a candidate"
        );
    }

    #[test]
    fn a_released_clients_next_attachment_is_no_candidate_either() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.owner.release("a", &mut f.tab);
        assert_eq!(f.owner(), Some("b".into()));
        assert!(!f.attached(3, "a", false, (90, 25)), "a watches");
        f.detach(2, true);
        assert_eq!(f.owner(), None, "a has not taken since its release");
        assert_eq!(f.tab.grid(), (100, 30));
    }

    #[test]
    fn release_from_a_non_owner_is_a_no_op() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.owner.release("b", &mut f.tab);
        assert_eq!(f.owner(), Some("a".into()));
        f.detach(1, true);
        assert_eq!(
            f.owner(),
            Some("b".into()),
            "a no-op release excludes nobody"
        );
    }

    #[test]
    fn release_during_grace_hands_off_now() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attached(2, "b", false, (100, 30));
        f.detach(1, false);
        f.owner.release("a", &mut f.tab);
        assert_eq!(f.owner(), Some("b".into()));
        assert_eq!(f.owner.grace_deadline(), None);
    }

    #[test]
    fn release_of_a_provisional_owner_hands_off() {
        let mut f = Fixture::new();
        f.attached(1, "a", true, (80, 24));
        f.attach(2, "b", true, (100, 30));
        f.owner.release("b", &mut f.tab);
        assert_eq!(f.owner(), Some("a".into()));
        f.owner.commit(2);
        assert_eq!(f.owner(), Some("a".into()));
    }

    // -- announcements -----------------------------------------------------

    #[test]
    fn every_change_of_grid_or_owner_is_announced_once() {
        let mut f = Fixture::new();
        assert_eq!(f.owner.announce(f.tab.grid()), None, "nothing changed yet");
        f.attached(1, "a", false, (80, 24));
        assert_eq!(f.owner.announce(f.tab.grid()), Some(1), "an owner change");
        assert_eq!(f.owner.announce(f.tab.grid()), None);
        f.resize(1, (100, 30), false);
        assert_eq!(f.owner.announce(f.tab.grid()), Some(2), "a grid change");
        f.attached(2, "a", false, (100, 30));
        assert_eq!(
            f.owner.announce(f.tab.grid()),
            None,
            "the same client at the same grid is not a change on the wire"
        );
        f.detach(1, false);
        f.detach(2, false);
        assert_eq!(f.owner.graced_client(), Some(("a", "a-label")));
        assert_eq!(
            f.owner.announce(f.tab.grid()),
            None,
            "a grace record names the owner it replaced"
        );
        assert_eq!(f.owner.grid_gen(), 2);
    }
}
