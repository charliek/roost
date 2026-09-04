//! The three host modals: **Add Host**, the **Stop Session**
//! confirmation (plan 037 §3.1), and the **upgrade prompt** a build
//! mismatch raises (§3.7).
//!
//! One field on `App` holds any of them, because they are the same kind
//! of thing: a question the user still owes an answer to, drawn over the
//! chrome, owning the pointer and the keyboard while it is up. The
//! delete-project confirmation next door has the same shape and the
//! same panel style — Stop deliberately reuses its copy structure so
//! the two destructive confirmations read alike.
//!
//! Add Host is the one flow in the whole feature that needs free text,
//! which is why it is a dialog rather than palette steps. Its validation
//! is split in two on purpose: what can be answered instantly (an empty
//! field, a label the registry would refuse, a target string that cannot
//! mean anything) is answered instantly, and only a draft that passes
//! spends a round trip on `session.identify` — over a socket or over
//! `ssh`, depending on what the target turned out to be.
//!
//! Add Host is also the one modal with a **Tab ring** (plan 044 §3.4):
//! Name → Target → Add & Connect → Cancel. The decisions the ring makes
//! are pure functions here and unit-tested below.
//!
//! The ring is the app's own because iced 0.14 cannot focus a button, and
//! that is also its hazard: the *pointer* can move widget focus with
//! nothing to tell the app, so both Tab and a pointer press ask the
//! widget tree who really holds the caret before the ring is believed.
//!
//! What is **not** covered by any test in this repo is the toolkit seam
//! the ring is bolted to — `find_focused_or_none` actually walking a live
//! widget tree, `unfocus` actually taking the caret out of a field, the
//! ring actually being drawn around a button, and the ordering that
//! Fix A rests on (a press reaches the widget tree before the
//! subscription's copy of it reaches us). Nothing here builds a widget
//! tree, so those are verified on the running app instead.

use iced::advanced::widget::Id;
use iced::keyboard::key::Named;
use iced::keyboard::{self, Key};

use crate::host_conn::ConnectFailure;

/// Where Add Host's Tab traversal currently sits (plan 044 §3.4).
///
/// A ring the app owns rather than the toolkit's own traversal: iced
/// 0.14's `button` is not `Focusable`, so `focus_next()` can only ever
/// walk the two text inputs and could never reach either button.
///
/// The order is a web form's — the fields in reading order, then the
/// primary action, then the dismiss. Cancel *renders* left of "Add &
/// Connect" and still comes after it here, because the ring follows the
/// form's meaning rather than the row's geometry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum AddHostFocus {
    #[default]
    Name,
    Target,
    Confirm,
    Cancel,
}

impl AddHostFocus {
    /// The ring in order, so the traversal and the tests read from one
    /// list rather than two hand-written `match`es that could drift.
    const RING: [Self; 4] = [Self::Name, Self::Target, Self::Confirm, Self::Cancel];
}

/// Which of a modal's two buttons draws the Tab ring.
///
/// Both false is a real state, not "no ring": it is what the row looks
/// like while the caret is in a field. The *absence* of a ring entirely
/// — the three modals that have no traversal — is `None` one level up,
/// because the wrapper reserves its space whether or not it draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ButtonRing {
    pub(super) cancel: bool,
    pub(super) confirm: bool,
}

/// What a key press means to the Add Host dialog once the ring has been
/// consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DialogAction {
    Submit,
    Cancel,
    Nothing,
}

/// The Add Host dialog's live contents.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(super) struct AddHostDraft {
    pub(super) name: String,
    pub(super) socket: String,
    /// Shown under the fields. Both halves of validation land here — a
    /// label the registry refuses and a socket that would not answer are
    /// the same thing to the user: a reason this cannot be saved yet.
    pub(super) error: Option<String>,
    /// The dial in flight, if any. The buttons go inert rather than
    /// disappearing, so the dialog does not resize under the pointer.
    ///
    /// A **generation**, not a flag, and private so nothing can compare
    /// it by hand: a reply has to prove it belongs to the submit that is
    /// still waiting. Field equality cannot prove that — cancel, reopen,
    /// and resubmit the same name and socket, and the first dial's
    /// answer would satisfy the second dialog. It is the same staleness
    /// contract `HostId` minting uses one layer down.
    verifying: Option<u64>,
    /// Where Tab traversal sits. Authoritative for Enter and Space —
    /// which reach the app only when no input has widget focus — and a
    /// *hint* for Tab, which asks the widget tree first because a mouse
    /// click can move focus without telling anyone (`text_input` has no
    /// focus callback).
    focus: AddHostFocus,
    /// A focus probe is out for this draft. Guards the gap between the
    /// Tab press and the widget tree's answer: without it two quick Tabs
    /// both read the pre-step ring and collapse into one step.
    tab_step_pending: bool,
    /// Tabs pressed while that probe was out, in order, each carrying its
    /// own direction.
    ///
    /// Queued rather than dropped: winit delivers a batch of events to
    /// one `update` pass, and the widget operation behind the probe is
    /// not drained until after it — so two Tabs in one batch are BOTH
    /// processed before any answer lands. Injected input routinely
    /// batches that way (this repo's known input-coalescing hazard), so
    /// dropping the second would make an e2e case that sends Tab three
    /// times land one stop short and read as a product bug.
    tab_step_queue: Vec<bool>,
}

impl AddHostDraft {
    /// Whether a dial is in flight — what the buttons and the
    /// "Connecting…" label read.
    pub(super) fn is_verifying(&self) -> bool {
        self.verifying.is_some()
    }

