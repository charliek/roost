//! Moving a paste or a drop onto a host tab (plan 047 §3.2 + §3.3).
//!
//! Three pieces, in the order a gesture meets them:
//!
//! * [`route`] — what a [`Target`] does with a set of dropped paths
//!   before anything is read. A local tab pastes today's bytes and a
//!   refused target refuses, both with no filesystem access at all; only
//!   a host tab asks for [`inspect`].
//! * [`inspect`] — the blocking-pool half: dedupe, `metadata`, and a
//!   [`Candidate`] per path. It is the only thing here that touches a
//!   disk, and it never runs for a target that has already refused.
//! * [`Gestures`] — the per-host FIFO that runs a plan's uploads one at
//!   a time and pastes what came back.
//!
//! [`Gestures`] is a state machine, not a driver: its inputs are
//! [`Gestures::begin_inspection`], [`Gestures::begin`],
//! [`Gestures::inspected`] and [`Gestures::settled`], and its outputs are
//! [`Effect`]s the `App` executes, the `ClipboardQueue`/`ScreenshotQueue`
//! shape one module over. Everything that needs the `App` — whether the
//! origin tab is still there, whether its host reconnected, whether the
//! frame froze — arrives as a [`TransferFacts`] value computed by the
//! adapter, so every rule below is testable with values in hand.
//!
//! The policy itself lives in `roost_ui_model::file_transfer`, which is
//! mirrored into Swift; nothing here re-decides what that module decided.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use roost_ui_model::file_transfer::{
    self as policy, status, Candidate, Item, ItemSource, Kind, Plan, Refusal, Skipped, Source,
    Target,
};
use roost_ui_model::keys::{HostId, TabKey};

use super::interactions::deliver_paste_image;
use crate::app::UiTask;
use crate::host_conn::{HostOpError, UploadResult, UploadSource};

/// Where a gesture's terminal outcome goes when something is waiting for
/// it. Native drops and clipboard images carry `None`; C6's
/// `tab.send_file` is the caller that waits.
pub(super) type GestureReply = tokio::sync::oneshot::Sender<GestureOutcome>;

/// How many gestures one host's lane will hold — the active one plus
/// everything queued behind it, pending slots included. Matches the
/// upload lane's own depth: past that a drop is a mistake, not a plan.
const MAX_QUEUED_GESTURES: usize = 8;

/// How a gesture ended. Exactly one of these reaches a [`GestureReply`],
/// on every path out — including the queue being dropped with gestures
/// still in it.
// C6's `tab.send_file` is the caller that reads these; until then the
// queue's own tests are what pin them.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug)]
pub(super) enum GestureOutcome {
    Pasted {
        text: String,
        uploads: Vec<Sent>,
        skipped: Vec<Skipped>,
    },
    /// The plan refused before any byte moved.
    Refused(Refusal),
    /// Upload `name` failed; nothing was pasted (§3.3's all-or-nothing).
    Failed { name: String, error: HostOpError },
    /// The uploads landed but the paste could not: the world moved under
    /// the gesture. Typed rather than stringified so C6 can map it onto
    /// the error precedence in §3.4.
    Lost(LostReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum LostReason {
    /// The host's incarnation is no longer the one the gesture started
    /// on. §3.3: the client cannot tell a link blip from a restarted
    /// session, so it refuses either way.
    Reconnected,
    /// Still this tab's own incarnation, but no longer connected —
    /// `HostConnSet::apply_state` keeps the incarnation across a drop, so
    /// this is the case the incarnation check cannot see.
    Disconnected,
    /// The frame froze mid-upload — the third of issue #376's three
    /// moments. Carries the sentence the other two answer with.
    Frozen(&'static str),
    TabClosed,
    /// The host's lane was already holding [`MAX_QUEUED_GESTURES`].
    QueueFull,
    /// The gesture's paths never came back from the blocking pool.
    Inspection,
    /// The queue was dropped with this gesture still in it.
    Shutdown,
}

/// One file that reached the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Sent {
    pub(super) source: SentSource,
    pub(super) name: String,
    /// The host path `session.put_file` answered with, already validated
    /// by the upload lane (§3.1).
    pub(super) path: String,
    pub(super) bytes: u64,
}

/// Where a [`Sent`] file came from on this machine — a dropped path, or
/// a clipboard image that never touched the disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SentSource {
    Path(PathBuf),
    ClipboardPng,
}

/// What the `App` must do next. Ordered: a [`Effect::Paste`] precedes
/// the status line that describes it.
#[derive(Debug)]
pub(super) enum Effect {
    Status(String),
    StartUpload {
        host: HostId,
        gesture: u64,
        index: usize,
        name: String,
        source: UploadSource,
    },
    Paste {
        tab: TabKey,
        text: String,
    },
}

/// The re-checks §3.3 owes the paste, answered by the `App` and handed
/// to the state machine as a value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PasteGate {
    Ready,
    TabClosed,
    /// This tab's own incarnation, but nothing is serving it any more.
    Disconnected,
    Reconnected,
    /// The frame is frozen; the payload is `FrozenFrame::paste_refusal`.
    Frozen(&'static str),
}

/// Everything the `App` knows about one tab that the pure rules below
/// need, read once per question so a target and a gate never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TransferFacts {
    is_local: bool,
    /// The origin tab is still one worth pasting into: it has a terminal,
    /// and — if it is local — the workspace still has it.
    tab_live: bool,
    /// `FrozenFrame::paste_refusal` for the frame this tab is showing.
    frozen: Option<&'static str>,
    connected: bool,
    /// The incarnation currently serving this tab's saved host, which
    /// must be the tab's own: a reconnect mints a fresh [`HostId`], so a
    /// tab keyed on the old one is showing a frame nothing is serving.
    live: Option<HostId>,
    /// The incarnation the tab itself is keyed on.
    tab_host: HostId,
}

/// What [`route`] decided, given only the target.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Route {
    /// A host tab: these paths need [`inspect`] before the planner can
    /// say anything about them.
    Inspect(Vec<PathBuf>),
    /// Decided with no filesystem access whatsoever.
    PasteText(String),
    Refuse(Refusal),
}

/// Decide what a set of dropped paths does, before anything is read.
///
/// The split matters: §3.2 refuses a frozen or unavailable target
/// "before anything is read", and a local drop has never stat'ed a
/// thing — so [`Route::Inspect`] is the single door to the filesystem
/// and only [`Target::Host`] opens it.
pub(super) fn route(target: Target, paths: Vec<PathBuf>) -> Route {
    if target == Target::Host {
        return Route::Inspect(paths);
    }
    // The planner checks the target before it looks at a source, so the
    // two refusing targets need no candidates fabricated for them.
    let source = match target {
        Target::Local => Source::Files(uninspected(paths)),
        _ => Source::Files(Vec::new()),
    };
    match policy::plan(source, target) {
        Plan::PasteText(text) => Route::PasteText(text),
        Plan::Refuse(refusal) => Route::Refuse(refusal),
        // `Source::Files` off a host target yields only those two.
        plan => {
            tracing::debug!(
                ?plan,
                ?target,
                "ignored an unreachable plan for dropped files"
            );
            Route::Refuse(Refusal::Empty)
        }
    }
}

/// Candidates whose `Kind` is a placeholder. Legal because the planner
/// reads `Kind` only for [`Target::Host`], which never gets here.
fn uninspected(paths: Vec<PathBuf>) -> Vec<Candidate> {
    paths
        .into_iter()
        .map(|path| Candidate {
            path,
            kind: Kind::Other,
        })
        .collect()
}

/// The one place that states the judgement a host gesture's plan is
/// either an upload or a refusal: `Target::Host` never yields a local
/// paste, whatever the source.
fn upload_plan(plan: Plan) -> Result<(Vec<Item>, Vec<Skipped>), Refusal> {
    match plan {
        Plan::Upload { items, skipped } => Ok((items, skipped)),
        Plan::Refuse(refusal) => Err(refusal),
        plan => {
            tracing::debug!(
                ?plan,
                "ignored an unreachable local plan for a host gesture"
            );
            Err(Refusal::Empty)
        }
    }
}

