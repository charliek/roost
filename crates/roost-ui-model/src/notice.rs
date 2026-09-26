//! What the window is telling the user in its terminal area, and the
//! vocabulary the bottom line shares with it (plan 072 §D7b).
//!
//! One pure decision, [`terminal_notice`], over everything the terminal
//! area could be saying: the widgets draw its answer, `app.notice_dump`
//! reports it, and a press is checked against it again before anything
//! runs ([`click_still_lands`]). Which surface carries which kind of
//! message is written down in `docs/development/user-messaging.md`.

use roost_ipc::LocalBackendMode;

use crate::host_sidebar::SectionState;

/// What a terminal notice is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    /// A host's session ended under the frame the window is still
    /// showing; its shells are gone with it.
    SessionEnded,
    /// The local session under `local-backend = session` cannot be
    /// started, and the terminal area has nothing else to show.
    LocalSessionCannotStart,
}

impl NoticeKind {
    /// The spelling in `app.notice_dump` and `app.notice_answer`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SessionEnded => "session_ended",
            Self::LocalSessionCannotStart => "local_session_cannot_start",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        [Self::SessionEnded, Self::LocalSessionCannotStart]
            .into_iter()
            .find(|kind| kind.as_str() == value)
    }
}

/// Which notice a press or an answer was made on: the kind, and the
/// saved host it is about.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NoticeKey {
    pub kind: NoticeKind,
    pub subject: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Warning => "warning",
            Self::Error => "error",
        }
    }
}

/// What a notice's button does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeActionId {
    /// Start a fresh session on the notice's host.
    Start,
}

impl NoticeActionId {
    /// The spelling in `app.notice_dump` and `app.notice_answer`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
        }
    }

    pub fn from_wire(value: &str) -> Option<Self> {
        [Self::Start]
            .into_iter()
            .find(|action| action.as_str() == value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeAction {
    pub id: NoticeActionId,
    pub label: &'static str,
    pub primary: bool,
}

/// Where in the terminal area a notice draws.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Over a frame kept on screen, under a scrim.
    OverFrame,
    /// At the top of a terminal area with nothing in it, with no scrim.
    EmptyArea,
}

impl Placement {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OverFrame => "over_frame",
            Self::EmptyArea => "empty_area",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub key: NoticeKey,
    pub severity: Severity,
    pub placement: Placement,
    pub message: String,
    pub detail: Option<String>,
    pub actions: Vec<NoticeAction>,
}

/// The host whose tab the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShownHost<'a> {
    pub saved_id: &'a str,
    pub label: &'a str,
    pub state: SectionState,
}

/// The local session's saved host, whatever the window is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SlotFacts<'a> {
    pub saved_id: &'a str,
    pub state: SectionState,
    pub reason: Option<&'a str>,
    pub detail: Option<&'a str>,
    pub retry_armed: bool,
}

/// Everything [`terminal_notice`] decides from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoticeInput<'a> {
    pub mode: LocalBackendMode,
    pub shown: Option<ShownHost<'a>>,
    pub slot: Option<SlotFacts<'a>>,
    /// Nothing is selected, so the terminal area draws no terminal.
    pub area_empty: bool,
    /// A local-backend switch is in flight.
    pub switching: bool,
    /// The UI's own log file, as a notice would name it.
    pub log_path: &'a str,
}

/// What the terminal area says, if anything, in priority order: a frame
/// on screen speaks for its own host first.
pub fn terminal_notice(input: &NoticeInput<'_>) -> Option<Notice> {
    match input.shown {
        Some(shown) if shown.state == SectionState::Stopped => Some(session_ended(shown)),
        _ => None,
    }
}

fn session_ended(shown: ShownHost<'_>) -> Notice {
    Notice {
        key: NoticeKey {
            kind: NoticeKind::SessionEnded,
            subject: shown.saved_id.to_string(),
        },
        severity: Severity::Warning,
        placement: Placement::OverFrame,
        message: format!("The session on {} ended.", shown.label),
        detail: None,
        // Deliberately not "reconnect": the shells are gone, and the
        // button starts a fresh session rather than finding this one.
        actions: vec![NoticeAction {
            id: NoticeActionId::Start,
            label: "Start a new session",
            primary: true,
        }],
    }
}