    /// The primary button's label. Inert while a dial is in flight
    /// rather than hidden — a button that vanishes mid-press moves the
    /// card under the pointer — so the label is what says so.
    pub(super) fn confirm_label(&self) -> &'static str {
        if self.is_verifying() {
            "Connecting…"
        } else {
            "Add & Connect"
        }
    }

    /// Arm a fresh dial. The error goes with it: the draft is being
    /// asked about again, so whatever was wrong last time is no longer
    /// what the dialog is saying.
    pub(super) fn begin_verify(&mut self, generation: u64) {
        self.verifying = Some(generation);
        self.error = None;
    }

    /// Claim the reply for `generation`, if it is the one still awaited.
    ///
    /// `false` for anything else, and the draft is left exactly as it
    /// was — a reply to a submit this draft has already moved past must
    /// not clear a *newer* dial's in-flight state.
    pub(super) fn claim_verify(&mut self, generation: u64) -> bool {
        if self.verifying != Some(generation) {
            return false;
        }
        self.verifying = None;
        true
    }

    /// A field changed.
    ///
    /// The error described the draft as it was; typing is the user
    /// answering it. So did any dial in flight — its answer is about a
    /// draft that no longer exists, so it is cancelled here rather than
    /// left to hold the buttons inert on "Connecting…" forever, waiting
    /// on a reply that will be dropped when it lands.
    pub(super) fn edited(&mut self) {
        self.error = None;
        self.verifying = None;
    }

    pub(super) fn ring(&self) -> AddHostFocus {
        self.focus
    }

    pub(super) fn set_ring(&mut self, focus: AddHostFocus) {
        self.focus = focus;
    }

    /// Claim the right to probe the widget tree for this Tab.
    ///
    /// `false` while an earlier probe is still out — the direction is
    /// queued instead, and the answer resolves every press at once.
    pub(super) fn begin_tab_step(&mut self, backwards: bool) -> bool {
        if self.tab_step_pending {
            self.tab_step_queue.push(backwards);
            return false;
        }
        self.tab_step_pending = true;
        true
    }

    /// Claim a probe's answer, if this draft is the one still waiting,
    /// together with every Tab that arrived behind it.
    ///
    /// A dialog cancelled and reopened while a probe was in flight gets a
    /// fresh draft that is waiting for nothing, so the old answer cannot
    /// step it.
    pub(super) fn claim_tab_step(&mut self) -> Option<Vec<bool>> {
        if !std::mem::take(&mut self.tab_step_pending) {
            return None;
        }
        Some(std::mem::take(&mut self.tab_step_queue))
    }

    /// Abandon an outstanding probe and anything queued behind it.
    ///
    /// For a pointer press that sets the ring outright: the answer in
    /// flight describes the tree as it was *before* that press, and
    /// stepping from it would walk off the stop the user just chose.
    /// Deliberately NOT folded into `set_ring` — the two `*_changed`
    /// handlers set the ring too, and a Tab followed by a typed character
    /// before the answer lands is correct as it stands (the character
    /// goes to the field that had the caret, and focus then moves on).
    pub(super) fn invalidate_tab_step(&mut self) {
        self.tab_step_pending = false;
        self.tab_step_queue.clear();
    }

    /// Which button the ring is drawn around, if either.
    ///
    /// An inert primary is not a stop: while a dial is in flight the
    /// button renders disabled and cannot be activated by pointer or
    /// key, so ringing it would advertise a dead control. `step` skips
    /// it for the same reason; this is the other half of the one rule.
    pub(super) fn button_ring(&self) -> ButtonRing {
        let ring = self.focus;
        ButtonRing {
            cancel: ring == AddHostFocus::Cancel,
            confirm: ring == AddHostFocus::Confirm && !self.is_verifying(),
        }
    }
}

/// Which modal is up, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum HostDialog {
    Add(AddHostDraft),
    /// Stop the session on a host. Carries the label as well as the id
    /// because the copy names it, and re-reading the registry at draw
    /// time would show a rename the user has not seen.
    ConfirmStop {
        saved_id: String,
        label: String,
    },
    /// The upgrade prompt (plan 037 §3.7): this host's session was
    /// started by a Roost this client cannot talk to.
    ///
    /// The composed copy is carried rather than the raw mismatch, for
    /// the same reason `ConfirmStop` carries its label: the dialog says
    /// what was true when it opened, and a reconnect attempt landing
    /// underneath it must not rewrite the question mid-read.
    ConfirmRestart {
        saved_id: String,
        prompt: super::host_notice::RestartPrompt,
    },
    /// Consent to install, update or start `roost-session` on a host
    /// reached over ssh (plan 039 §3.5).
    ///
    /// The fourth member of the family rather than a surface of its own,
    /// because it is the same kind of question the other three are: one
    /// the user still owes an answer to, over chrome that keeps working
    /// underneath. It carries a snapshot for `ConfirmStop`'s reason —
    /// the card describes the far side as a *read-only* probe found it,
    /// and nothing has been touched over there yet.
    Bootstrap(super::bootstrap::BootstrapDraft),
}

impl HostDialog {
    pub(super) fn add() -> Self {
        Self::Add(AddHostDraft::default())
    }

    pub(super) fn draft_mut(&mut self) -> Option<&mut AddHostDraft> {
        match self {
            Self::Add(draft) => Some(draft),
            Self::ConfirmStop { .. } | Self::ConfirmRestart { .. } | Self::Bootstrap(_) => None,
        }
    }
}