/// Dedupe first-seen, then `metadata` each path (following symlinks).
///
/// BLOCKING: `metadata` hits the filesystem, so this runs on the
/// blocking pool — `UiTask::InspectFiles`, never the winit thread.
pub(crate) fn inspect(paths: Vec<PathBuf>) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .map(|path| {
            let kind = kind_of(&path);
            Candidate { path, kind }
        })
        .collect()
}

fn kind_of(path: &Path) -> Kind {
    match std::fs::metadata(path) {
        // `is_file` excludes FIFOs, devices and procfs entries, whose
        // `len()` lies — §3.2 makes `Regular` the only uploadable kind
        // for exactly that reason.
        Ok(meta) if meta.is_file() => Kind::Regular { len: meta.len() },
        Ok(meta) if meta.is_dir() => Kind::Directory,
        Ok(_) => Kind::Other,
        Err(error) => match error.kind() {
            std::io::ErrorKind::NotFound => Kind::Missing,
            std::io::ErrorKind::PermissionDenied => Kind::Unreadable,
            _ => Kind::Other,
        },
    }
}

/// Which [`Target`] a tab is.
pub(super) fn target_of(facts: &TransferFacts) -> Target {
    if facts.is_local {
        return Target::Local;
    }
    if facts.frozen.is_some() {
        return Target::Frozen;
    }
    if facts.connected && facts.live == Some(facts.tab_host) {
        Target::Host
    } else {
        Target::Unavailable
    }
}

/// The re-checks, in the order §3.3 lists them — except that a frozen
/// frame answers before the incarnation question, because its refusal is
/// the specific one and the other two paste routes (#376) already answer
/// with that exact sentence. A *reconnect* cannot hide behind it: the
/// caller resolves `frozen` against the gesture's own incarnation, which
/// a reconnect no longer has a view for.
///
/// `connected` is asked separately from the incarnation because
/// `HostConnSet::apply_state` keeps an incarnation across a drop: a
/// disconnected host is still `Some(tab_host)`, and pasting into it
/// would write down a data connection nothing is reading.
pub(super) fn paste_gate(facts: &TransferFacts) -> PasteGate {
    if !facts.tab_live {
        return PasteGate::TabClosed;
    }
    if let Some(refusal) = facts.frozen {
        return PasteGate::Frozen(refusal);
    }
    let ours = facts.live == Some(facts.tab_host);
    if ours && !facts.connected {
        return PasteGate::Disconnected;
    }
    if !ours {
        return PasteGate::Reconnected;
    }
    PasteGate::Ready
}

/// Answer a refusal: hand the outcome to whoever is waiting, and say
/// what the status line should be (`None` = log it and stay quiet).
///
/// `frozen` is `FrozenFrame::paste_refusal` for the frame that refused,
/// which only the `App` can name.
pub(super) fn refuse(
    refusal: Refusal,
    label: &str,
    frozen: Option<&'static str>,
    reply: Option<GestureReply>,
) -> Option<String> {
    let status = match &refusal {
        Refusal::Frozen => frozen.map(str::to_string),
        Refusal::Unavailable => Some(HOST_UNAVAILABLE.to_string()),
        // Today's `FileDropDisposition::Invalid`: a drop with nothing
        // safe in it has always been silent.
        Refusal::Empty => None,
        Refusal::NothingUploadable(skipped) => Some(status::nothing_uploadable(label, skipped)),
        Refusal::GestureOverBudget { total } => Some(status::over_budget(*total)),
    };
    answer(reply, GestureOutcome::Refused(refusal));
    status
}

/// How the rest of the app words a host that cannot take work
/// (`app.rs`'s reorder, close and open refusals).
pub(super) const HOST_UNAVAILABLE: &str = "that host is not accepting operations";

/// A2: a lane already at [`MAX_QUEUED_GESTURES`] refuses at admission
/// rather than growing without bound. Answers the reply and returns the
/// line the `App` should show.
fn queue_full(name: &str, label: &str, reply: Option<GestureReply>) -> String {
    answer(reply, GestureOutcome::Lost(LostReason::QueueFull));
    status::failed(name, label, "too many files are waiting for this host")
}

fn answer(reply: Option<GestureReply>, outcome: GestureOutcome) {
    if let Some(reply) = reply {
        // A dropped receiver is an ordinary outcome: a `roostctl` caller
        // that hung up does not stop the gesture.
        let _ = reply.send(outcome);
    }
}

/// One file waiting its turn inside a gesture.
#[derive(Debug)]
pub(super) struct Queued {
    name: String,
    bytes: u64,
    /// Taken when the upload starts. An item starts exactly once, and
    /// this is what makes that a fact rather than a convention — the
    /// clipboard image's bytes move onto the wire instead of being
    /// cloned onto it.
    source: Option<UploadSource>,
    origin: SentSource,
}

/// A planned gesture: its uploads in order, what it skipped, and where
/// its outcome goes.
#[derive(Debug)]
struct Gesture {
    id: u64,
    tab: TabKey,
    label: String,
    uploads: Vec<Queued>,
    skipped: Vec<Skipped>,
    reply: Option<GestureReply>,
}

/// A gesture whose uploads are running.
#[derive(Debug)]
struct Active {
    gesture: Gesture,
    /// Which upload is in flight. Every settle carries its own index, so
    /// an answer for anything else is stale and dropped.
    index: usize,
    sent: Vec<Sent>,
}

/// A gesture whose paths are out at [`inspect`], holding its FIFO place
/// meanwhile. The reply lives here, so an app that shuts down mid-
/// inspection still answers it.
#[derive(Debug)]
struct Pending {
    tab: TabKey,
    label: String,
    reply: Option<GestureReply>,
}

/// One place in a host's FIFO. A drop takes its place at admission and
/// turns [`Slot::Ready`] when its inspection lands, so a stalled
/// `metadata` blocks its lane instead of being overtaken by a later drop.
#[derive(Debug)]
enum Slot {
    Pending { id: u64, pending: Pending },
    Ready(Gesture),
}

impl Slot {
    fn reply(self) -> Option<GestureReply> {
        match self {
            Slot::Pending { pending, .. } => pending.reply,
            Slot::Ready(gesture) => gesture.reply,
        }
    }
}

#[derive(Debug, Default)]
struct Lane {
    active: Option<Active>,
    queued: VecDeque<Slot>,
}

impl Lane {
    fn depth(&self) -> usize {
        usize::from(self.active.is_some()) + self.queued.len()
    }

    /// The head, if it is a gesture that can start. A [`Slot::Pending`]
    /// head answers `None` — holding the lane is the point of it.
    fn take_ready_head(&mut self) -> Option<Gesture> {
        if !matches!(self.queued.front(), Some(Slot::Ready(_))) {
            return None;
        }
        match self.queued.pop_front() {
            Some(Slot::Ready(gesture)) => Some(gesture),
            _ => None,
        }
    }
}

/// One FIFO per host incarnation.
///
/// Within a gesture the uploads run one at a time, in order, because the
/// pasted order has to match the drop order. Between gestures on one
/// host the next starts only when the previous pasted or failed. Two
/// hosts share nothing but this map.
//
// A clipboard image and a drop racing each other onto the same host
// order by which one reached this queue; two input gestures ordering
// across each other is out of scope.
#[derive(Debug, Default)]
pub(super) struct Gestures {
    next_id: u64,
    lanes: HashMap<HostId, Lane>,
}

impl Gestures {
    fn allocate(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }

    fn has_room(&self, host: HostId) -> bool {
        self.lanes
            .get(&host)
            .is_none_or(|lane| lane.depth() < MAX_QUEUED_GESTURES)
    }

