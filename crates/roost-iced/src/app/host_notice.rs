//! What a host's connection state says to the user (plan 037 §3.1,
//! §3.7): the banner over its last frame, and the prompt its Connect
//! verb raises when the builds disagree.
//!
//! Pure, and deliberately kept away from the widgets: "which banner,
//! which buttons, and is there a restart button at all" is the part that
//! must be right for **every** connection state, and a table test is the
//! only way to say that once. The adapter next door paints whatever
//! these answer and adds nothing of its own.
//!
//! The two live together because they are the same question asked at two
//! moments — a state that took the window away from the user gets a
//! banner, and a state that needs a decision gets a dialog — and reading
//! them side by side is how the copy stays consistent.

use crate::host_conn::state::{
    BuildMismatch, HostConnState, MismatchKind, RestartAction, REQUIRED_PAYLOAD_KIND,
};

/// The banner drawn over a host tab's last frame.
///
/// One component, two messages: the session was taken away from this
/// window, or it ended. Both leave the frame on screen — those pixels
/// are the last true thing this client knows — and both offer the same
/// way out, which is an ordinary Connect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostBanner {
    pub(super) message: String,
    /// The button's label. It is always a Connect underneath; only the
    /// wording changes, because "reconnect" and "start a new session"
    /// are very different promises about what comes back.
    pub(super) action: &'static str,
}

/// A frame nothing will ever update again, and why.
///
/// `pub(crate)` because the banner's button carries it: the click has to
/// name the frame it was drawn on, so the app can refuse one that landed
/// after the host moved on (see [`click_still_lands`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrozenFrame {
    /// Another window holds the session now.
    TakenOver,
    /// The session ended; its shells are gone with it.
    Stopped,
}

/// Whether a host's state leaves its last frame frozen on screen.
///
/// `None` for every state that is still driving the tab or that the
/// sidebar already explains: a connected host needs no banner, and a
/// disconnected or reconnecting one is saying so in its section band
/// with a ↻ beside it. Only the two terminal states — somebody else has
/// it, or it is gone — leave a frame on screen that nothing will ever
/// update again, and those are the two that have to say so where the
/// user is looking.
///
/// The kind is separate from its wording because two very different
/// readers ask: the terminal area wants the sentence, and the selection
/// reconcile wants only whether there is a frame worth keeping — which
/// is a question about the state, not about the copy over it. One table
/// answers both, so a state added later cannot mean "frozen" to one and
/// not the other.
pub(super) fn frozen_frame(state: &HostConnState) -> Option<FrozenFrame> {
    match state {
        HostConnState::TakenOver => Some(FrozenFrame::TakenOver),
        HostConnState::Stopped => Some(FrozenFrame::Stopped),
        HostConnState::Disconnected(_)
        | HostConnState::Connecting { .. }
        | HostConnState::Connected
        | HostConnState::NeedsRestart(_) => None,
    }
}

/// Whether a banner click still names the frame it was drawn on.
///
/// The banner is a picture of a past frame, and a click carries the
/// latency of a human hand: a second press, or a press on pixels the
/// compositor has not repainted yet, can arrive after the host has
/// already advanced to `Connecting`/`Connected` — where honoring it
/// would abort the very attempt the first press started. And the two
/// banners promise different things, so a click on "Reconnect here"
/// must not be honored once the state underneath became `Stopped` and
/// the honest button is "Start a new session": that would lose a
/// session's scrollback silently, which is exactly what plan 037 §3.2
/// forbids.
///
/// `current` is what [`frozen_frame`] says about the host **now**.
pub(super) fn click_still_lands(rendered: FrozenFrame, current: Option<FrozenFrame>) -> bool {
    current == Some(rendered)
}

impl FrozenFrame {
    /// What this frame says to the user, over the pixels it froze.
    pub(super) fn banner(self, label: &str) -> HostBanner {
        match self {
            Self::TakenOver => HostBanner {
                message: format!("{label} was taken over by another Roost window."),
                action: "Reconnect here",
            },
            Self::Stopped => HostBanner {
                // Deliberately not "reconnect": the shells are gone, and
                // the button starts a fresh session rather than finding
                // this one.
                message: format!("The session on {label} ended."),
                action: "Start a new session",
            },
        }
    }
}