/// Which ring stop the *widget tree* says the dialog is on.
///
/// A focused input always wins: a pointer press into a field moves widget
/// focus with nothing to tell the app about it, so the stored ring can be
/// stale and the tree cannot.
///
/// Anything else — nothing focused at all, or a focusable widget outside
/// this dialog holding the caret — falls back to the stored ring, which
/// is the last stop we actually knew. Nothing focused is what a ringed
/// button looks like (buttons are not `Focusable` in iced 0.14), and when
/// the caret was simply taken away (a press on the card's body text,
/// an `unfocus`) the last stop is still where the user was. Re-entering
/// the ring from its edge instead would let a *forward* Tab move
/// backwards, which reads as a bug however well documented the rule is.
pub(super) fn current_stop(
    found: Option<&Id>,
    name_id: &Id,
    target_id: &Id,
    ring: AddHostFocus,
) -> AddHostFocus {
    match found {
        Some(id) if id == name_id => AddHostFocus::Name,
        Some(id) if id == target_id => AddHostFocus::Target,
        _ => ring,
    }
}

/// Advance the ring one stop, skipping any stop that cannot be activated.
///
/// `confirm_is_inert` is the dial in flight: the primary renders disabled
/// and refuses both pointer and key, so Tab must pass over it the way
/// every other toolkit passes over a disabled control. Stepping *off* an
/// inert Confirm still works — the skip is on where a step lands, not on
/// where it starts.
pub(super) fn step(stop: AddHostFocus, backwards: bool, confirm_is_inert: bool) -> AddHostFocus {
    let ring = AddHostFocus::RING;
    let mut at = ring
        .iter()
        .position(|entry| *entry == stop)
        .unwrap_or_default();
    loop {
        at = if backwards {
            (at + ring.len() - 1) % ring.len()
        } else {
            (at + 1) % ring.len()
        };
        let landed = ring[at];
        if !(confirm_is_inert && landed == AddHostFocus::Confirm) {
            return landed;
        }
    }
}

/// Step the ring on a focus probe's answer, guarded.
///
/// Returns the new stop, or `None` when the answer belongs to a dialog
/// that is gone: closed, replaced by one of the confirmations, cancelled
/// and reopened while the probe was out, or overtaken by a pointer press
/// that set the ring outright.
///
/// Tabs that arrived while the probe was out are resolved here too,
/// stepping on from the answer — one probe settles the whole batch,
/// because after the first step the ring is known and nothing between two
/// presses in one batch could have moved the caret.
pub(super) fn resolve_tab_step(
    dialog: Option<&mut HostDialog>,
    found: Option<&Id>,
    name_id: &Id,
    target_id: &Id,
    backwards: bool,
) -> Option<AddHostFocus> {
    let draft = dialog?.draft_mut()?;
    let queued = draft.claim_tab_step()?;
    let inert = draft.is_verifying();
    let mut next = step(
        current_stop(found, name_id, target_id, draft.ring()),
        backwards,
        inert,
    );
    for also_backwards in queued {
        next = step(next, also_backwards, inert);
    }
    draft.set_ring(next);
    Some(next)
}

/// Re-sync the ring to what the widget tree says, without stepping it.
///
/// The answer to a pointer press: a click into a field moves widget focus
/// and `on_input` only notices once a character is typed, so between the
/// two the accent ring can sit on Cancel while the caret is in Target —
/// and Enter, captured by the input, would then *submit* under an
/// affordance that says cancel.
///
/// Deliberately does not touch the Tab probe's pending flag: this is a
/// correction, not a traversal, and a Tab already in flight is still the
/// user's.
pub(super) fn resolve_focus_resync(
    dialog: Option<&mut HostDialog>,
    found: Option<&Id>,
    name_id: &Id,
    target_id: &Id,
) -> Option<AddHostFocus> {
    let draft = dialog?.draft_mut()?;
    let at = current_stop(found, name_id, target_id, draft.ring());
    draft.set_ring(at);
    Some(at)
}

/// Whether this press is a bare Tab or Shift+Tab, and which way it goes.
///
/// Shift picks the direction and is the only modifier the ring takes:
/// Ctrl+Tab and Alt+Tab belong to the window manager and the app's own
/// accelerators, and stepping the ring on them would move the focus ring
/// while the user alt-tabs away from the window entirely.
pub(super) fn tab_step_direction(event: &keyboard::Event) -> Option<bool> {
    let keyboard::Event::KeyPressed {
        key,
        repeat: false,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    if !matches!(key.as_ref(), Key::Named(Named::Tab)) || has_command_modifier(*modifiers) {
        return None;
    }
    Some(modifiers.shift())
}

/// Any modifier that makes a key mean something other than itself.
/// Shift is excluded on purpose — it shifts a character rather than
/// re-targeting the press.
fn has_command_modifier(modifiers: keyboard::Modifiers) -> bool {
    modifiers.control() || modifiers.alt() || modifiers.logo()
}

/// What Enter or Space means to the Add Host dialog.
///
/// The ring is authoritative here with no widget query, because a focused
/// `text_input` **captures** both (Enter has its own arm; Space is
/// printable) and the app's keyboard subscription forwards only ignored
/// events. So reaching this function is itself the proof that no input
/// holds focus.
///
/// `is_verifying` because the primary goes inert while a dial is in
/// flight — the view omits its `on_press` — and a key on an inert button
/// has to be the same no-op a click on it is.
pub(super) fn dialog_key_action(
    event: &keyboard::Event,
    ring: AddHostFocus,
    is_verifying: bool,
) -> DialogAction {
    let keyboard::Event::KeyPressed {
        key,
        repeat: false,
        modifiers,
        ..
    } = event
    else {
        return DialogAction::Nothing;
    };
    let submit = if is_verifying {
        DialogAction::Nothing
    } else {
        DialogAction::Submit
    };
    // Space is the button-activation key, so it takes no modifiers:
    // Ctrl/Alt/Super+Space are a compositor's input-switcher and the
    // app's own accelerator space, and dialing a host off one would be a
    // surprise. Enter's modifier-blindness is older than the ring and
    // deliberately left as it was.
    let bare = !has_command_modifier(*modifiers);
    match (key.as_ref(), ring) {
        (Key::Named(Named::Enter), AddHostFocus::Cancel) => DialogAction::Cancel,
        (Key::Named(Named::Enter), AddHostFocus::Confirm) => submit,
        // Blurred, or a ring the tree has not been asked about: Enter
        // keeps the meaning it had before there was a ring at all — the
        // dialog's primary action.
        (Key::Named(Named::Enter), _) => submit,
        (Key::Named(Named::Space), AddHostFocus::Cancel) if bare => DialogAction::Cancel,
        (Key::Named(Named::Space), AddHostFocus::Confirm) if bare => submit,
        _ => DialogAction::Nothing,
    }
}

/// A draft that is ready to dial: both fields trimmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostDraftTarget {
    pub(super) label: String,
    pub(super) target: String,
}

