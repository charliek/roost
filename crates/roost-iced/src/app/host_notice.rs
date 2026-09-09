//! What a host's connection state says to the user (plan 037 §3.1,
//! §3.7): the banner over its last frame, and the prompt its Connect
//! verb raises when the session is one this client cannot talk to at
//! all — a protocol it does not speak, no payload kind it can decode,
//! or a build skew against a session too old to serve `vt`. A skew a
//! session *can* serve connects instead, at reduced fidelity, and is
//! asked about from the band rather than from this gate —
//! [`restart_prompt_for_skew`] is that question.
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
    BuildMismatch, HostConnState, MismatchKind, RestartAction, Skew, CLIENT_PAYLOAD_KINDS,
};
use roost_ui_model::host_sidebar::FidelityAction;

/// A line the window owes a host tab, over the pixels it is drawing.
///
/// Two producers, one shape: [`frozen_frame`]'s banner over a frame
/// nothing will update again, and [`foreground_strip`]'s status line over
/// a frame that is still live but is no longer this window's to drive.
/// Both are a sentence and one button, and the button is a Connect
/// underneath either way — only the wording changes, because "start a
/// new session" and "take the foreground" are very different promises
/// about what comes back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct HostBanner {
    pub(super) message: String,
    pub(super) action: &'static str,
}

/// The status strip's action, named once: the widget's label and the
/// tests that pin it read the same string.
pub(super) const TAKE_FOREGROUND: &str = "Take the foreground";

/// A frame nothing will ever update again, and why.
///
/// One variant since plan 057 — a takeover no longer freezes anything,
/// because the session closes nothing and the attach goes on streaming —
/// and still an enum, because the click check below is a question about
/// *which* frame was drawn and a second one may yet exist.
///
/// `pub(crate)` because the banner's button carries it: the click has to
/// name the frame it was drawn on, so the app can refuse one that landed
/// after the host moved on (see [`click_still_lands`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FrozenFrame {
    /// Another client holds the session, and this one has nothing left
    /// reading it — a session too old to keep the connections open on a
    /// takeover, or a connection that was only ever an observer.
    TakenOver,
    /// The session ended; its shells are gone with it.
    Stopped,
}

/// Whether a host's state leaves its last frame frozen on screen.
///
/// `None` for every state that is still driving the tab or that the
/// sidebar already explains: a connected host needs no banner, and a
/// disconnected or reconnecting one is saying so in its section band
/// with a ↻ beside it.
///
/// **`TakenOver` is two worlds, and `serving_in_place` is which one**
/// (plan 057 §3.5). Against a session advertising `open_input` the
/// takeover moved the foreground and closed nothing: that frame is live,
/// takes keys, and gets [`foreground_strip`]'s line rather than a scrim.
/// Against a session that predates it — and for a connection that was
/// only ever an observer — the data connections really are gone, the
/// frame stops updating, and it needs the scrim and the full-reconnect
/// button it has always had. The flag is the *task's* answer, not an
/// inference from the state, because only the task knows whether it is
/// still serving on the control connection it holds.
///
/// The kind is separate from its wording because two very different
/// readers ask: the terminal area wants the sentence, and the selection
/// reconcile wants only whether there is a frame worth keeping — which
/// is a question about the state, not about the copy over it. One table
/// answers both, so a state added later cannot mean "frozen" to one and
/// not the other.
pub(super) fn frozen_frame(state: &HostConnState, serving_in_place: bool) -> Option<FrozenFrame> {
    match state {
        HostConnState::Stopped => Some(FrozenFrame::Stopped),
        HostConnState::TakenOver { .. } => (!serving_in_place).then_some(FrozenFrame::TakenOver),
        HostConnState::Disconnected(_)
        | HostConnState::Connecting { .. }
        | HostConnState::Connected
        | HostConnState::NeedsRestart(_) => None,
    }
}