    /// Reserve this gesture's place in its host's FIFO while its paths
    /// are inspected off the UI thread.
    ///
    /// `Err` is a lane that is already full: it carries the status line
    /// and the reply has been answered.
    pub(super) fn begin_inspection(
        &mut self,
        tab: TabKey,
        label: String,
        reply: Option<GestureReply>,
    ) -> Result<u64, String> {
        if !self.has_room(tab.host) {
            return Err(queue_full("files", &label, reply));
        }
        let id = self.allocate();
        self.lanes
            .entry(tab.host)
            .or_default()
            .queued
            .push_back(Slot::Pending {
                id,
                pending: Pending { tab, label, reply },
            });
        Ok(id)
    }

    /// A gesture with nothing to inspect — the clipboard image, whose one
    /// item is already bytes — enters its lane ready to run.
    pub(super) fn begin(
        &mut self,
        tab: TabKey,
        label: String,
        uploads: Vec<Queued>,
        skipped: Vec<Skipped>,
        reply: Option<GestureReply>,
    ) -> Vec<Effect> {
        let Some(first) = uploads.first() else {
            // The planner refuses an empty upload, so this is a caller
            // bug rather than a state to carry.
            tracing::debug!(?tab, "ignored a gesture with nothing to upload");
            answer(reply, GestureOutcome::Refused(Refusal::Empty));
            return Vec::new();
        };
        if !self.has_room(tab.host) {
            let name = first.name.clone();
            return vec![Effect::Status(queue_full(&name, &label, reply))];
        }
        let id = self.allocate();
        self.lanes
            .entry(tab.host)
            .or_default()
            .queued
            .push_back(Slot::Ready(Gesture {
                id,
                tab,
                label,
                uploads,
                skipped,
                reply,
            }));
        self.start_head(tab.host)
    }

    /// The tab a pending slot is holding a place for, so the adapter can
    /// re-derive its target before the slot becomes a gesture.
    pub(super) fn pending_tab(&self, id: u64) -> Option<TabKey> {
        self.lanes.values().find_map(|lane| {
            lane.queued.iter().find_map(|slot| match slot {
                Slot::Pending { id: slot, pending } if *slot == id => Some(pending.tab),
                _ => None,
            })
        })
    }

    /// The pending slot `id`'s host and position, if it is still there.
    fn locate_pending(&self, id: u64) -> Option<(HostId, usize)> {
        self.lanes.iter().find_map(|(host, lane)| {
            lane.queued
                .iter()
                .position(|slot| matches!(slot, Slot::Pending { id: slot, .. } if *slot == id))
                .map(|index| (*host, index))
        })
    }

    /// Take the pending slot `id` out of its lane. The caller owns the
    /// answer from here, and must call [`Self::start_head`] after it.
    fn take_pending(&mut self, id: u64) -> Option<Pending> {
        let (host, index) = self.locate_pending(id)?;
        let lane = self.lanes.get_mut(&host)?;
        match lane.queued.remove(index) {
            Some(Slot::Pending { pending, .. }) => Some(pending),
            _ => None,
        }
    }

    /// The inspection landed: the slot it reserved becomes the gesture it
    /// was holding a place for, in place, and the lane's head runs if it
    /// can now.
    pub(super) fn inspected(
        &mut self,
        id: u64,
        uploads: Vec<Queued>,
        skipped: Vec<Skipped>,
    ) -> Vec<Effect> {
        let Some((host, index)) = self.locate_pending(id) else {
            tracing::debug!(id, "ignored an inspection for a slot that is gone");
            return Vec::new();
        };
        let Some(lane) = self.lanes.get_mut(&host) else {
            return Vec::new();
        };
        let Some(Slot::Pending { pending, .. }) = lane.queued.remove(index) else {
            return Vec::new();
        };
        let Pending { tab, label, reply } = pending;
        lane.queued.insert(
            index,
            Slot::Ready(Gesture {
                id,
                tab,
                label,
                uploads,
                skipped,
                reply,
            }),
        );
        self.start_head(host)
    }

    /// The inspection never came back — a stall past its budget, or a
    /// blocking task that did not join. The slot is answered and dropped
    /// so the lane moves on; a late answer for a slot that is already
    /// gone is ignored.
    pub(super) fn inspection_failed(&mut self, id: u64, message: &str) -> Vec<Effect> {
        let Some(pending) = self.take_pending(id) else {
            tracing::debug!(
                id,
                message,
                "ignored a failed inspection for a slot that is gone"
            );
            return Vec::new();
        };
        tracing::info!(tab = ?pending.tab, message, "a file inspection did not finish");
        let line = status::failed("files", &pending.label, message);
        answer(pending.reply, GestureOutcome::Lost(LostReason::Inspection));
        let mut effects = vec![Effect::Status(line)];
        effects.extend(self.start_head(pending.tab.host));
        effects
    }

    /// The origin tab of whatever is running on this host, so the `App`
    /// can compute a [`PasteGate`] for it before it settles an upload.
    pub(super) fn active_tab(&self, host: HostId, gesture: u64) -> Option<TabKey> {
        let active = self.lanes.get(&host)?.active.as_ref()?;
        (active.gesture.id == gesture).then_some(active.gesture.tab)
    }

    /// One upload answered.
    ///
    /// `gate` is only consulted when this settle finishes the gesture;
    /// it is passed by value rather than as a closure so the `App` can
    /// compute it without lending this queue a borrow of itself.
    pub(super) fn settled(
        &mut self,
        host: HostId,
        gesture: u64,
        index: usize,
        result: UploadResult,
        gate: PasteGate,
    ) -> Vec<Effect> {
        let Some(lane) = self.lanes.get_mut(&host) else {
            tracing::debug!(
                ?host,
                gesture,
                index,
                "ignored an upload for a lane that is gone"
            );
            return Vec::new();
        };
        let Some(active) = lane
            .active
            .as_mut()
            .filter(|active| active.gesture.id == gesture && active.index == index)
        else {
            tracing::debug!(?host, gesture, index, "ignored a stale upload answer");
            return Vec::new();
        };
        let failure = match result {
            Ok(landed) => {
                let queued = &active.gesture.uploads[index];
                active.sent.push(Sent {
                    source: queued.origin.clone(),
                    name: queued.name.clone(),
                    path: landed.path,
                    bytes: landed.bytes,
                });
                active.index += 1;
                let next = active.index;
                if next < active.gesture.uploads.len() {
                    return start(
                        host,
                        active.gesture.id,
                        next,
                        &mut active.gesture.uploads[next],
                        &active.gesture.label,
                    );
                }
                None
            }
            Err(error) => Some((active.gesture.uploads[index].name.clone(), error)),
        };
        let Some(finished) = lane.active.take() else {
            return Vec::new();
        };
        let mut effects = match failure {
            Some((name, error)) => finish_failed(finished, name, error),
            None => finish_pasted(finished, gate),
        };
        effects.extend(self.start_head(host));
        effects
    }

    fn start_head(&mut self, host: HostId) -> Vec<Effect> {
        let Some(lane) = self.lanes.get_mut(&host) else {
            return Vec::new();
        };
        if lane.active.is_some() {
            return Vec::new();
        }
        if let Some(mut gesture) = lane.take_ready_head() {
            let effects = start(
                host,
                gesture.id,
                0,
                gesture
                    .uploads
                    .first_mut()
                    .expect("a queued gesture has at least one upload"),
                &gesture.label,
            );
            lane.active = Some(Active {
                gesture,
                index: 0,
                sent: Vec::new(),
            });
            return effects;
        }
        // A pending head keeps its lane; an empty one is dropped so
        // `lanes` stays the set of hosts with work on them.
        if lane.queued.is_empty() {
            self.lanes.remove(&host);
        }
        Vec::new()
    }
}

impl Drop for Gestures {
    /// Nothing waiting on a gesture may wait forever, including at
    /// shutdown — §3.4's "so a `roostctl` caller can never hang".
    fn drop(&mut self) {
        for (_, mut lane) in self.lanes.drain() {
            if let Some(active) = lane.active.take() {
                answer(
                    active.gesture.reply,
                    GestureOutcome::Lost(LostReason::Shutdown),
                );
            }
            for slot in lane.queued.drain(..) {
                answer(slot.reply(), GestureOutcome::Lost(LostReason::Shutdown));
            }
        }
    }
}

