//! The two host modals: **Add Host** and the **Stop Session**
//! confirmation (plan 037 §3.1).
//!
//! One field on `App` holds either, because they are the same kind of
//! thing: a question the user still owes an answer to, drawn over the
//! chrome, owning the pointer and the keyboard while it is up. The
//! delete-project confirmation next door has the same shape and the
//! same panel style — Stop deliberately reuses its copy structure so
//! the two destructive confirmations read alike.
//!
//! Add Host is the one flow in the whole feature that needs free text,
//! which is why it is a dialog rather than palette steps. Its validation
//! is split in two on purpose: what can be answered instantly (an empty
//! field, a label the registry would refuse) is answered instantly, and
//! only a draft that passes spends a round trip dialing
//! `session.identify`.

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
}

impl AddHostDraft {
    /// Whether a dial is in flight — what the buttons and the
    /// "Connecting…" label read.
    pub(super) fn is_verifying(&self) -> bool {
        self.verifying.is_some()
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
}

impl HostDialog {
    pub(super) fn add() -> Self {
        Self::Add(AddHostDraft::default())
    }

    pub(super) fn draft_mut(&mut self) -> Option<&mut AddHostDraft> {
        match self {
            Self::Add(draft) => Some(draft),
            Self::ConfirmStop { .. } => None,
        }
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
    if target.is_empty() {
        return Err("a socket path is required".into());
    }
    Ok(HostDraftTarget {
        label: label.to_string(),
        target: target.to_string(),
    })
}

/// Dial a prospective host and check it is a session this client can
/// talk to at all.
///
/// The check itself is [`roost_ipc::session_launch::verify_target`] —
/// the same bar `roostctl host add --verify` applies, stated once so the
/// dialog and the CLI cannot drift apart about what "verified" means.
/// All this adds is the dialog's own error shape: a `String` to show
/// under the fields.
pub(super) async fn verify_target(target: String) -> Result<(), String> {
    let budget =
        roost_ipc::session_launch::IPC_TIMEOUT.mul_f64(roost_ipc::session_launch::timeout_scale());
    roost_ipc::session_launch::verify_target(&target, budget)
        .await
        .map(drop)
        .map_err(|error| format!("{error:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            Err("a socket path is required".into())
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

    /// The label is checked before the socket: a name the registry will
    /// never accept is the more useful thing to say first, and it is the
    /// half the user can fix without knowing a path.
    #[test]
    fn the_label_is_checked_before_the_socket() {
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