/// The instant half of Add Host's validation.
///
/// `label_check` is the registry's own rule, injected rather than called
/// so this stays a pure function: the adapter passes
/// `Workspace::check_host_label`, and a test passes whatever it wants to
/// prove about the ordering.
///
/// Field order is the reading order — a user who has filled in neither
/// field is told about the first one, not the second.
pub(super) fn validate_draft(
    draft: &AddHostDraft,
    label_check: impl FnOnce(&str) -> Result<(), String>,
) -> Result<HostDraftTarget, String> {
    let label = draft.name.trim();
    let target = draft.socket.trim();
    if label.is_empty() {
        return Err("a name is required".into());
    }
    label_check(label)?;
    // The classifier, not an is-empty check: it is the one place that
    // decides what a target string means, and its refusals are written
    // for exactly this reader — an empty field, a `-oProxyCommand=…`
    // that would reach `ssh` as a flag, a `host:22` that is ambiguous
    // with a socket path. Showing its message verbatim keeps the dialog
    // and `roostctl host add` saying the same thing about the same
    // string.
    roost_ipc::ssh::classify(target).map_err(|error| format!("{error:#}"))?;
    Ok(HostDraftTarget {
        label: label.to_string(),
        target: target.to_string(),
    })
}

/// Reach a prospective host and check it is a session this client can
/// talk to at all.
///
/// The check itself is [`roost_ipc::ssh::verify_transport`] — one bar
/// however the target is reached, and the same call `roostctl host add
/// --verify` makes, so the dialog and the CLI cannot drift apart about
/// what "verified" means. All this adds is the dialog's own error shape.
///
/// That shape is a [`ConnectFailure`] rather than a `String`: the
/// message under the fields is unchanged, but "Add & Connect" is one of
/// the two doors a bootstrap offer opens from (plan 039 §3.5), and
/// deciding *this was the not-found family* has to read the family — not
/// a substring of the sentence shown to the user.
///
/// The classify is a second one (`validate_draft` already ran it), and
/// deliberately so: this half runs off the main thread, on a target the
/// dialog only carries as a string.
pub(super) async fn verify_target(target: String) -> Result<(), ConnectFailure> {
    let transport = roost_ipc::ssh::classify(&target)
        .map_err(|error| ConnectFailure::unclassified(format!("{error:#}")))?;
    roost_ipc::ssh::verify_transport(&transport)
        .await
        .map(drop)
        .map_err(ConnectFailure::from)
}

#[cfg(test)]
mod tests {
    use iced::keyboard::key::{Code, Physical};
    use iced::keyboard::Location;

    use super::*;