fn start(
    host: HostId,
    gesture: u64,
    index: usize,
    queued: &mut Queued,
    label: &str,
) -> Vec<Effect> {
    let Some(source) = queued.source.take() else {
        tracing::debug!(
            ?host,
            gesture,
            index,
            "ignored a second start for one upload"
        );
        return Vec::new();
    };
    vec![
        Effect::Status(status::sending(&queued.name, queued.bytes, label)),
        Effect::StartUpload {
            host,
            gesture,
            index,
            name: queued.name.clone(),
            source,
        },
    ]
}

/// Every upload landed. Whether the paste does is the gate's answer.
fn finish_pasted(active: Active, gate: PasteGate) -> Vec<Effect> {
    let Active { gesture, sent, .. } = active;
    let Gesture {
        tab,
        label,
        skipped,
        reply,
        ..
    } = gesture;
    match gate {
        PasteGate::Ready => {
            let text = sent
                .iter()
                .map(|file| file.path.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            let names: Vec<String> = sent.iter().map(|file| file.name.clone()).collect();
            let line = status::sent(&names, &label, &skipped);
            answer(
                reply,
                GestureOutcome::Pasted {
                    text: text.clone(),
                    uploads: sent,
                    skipped,
                },
            );
            vec![Effect::Paste { tab, text }, Effect::Status(line)]
        }
        PasteGate::Reconnected => lost(
            &sent,
            &label,
            "the host reconnected",
            LostReason::Reconnected,
            reply,
        ),
        PasteGate::Disconnected => lost(
            &sent,
            &label,
            "the host disconnected",
            LostReason::Disconnected,
            reply,
        ),
        PasteGate::Frozen(refusal) => {
            answer(reply, GestureOutcome::Lost(LostReason::Frozen(refusal)));
            vec![Effect::Status(refusal.to_string())]
        }
        PasteGate::TabClosed => {
            tracing::debug!(?tab, "discarded a finished upload gesture for a closed tab");
            answer(reply, GestureOutcome::Lost(LostReason::TabClosed));
            Vec::new()
        }
    }
}

/// The uploads landed but the paste cannot: one sentence, naming one
/// file. One name for a batch, deliberately — the status table names a
/// file, and the first is the one the user pointed at.
fn lost(
    sent: &[Sent],
    label: &str,
    reason: &str,
    lost: LostReason,
    reply: Option<GestureReply>,
) -> Vec<Effect> {
    let name = sent.first().map(|file| file.name.as_str()).unwrap_or("");
    let line = status::failed(name, label, reason);
    answer(reply, GestureOutcome::Lost(lost));
    vec![Effect::Status(line)]
}

/// Upload `name` failed: nothing is pasted, the files already up there
/// stay (harmless, swept at stop), and the line names the file.
fn finish_failed(active: Active, name: String, error: HostOpError) -> Vec<Effect> {
    let Gesture { label, reply, .. } = active.gesture;
    let line = status::failed(&name, &label, &error.to_string());
    answer(reply, GestureOutcome::Failed { name, error });
    vec![Effect::Status(line)]
}

/// The inspected length of every regular file in a batch, which is what
/// the "Sending …" line reports.
fn inspected_sizes(candidates: &[Candidate]) -> HashMap<PathBuf, u64> {
    candidates
        .iter()
        .filter_map(|candidate| match candidate.kind {
            Kind::Regular { len } => Some((candidate.path.clone(), len)),
            _ => None,
        })
        .collect()
}

/// Turn a planner `Upload`'s items into the queue's own: `sizes` carries
/// the inspected lengths the status line wants and the planner's [`Item`]
/// deliberately does not, and `png` is the clipboard image's bytes for
/// the one item whose source is not a path.
fn queued_uploads(
    items: Vec<Item>,
    sizes: &HashMap<PathBuf, u64>,
    mut png: Option<Vec<u8>>,
) -> Vec<Queued> {
    items
        .into_iter()
        .map(|item| match item.source {
            ItemSource::Path(path) => Queued {
                name: item.name,
                bytes: sizes.get(&path).copied().unwrap_or_default(),
                source: Some(UploadSource::Path(path.clone())),
                origin: SentSource::Path(path),
            },
            // At most one item is ever the clipboard's, so taking the
            // bytes rather than cloning them is enough.
            ItemSource::ClipboardPng => {
                let bytes = png.take().unwrap_or_default();
                Queued {
                    name: item.name,
                    bytes: bytes.len() as u64,
                    source: Some(UploadSource::Bytes(bytes)),
                    origin: SentSource::ClipboardPng,
                }
            }
        })
        .collect()
}

/// Where a planned gesture goes: into the slot its inspection reserved,
/// or straight onto its lane.
enum Admission {
    Slot(u64),
    Direct {
        label: String,
        reply: Option<GestureReply>,
    },
}

// ── the `App` adapter ──
//
// Everything above is values; this is the thin layer that asks the app
// the questions the state machine cannot, and performs its effects.

impl super::App {
    /// THE entry point for files headed at a tab: native drops call it
    /// with `reply: None`, and C6's `tab.send_file` calls it with a
    /// waiting oneshot. A drop is the op minus the window event.
    pub(super) fn send_files(
        &mut self,
        tab: TabKey,
        paths: Vec<PathBuf>,
        reply: Option<GestureReply>,
    ) -> UiTask {
        let facts = self.transfer_facts(tab);
        let target = target_of(&facts);
        // A local tab keeps the route it has always had: no inspection,
        // no queue, and the paste before this function returns.
        if target == Target::Local {
            return self.paste_local_files(tab, paths, facts.tab_live, reply);
        }
        if !facts.tab_live {
            tracing::debug!(?tab, "discarded a file gesture for a closed tab");
            answer(reply, GestureOutcome::Lost(LostReason::TabClosed));
            return UiTask::None;
        }
        match route(target, paths) {
            Route::Inspect(paths) => {
                let label = self.transfer_host_label(tab.host);
                match self.gestures.begin_inspection(tab, label, reply) {
                    Ok(id) => UiTask::InspectFiles { id, paths },
                    Err(line) => {
                        self.set_status(line);
                        UiTask::None
                    }
                }
            }
            Route::Refuse(refusal) => {
                self.refuse_gesture(tab, refusal, reply);
                UiTask::None
            }
            Route::PasteText(text) => {
                tracing::debug!(?tab, text, "ignored a local paste route for a host tab");
                answer(reply, GestureOutcome::Refused(Refusal::Empty));
                UiTask::None
            }
        }
    }

    fn paste_local_files(
        &mut self,
        tab: TabKey,
        paths: Vec<PathBuf>,
        live: bool,
        reply: Option<GestureReply>,
    ) -> UiTask {
        if !live {
            tracing::debug!(?tab, "discarded a file gesture for a closed tab");
            answer(reply, GestureOutcome::Lost(LostReason::TabClosed));
            return UiTask::None;
        }
        let outcome = match route(Target::Local, paths) {
            Route::PasteText(text) => {
                deliver_paste_image(&self.tabs, tab, Some(&text));
                GestureOutcome::Pasted {
                    text,
                    uploads: Vec::new(),
                    skipped: Vec::new(),
                }
            }
            _ => {
                tracing::debug!(?tab, "ignored a file gesture with no safe local paths");
                GestureOutcome::Refused(Refusal::Empty)
            }
        };
        answer(reply, outcome);
        UiTask::None
    }

    /// The inspection came back. The target is re-derived rather than
    /// remembered: the host can have left `Connected` while the blocking
    /// pool was stat-ing, and then nothing may be sent.
    pub fn files_inspected(&mut self, id: u64, result: Result<Vec<Candidate>, String>) -> UiTask {
        let candidates = match result {
            Ok(candidates) => candidates,
            Err(message) => {
                let effects = self.gestures.inspection_failed(id, &message);
                return self.run_transfer_effects(effects);
            }
        };
        let Some(tab) = self.gestures.pending_tab(id) else {
            tracing::debug!(id, "ignored a stale file inspection");
            return UiTask::None;
        };
        let target = self.transfer_target(tab);
        let sizes = inspected_sizes(&candidates);
        let plan = policy::plan(Source::Files(candidates), target);
        self.start_plan(tab, plan, &sizes, None, Admission::Slot(id))
    }

    /// A host tab's clipboard image, already encoded and named on the
    /// blocking pool, and still only in memory (plan 047 §3.2: it never
    /// touches this machine's disk).
    pub(super) fn send_clipboard_png(&mut self, tab: TabKey, name: String, png: Vec<u8>) -> UiTask {
        let png_len = png.len() as u64;
        let target = self.transfer_target(tab);
        let label = self.transfer_host_label(tab.host);
        let plan = policy::plan(Source::ClipboardImage { name, png_len }, target);
        self.start_plan(
            tab,
            plan,
            &HashMap::new(),
            Some(png),
            Admission::Direct { label, reply: None },
        )
    }

    /// The one place a host plan becomes running uploads or a refusal.
    fn start_plan(
        &mut self,
        tab: TabKey,
        plan: Plan,
        sizes: &HashMap<PathBuf, u64>,
        png: Option<Vec<u8>>,
        admission: Admission,
    ) -> UiTask {
        match upload_plan(plan) {
            Ok((items, skipped)) => {
                let uploads = queued_uploads(items, sizes, png);
                let effects = match admission {
                    Admission::Slot(id) => self.gestures.inspected(id, uploads, skipped),
                    Admission::Direct { label, reply } => {
                        self.gestures.begin(tab, label, uploads, skipped, reply)
                    }
                };
                self.run_transfer_effects(effects)
            }
            Err(refusal) => match admission {
                Admission::Slot(id) => {
                    let reply = self.gestures.take_pending(id).and_then(|slot| slot.reply);
                    self.refuse_gesture(tab, refusal, reply);
                    let effects = self.gestures.start_head(tab.host);
                    self.run_transfer_effects(effects)
                }
                Admission::Direct { reply, .. } => {
                    self.refuse_gesture(tab, refusal, reply);
                    UiTask::None
                }
            },
        }
    }

    /// One upload answered — `Message::UploadSettled`.
    pub fn upload_settled(
        &mut self,
        host: HostId,
        gesture: u64,
        index: usize,
        result: UploadResult,
    ) -> UiTask {
        let gate = match self.gestures.active_tab(host, gesture) {
            Some(tab) => paste_gate(&self.transfer_facts(tab)),
            None => PasteGate::TabClosed,
        };
        let effects = self.gestures.settled(host, gesture, index, result, gate);
        self.run_transfer_effects(effects)
    }

    fn refuse_gesture(&mut self, tab: TabKey, refusal: Refusal, reply: Option<GestureReply>) {
        let label = self.transfer_host_label(tab.host);
        let frozen = self
            .frozen_host_frame_for(tab)
            .map(|frame| frame.paste_refusal());
        tracing::debug!(?tab, ?refusal, "refused a file gesture");
        if let Some(line) = refuse(refusal, &label, frozen, reply) {
            self.set_status(line);
        }
    }

    fn run_transfer_effects(&mut self, effects: Vec<Effect>) -> UiTask {
        let mut task = UiTask::None;
        for effect in effects {
            match effect {
                Effect::Status(line) => self.set_status(line),
                Effect::Paste { tab, text } => deliver_paste_image(&self.tabs, tab, Some(&text)),
                Effect::StartUpload {
                    host,
                    gesture,
                    index,
                    name,
                    source,
                } => task = task.then(self.upload_task(host, gesture, index, name, source)),
            }
        }
        task
    }

    fn upload_task(
        &self,
        host: HostId,
        gesture: u64,
        index: usize,
        name: String,
        source: UploadSource,
    ) -> UiTask {
        let future: crate::app::UploadFuture = match self.hosts.ops_for(host) {
            Some(ops) => Box::pin(ops.put_file(name, source)),
            // The lane died with its incarnation, so this answer is the
            // truthful one and it arrives now — §3.3's fail-fast, with
            // no timer of our own.
            None => Box::pin(std::future::ready(Err(HostOpError::Disconnected))),
        };
        UiTask::Upload {
            host,
            gesture,
            index,
            future,
        }
    }

    /// Every fact the pure rules need about one tab, read once.
    ///
    /// `connected` comes from the connection's own state rather than the
    /// reconciled view cache: the view is what the window is showing, and
    /// a gesture is about what the wire can carry.
    fn transfer_facts(&self, tab: TabKey) -> TransferFacts {
        let saved = self.host_view(tab.host).map(|view| view.saved_id.as_str());
        TransferFacts {
            is_local: tab.is_local(),
            tab_live: self.tabs.contains_key(&tab)
                && tab
                    .local_tab()
                    .is_none_or(|tab_id| self.workspace.tab(tab_id).is_ok()),
            frozen: self
                .frozen_host_frame_for(tab)
                .map(|frame| frame.paste_refusal()),
            connected: saved.is_some_and(|saved| {
                self.hosts
                    .state(saved)
                    .is_some_and(crate::host_conn::HostConnState::is_connected)
            }),
            live: saved.and_then(|saved| self.hosts.incarnation(saved)),
            tab_host: tab.host,
        }
    }

    pub(super) fn transfer_target(&self, tab: TabKey) -> Target {
        target_of(&self.transfer_facts(tab))
    }

    /// The label a status line names this host by. (`host_label` on
    /// `App` already means "by saved id"; this one is by incarnation.)
    fn transfer_host_label(&self, host: HostId) -> String {
        self.host_view(host)
            .map(|view| view.label.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use roost_ipc::messages::SessionPutFileResult;

    const HOST: HostId = HostId::LOCAL;

    fn host(id: u32) -> HostId {
        HostId::new(id)
    }

    fn tab(host: u32, id: i64) -> TabKey {
        TabKey::new(HostId::new(host), id)
    }

    fn facts(
        is_local: bool,
        tab_live: bool,
        frozen: Option<&'static str>,
        connected: bool,
        live: Option<HostId>,
        tab_host: HostId,
    ) -> TransferFacts {
        TransferFacts {
            is_local,
            tab_live,
            frozen,
            connected,
            live,
            tab_host,
        }
    }

    fn landed(path: &str, bytes: u64) -> UploadResult {
        Ok(SessionPutFileResult {
            path: path.to_string(),
            bytes,
        })
    }

    fn item(name: &str, path: &str) -> Item {
        Item {
            name: name.to_string(),
            source: ItemSource::Path(PathBuf::from(path)),
        }
    }

    fn regular(path: &str, len: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(path),
            kind: Kind::Regular { len },
        }
    }

    /// The uploads an inspected batch of `(name, path, len)` plans to.
    fn uploads(names: &[(&str, &str, u64)]) -> Vec<Queued> {
        let items: Vec<Item> = names
            .iter()
            .map(|(name, path, _)| item(name, path))
            .collect();
        let candidates: Vec<Candidate> = names
            .iter()
            .map(|(_, path, len)| regular(path, *len))
            .collect();
        queued_uploads(items, &inspected_sizes(&candidates), None)
    }

    fn statuses(effects: &[Effect]) -> Vec<String> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Status(line) => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    fn pastes(effects: &[Effect]) -> Vec<(TabKey, String)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Paste { tab, text } => Some((*tab, text.clone())),
                _ => None,
            })
            .collect()
    }

    fn started(effects: &[Effect]) -> Vec<(HostId, u64, usize, String)> {
        effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::StartUpload {
                    host,
                    gesture,
                    index,
                    name,
                    ..
                } => Some((*host, *gesture, *index, name.clone())),
                _ => None,
            })
            .collect()
    }

    /// A gesture, begun. Returns its id and the effects that started it.
    fn begun(
        gestures: &mut Gestures,
        key: TabKey,
        label: &str,
        names: &[(&str, &str, u64)],
        reply: Option<GestureReply>,
    ) -> (u64, Vec<Effect>) {
        let effects = gestures.begin(key, label.to_string(), uploads(names), Vec::new(), reply);
        (gestures.next_id, effects)
    }

    // -- route + inspection ------------------------------------------

    /// §3.2: a refused target answers "before anything is read". The one
    /// door to the filesystem is [`Route::Inspect`], and neither refusal
    /// opens it — the paths below are never stat'ed, so their real kind
    /// never reaches the plan.
    #[test]
    fn a_refused_target_never_reaches_the_inspection_step() {
        let paths = vec![PathBuf::from("/tmp/roost-does-not-exist-047")];
        assert_eq!(
            route(Target::Unavailable, paths.clone()),
            Route::Refuse(Refusal::Unavailable)
        );
        assert_eq!(
            route(Target::Frozen, paths.clone()),
            Route::Refuse(Refusal::Frozen)
        );
        assert_eq!(route(Target::Host, paths.clone()), Route::Inspect(paths));
    }

    /// A local drop still does no I/O at all: the same paths route
    /// straight to today's `drop_content` bytes.
    #[test]
    fn a_local_target_pastes_todays_bytes_without_inspecting() {
        assert_eq!(
            route(
                Target::Local,
                vec![
                    PathBuf::from("/tmp/My File.png"),
                    PathBuf::from("/tmp/My File.png"),
                    PathBuf::from("/tmp/second.png"),
                ]
            ),
            Route::PasteText("/tmp/My\\ File.png\n/tmp/second.png".to_string())
        );
    }

    #[test]
    fn inspection_dedupes_first_seen_and_classifies_by_metadata() {
        let dir = tempfile::tempdir().expect("temp dir");
        let file = dir.path().join("a.txt");
        std::fs::write(&file, b"1234").expect("write");
        let missing = dir.path().join("gone.txt");

        let candidates = inspect(vec![
            file.clone(),
            file.clone(),
            dir.path().to_path_buf(),
            missing.clone(),
        ]);
        assert_eq!(
            candidates,
            vec![
                Candidate {
                    path: file,
                    kind: Kind::Regular { len: 4 }
                },
                Candidate {
                    path: dir.path().to_path_buf(),
                    kind: Kind::Directory
                },
                Candidate {
                    path: missing,
                    kind: Kind::Missing
                },
            ]
        );
    }

    // -- target + gate -----------------------------------------------

    #[test]
    fn a_host_tab_is_only_a_host_target_while_its_own_incarnation_serves_it() {
        let live = host(4);
        assert_eq!(
            target_of(&facts(true, true, None, false, None, HostId::LOCAL)),
            Target::Local
        );
        assert_eq!(
            target_of(&facts(false, true, Some("frozen"), true, Some(live), live)),
            Target::Frozen
        );
        assert_eq!(
            target_of(&facts(false, true, None, true, Some(live), live)),
            Target::Host
        );
        assert_eq!(
            target_of(&facts(false, true, None, false, Some(live), live)),
            Target::Unavailable,
            "connected is not implied by having an incarnation"
        );
        assert_eq!(
            target_of(&facts(false, true, None, true, Some(host(5)), live)),
            Target::Unavailable,
            "a fresh incarnation is not the one this tab is keyed on"
        );
    }

    /// §3.3: a reconnect refuses whether or not the session restarted.
    /// The client cannot tell the two apart, and both reach the gate as
    /// "the live incarnation is not this gesture's".
    #[test]
    fn a_reconnect_refuses_whether_or_not_the_new_incarnation_is_connected() {
        let gesture_host = host(3);
        assert_eq!(
            paste_gate(&facts(
                false,
                true,
                None,
                true,
                Some(gesture_host),
                gesture_host
            )),
            PasteGate::Ready
        );
        assert_eq!(
            paste_gate(&facts(false, true, None, true, Some(host(4)), gesture_host)),
            PasteGate::Reconnected,
            "a link blip reconnected to a live incarnation"
        );
        assert_eq!(
            paste_gate(&facts(false, true, None, true, None, gesture_host)),
            PasteGate::Reconnected,
            "and a session that did not come back at all"
        );
        assert_eq!(
            paste_gate(&facts(
                false,
                false,
                None,
                true,
                Some(gesture_host),
                gesture_host
            )),
            PasteGate::TabClosed
        );
        assert_eq!(
            paste_gate(&facts(
                false,
                true,
                Some("frozen"),
                true,
                Some(gesture_host),
                gesture_host
            )),
            PasteGate::Frozen("frozen")
        );
    }

    /// `HostConnSet::apply_state` keeps the incarnation across a drop, so
    /// the incarnation check alone would let a paste land in a dead data
    /// connection. Connectedness is its own fact and its own refusal.
    #[test]
    fn a_disconnected_host_refuses_the_paste_on_its_own_incarnation() {
        let gesture_host = host(3);
        assert_eq!(
            paste_gate(&facts(
                false,
                true,
                None,
                false,
                Some(gesture_host),
                gesture_host
            )),
            PasteGate::Disconnected
        );

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            Some(tx),
        );
        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Disconnected,
        );
        assert!(pastes(&effects).is_empty(), "nothing reaches a dead link");
        assert_eq!(
            statuses(&effects),
            vec!["Could not send a.txt to workbox: the host disconnected"]
        );
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Lost(LostReason::Disconnected)
        ));
    }

    // -- the gesture queue -------------------------------------------

    #[test]
    fn a_gesture_uploads_then_pastes_the_returned_host_path() {
        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        let (id, effects) = begun(
            &mut gestures,
            key,
            "workbox",
            &[("shot.png", "/home/me/shot.png", 4 * 1024 * 1024)],
            None,
        );
        assert_eq!(
            statuses(&effects),
            vec!["Sending shot.png (4 MiB) to workbox…"]
        );
        assert_eq!(started(&effects), vec![(host(3), id, 0, "shot.png".into())]);

        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/home/me/.cache/roost-session/files/ab/shot.png", 4),
            PasteGate::Ready,
        );
        assert_eq!(
            pastes(&effects),
            vec![(
                key,
                "/home/me/.cache/roost-session/files/ab/shot.png".to_string()
            )],
            "the host path is pasted, never the local one"
        );
        assert_eq!(statuses(&effects), vec!["Sent shot.png to workbox"]);
    }

    /// Uploads inside a gesture are serial and ordered, and the paste is
    /// the returned paths newline-joined in that order.
    #[test]
    fn a_gestures_uploads_run_one_at_a_time_and_paste_in_order() {
        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        let (id, effects) = begun(
            &mut gestures,
            key,
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1), ("b.txt", "/tmp/b.txt", 2)],
            None,
        );
        assert_eq!(started(&effects), vec![(host(3), id, 0, "a.txt".into())]);

        let effects = gestures.settled(host(3), id, 0, landed("/files/a.txt", 1), PasteGate::Ready);
        assert!(pastes(&effects).is_empty(), "nothing pastes mid-gesture");
        assert_eq!(started(&effects), vec![(host(3), id, 1, "b.txt".into())]);

        let effects = gestures.settled(host(3), id, 1, landed("/files/b.txt", 2), PasteGate::Ready);
        assert_eq!(
            pastes(&effects),
            vec![(key, "/files/a.txt\n/files/b.txt".to_string())]
        );
        assert_eq!(statuses(&effects), vec!["Sent 2 files to workbox"]);
    }

    #[test]
    fn a_second_gesture_waits_for_the_first_and_two_hosts_do_not_block_each_other() {
        let mut gestures = Gestures::default();
        let (first, effects) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            None,
        );
        assert_eq!(started(&effects), vec![(host(3), first, 0, "a.txt".into())]);

        let (second, effects) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("b.txt", "/tmp/b.txt", 1)],
            None,
        );
        assert!(
            started(&effects).is_empty(),
            "the second gesture waits for the first"
        );

        // A different host runs at once, on its own lane.
        let (other, effects) = begun(
            &mut gestures,
            tab(9, 4),
            "otherbox",
            &[("c.txt", "/tmp/c.txt", 1)],
            None,
        );
        assert_eq!(started(&effects), vec![(host(9), other, 0, "c.txt".into())]);

        let effects = gestures.settled(
            host(3),
            first,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Ready,
        );
        assert_eq!(
            started(&effects),
            vec![(host(3), second, 0, "b.txt".into())],
            "the next gesture starts only once the previous one pasted"
        );
    }

    /// A1: a drop takes its FIFO place at admission, not when its
    /// `metadata` comes back — otherwise a slow drop is overtaken by a
    /// later one on the same host and the pastes land out of order.
    #[test]
    fn a_pending_inspection_holds_its_place_and_nothing_overtakes_it() {
        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        let first = gestures
            .begin_inspection(key, "workbox".into(), None)
            .expect("room on the lane");

        let effects = gestures.begin(
            key,
            "workbox".into(),
            uploads(&[("b.txt", "/tmp/b.txt", 1)]),
            Vec::new(),
            None,
        );
        assert!(
            started(&effects).is_empty(),
            "a pending head blocks its own lane"
        );

        let effects = gestures.inspected(first, uploads(&[("a.txt", "/tmp/a.txt", 1)]), Vec::new());
        assert_eq!(
            started(&effects),
            vec![(host(3), first, 0, "a.txt".into())],
            "the slot that was reserved first runs first"
        );

        let effects = gestures.settled(
            host(3),
            first,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Ready,
        );
        let next = started(&effects);
        let [(_, _, _, name)] = next.as_slice() else {
            panic!("the gesture behind the inspection runs next")
        };
        assert_eq!(name, "b.txt");
    }

    /// A2: a lane holds the active gesture plus seven behind it, and the
    /// ninth is refused at admission — once, whichever door it came in.
    #[test]
    fn a_full_lane_refuses_at_admission_and_answers_exactly_once() {
        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        for index in 0..MAX_QUEUED_GESTURES {
            let effects = gestures.begin(
                key,
                "workbox".into(),
                uploads(&[("a.txt", "/tmp/a.txt", 1)]),
                Vec::new(),
                None,
            );
            assert_eq!(
                started(&effects).len(),
                usize::from(index == 0),
                "only the head of a lane runs"
            );
        }

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let effects = gestures.begin(
            key,
            "workbox".into(),
            uploads(&[("late.txt", "/tmp/late.txt", 1)]),
            Vec::new(),
            Some(tx),
        );
        assert!(started(&effects).is_empty());
        assert_eq!(
            statuses(&effects),
            vec!["Could not send late.txt to workbox: too many files are waiting for this host"]
        );
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Lost(LostReason::QueueFull)
        ));
        assert!(rx.try_recv().is_err(), "and only once");

        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let line = gestures
            .begin_inspection(key, "workbox".into(), Some(tx))
            .expect_err("the lane is full");
        assert_eq!(
            line,
            "Could not send files to workbox: too many files are waiting for this host"
        );
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Lost(LostReason::QueueFull)
        ));
    }

    /// A5: a `metadata` that never comes back cannot hold its reply — or
    /// its lane — forever.
    #[test]
    fn a_stalled_inspection_answers_its_slot_and_releases_the_lane() {
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        let stalled = gestures
            .begin_inspection(key, "workbox".into(), Some(tx))
            .expect("room on the lane");
        gestures.begin(
            key,
            "workbox".into(),
            uploads(&[("b.txt", "/tmp/b.txt", 1)]),
            Vec::new(),
            None,
        );

        let effects =
            gestures.inspection_failed(stalled, "inspecting the dropped files took too long");
        assert_eq!(
            statuses(&effects),
            vec![
                "Could not send files to workbox: inspecting the dropped files took too long"
                    .to_string(),
                "Sending b.txt (0 MiB) to workbox…".to_string(),
            ]
        );
        let next = started(&effects);
        let [(_, _, _, name)] = next.as_slice() else {
            panic!("the lane moves on")
        };
        assert_eq!(name, "b.txt");
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Lost(LostReason::Inspection)
        ));
        assert!(rx.try_recv().is_err(), "and only once");

        // A late answer for a slot that is already gone changes nothing.
        assert!(gestures.inspection_failed(stalled, "too late").is_empty());
        assert!(gestures
            .inspected(stalled, uploads(&[("c.txt", "/tmp/c.txt", 1)]), Vec::new())
            .is_empty());
    }

    /// §3.3's all-or-nothing: a failure on item k pastes nothing, names
    /// the file, and lets the next gesture start.
    #[test]
    fn a_mid_gesture_failure_pastes_nothing_and_releases_the_lane() {
        let mut gestures = Gestures::default();
        let (first, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[
                ("a.txt", "/tmp/a.txt", 1),
                ("big.bin", "/tmp/big.bin", 2),
                ("c.txt", "/tmp/c.txt", 3),
            ],
            None,
        );
        let (second, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("d.txt", "/tmp/d.txt", 1)],
            None,
        );

        gestures.settled(
            host(3),
            first,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Ready,
        );
        let effects = gestures.settled(
            host(3),
            first,
            1,
            Err(HostOpError::Rejected {
                code: roost_ipc::client::ServerCode::TooLarge,
                message: "over the 10 MiB limit".into(),
            }),
            PasteGate::Ready,
        );
        assert!(pastes(&effects).is_empty(), "an all-or-nothing gesture");
        assert_eq!(
            statuses(&effects),
            vec![
                "Could not send big.bin to workbox: too-large: over the 10 MiB limit".to_string(),
                "Sending d.txt (0 MiB) to workbox…".to_string(),
            ]
        );
        assert_eq!(
            started(&effects),
            vec![(host(3), second, 0, "d.txt".into())]
        );
    }

    #[test]
    fn a_frame_that_freezes_mid_upload_pastes_nothing_and_says_so() {
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            None,
        );
        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Frozen("this session was taken over — reconnect to paste"),
        );
        assert!(pastes(&effects).is_empty());
        assert_eq!(
            statuses(&effects),
            vec!["this session was taken over — reconnect to paste"]
        );
    }

    #[test]
    fn a_reconnect_before_the_paste_names_the_file_and_pastes_nothing() {
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            None,
        );
        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/files/a.txt", 1),
            PasteGate::Reconnected,
        );
        assert!(pastes(&effects).is_empty());
        assert_eq!(
            statuses(&effects),
            vec!["Could not send a.txt to workbox: the host reconnected"]
        );
    }

    #[test]
    fn a_stale_answer_never_moves_a_gesture() {
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            None,
        );
        assert!(gestures
            .settled(host(3), id, 4, landed("/files/x", 1), PasteGate::Ready)
            .is_empty());
        assert!(gestures
            .settled(host(3), id + 99, 0, landed("/files/x", 1), PasteGate::Ready)
            .is_empty());
        assert!(gestures
            .settled(HOST, id, 0, landed("/files/x", 1), PasteGate::Ready)
            .is_empty());
        assert_eq!(gestures.active_tab(host(3), id), Some(tab(3, 7)));
    }

    // -- the reply, on every terminal path ---------------------------

    fn outcome(rx: &mut tokio::sync::oneshot::Receiver<GestureOutcome>) -> GestureOutcome {
        rx.try_recv().expect("the gesture answered exactly once")
    }

    #[test]
    fn every_terminal_path_answers_the_reply_exactly_once() {
        // Refusal.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let line = refuse(Refusal::Unavailable, "workbox", None, Some(tx));
        assert_eq!(line.as_deref(), Some(HOST_UNAVAILABLE));
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Refused(Refusal::Unavailable)
        ));
        assert!(rx.try_recv().is_err(), "and only once");

        // Failure.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            Some(tx),
        );
        gestures.settled(
            host(3),
            id,
            0,
            Err(HostOpError::Disconnected),
            PasteGate::Ready,
        );
        assert!(matches!(
            outcome(&mut rx),
            GestureOutcome::Failed {
                name,
                error: HostOpError::Disconnected,
            } if name == "a.txt"
        ));

        // Incarnation change, dead link, frozen, closed tab.
        for (gate, expected) in [
            (PasteGate::Reconnected, LostReason::Reconnected),
            (PasteGate::Disconnected, LostReason::Disconnected),
            (PasteGate::Frozen("gone"), LostReason::Frozen("gone")),
            (PasteGate::TabClosed, LostReason::TabClosed),
        ] {
            let (tx, mut rx) = tokio::sync::oneshot::channel();
            let mut gestures = Gestures::default();
            let (id, _) = begun(
                &mut gestures,
                tab(3, 7),
                "workbox",
                &[("a.txt", "/tmp/a.txt", 1)],
                Some(tx),
            );
            gestures.settled(host(3), id, 0, landed("/files/a.txt", 1), gate);
            assert!(
                matches!(outcome(&mut rx), GestureOutcome::Lost(reason) if reason == expected),
                "{gate:?} must answer {expected:?}"
            );
        }

        // Pasted.
        let (tx, mut rx) = tokio::sync::oneshot::channel();
        let mut gestures = Gestures::default();
        let (id, _) = begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            Some(tx),
        );
        gestures.settled(host(3), id, 0, landed("/files/a.txt", 1), PasteGate::Ready);
        let GestureOutcome::Pasted {
            text,
            uploads,
            skipped,
        } = outcome(&mut rx)
        else {
            panic!("a landed gesture pastes")
        };
        assert_eq!(text, "/files/a.txt");
        assert_eq!(
            uploads,
            vec![Sent {
                source: SentSource::Path(PathBuf::from("/tmp/a.txt")),
                name: "a.txt".into(),
                path: "/files/a.txt".into(),
                bytes: 1,
            }]
        );
        assert!(skipped.is_empty());
    }

    /// Dropping the queue — app shutdown — answers everything still in
    /// it: running, queued behind it, and out at inspection.
    #[test]
    fn dropping_the_queue_answers_every_gesture_it_still_holds() {
        let (running_tx, mut running) = tokio::sync::oneshot::channel();
        let (queued_tx, mut queued) = tokio::sync::oneshot::channel();
        let (inspecting_tx, mut inspecting) = tokio::sync::oneshot::channel();
        let mut gestures = Gestures::default();
        begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("a.txt", "/tmp/a.txt", 1)],
            Some(running_tx),
        );
        begun(
            &mut gestures,
            tab(3, 7),
            "workbox",
            &[("b.txt", "/tmp/b.txt", 1)],
            Some(queued_tx),
        );
        gestures
            .begin_inspection(tab(3, 7), "workbox".into(), Some(inspecting_tx))
            .expect("room on the lane");

        drop(gestures);
        for rx in [&mut running, &mut queued, &mut inspecting] {
            assert!(matches!(
                outcome(rx),
                GestureOutcome::Lost(LostReason::Shutdown)
            ));
        }
    }

    /// The refusal copy, over the pinned status table.
    #[test]
    fn refusals_say_what_happened() {
        assert_eq!(
            refuse(Refusal::Frozen, "workbox", Some("this session ended"), None).as_deref(),
            Some("this session ended")
        );
        assert_eq!(refuse(Refusal::Empty, "workbox", None, None), None);
        assert_eq!(
            refuse(
                Refusal::GestureOverBudget {
                    total: 540 * 1024 * 1024
                },
                "workbox",
                None,
                None
            )
            .as_deref(),
            Some("That drop is 540 MiB, over the 256 MiB per-drop limit")
        );
        assert_eq!(
            refuse(
                Refusal::NothingUploadable(vec![Skipped {
                    path: PathBuf::from("/tmp/build"),
                    reason: policy::SkipReason::Directory,
                }]),
                "workbox",
                None,
                None
            )
            .as_deref(),
            Some("Nothing to send to workbox (skipped: build/ is a directory)")
        );
    }

    /// S14's pin: a frozen host's clipboard image never reaches an
    /// upload. The planner refuses on the target alone, and `refuse`
    /// answers with the same sentence the other two paste routes do
    /// (#376) — so the deleted pre-check bought nothing.
    #[test]
    fn a_frozen_host_refuses_a_clipboard_image_with_the_paste_refusal_line() {
        const REFUSAL: &str = "this session was taken over — reconnect to paste";
        let frozen = facts(false, true, Some(REFUSAL), true, Some(host(3)), host(3));
        assert_eq!(target_of(&frozen), Target::Frozen);

        let plan = policy::plan(
            Source::ClipboardImage {
                name: "roost-image-1-0123456789abcdef.png".into(),
                png_len: 2048,
            },
            target_of(&frozen),
        );
        let refusal = upload_plan(plan).expect_err("a frozen target uploads nothing");
        assert_eq!(refusal, Refusal::Frozen);
        assert_eq!(
            refuse(refusal, "workbox", frozen.frozen, None).as_deref(),
            Some(REFUSAL)
        );
    }

    /// The paste effect, applied to a real host tab: what the terminal
    /// queues onto its input side is the *host's* path — bare, then
    /// bracketed once the program asks for it — and never the local one
    /// the file came from.
    #[test]
    fn the_paste_effect_reaches_a_host_tabs_input_as_the_host_path() {
        let (input_tx, _input_rx) = tokio::sync::mpsc::unbounded_channel();
        let (mut terminal, capture) =
            super::super::terminal_tab::attach_test_host_terminal(80, 24, input_tx);

        let mut gestures = Gestures::default();
        let key = tab(3, 7);
        let (id, _) = begun(
            &mut gestures,
            key,
            "workbox",
            &[("shot.png", "/home/me/Desktop/shot.png", 1)],
            None,
        );
        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/home/remote/.cache/roost-session/files/ab/shot.png", 1),
            PasteGate::Ready,
        );
        let landed_pastes = pastes(&effects);
        let [(pasted, text)] = landed_pastes.as_slice() else {
            panic!("a landed gesture pastes once")
        };
        assert_eq!(*pasted, key);
        terminal.paste(Some(text));
        assert_eq!(
            capture.lock().unwrap().as_slice(),
            b"/home/remote/.cache/roost-session/files/ab/shot.png"
        );

        capture.lock().unwrap().clear();
        terminal.write_vt(b"\x1b[?2004h");
        terminal.paste(Some(text));
        assert_eq!(
            capture.lock().unwrap().as_slice(),
            b"\x1b[200~/home/remote/.cache/roost-session/files/ab/shot.png\x1b[201~"
        );
    }

    /// A clipboard image is one item whose bytes ride the gesture — it
    /// never touches this machine's disk, so there is no candidate to
    /// take a size from, and the bytes are moved onto the wire rather
    /// than copied onto it.
    #[test]
    fn a_clipboard_image_gesture_uploads_its_bytes_under_the_minted_name() {
        let mut gestures = Gestures::default();
        let planned = queued_uploads(
            vec![Item {
                name: "roost-image-1-0123456789abcdef.png".into(),
                source: ItemSource::ClipboardPng,
            }],
            &HashMap::new(),
            Some(vec![0u8; 2048]),
        );
        let effects = gestures.begin(tab(3, 7), "workbox".into(), planned, Vec::new(), None);
        let id = gestures.next_id;
        let Some(Effect::StartUpload { source, name, .. }) = effects
            .iter()
            .find(|effect| matches!(effect, Effect::StartUpload { .. }))
        else {
            panic!("the clipboard image must start an upload")
        };
        assert_eq!(name, "roost-image-1-0123456789abcdef.png");
        assert_eq!(*source, UploadSource::Bytes(vec![0u8; 2048]));

        let effects = gestures.settled(
            host(3),
            id,
            0,
            landed("/files/roost-image-1-0123456789abcdef.png", 2048),
            PasteGate::Ready,
        );
        assert_eq!(
            pastes(&effects),
            vec![(
                tab(3, 7),
                "/files/roost-image-1-0123456789abcdef.png".to_string()
            )]
        );
    }
}