/// The one-line status strip over a host tab's **live** grid when
/// another client holds the foreground (plan 057 §3.5).
///
/// Not a banner over a corpse and deliberately not shaped like one: the
/// attach is still streaming, the keyboard still reaches it, and the
/// only thing this window lost is the foreground — effects, the focus
/// that mutes notifications, and the settings ops. So the sentence is in
/// the present tense and names who has it, and the button takes it back
/// in place rather than reconnecting.
///
/// `taken_by` is the claimant's *self-reported* label (plan 049 §3.9)
/// and the copy says so: nothing authenticates it, so the sentence
/// attributes the name to the client rather than asserting it. `None` —
/// a takeover this client inferred from a probe rather than being told
/// about — says only that somebody has it.
///
/// `serving_in_place` is [`frozen_frame`]'s flag and answers the same
/// fork from the other side, so the two are mutually exclusive by
/// construction: a deposed host that is *not* serving gets the scrim,
/// never this line over a grid nothing is feeding.
pub(super) fn foreground_strip(
    state: &HostConnState,
    label: &str,
    serving_in_place: bool,
) -> Option<HostBanner> {
    let HostConnState::TakenOver { taken_by } = state else {
        return None;
    };
    if !serving_in_place {
        return None;
    }
    Some(HostBanner {
        message: match taken_by.as_deref() {
            Some(taker) => format!("{label} is driven by a client reporting itself as {taker}."),
            None => format!("{label} is driven by another client."),
        },
        action: TAKE_FOREGROUND,
    })
}

/// Whether a banner click still names the frame it was drawn on.
///
/// The banner is a picture of a past frame, and a click carries the
/// latency of a human hand: a second press, or a press on pixels the
/// compositor has not repainted yet, can arrive after the host has
/// already advanced to `Connecting`/`Connected` — where honoring it
/// would abort the very attempt the first press started. "Start a new
/// session" is a promise about a session that has ended, and honoring it
/// against a host that has since come back would start a second one.
///
/// `current` is what [`frozen_frame`] says about the host **now**.
pub(super) fn click_still_lands(rendered: FrozenFrame, current: Option<FrozenFrame>) -> bool {
    current == Some(rendered)
}