/// Whether a press still names the notice it was drawn on.
///
/// A click carries the latency of a human hand: a second press, or one
/// on pixels the compositor has not repainted yet, can arrive after the
/// window has moved on — and "Start a new session" honored against a
/// host that has since come back would start a second one. So the same
/// notice must still be shown, and still offer that action.
pub fn click_still_lands(
    rendered: &NoticeKey,
    action: NoticeActionId,
    current: Option<&Notice>,
) -> bool {
    current.is_some_and(|notice| {
        notice.key == *rendered && notice.actions.iter().any(|offered| offered.id == action)
    })
}

/// Which showing of the terminal notice a reader saw (plan 072 panel
/// correction 16).
///
/// A key cannot tell a notice from the same notice shown again: a session
/// that ended, came back and ended again draws an identical strip, and an
/// answer read off the first must not press the second. Every change of
/// the shown key — to another, or to none — bumps the generation.
#[derive(Debug, Default)]
pub struct NoticeGeneration {
    shown: Option<NoticeKey>,
    generation: u64,
}

impl NoticeGeneration {
    /// Record what is shown now, and answer the generation it is.
    pub fn observe(&mut self, current: Option<&NoticeKey>) -> u64 {
        if self.shown.as_ref() != current {
            self.shown = current.cloned();
            self.generation += 1;
        }
        self.generation
    }

    /// [`click_still_lands`], for a caller that also names the
    /// generation it read.
    pub fn answer_lands(
        &mut self,
        rendered: &NoticeKey,
        generation: u64,
        action: NoticeActionId,
        current: Option<&Notice>,
    ) -> bool {
        self.observe(current.map(|notice| &notice.key)) == generation
            && click_still_lands(rendered, action, current)
    }
}

/// What the bottom line is showing: the toast, or a standing durability
/// failure it uncovers when the toast expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BottomLineSource {
    Status,
    Durability,
}