    fn press(key: Key) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(Code::Enter),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat: false,
        }
    }

    fn modified(key: Key, modifiers: keyboard::Modifiers) -> keyboard::Event {
        let keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            text,
            repeat,
            ..
        } = press(key)
        else {
            unreachable!()
        };
        keyboard::Event::KeyPressed {
            key,
            modified_key,
            physical_key,
            location,
            modifiers,
            text,
            repeat,
        }
    }

    fn enter() -> keyboard::Event {
        press(Key::Named(Named::Enter))
    }

    fn space() -> keyboard::Event {
        press(Key::Named(Named::Space))
    }

    /// Walk the ring one direction and check it comes back to where it
    /// started — the traversal Charlie asked for, spelled out.
    #[test]
    fn tab_walks_name_target_confirm_cancel_and_wraps() {
        let mut at = AddHostFocus::Name;
        let mut walked = vec![at];
        for _ in 0..4 {
            at = step(at, false, false);
            walked.push(at);
        }
        assert_eq!(
            walked,
            vec![
                AddHostFocus::Name,
                AddHostFocus::Target,
                AddHostFocus::Confirm,
                AddHostFocus::Cancel,
                AddHostFocus::Name,
            ],
            "Cancel draws left of the primary and still follows it: the ring \
             is the form's order, not the row's geometry"
        );
    }

    #[test]
    fn shift_tab_walks_the_ring_backwards() {
        let mut at = AddHostFocus::Name;
        let mut walked = vec![at];
        for _ in 0..4 {
            at = step(at, true, false);
            walked.push(at);
        }
        assert_eq!(
            walked,
            vec![
                AddHostFocus::Name,
                AddHostFocus::Cancel,
                AddHostFocus::Confirm,
                AddHostFocus::Target,
                AddHostFocus::Name,
            ]
        );
    }

    /// A forward Tab must never move backwards. The caret can be taken
    /// away without the ring hearing (a press on the card's body text),
    /// and re-entering the ring at its first stop would send a forward
    /// Tab from Target back to Name.
    #[test]
    fn a_caret_taken_away_resumes_from_the_last_known_stop() {
        let (name, target) = (Id::unique(), Id::unique());
        for (ring, forward, backward) in [
            (
                AddHostFocus::Name,
                AddHostFocus::Target,
                AddHostFocus::Cancel,
            ),
            (
                AddHostFocus::Target,
                AddHostFocus::Confirm,
                AddHostFocus::Name,
            ),
        ] {
            let at = current_stop(None, &name, &target, ring);
            assert_eq!(at, ring, "nothing focused: the last stop we knew stands");
            assert_eq!(step(at, false, false), forward);
            assert_eq!(step(at, true, false), backward);
        }
    }

    /// The whole reason Tab asks the widget tree: a pointer press into a
    /// field moves focus with nothing to tell the app, so the stored ring
    /// can be stale and the tree cannot.
    #[test]
    fn a_focused_input_beats_a_stale_ring() {
        let (name, target) = (Id::unique(), Id::unique());
        assert_eq!(
            current_stop(Some(&target), &name, &target, AddHostFocus::Cancel),
            AddHostFocus::Target,
        );
        assert_eq!(
            step(
                current_stop(Some(&target), &name, &target, AddHostFocus::Cancel),
                false,
                false
            ),
            AddHostFocus::Confirm,
            "the recovered Tab goes on from the clicked field, not from the ring"
        );
    }

    /// Buttons are not focusable in iced 0.14, so "no widget has focus" is
    /// exactly what a ringed button looks like — the ring stands.
    #[test]
    fn nothing_focused_keeps_the_ring_wherever_it_was() {
        let (name, target) = (Id::unique(), Id::unique());
        for ring in [
            AddHostFocus::Name,
            AddHostFocus::Target,
            AddHostFocus::Confirm,
            AddHostFocus::Cancel,
        ] {
            assert_eq!(current_stop(None, &name, &target, ring), ring);
        }
    }

    /// A focusable widget outside this dialog holding the caret describes
    /// no ring stop, so the ring stays where it was rather than guessing.
    #[test]
    fn a_focused_widget_that_is_not_ours_leaves_the_ring_alone() {
        let (name, target, stranger) = (Id::unique(), Id::unique(), Id::unique());
        assert_eq!(
            current_stop(Some(&stranger), &name, &target, AddHostFocus::Target),
            AddHostFocus::Target
        );
    }

    /// While a dial is in flight the primary renders disabled and refuses
    /// pointer and key alike, so Tab passes over it the way every other
    /// toolkit passes over a disabled control — and the ring is not drawn
    /// on it either. Two halves of one rule: an inert stop is not a stop.
    #[test]
    fn an_inert_primary_is_skipped_by_tab_and_never_ringed() {
        assert_eq!(
            step(AddHostFocus::Target, false, true),
            AddHostFocus::Cancel
        );
        assert_eq!(step(AddHostFocus::Cancel, true, true), AddHostFocus::Target);
        // Stepping OFF an inert Confirm still works — the skip is on
        // where a step lands, not on where it starts. That case is real:
        // activating the primary is the usual way to be sitting on it.
        assert_eq!(
            step(AddHostFocus::Confirm, false, true),
            AddHostFocus::Cancel
        );
        assert_eq!(
            step(AddHostFocus::Confirm, true, true),
            AddHostFocus::Target
        );

        let mut draft = draft("pop-os", "/tmp/s.sock");
        draft.set_ring(AddHostFocus::Confirm);
        assert_eq!(
            draft.button_ring(),
            ButtonRing {
                cancel: false,
                confirm: true
            }
        );
        draft.begin_verify(1);
        assert_eq!(
            draft.button_ring(),
            ButtonRing::default(),
            "a dial in flight takes the ring off the button it disabled"
        );
        draft.set_ring(AddHostFocus::Cancel);
        assert_eq!(
            draft.button_ring(),
            ButtonRing {
                cancel: true,
                confirm: false
            },
            "Cancel stays live and ringed while the dial runs"
        );
    }

    /// Enter and Space reach the app only when no input has widget focus
    /// (a focused `text_input` captures both), so the ring answers them
    /// with no widget query at all.
    #[test]
    fn enter_and_space_activate_the_ringed_button() {
        for (ring, expected) in [
            (AddHostFocus::Confirm, DialogAction::Submit),
            (AddHostFocus::Cancel, DialogAction::Cancel),
        ] {
            assert_eq!(dialog_key_action(&enter(), ring, false), expected);
            assert_eq!(dialog_key_action(&space(), ring, false), expected);
        }
    }

    /// Space is the button-activation key and takes no modifiers: a
    /// compositor's input switcher and the app's own accelerator space
    /// both live on Ctrl/Alt/Super+Space, and dialing a host off one
    /// would be a surprise.
    #[test]
    fn a_modified_space_activates_nothing() {
        for modifiers in [
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
        ] {
            for ring in [AddHostFocus::Confirm, AddHostFocus::Cancel] {
                assert_eq!(
                    dialog_key_action(&modified(Key::Named(Named::Space), modifiers), ring, false),
                    DialogAction::Nothing,
                    "{modifiers:?}+Space must not activate {ring:?}"
                );
            }
        }
        assert_eq!(
            dialog_key_action(
                &modified(Key::Named(Named::Space), keyboard::Modifiers::SHIFT),
                AddHostFocus::Confirm,
                false
            ),
            DialogAction::Submit,
            "Shift only shifts a character; it does not re-target the press"
        );
    }

    /// Shift picks the direction and is the only modifier the ring takes.
    /// Ctrl+Tab and Alt+Tab belong to the window manager and the app's
    /// accelerators — stepping on them would move the ring while the user
    /// alt-tabs away from the window entirely.
    #[test]
    fn tab_takes_shift_and_no_other_modifier() {
        assert_eq!(
            tab_step_direction(&press(Key::Named(Named::Tab))),
            Some(false)
        );
        assert_eq!(
            tab_step_direction(&modified(
                Key::Named(Named::Tab),
                keyboard::Modifiers::SHIFT
            )),
            Some(true)
        );
        for modifiers in [
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        ] {
            assert_eq!(
                tab_step_direction(&modified(Key::Named(Named::Tab), modifiers)),
                None,
                "{modifiers:?}+Tab is not the ring's"
            );
        }
        assert_eq!(
            tab_step_direction(&enter()),
            None,
            "and only Tab steps the ring"
        );
    }

    /// Blurred: Enter keeps the meaning it had before there was a ring —
    /// the dialog's primary action — and Space, which only ever meant
    /// "press the focused button", means nothing.
    #[test]
    fn a_ring_on_a_field_submits_on_enter_and_ignores_space() {
        for ring in [AddHostFocus::Name, AddHostFocus::Target] {
            assert_eq!(
                dialog_key_action(&enter(), ring, false),
                DialogAction::Submit
            );
            assert_eq!(
                dialog_key_action(&space(), ring, false),
                DialogAction::Nothing
            );
        }
    }

    /// The primary goes inert while a dial is in flight — the view omits
    /// its `on_press` — so a key on it has to be the same no-op a click
    /// is. Cancel stays live: the dialog can always be dismissed.
    #[test]
    fn an_inert_primary_is_a_no_op_for_the_keyboard_too() {
        for key in [enter(), space()] {
            assert_eq!(
                dialog_key_action(&key, AddHostFocus::Confirm, true),
                DialogAction::Nothing
            );
            assert_eq!(
                dialog_key_action(&key, AddHostFocus::Cancel, true),
                DialogAction::Cancel
            );
        }
        assert_eq!(
            dialog_key_action(&enter(), AddHostFocus::Name, true),
            DialogAction::Nothing,
            "a ring on a field means Enter is the primary action, equally inert"
        );
    }

    /// `main` forwards a *captured* Enter release as its own message
    /// (`Message::CapturedEnterRelease`), which lands on the rename
    /// latch and never enters the keyboard route at all — so this
    /// function is unreachable from it. The release that *is* ignored
    /// (nothing focused) does reach here, and must not fire a second
    /// activation behind a field's `on_submit`.
    #[test]
    fn only_a_fresh_press_activates_anything() {
        let release = keyboard::Event::KeyReleased {
            modified_key: Key::Named(Named::Enter),
            key: Key::Named(Named::Enter),
            physical_key: Physical::Code(Code::Enter),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
        };
        assert_eq!(
            dialog_key_action(&release, AddHostFocus::Confirm, false),
            DialogAction::Nothing
        );
        assert_eq!(tab_step_direction(&release), None);

        let keyboard::Event::KeyPressed { key, .. } = enter() else {
            unreachable!()
        };
        let repeat = keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: Physical::Code(Code::Enter),
            location: Location::Standard,
            modifiers: keyboard::Modifiers::default(),
            text: None,
            repeat: true,
        };
        assert_eq!(
            dialog_key_action(&repeat, AddHostFocus::Cancel, false),
            DialogAction::Nothing,
            "a held key does not re-activate, as everywhere else in the chrome"
        );
    }

    /// Enter and Space produce the *same* action, which is exactly why the
    /// caller cannot treat the action alone as "an Enter happened": the
    /// rename-completion latch is cleared by its own key's release, so
    /// latching Enter for a Space activation strands it and the latch then
    /// eats the user's next Enter press. `App`'s route re-reads the key
    /// for that reason; this pins the ambiguity that makes it necessary.
    #[test]
    fn enter_and_space_are_indistinguishable_in_the_action_alone() {
        assert_eq!(
            dialog_key_action(&space(), AddHostFocus::Confirm, false),
            dialog_key_action(&enter(), AddHostFocus::Confirm, false),
        );
        assert_eq!(
            dialog_key_action(&space(), AddHostFocus::Cancel, false),
            dialog_key_action(&enter(), AddHostFocus::Cancel, false),
        );
    }

    /// A press on a button moves the ring onto it, so the drawn ring never
    /// stays on the stop the user just clicked away from.
    #[test]
    fn a_button_press_moves_the_ring_onto_that_button() {
        let mut draft = draft("pop-os", "/tmp/s.sock");
        assert_eq!(
            draft.ring(),
            AddHostFocus::Name,
            "a fresh dialog opens on Name"
        );
        draft.set_ring(AddHostFocus::Confirm);
        assert_eq!(
            step(draft.ring(), false, false),
            AddHostFocus::Cancel,
            "the Tab after a click on Add & Connect goes to Cancel, not Target"
        );
    }

    /// A pointer press re-syncs the ring to the tree WITHOUT stepping it.
    ///
    /// Without this, clicking into Target while the ring sits on Cancel
    /// leaves an accent ring around Cancel and the caret in Target — and
    /// Enter, captured by the input, would then submit under an
    /// affordance that says cancel.
    #[test]
    fn a_pointer_press_corrects_the_ring_without_advancing_it() {
        let (name, target) = (Id::unique(), Id::unique());
        let mut dialog = HostDialog::add();
        dialog.draft_mut().unwrap().set_ring(AddHostFocus::Cancel);

        assert_eq!(
            resolve_focus_resync(Some(&mut dialog), Some(&target), &name, &target),
            Some(AddHostFocus::Target),
        );
        let draft = dialog.draft_mut().unwrap();
        assert_eq!(draft.ring(), AddHostFocus::Target);
        assert_eq!(
            draft.button_ring(),
            ButtonRing::default(),
            "and no button is ringed while the caret is in a field"
        );

        // A press that lands on neither field (the card's body text, a
        // button) leaves the ring where it was — a correction, never a
        // step.
        assert_eq!(
            resolve_focus_resync(Some(&mut dialog), None, &name, &target),
            Some(AddHostFocus::Target),
        );
        assert_eq!(
            resolve_focus_resync(None, Some(&name), &name, &target),
            None,
            "and a press answered after the dialog closed is dropped"
        );
    }

    /// A press must not steal a Tab that is already in flight: the resync
    /// is a correction, not a traversal.
    #[test]
    fn a_resync_leaves_a_tab_already_in_flight_alone() {
        let (name, target) = (Id::unique(), Id::unique());
        let mut dialog = HostDialog::add();
        assert!(dialog.draft_mut().unwrap().begin_tab_step(false));
        let _ = resolve_focus_resync(Some(&mut dialog), Some(&name), &name, &target);
        assert_eq!(
            resolve_tab_step(Some(&mut dialog), Some(&name), &name, &target, false),
            Some(AddHostFocus::Target),
            "the Tab's own answer still lands"
        );
    }

    /// The probe is asynchronous, so the dialog it was asked about may be
    /// gone by the time the tree answers.
    #[test]
    fn a_focus_answer_for_a_dialog_that_is_gone_is_dropped() {
        let (name, target) = (Id::unique(), Id::unique());

        assert_eq!(resolve_tab_step(None, None, &name, &target, false), None);

        let mut other = HostDialog::ConfirmStop {
            saved_id: "h1".into(),
            label: "pop-os".into(),
        };
        assert_eq!(
            resolve_tab_step(Some(&mut other), None, &name, &target, false),
            None,
            "a confirmation replaced the Add card; it has no ring to step"
        );

        // Cancelled and reopened while the probe was out: the fresh draft
        // is waiting for nothing, so the old answer cannot step it.
        let mut reopened = HostDialog::add();
        assert_eq!(
            resolve_tab_step(Some(&mut reopened), None, &name, &target, false),
            None
        );
        assert_eq!(
            reopened.draft_mut().unwrap().ring(),
            AddHostFocus::Name,
            "and it is left exactly as it opened"
        );
    }

    /// A press on "Add & Connect" while a Tab is in flight invalidates
    /// it. Otherwise the probe answers `found = None` (the press
    /// unfocused the field), reads the ring the press just set to
    /// Confirm, steps, and lands on Cancel while the dial runs — so the
    /// user's next Enter, the reflex when the error text appears, would
    /// dismiss the dialog instead of retrying.
    #[test]
    fn a_confirm_press_invalidates_a_tab_already_in_flight() {
        let (name, target) = (Id::unique(), Id::unique());
        let mut dialog = HostDialog::add();
        let draft = dialog.draft_mut().unwrap();
        draft.set_ring(AddHostFocus::Target);
        assert!(draft.begin_tab_step(false));

        // The press: what `App::add_host_confirm_pressed` does.
        draft.invalidate_tab_step();
        draft.set_ring(AddHostFocus::Confirm);

        assert_eq!(
            resolve_tab_step(Some(&mut dialog), None, &name, &target, false),
            None,
            "the answer in flight described the tree before the press"
        );
        assert_eq!(
            dialog.draft_mut().unwrap().ring(),
            AddHostFocus::Confirm,
            "so the ring stays on the button the user actually pressed"
        );
    }

    /// Two Tabs inside one probe's round trip must move TWO stops.
    ///
    /// winit hands a batch of events to one `update` pass and the widget
    /// operation behind the probe is drained after it, so both presses
    /// are processed before any answer lands. Injected input batches that
    /// way routinely, so dropping the second would make a lane that sends
    /// three Tabs land on Confirm and read as a product bug.
    #[test]
    fn every_tab_in_one_batch_moves_a_stop() {
        let (name, target) = (Id::unique(), Id::unique());
        let mut dialog = HostDialog::add();
        let draft = dialog.draft_mut().unwrap();
        assert!(
            draft.begin_tab_step(false),
            "the first Tab issues the probe"
        );
        assert!(!draft.begin_tab_step(false), "the second queues behind it");
        assert!(!draft.begin_tab_step(false), "and so does the third");

        assert_eq!(
            resolve_tab_step(Some(&mut dialog), Some(&name), &name, &target, false),
            Some(AddHostFocus::Cancel),
            "three Tabs from Name land on Cancel, not on Target"
        );
        assert_eq!(
            resolve_tab_step(Some(&mut dialog), None, &name, &target, false),
            None,
            "and one answer settles the batch; a duplicate is not a second one"
        );

        // Directions are kept per press, so a Tab and a Shift+Tab in one
        // batch cancel out rather than both going forward.
        let draft = dialog.draft_mut().unwrap();
        draft.set_ring(AddHostFocus::Name);
        assert!(draft.begin_tab_step(false));
        assert!(!draft.begin_tab_step(true));
        assert_eq!(
            resolve_tab_step(Some(&mut dialog), Some(&name), &name, &target, false),
            Some(AddHostFocus::Name),
        );
    }

    /// The end-to-end shape of one press: probe out, tree answers, ring
    /// moves, and the stop says whether the caret goes to a field or
    /// leaves the fields entirely.
    #[test]
    fn a_full_step_moves_the_ring_and_says_where_the_caret_goes() {
        let (name, target) = (Id::unique(), Id::unique());
        let mut dialog = HostDialog::add();

        for (found, expected) in [
            (Some(name.clone()), AddHostFocus::Target),
            (Some(target.clone()), AddHostFocus::Confirm),
            (None, AddHostFocus::Cancel),
            (None, AddHostFocus::Name),
        ] {
            assert!(dialog.draft_mut().unwrap().begin_tab_step(false));
            assert_eq!(
                resolve_tab_step(Some(&mut dialog), found.as_ref(), &name, &target, false),
                Some(expected)
            );
            assert_eq!(dialog.draft_mut().unwrap().ring(), expected);
        }
    }

    fn draft(name: &str, socket: &str) -> AddHostDraft {
        AddHostDraft {
            name: name.into(),
            socket: socket.into(),
            ..AddHostDraft::default()
        }
    }

    fn accept(_: &str) -> Result<(), String> {
        Ok(())
    }

    #[test]
    fn a_complete_draft_is_trimmed_on_the_way_through() {
        let target = validate_draft(&draft("  pop-os \n", "\t/tmp/s.sock "), accept).unwrap();
        assert_eq!(target.label, "pop-os");
        assert_eq!(target.target, "/tmp/s.sock");
    }

    /// Whitespace is not a name. The registry trims before it validates
    /// too, so a draft of spaces has to fail here for the same reason —
    /// otherwise the dialog would dial a socket for a save that could
    /// never land.
    #[test]
    fn blank_fields_are_refused_before_anything_is_dialed() {
        assert_eq!(
            validate_draft(&draft("   ", "/tmp/s.sock"), accept),
            Err("a name is required".into())
        );
        assert_eq!(
            validate_draft(&draft("pop-os", "  "), accept),
            Err("target is empty".into())
        );
    }

    /// Every spelling the helper text offers is one the dialog accepts.
    #[test]
    fn ssh_targets_are_accepted_in_every_spelling_the_helper_names() {
        for target in [
            "workbox",
            "user@host",
            "ssh://host:2222",
            "ssh://[::1]:22",
            "localhost",
            "/tmp/s.sock",
            "./s.sock",
        ] {
            let checked = validate_draft(&draft("pop-os", target), accept)
                .unwrap_or_else(|error| panic!("{target:?} was refused: {error}"));
            assert_eq!(checked.target, target);
        }
    }

    /// The two refusals the classifier exists for, surfaced verbatim: a
    /// `host:22` is ambiguous with a socket path, and a leading `-`
    /// would reach `ssh` as a flag rather than a destination.
    #[test]
    fn the_classifiers_refusals_are_what_the_dialog_shows() {
        let error = validate_draft(&draft("pop-os", "host:22"), accept)
            .expect_err("host:22 must be refused");
        assert!(
            error.contains("ssh://host:port"),
            "{error} must name the spelling that works"
        );

        let error = validate_draft(&draft("pop-os", "-oProxyCommand=id"), accept)
            .expect_err("a leading dash must be refused");
        assert!(
            error.contains("looks like an option"),
            "{error} must say why a dash is not a host"
        );
    }

    /// The label rules are the registry's, surfaced verbatim: the dialog
    /// does not paraphrase "local is reserved" into wording that could
    /// drift from what `roostctl host add` prints.
    #[test]
    fn the_registrys_own_refusal_is_what_the_dialog_shows() {
        let refuse = |label: &str| Err(format!("host label \"{label}\" is already saved"));
        assert_eq!(
            validate_draft(&draft("pop-os", "/tmp/s.sock"), refuse),
            Err("host label \"pop-os\" is already saved".into())
        );
    }

    /// The label is checked before the target: a name the registry will
    /// never accept is the more useful thing to say first, and it is the
    /// half the user can fix without knowing a host or a path.
    #[test]
    fn the_label_is_checked_before_the_target() {
        let refuse = |_: &str| Err("reserved".to_string());
        assert_eq!(
            validate_draft(&draft("local", ""), refuse),
            Err("reserved".into()),
        );
    }

    /// Editing while a dial is in flight cancels it. Without this the
    /// dialog is wedged: the reply is dropped as stale, nothing clears
    /// `verifying`, and "Connecting…" stays up with submit disabled
    /// forever.
    #[test]
    fn editing_a_field_cancels_the_dial_that_was_describing_it() {
        let mut draft = draft("pop-os", "/tmp/s.sock");
        draft.begin_verify(1);
        assert!(draft.is_verifying());

        draft.socket = "/tmp/other.sock".into();
        draft.edited();
        assert!(
            !draft.is_verifying(),
            "the user can submit again immediately"
        );
        assert!(
            !draft.claim_verify(1),
            "and the cancelled dial's answer lands nowhere"
        );
    }

    /// Identity, not field equality. Cancel, reopen, and type the same
    /// name and socket: the first dial's reply must not satisfy the
    /// second dialog, which is asking a question of its own.
    #[test]
    fn a_reply_belongs_to_the_submit_that_asked_for_it() {
        let mut first = draft("pop-os", "/tmp/s.sock");
        first.begin_verify(1);

        // The dialog is cancelled and reopened with identical fields.
        let mut second = draft("pop-os", "/tmp/s.sock");
        second.begin_verify(2);
        assert_eq!(first.name, second.name);
        assert_eq!(first.socket, second.socket);

        assert!(
            !second.claim_verify(1),
            "the previous dialog's answer is not this dialog's"
        );
        assert!(second.is_verifying(), "and it did not clear the live dial");
        assert!(second.claim_verify(2));
        assert!(!second.is_verifying());
        assert!(
            !second.claim_verify(2),
            "an answer is claimed once; a duplicate is not a second one"
        );
    }

    /// A fresh dialog awaits nothing, so no reply in flight from a
    /// previous one can be claimed by it.
    #[test]
    fn a_freshly_opened_dialog_claims_nothing() {
        let mut fresh = AddHostDraft::default();
        assert!(!fresh.is_verifying());
        for generation in 0..3 {
            assert!(!fresh.claim_verify(generation));
        }
    }

    /// Arming clears the error: the draft is being asked about again, so
    /// last time's refusal is no longer what the dialog is saying.
    #[test]
    fn arming_a_dial_clears_the_previous_refusal() {
        let mut draft = draft("pop-os", "/tmp/s.sock");
        draft.error = Some("unreachable".into());
        draft.begin_verify(1);
        assert_eq!(draft.error, None);
    }
}