impl FrozenFrame {
    /// What this frame says to the user, over the pixels it froze.
    ///
    /// `taken_by` is the claimant's *self-reported* label (plan 049
    /// §3.9) and the copy says so; `None` — a takeover this client
    /// inferred from a probe rather than being told about — says only
    /// that somebody did.
    pub(super) fn banner(self, label: &str, taken_by: Option<&str>) -> HostBanner {
        match self {
            Self::TakenOver => HostBanner {
                message: match taken_by {
                    Some(taker) => {
                        format!("{label} was taken over by a client reporting itself as {taker}.")
                    }
                    None => format!("{label} was taken over by another client."),
                },
                // A full reconnect, deliberately: this frame is frozen
                // because nothing is serving it any more, so there is no
                // foreground to take back in place.
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

    /// Why a paste into this frame is refused (issue #376), paired with
    /// the remedy the banner beside it offers.
    pub(super) fn paste_refusal(self) -> &'static str {
        match self {
            Self::TakenOver => "this session was taken over — reconnect to paste",
            Self::Stopped => "this session ended — start a new session to paste",
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

/// Why a connection is at reduced fidelity, and what that costs — the
/// sentences both cards raised from the band lead with (plan 056 §3.6).
///
/// One string, because the localhost restart prompt below and the ssh
/// consent card ([`super::bootstrap::bootstrap_copy`]) are the same
/// explanation with different actions after it, and copy that only
/// happens to match is copy that drifts.
pub(super) fn reduced_fidelity_reason(skew: &Skew) -> String {
    format!(
        "This session is attached at reduced fidelity: it was started by a roost-session \
         built against {}, and this Roost is built against {}. Links, the alternate screen \
         and soft wrapping are off until it runs the matching build.",
        skew.session_build, skew.client_build
    )
}

/// Compose the restart dialog for a **localhost** session this client
/// is attached to across a libghostty build skew (plan 056 §3.6).
///
/// The sibling of [`restart_prompt`]'s `RestartLocal` arm, and a
/// separate function rather than a fourth arm of it because the two
/// answer different questions. That one is raised at a host this client
/// **cannot talk to**; this one at a host it is talking to right now,
/// on the `vt` fallback, whose links and alternate screen are gone. The
/// only way to reuse the arm would be to synthesize a [`BuildMismatch`]
/// with a kind that lies about protocol fields it never had.
///
/// It claims no direction, for [`vintage`]'s reason: two libghostty
/// build strings are merely different.
pub(super) fn restart_prompt_for_skew(label: &str, skew: &Skew) -> RestartPrompt {
    RestartPrompt {
        title: format!("Restart the session on {label}?"),
        body: format!(
            "{} Restarting reopens every tab as a fresh shell in its directory. Running \
             programs end.",
            reduced_fidelity_reason(skew)
        ),
        action: RestartAction::RestartLocal,
        confirm: Some("Restart session".to_string()),
    }
}

/// The band's pill, on every transport. The *fact* does not vary — this
/// connection is on the `vt` fallback wherever the session lives — and
/// only what can be done about it does.
pub(super) const FIDELITY_PILL: &str = "reduced fidelity";

/// The two things a reduced-fidelity section draws: the pill on its band
/// and the inline row under it (plan 056 §3.4's matrix).
///
/// One value, because the pill and the row are one offer shown twice —
/// a band that invites a press over a row that only points, or the
/// reverse, would be the window disagreeing with itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FidelityChrome {
    /// Whether both are buttons. `false` is a socket target: somebody
    /// else's process, over a transport that reaches its socket and not
    /// its binary, so there is nothing here to press.
    pub(super) pressable: bool,
    /// The inline row's whole text, glyph included.
    pub(super) row: String,
}

/// What the band and the row say for one [`FidelityAction`].
pub(super) fn fidelity_chrome(action: FidelityAction, label: &str) -> FidelityChrome {
    match action {
        FidelityAction::Update => FidelityChrome {
            pressable: true,
            row: "⬆ Update roost-session".to_string(),
        },
        FidelityAction::Restart => FidelityChrome {
            pressable: true,
            row: "↻ Restart session".to_string(),
        },
        FidelityAction::Manual => FidelityChrome {
            pressable: false,
            row: format!("Restart it on {label} to restore fidelity"),
        },
    }
}

/// The status-bar sentence a connection's **first** `vt` attach owes
/// (plan 056 §3.5).
///
/// The one surface keyed on the attach rather than on the connection's
/// own fidelity fact: the pill, the row and the verbs describe a
/// property of the link and appear the moment it is up, while this is
/// about the terminal the person is looking at, so it waits until there
/// is one. It says what was lost without the build strings — those are
/// on the card the pill opens, and a 5 s banner is not where a hex
/// build id earns its space.
pub(super) fn fidelity_sentence(label: &str) -> String {
    format!(
        "{label} is attached at reduced fidelity: links, the alternate screen and soft \
         wrapping are off until it runs a matching build."
    )
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
            format!(
                "it offers {offered}, this client decodes {}",
                CLIENT_PAYLOAD_KINDS.join(", ")
            )
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
            session_payload_kinds: vec!["sixel-mosaic".into()],
            restart,
        }
    }

    /// The two production halves composed exactly as the terminal area
    /// composes them. `serving` is the task's in-place answer; a state
    /// that is not a takeover ignores it.
    fn banner(label: &str, state: &HostConnState, serving: bool) -> Option<HostBanner> {
        Some(frozen_frame(state, serving)?.banner(label, state.taken_by()))
    }

    fn every_state() -> Vec<HostConnState> {
        vec![
            HostConnState::Disconnected(Disconnected {
                reason: "session ended".into(),
                detail: None,
                retry_in: None,
            }),
            HostConnState::Connecting { previous: None },
            HostConnState::Connected,
            HostConnState::TakenOver { taken_by: None },
            HostConnState::Stopped,
            HostConnState::NeedsRestart(mismatch(MismatchKind::Build, RestartAction::RestartLocal)),
        ]
    }

    /// The whole banner decision, state by state — the point of the
    /// table being that a state added later fails this test rather than
    /// silently rendering nothing over a frozen frame.
    #[test]
    fn only_a_session_that_ended_puts_a_banner_over_the_frame() {
        let banners: Vec<Option<HostBanner>> = every_state()
            .iter()
            .map(|state| banner("pop-os", state, true))
            .collect();
        assert_eq!(banners[0], None, "disconnected explains itself in the band");
        assert_eq!(banners[1], None, "connecting is not a failure yet");
        assert_eq!(banners[2], None, "a connected host says nothing");
        assert_eq!(
            banners[3], None,
            "a takeover that closed nothing leaves a live frame: a strip, not a scrim"
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

    /// The three worlds `TakenOver` covers, and the one signal that
    /// separates them (plan 057 §3.5, review F3).
    ///
    /// Deposed-but-serving is the `open_input` world: the connections
    /// stayed open, so the frame is live and the strip offers the
    /// in-place takeback. The other two — a session too old to keep them
    /// open, and a connection that was only ever an observer — have
    /// nothing feeding that frame, so they get the scrim and a button
    /// that promises a reconnect, which is what it actually performs.
    #[test]
    fn a_deposed_host_with_nothing_serving_it_is_frozen_not_stripped() {
        let taken = HostConnState::TakenOver {
            taken_by: Some("a phone".into()),
        };

        assert_eq!(
            frozen_frame(&taken, true),
            None,
            "a deposed-but-serving host still has a live grid"
        );
        assert_eq!(
            foreground_strip(&taken, "pop-os", true),
            Some(HostBanner {
                message: "pop-os is driven by a client reporting itself as a phone.".into(),
                action: TAKE_FOREGROUND,
            })
        );

        assert_eq!(
            frozen_frame(&taken, false),
            Some(FrozenFrame::TakenOver),
            "nothing is serving that frame: it is frozen"
        );
        assert_eq!(
            banner("pop-os", &taken, false),
            Some(HostBanner {
                message: "pop-os was taken over by a client reporting itself as a phone.".into(),
                action: "Reconnect here",
            }),
            "the button promises the reconnect it will actually do"
        );
        assert_eq!(
            foreground_strip(&taken, "pop-os", false),
            None,
            "no present-tense strip over a frame nothing is feeding"
        );

        // The inferred takeover — an observer-only settlement, which is
        // never in-place — still names the state without a taker.
        assert_eq!(
            banner(
                "pop-os",
                &HostConnState::TakenOver { taken_by: None },
                false
            ),
            Some(HostBanner {
                message: "pop-os was taken over by another client.".into(),
                action: "Reconnect here",
            })
        );
        assert_eq!(
            FrozenFrame::TakenOver.paste_refusal(),
            "this session was taken over — reconnect to paste"
        );
    }

    /// The strip's own table: it is the takeover's, and nothing else's.
    /// A stopped session must not get one — its frame is a corpse and
    /// the banner above says so — and the copy names the taker where the
    /// session named one, attributed rather than asserted.
    #[test]
    fn only_a_takeover_puts_a_status_strip_over_a_live_grid() {
        let strips: Vec<Option<HostBanner>> = every_state()
            .iter()
            .map(|state| foreground_strip(state, "pop-os", true))
            .collect();
        assert_eq!(
            strips,
            vec![
                None,
                None,
                None,
                Some(HostBanner {
                    message: "pop-os is driven by another client.".into(),
                    action: TAKE_FOREGROUND,
                }),
                None,
                None,
            ]
        );
        assert_eq!(
            foreground_strip(
                &HostConnState::TakenOver {
                    taken_by: Some("a phone".into())
                },
                "pop-os",
                true
            ),
            Some(HostBanner {
                message: "pop-os is driven by a client reporting itself as a phone.".into(),
                action: TAKE_FOREGROUND,
            })
        );
    }

    /// The one thing the two lines must not share: a stopped session's
    /// shells are gone, so its button may not promise the foreground —
    /// there is nothing left to drive.
    #[test]
    fn the_strip_and_the_banner_promise_different_things() {
        let taken = foreground_strip(&HostConnState::TakenOver { taken_by: None }, "pop-os", true)
            .expect("takeover strip");
        let stopped = banner("pop-os", &HostConnState::Stopped, true).expect("stopped banner");
        assert_ne!(taken.action, stopped.action);
        assert!(taken.message.contains("is driven by"));
        assert!(stopped.message.contains("ended"));
    }

    /// A banner click is a promise about the frame it was drawn on, and
    /// a click carries the latency of a human hand: a second press, or
    /// one on pixels the compositor has not repainted, can arrive after
    /// the host advanced to `Connecting`/`Connected` — where honoring it
    /// would abort the very attempt the first press started. Only the
    /// frame still on screen is acted on.
    #[test]
    fn a_banner_click_lands_only_on_the_frame_it_was_drawn_on() {
        for state in every_state() {
            for serving in [false, true] {
                let current = frozen_frame(&state, serving);
                assert_eq!(
                    click_still_lands(FrozenFrame::Stopped, current),
                    current == Some(FrozenFrame::Stopped),
                    "against {state:?} (serving: {serving})"
                );
            }
        }
        assert!(click_still_lands(
            FrozenFrame::Stopped,
            Some(FrozenFrame::Stopped)
        ));
        assert!(
            !click_still_lands(FrozenFrame::Stopped, None),
            "a connect is already under way; a second press must not abort it"
        );
    }

    /// The paste-refusal copy (issue #376) names the state and the
    /// remedy it can actually offer.
    #[test]
    fn paste_refusal_names_state_and_remedy() {
        let stopped = FrozenFrame::Stopped.paste_refusal();
        assert!(stopped.contains("ended") && stopped.contains("new session"));
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

    /// The fourth prompt, and the one raised at a host this client is
    /// **attached to**: it leads with the two builds that disagreed,
    /// then says what a restart costs, and claims no direction.
    #[test]
    fn the_skew_prompt_names_both_builds_and_what_a_restart_costs() {
        let skew = Skew {
            session_build: "gb-old".into(),
            client_build: "gb-new".into(),
        };
        let prompt = restart_prompt_for_skew("localhost", &skew);

        assert_eq!(prompt.title, "Restart the session on localhost?");
        assert_eq!(prompt.action, RestartAction::RestartLocal);
        assert_eq!(prompt.confirm.as_deref(), Some("Restart session"));
        assert_eq!(prompt.dismiss_label(), "Not now");
        assert!(
            prompt.body.starts_with(
                "This session is attached at reduced fidelity: it was started by a roost-session \
                 built against gb-old, and this Roost is built against gb-new."
            ),
            "{}",
            prompt.body
        );
        assert!(
            prompt
                .body
                .contains("Links, the alternate screen and soft wrapping are off"),
            "the user is told what they have lost: {}",
            prompt.body
        );
        assert!(
            prompt.body.contains("Running programs end."),
            "and what a restart costs: {}",
            prompt.body
        );
        assert!(
            !prompt.body.contains("older") && !prompt.body.contains("newer"),
            "two build strings are merely different: {}",
            prompt.body
        );
        assert!(
            !prompt.body.contains("needs a restart"),
            "this host is connected and serving; it is not waiting for anything: {}",
            prompt.body
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
        assert!(
            kind.contains("sixel-mosaic")
                && CLIENT_PAYLOAD_KINDS
                    .iter()
                    .all(|decodable| kind.contains(*decodable)),
            "the detail names what was offered and what this client decodes: {kind}"
        );

        // A session offering nothing at all still reads as a sentence.
        let mut empty = mismatch(MismatchKind::PayloadKind, RestartAction::RestartLocal);
        empty.session_payload_kinds.clear();
        assert!(detail(&empty).contains("offers nothing"));
    }

    /// Plan 056 §3.4's matrix, the two widget columns: what the band's
    /// pill and the row under it draw for each action, and which of them
    /// respond to a press.
    #[test]
    fn the_fidelity_matrix_answers_one_pair_of_widgets_per_action() {
        let update = fidelity_chrome(FidelityAction::Update, "pop-os");
        assert!(update.pressable, "an ssh host can be sent a build");
        assert_eq!(update.row, "⬆ Update roost-session");

        let restart = fidelity_chrome(FidelityAction::Restart, "localhost");
        assert!(restart.pressable, "our own session is ours to restart");
        assert_eq!(restart.row, "↻ Restart session");

        let manual = fidelity_chrome(FidelityAction::Manual, "build-box");
        assert!(
            !manual.pressable,
            "a socket target's process is not this client's to touch"
        );
        assert_eq!(manual.row, "Restart it on build-box to restore fidelity");
        assert!(
            !manual.row.starts_with('⬆') && !manual.row.starts_with('↻'),
            "and it wears no action glyph, because it is not an action: {}",
            manual.row
        );
    }

    /// The pill says the same thing everywhere — the fact is the
    /// connection's, not the transport's — and it never grows a second
    /// spelling per action.
    #[test]
    fn the_pill_is_one_string_on_every_transport() {
        assert_eq!(FIDELITY_PILL, "reduced fidelity");
    }

    /// The banner names the host and what it costs, and deliberately not
    /// the two build strings: the card the pill opens carries those.
    #[test]
    fn the_first_vt_attach_says_what_was_lost() {
        let said = fidelity_sentence("pop-os");
        assert_eq!(
            said,
            "pop-os is attached at reduced fidelity: links, the alternate screen and soft \
             wrapping are off until it runs a matching build."
        );
    }
}