/// The upgrade dialog's contents (plan 037 §3.7, plan 039 §3.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RestartPrompt {
    pub(super) title: String,
    pub(super) body: String,
    /// What this client can do about it. The dialog's primary button —
    /// which one, and whether there is one at all — falls out of this
    /// rather than out of a separate boolean that could disagree with
    /// it.
    pub(super) action: RestartAction,
    /// The primary button's label, already naming the host where that
    /// matters. `None` renders the state and the pointer with no button
    /// at all, which is plan 037 §3.1's "no dead button" made literal.
    pub(super) confirm: Option<String>,
}

impl RestartPrompt {
    /// The dismissing button. "Not now" only reads right beside an
    /// action the user is declining; a dialog with nothing to decline
    /// says "Close".
    pub(super) fn dismiss_label(&self) -> &'static str {
        if self.confirm.is_some() {
            "Not now"
        } else {
            "Close"
        }
    }
}

/// Compose the upgrade dialog for a host whose compatibility gate
/// refused.
///
/// Three actions, three prompts. The middle one is plan 039's: a host
/// reached over ssh can be updated from here, so it gets a body that
/// says what will happen and a button that does it — the *offer* is
/// structural (this is an ssh host), and whether a matching build
/// actually exists to install is resolved when the user confirms.
pub(super) fn restart_prompt(label: &str, mismatch: &BuildMismatch) -> RestartPrompt {
    let vintage = vintage(mismatch);
    let detail = detail(mismatch);
    match mismatch.restart {
        RestartAction::RestartLocal => RestartPrompt {
            title: format!("Restart the session on {label}?"),
            body: format!(
                "This session was started by {vintage} ({detail}). Restarting \
                 reopens every tab as a fresh shell in its directory — running \
                 programs end.",
            ),
            action: mismatch.restart,
            confirm: Some("Restart session".to_string()),
        },
        RestartAction::OfferRemoteUpdate => RestartPrompt {
            title: format!("The session on {label} needs a restart"),
            body: format!(
                "This session was started by {vintage} ({detail}). Roost can \
                 install the matching roost-session on {label} over ssh and \
                 restart the session there — it will show you what it would do \
                 before anything is changed.{}",
                // Said here as well as on the consent card, because this
                // is where the user decides whether to look at all.
                if session_is_newer(mismatch) {
                    " That session is newer than this Roost, so it would install \
                     an older build; upgrading this Roost is likely the fix."
                } else {
                    ""
                }
            ),
            action: mismatch.restart,
            confirm: Some(format!("Update roost-session on {label}")),
        },
        RestartAction::None => RestartPrompt {
            title: format!("The session on {label} needs a restart"),
            body: format!(
                "This session was started by {vintage} ({detail}). Only the \
                 machine running it can restart it — stop and start the session \
                 there (`roostctl session stop`, then `roostctl session start`). \
                 See the host sessions guide.",
            ),
            action: mismatch.restart,
            confirm: None,
        },
    }
}

/// What a Connect on a host whose compatibility gate already refused
/// actually does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ConnectRoute {
    /// Dial. Plan 038's behavior, unchanged.
    Dial,
    /// Raise the upgrade prompt instead — dialing would reproduce the
    /// same refusal (plan 037 §3.7).
    Prompt,
}

/// The gate on `host_connect_requested`.
///
/// **A modal is never raised at a machine** (plan 039 §3.5), and this
/// verb has two doors onto it: the sidebar's ↻ and a palette row —
/// which `palette.activate` reaches over the IPC socket, with a shipped
/// `roostctl palette open host` in front of it. The row is the same row
/// either way, so the origin is the only thing that separates a person
/// from `roostctl`, and an unattended UI left holding a modal swallows
/// keyboard input until somebody dismisses it.
///
/// `host.connect` already routes around this prompt entirely, dialing
/// straight through; this makes the palette's door agree with it rather
/// than being the one that got missed.
pub(super) fn connect_route(
    origin: crate::host_conn::RequestOrigin,
    needs_restart: bool,
) -> ConnectRoute {
    if needs_restart && origin == crate::host_conn::RequestOrigin::User {
        ConnectRoute::Prompt
    } else {
        ConnectRoute::Dial
    }
}