impl BottomLineSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Durability => "durability",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STATES: [SectionState; 6] = [
        SectionState::Local,
        SectionState::Connected,
        SectionState::Connecting,
        SectionState::Disconnected,
        SectionState::NeedsRestart,
        SectionState::Stopped,
    ];

    const MODES: [LocalBackendMode; 2] = [LocalBackendMode::InProcess, LocalBackendMode::Session];

    fn shown(state: SectionState) -> ShownHost<'static> {
        ShownHost {
            saved_id: "hs-1",
            label: "pop-os",
            state,
        }
    }

    /// A slot in the worst shape it can be in — down for good, with a
    /// reason and a detail and nothing retrying — so that a row this
    /// table says nothing about is a row the slot's state did not reach.
    fn settled_slot(state: SectionState) -> SlotFacts<'static> {
        SlotFacts {
            saved_id: "hs-0",
            state,
            reason: Some("roost-session exited early (status 1)"),
            detail: Some("roost-session exited before it was ready (exit status 1): boom"),
            retry_armed: false,
        }
    }

    fn input(
        mode: LocalBackendMode,
        shown: Option<ShownHost<'static>>,
        slot: Option<SlotFacts<'static>>,
        switching: bool,
    ) -> NoticeInput<'static> {
        NoticeInput {
            mode,
            shown,
            slot,
            area_empty: shown.is_none(),
            switching,
            log_path: "/home/u/.local/state/roost/roost.log",
        }
    }

    fn session_ended_key() -> NoticeKey {
        NoticeKey {
            kind: NoticeKind::SessionEnded,
            subject: "hs-1".into(),
        }
    }

    fn ended() -> Notice {
        terminal_notice(&input(
            LocalBackendMode::Session,
            Some(shown(SectionState::Stopped)),
            None,
            false,
        ))
        .expect("a stopped frame says so")
    }

    /// The whole decision, row by row: every state the shown host (and
    /// the slot) can be in, a frame on screen or an empty area, a switch
    /// in flight or not, under either backend. Only a frame whose session
    /// ended says anything, and it says so whatever else is true.
    #[test]
    fn only_a_frame_whose_session_ended_puts_a_notice_in_the_terminal_area() {
        for state in STATES {
            for on_screen in [true, false] {
                for switching in [false, true] {
                    for mode in MODES {
                        let notice = terminal_notice(&input(
                            mode,
                            on_screen.then(|| shown(state)),
                            Some(settled_slot(state)),
                            switching,
                        ));
                        let expected =
                            (on_screen && state == SectionState::Stopped).then(session_ended_key);
                        assert_eq!(
                            notice.map(|notice| notice.key),
                            expected,
                            "{state:?}, on screen {on_screen}, switching {switching}, {mode:?}"
                        );
                    }
                }
            }
        }
    }

    /// The strip's words are the ones it has always had (plan 037 §3.1),
    /// compared as literals so a reworded table cannot pass.
    #[test]
    fn a_session_that_ended_is_said_in_the_words_the_strip_always_used() {
        assert_eq!(
            ended(),
            Notice {
                key: session_ended_key(),
                severity: Severity::Warning,
                placement: Placement::OverFrame,
                message: "The session on pop-os ended.".into(),
                detail: None,
                actions: vec![NoticeAction {
                    id: NoticeActionId::Start,
                    label: "Start a new session",
                    primary: true,
                }],
            }
        );
    }

    #[test]
    fn a_press_lands_only_on_the_notice_it_was_drawn_on() {
        let current = ended();
        assert!(click_still_lands(
            &session_ended_key(),
            NoticeActionId::Start,
            Some(&current)
        ));
        assert!(
            !click_still_lands(&session_ended_key(), NoticeActionId::Start, None),
            "the notice is gone: a connect is already under way"
        );
    }

    #[test]
    fn a_press_drawn_on_another_notice_is_refused() {
        let current = ended();
        let other_host = NoticeKey {
            kind: NoticeKind::SessionEnded,
            subject: "hs-2".into(),
        };
        assert!(!click_still_lands(
            &other_host,
            NoticeActionId::Start,
            Some(&current)
        ));
        let other_kind = NoticeKey {
            kind: NoticeKind::LocalSessionCannotStart,
            subject: "hs-1".into(),
        };
        assert!(!click_still_lands(
            &other_kind,
            NoticeActionId::Start,
            Some(&current)
        ));
    }

    #[test]
    fn a_press_on_an_action_the_notice_no_longer_offers_is_refused() {
        let mut current = ended();
        current.actions.clear();
        assert!(!click_still_lands(
            &session_ended_key(),
            NoticeActionId::Start,
            Some(&current)
        ));
    }

    /// A notice that went away and came back is the same key but not the
    /// same showing, and an answer read off the first is refused.
    #[test]
    fn an_answer_from_an_earlier_showing_of_the_same_notice_is_refused() {
        let current = ended();
        let key = session_ended_key();
        let mut generation = NoticeGeneration::default();
        let first = generation.observe(Some(&key));
        assert_eq!(
            generation.observe(Some(&key)),
            first,
            "unchanged is unbumped"
        );
        let gone = generation.observe(None);
        assert_ne!(gone, first);
        let again = generation.observe(Some(&key));
        assert_ne!(again, gone);

        assert!(!generation.answer_lands(&key, first, NoticeActionId::Start, Some(&current)));
        assert!(generation.answer_lands(&key, again, NoticeActionId::Start, Some(&current)));
        assert!(
            !generation.answer_lands(&key, again, NoticeActionId::Start, None),
            "and the current generation still needs the notice on screen"
        );
    }

    #[test]
    fn every_wire_spelling_reads_back() {
        for kind in [
            NoticeKind::SessionEnded,
            NoticeKind::LocalSessionCannotStart,
        ] {
            assert_eq!(NoticeKind::from_wire(kind.as_str()), Some(kind));
        }
        assert_eq!(
            NoticeActionId::from_wire(NoticeActionId::Start.as_str()),
            Some(NoticeActionId::Start)
        );
        assert_eq!(NoticeKind::from_wire("nonesuch"), None);
        assert_eq!(NoticeActionId::from_wire("reconnect"), None);
    }
}