/// Whether the *session* is the newer of the two, where that is
/// knowable at all.
///
/// [`vintage`]'s direction, as a predicate: protocol numbers order, so
/// "newer" is a fact there; two libghostty build strings that disagree
/// are merely different, and a downgrade warning guessed off one would
/// be a dialog inventing a direction.
pub(super) fn session_is_newer(mismatch: &BuildMismatch) -> bool {
    matches!(mismatch.kind, MismatchKind::Protocol)
        && mismatch.session_protocol > mismatch.client_protocol
}

/// Which direction the skew runs, said only where it is actually known.
///
/// Protocol numbers order; build strings do not — two libghostty builds
/// that disagree are just different, and guessing which is newer from an
/// opaque identifier is how a dialog ends up lying to a user.
fn vintage(mismatch: &BuildMismatch) -> &'static str {
    match mismatch.kind {
        MismatchKind::Protocol if mismatch.session_protocol < mismatch.client_protocol => {
            "an older Roost"
        }
        MismatchKind::Protocol => "a newer Roost",
        MismatchKind::PayloadKind | MismatchKind::Build => "a different Roost build",
    }
}

/// The two values that disagreed, verbatim. A user staring at "started
/// by a different Roost build" wants to see which two builds those were.
fn detail(mismatch: &BuildMismatch) -> String {
    match mismatch.kind {
        MismatchKind::Protocol => format!(
            "session protocol {}, this client speaks {}",
            mismatch.session_protocol, mismatch.client_protocol
        ),
        MismatchKind::PayloadKind => {
            let offered = mismatch.session_payload_kinds.join(", ");
            let offered = if offered.is_empty() {
                "nothing".to_string()
            } else {
                offered
            };
            format!("it offers {offered}, this client needs {REQUIRED_PAYLOAD_KIND}")
        }
        MismatchKind::Build => format!(
            "libghostty {} against this client's {}",
            mismatch.session_build, mismatch.client_build
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host_conn::state::Disconnected;
    use roost_ipc::messages::SESSION_PROTOCOL_VERSION;

    fn mismatch(kind: MismatchKind, restart: RestartAction) -> BuildMismatch {
        BuildMismatch {
            kind,
            session_protocol: 1,
            client_protocol: SESSION_PROTOCOL_VERSION,
            session_build: "gb-old".into(),
            client_build: "gb-new".into(),
            session_payload_kinds: vec!["vt".into()],
            restart,
        }
    }

    /// The two production halves composed exactly as the terminal area
    /// composes them.
    fn banner(label: &str, state: &HostConnState) -> Option<HostBanner> {
        Some(frozen_frame(state)?.banner(label))
    }

    fn every_state() -> Vec<HostConnState> {
        vec![
            HostConnState::Disconnected(Disconnected {
                reason: "session ended".into(),
                retry_in: None,
            }),
            HostConnState::Connecting { previous: None },
            HostConnState::Connected,
            HostConnState::TakenOver,
            HostConnState::Stopped,
            HostConnState::NeedsRestart(mismatch(MismatchKind::Build, RestartAction::RestartLocal)),
        ]
    }

    /// The whole banner decision, state by state — the point of the
    /// table being that a state added later fails this test rather than
    /// silently rendering nothing over a frozen frame.
    #[test]
    fn only_the_two_terminal_states_put_a_banner_over_the_frame() {
        let banners: Vec<Option<HostBanner>> = every_state()
            .iter()
            .map(|state| banner("pop-os", state))
            .collect();
        assert_eq!(banners[0], None, "disconnected explains itself in the band");
        assert_eq!(banners[1], None, "connecting is not a failure yet");
        assert_eq!(banners[2], None, "a connected host says nothing");
        assert_eq!(
            banners[3],
            Some(HostBanner {
                message: "pop-os was taken over by another Roost window.".into(),
                action: "Reconnect here",
            })
        );
        assert_eq!(
            banners[4],
            Some(HostBanner {
                message: "The session on pop-os ended.".into(),
                action: "Start a new session",
            })
        );
        assert_eq!(
            banners[5], None,
            "a build mismatch is answered by its dialog, not a banner"
        );
    }

    /// The one thing the two banners must not share: a stopped session's
    /// shells are gone, so its button may not promise a reconnect.
    #[test]
    fn the_two_banners_promise_different_things() {
        let taken = banner("pop-os", &HostConnState::TakenOver).expect("takeover banner");
        let stopped = banner("pop-os", &HostConnState::Stopped).expect("stopped banner");
        assert_ne!(taken.action, stopped.action);
        assert!(taken.message.contains("taken over"));
        assert!(stopped.message.contains("ended"));
    }

    /// A banner click is a promise about the frame it was drawn on, and
    /// the two ways it goes stale are both damaging: a host that has
    /// already advanced to `Connecting`/`Connected` would have the
    /// attempt in flight aborted by a second press, and a `TakenOver`
    /// frame that became `Stopped` would honor "Reconnect here" as
    /// "start a fresh session" — silent scrollback loss, which plan 037
    /// §3.2 forbids. Only the frame still on screen is acted on.
    #[test]
    fn a_banner_click_lands_only_on_the_frame_it_was_drawn_on() {
        for state in every_state() {
            let current = frozen_frame(&state);
            for rendered in [FrozenFrame::TakenOver, FrozenFrame::Stopped] {
                assert_eq!(
                    click_still_lands(rendered, current),
                    current == Some(rendered),
                    "{rendered:?} against {state:?}"
                );
            }
        }
        // Spelled out for the three cases the table above proves in
        // aggregate, so a reader sees which is which.
        assert!(click_still_lands(
            FrozenFrame::TakenOver,
            Some(FrozenFrame::TakenOver)
        ));
        assert!(
            !click_still_lands(FrozenFrame::TakenOver, Some(FrozenFrame::Stopped)),
            "the button promised a reconnect; the session has since ended"
        );
        assert!(
            !click_still_lands(FrozenFrame::TakenOver, None),
            "a reconnect is already under way; a second press must not abort it"
        );
    }

    /// Three actions, three prompts: the local restart, the ssh update
    /// offer, and the remote Unix-socket host that gets a pointer at the
    /// docs and no button — no dead button (plan 037 §3.1).
    #[test]
    fn each_restart_action_gets_the_prompt_it_can_act_on() {
        let local = restart_prompt(
            "localhost",
            &mismatch(MismatchKind::Build, RestartAction::RestartLocal),
        );
        assert_eq!(local.confirm.as_deref(), Some("Restart session"));
        assert_eq!(local.dismiss_label(), "Not now");
        assert!(local.title.starts_with("Restart the session"));
        assert!(
            local.body.contains("running programs end"),
            "{}",
            local.body
        );

        let offer = restart_prompt(
            "pop-os",
            &mismatch(MismatchKind::Build, RestartAction::OfferRemoteUpdate),
        );
        assert_eq!(
            offer.confirm.as_deref(),
            Some("Update roost-session on pop-os"),
            "the button names the host it would reach"
        );
        assert_eq!(offer.dismiss_label(), "Not now");
        assert!(offer.title.contains("needs a restart"));
        assert!(offer.body.contains("over ssh"), "{}", offer.body);
        assert!(
            offer.body.contains("before anything is changed"),
            "the offer promises consent first: {}",
            offer.body
        );
        assert!(
            !offer.body.contains("host sessions guide"),
            "a host with a button is not sent to the docs instead: {}",
            offer.body
        );

        let none = restart_prompt(
            "remote.sock",
            &mismatch(MismatchKind::Build, RestartAction::None),
        );
        assert_eq!(none.confirm, None);
        assert_eq!(none.dismiss_label(), "Close");
        assert!(none.title.contains("needs a restart"));
        assert!(
            none.body.contains("host sessions guide"),
            "a host nothing can be offered for is pointed at the docs: {}",
            none.body
        );
        assert!(
            !none.body.contains("Restarting reopens"),
            "and is never told what a button it does not have would do"
        );
    }

    /// Only a person is ever answered with a modal.
    ///
    /// `palette.activate` is a shipped, ungated IPC op with a
    /// `roostctl palette open host` in front of it, and its `Connect`
    /// row is the very row a click runs. Without the origin, a
    /// machine-driven activation on a `NeedsRestart` ssh host would
    /// raise this prompt — and its button now starts a remote install —
    /// on a UI nobody is watching.
    #[test]
    fn a_connect_arriving_over_ipc_never_raises_the_upgrade_prompt() {
        use crate::host_conn::RequestOrigin;

        assert_eq!(
            connect_route(RequestOrigin::User, true),
            ConnectRoute::Prompt,
            "a person on a mismatched host gets the question"
        );
        assert_eq!(
            connect_route(RequestOrigin::Ipc, true),
            ConnectRoute::Dial,
            "a machine gets what host.connect already gives it: a dial"
        );
        for origin in [RequestOrigin::User, RequestOrigin::Ipc] {
            assert_eq!(
                connect_route(origin, false),
                ConnectRoute::Dial,
                "a host with no mismatch has nothing to prompt about: {origin:?}"
            );
        }
    }

    /// The remote offer says which way the skew runs, but only where
    /// that is knowable — and it keeps its button either way. A
    /// downgrade the user has a reason for is theirs to make; a
    /// downgrade they did not notice is not.
    #[test]
    fn the_remote_offer_names_a_downgrade_and_still_offers_it() {
        let mut newer = mismatch(MismatchKind::Protocol, RestartAction::OfferRemoteUpdate);
        newer.session_protocol = SESSION_PROTOCOL_VERSION + 1;
        assert!(session_is_newer(&newer));
        let prompt = restart_prompt("pop-os", &newer);
        assert!(
            prompt.body.contains("install an older build"),
            "{}",
            prompt.body
        );
        assert!(prompt.confirm.is_some(), "the offer stands");

        let mut older = mismatch(MismatchKind::Protocol, RestartAction::OfferRemoteUpdate);
        older.session_protocol = SESSION_PROTOCOL_VERSION - 1;
        assert!(!session_is_newer(&older));
        assert!(!restart_prompt("pop-os", &older)
            .body
            .contains("older build"));

        // Two build strings that disagree are merely different, so no
        // direction is claimed.
        let build = mismatch(MismatchKind::Build, RestartAction::OfferRemoteUpdate);
        assert!(!session_is_newer(&build));
        assert!(!restart_prompt("pop-os", &build)
            .body
            .contains("older build"));
    }

    /// Direction is claimed only where it is known. Protocol numbers
    /// order, so older/newer is a fact; two build strings are merely
    /// different.
    #[test]
    fn the_skews_direction_is_only_claimed_when_it_is_knowable() {
        let mut older = mismatch(MismatchKind::Protocol, RestartAction::RestartLocal);
        older.session_protocol = SESSION_PROTOCOL_VERSION - 1;
        assert_eq!(vintage(&older), "an older Roost");

        let mut newer = mismatch(MismatchKind::Protocol, RestartAction::RestartLocal);
        newer.session_protocol = SESSION_PROTOCOL_VERSION + 1;
        assert_eq!(vintage(&newer), "a newer Roost");

        assert_eq!(
            vintage(&mismatch(MismatchKind::Build, RestartAction::RestartLocal)),
            "a different Roost build"
        );
        assert_eq!(
            vintage(&mismatch(
                MismatchKind::PayloadKind,
                RestartAction::RestartLocal
            )),
            "a different Roost build"
        );
    }

    /// Each half of the gate names the two values that disagreed, so the
    /// dialog is diagnosable rather than merely apologetic.
    #[test]
    fn every_mismatch_kind_shows_what_disagreed() {
        let protocol = detail(&mismatch(
            MismatchKind::Protocol,
            RestartAction::RestartLocal,
        ));
        assert!(protocol.contains('1') && protocol.contains(&SESSION_PROTOCOL_VERSION.to_string()));

        let build = detail(&mismatch(MismatchKind::Build, RestartAction::RestartLocal));
        assert!(build.contains("gb-old") && build.contains("gb-new"));

        let kind = detail(&mismatch(
            MismatchKind::PayloadKind,
            RestartAction::RestartLocal,
        ));
        assert!(kind.contains("vt") && kind.contains(REQUIRED_PAYLOAD_KIND));

        // A session offering nothing at all still reads as a sentence.
        let mut empty = mismatch(MismatchKind::PayloadKind, RestartAction::RestartLocal);
        empty.session_payload_kinds.clear();
        assert!(detail(&empty).contains("offers nothing"));
    }
}
