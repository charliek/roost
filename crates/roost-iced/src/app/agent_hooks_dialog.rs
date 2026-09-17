//! The agent-hooks consent card (plan 064 §3.5): one dialog, two modes.
//!
//! **First run** is raised once per process on the local chrome when
//! nobody has answered the `agent-hooks` key yet; **preferences** is the
//! same card opened deliberately from `Agent Hooks…` in the command
//! palette. They differ in three things and nothing else: what the two
//! buttons say, whether each row carries a status line, and who raised
//! it. The rows, the copy and the keyboard ring are one implementation
//! because they are one card.
//!
//! Pure on purpose, like [`super::bootstrap`] and
//! [`super::host_notice`]: the prefill rule, the status wording and the
//! ring are the parts that have to be right for every combination, and
//! a table test is the only way to say that once. `app.rs` renders what
//! these answer and adds nothing.
//!
//! The strings are the plan's, verbatim, and the Mac sheet (C8) says the
//! same words. They live here rather than at the widgets so
//! `app.dialog_dump` and the card cannot disagree about the copy.

use iced::keyboard::key::Named;
use iced::keyboard::{self, Key};
use roost_agent::Agent;
use roost_agent_install::Status;
use roost_ipc::messages::AgentSetHooksAgents;
use roost_ui_model::config::AgentHooks;

pub(crate) const TITLE: &str = "Agent hooks";

pub(crate) const LEDE: &str = "Roost adds a hook to each agent you switch on so its tabs show \
     status and send notifications. It edits the agent files named below, plus Roost's own \
     config, and nothing else. roostctl agent uninstall --all puts the agent files back.";

pub(crate) const FOOTER: &str = "Switching an agent on applies here and on every host this Roost \
     connects to. Switching one off applies here only. Change it any time from Agent Hooks… in \
     the command palette.";

/// codex is the one agent whose install writes a second file, and the
/// only one that would otherwise put a dialog of its own in front of the
/// user the first time a hook fires.
const CODEX_NOTE: &str =
    "Also pre-trusts Roost's hooks in config.toml so codex won't show its own review dialog.";

/// OpenCode has no command hooks at all, so what Roost installs there is
/// a plugin — a different kind of thing in a different place, and the
/// row says so rather than letting "hook" stand for both.
const OPENCODE_NOTE: &str = "This is a plugin that runs inside OpenCode, not a hook entry.";

/// Which of the two cards this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardMode {
    /// Nobody has answered the key yet, and the app asked unprompted.
    FirstRun,
    /// The user opened it from the palette.
    Preferences,
}

impl CardMode {
    /// What `app.dialog_dump` reports.
    pub(crate) fn wire_name(self) -> &'static str {
        match self {
            Self::FirstRun => "first_run",
            Self::Preferences => "preferences",
        }
    }

    fn dismiss_label(self) -> &'static str {
        match self {
            Self::FirstRun => "Decide later",
            Self::Preferences => "Cancel",
        }
    }

    /// The primary button. Both modes name the count, because the button
    /// is the last thing read before five config files are edited — and
    /// both have a distinct spelling for zero, since "Instrument 0" and
    /// a bare "Apply" would both hide that confirming now writes `off`.
    fn confirm_label(self, on: usize) -> String {
        match (self, on) {
            (Self::FirstRun, 0) => "Turn off".to_string(),
            (Self::FirstRun, on) => format!("Instrument {on}"),
            (Self::Preferences, 0) => "Apply: turn off".to_string(),
            (Self::Preferences, _) => "Apply".to_string(),
        }
    }
}

/// One agent's row. All five are always drawn, present or not: an agent
/// the user has only on a host is still something they can consent to
/// here, and a row that vanished would make the card's list depend on
/// what happens to be installed today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHooksRow {
    pub(crate) agent: Agent,
    pub(crate) on: bool,
    pub(crate) found: bool,
    /// Rendered in preferences mode only, so `None` on first run — the
    /// dump reports what the card shows, not what it could have.
    pub(crate) status: Option<String>,
    pub(crate) files: Vec<String>,
}

impl AgentHooksRow {
    /// The chip beside the name.
    pub(crate) fn chip(&self) -> &'static str {
        if self.found {
            "found here"
        } else {
            "not found here"
        }
    }
}

/// The display name for each agent — what a person calls the product,
/// not the `source` token the wire and the config key use.
pub(crate) fn display_name(agent: Agent) -> &'static str {
    match agent {
        Agent::Claude => "Claude Code",
        Agent::Codex => "Codex",
        Agent::Grok => "Grok",
        Agent::Cursor => "Cursor",
        Agent::Opencode => "OpenCode",
    }
}

/// The extra sentence two of the five rows carry.
pub(crate) fn note(agent: Agent) -> Option<&'static str> {
    match agent {
        Agent::Codex => Some(CODEX_NOTE),
        Agent::Opencode => Some(OPENCODE_NOTE),
        Agent::Claude | Agent::Grok | Agent::Cursor => None,
    }
}

/// Which switches start on.
///
/// **The key, never the disk.** A row that is wired but not named in the
/// key starts off, so Apply taking its entries back out is a visible act
/// the user chose rather than a silent narrowing of what they already
/// had. `Ask` is the one state with no key to read: there the fallback
/// is what is installed here, which is plan 064's D2 — an agent that
/// exists only on a host stays off until somebody switches it on once.
fn prefill(key: &AgentHooks, status: &Status) -> bool {
    match key {
        AgentHooks::Ask => status.present,
        AgentHooks::Off | AgentHooks::Allow { .. } => status.allowed,
    }
}

/// The status line preferences mode shows under each row.
///
/// The order of the tests is the meaning: the record and the agent's own
/// files are independent sources ([`Status`] keeps them apart on
/// purpose), and "is there anything of Roost's in these files" has to be
/// answered before any version can be. `up_to_date` alone cannot say it
/// — an agent the mode does not name plans no edits and so looks current
/// while carrying nothing at all.
pub(crate) fn row_status(status: &Status) -> String {
    if !status.present {
        return "not found".to_string();
    }
    if !status.entries_on_disk {
        return "found, not wired".to_string();
    }
    if !status.allowed {
        return "wired, not allowed".to_string();
    }
    if status.up_to_date {
        return format!("wired v{}", roost_agent_install::INTEGRATION_VERSION);
    }
    match status.wired {
        Some(version) => format!("wired v{version}, out of date"),
        // Entries on disk with no record to name their version: a wiped
        // `~/.config/roost`, or a record restored from a backup.
        None => "wired, out of date".to_string(),
    }
}

/// Every row the card will draw, from one `roost_agent_install::status`
/// walk plus the key it was resolved against.
///
/// `statuses` arrives in `ALL_AGENTS` order because `status` walks that
/// list, which is also the order the card draws and the dump reports.
pub(crate) fn rows(mode: CardMode, key: &AgentHooks, statuses: &[Status]) -> Vec<AgentHooksRow> {
    statuses
        .iter()
        .map(|status| AgentHooksRow {
            agent: status.agent,
            on: prefill(key, status),
            found: status.present,
            status: matches!(mode, CardMode::Preferences).then(|| row_status(status)),
            files: status
                .files
                .iter()
                .map(|path| path.display().to_string())
                .collect(),
        })
        .collect()
}

/// Where the card's Tab traversal sits.
///
/// Its own ring rather than the toolkit's, for [`super::host_dialog`]'s
/// reason — iced 0.14 cannot focus a button — but with none of Add
/// Host's widget-tree probing: this card has no text input, so nothing
/// in it can take the caret and the stored ring is never stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardFocus {
    Switch(usize),
    Confirm,
    Cancel,
}

/// What a key press means once the ring has been consulted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardKey {
    Toggle(usize),
    Confirm,
    Cancel,
    Nothing,
}

/// Advance the ring one stop, wrapping.
///
/// The order is the form's: the rows in reading order, then the primary
/// action, then the dismiss — the same rule Add Host's ring follows, so
/// Cancel still comes last however the row is drawn.
pub(crate) fn step(at: CardFocus, backwards: bool, switches: usize) -> CardFocus {
    let stops = switches + 2;
    let index = match at {
        CardFocus::Switch(index) if index < switches => index,
        CardFocus::Switch(_) => 0,
        CardFocus::Confirm => switches,
        CardFocus::Cancel => switches + 1,
    };
    let next = if backwards {
        (index + stops - 1) % stops
    } else {
        (index + 1) % stops
    };
    stop_at(next, switches)
}

fn stop_at(index: usize, switches: usize) -> CardFocus {
    match index.checked_sub(switches) {
        None => CardFocus::Switch(index),
        Some(0) => CardFocus::Confirm,
        Some(_) => CardFocus::Cancel,
    }
}

/// Whether this press is a bare Tab or Shift+Tab, and which way it goes.
pub(crate) fn tab_step_direction(event: &keyboard::Event) -> Option<bool> {
    super::host_dialog::tab_step_direction(event)
}

/// What Enter, Space or Escape means to the card.
///
/// Space is the button-activation key and so is also what toggles a
/// switch — that is the platform convention for a focused control, and
/// it takes no modifier for Add Host's reason (Ctrl/Alt/Super+Space
/// belong to a compositor's input switcher and the app's accelerators).
///
/// Enter is the **dialog's primary action wherever the ring sits**,
/// including on a switch: that is what Enter means in a dialog, and it
/// is the same rule Add Host follows from a field. A user who wants to
/// flip a switch from the keyboard presses Space, which is what the
/// switch itself responds to.
pub(crate) fn key_action(event: &keyboard::Event, at: CardFocus) -> CardKey {
    let keyboard::Event::KeyPressed {
        key,
        repeat: false,
        modifiers,
        ..
    } = event
    else {
        return CardKey::Nothing;
    };
    let bare = !(modifiers.control() || modifiers.alt() || modifiers.logo());
    match (key.as_ref(), at) {
        (Key::Named(Named::Escape), _) => CardKey::Cancel,
        (Key::Named(Named::Enter), CardFocus::Cancel) => CardKey::Cancel,
        (Key::Named(Named::Enter), _) => CardKey::Confirm,
        (Key::Named(Named::Space), CardFocus::Switch(index)) if bare => CardKey::Toggle(index),
        (Key::Named(Named::Space), CardFocus::Confirm) if bare => CardKey::Confirm,
        (Key::Named(Named::Space), CardFocus::Cancel) if bare => CardKey::Cancel,
        _ => CardKey::Nothing,
    }
}

/// The card's live contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHooksDraft {
    mode: CardMode,
    rows: Vec<AgentHooksRow>,
    focus: CardFocus,
}

impl AgentHooksDraft {
    pub(crate) fn new(mode: CardMode, rows: Vec<AgentHooksRow>) -> Self {
        Self {
            mode,
            rows,
            focus: CardFocus::Switch(0),
        }
    }

    pub(crate) fn mode(&self) -> CardMode {
        self.mode
    }

    pub(crate) fn rows(&self) -> &[AgentHooksRow] {
        &self.rows
    }

    pub(crate) fn focus(&self) -> CardFocus {
        self.focus
    }

    pub(crate) fn step_focus(&mut self, backwards: bool) {
        self.focus = step(self.focus, backwards, self.rows.len());
    }

    /// Flip one switch, and move the ring onto it — the ring must never
    /// be drawn somewhere the user is not acting.
    pub(crate) fn toggle(&mut self, index: usize) {
        let Some(row) = self.rows.get_mut(index) else {
            return;
        };
        row.on = !row.on;
        self.focus = CardFocus::Switch(index);
    }

    /// `app.dialog_answer`'s `toggle:<agent>`. By name rather than by
    /// index so the harness names what it means; an agent no row carries
    /// is an error rather than a silent no-op.
    pub(crate) fn toggle_named(&mut self, name: &str) -> Result<(), String> {
        let index = self
            .rows
            .iter()
            .position(|row| row.agent.source() == name)
            .ok_or_else(|| format!("the agent-hooks card has no row for {name:?}"))?;
        self.toggle(index);
        Ok(())
    }

    pub(crate) fn on_count(&self) -> usize {
        self.rows.iter().filter(|row| row.on).count()
    }

    /// What confirming sends to `agent.set_hooks`.
    ///
    /// Every switch off is the word `off`, not an empty list: the op
    /// refuses an empty `agents`, and `off` is the spelling that says
    /// "the user answered, and the answer was nothing" — an empty value
    /// would parse back as `Ask` and bring this card round again.
    pub(crate) fn wire_agents(&self) -> AgentSetHooksAgents {
        let names: Vec<String> = self
            .rows
            .iter()
            .filter(|row| row.on)
            .map(|row| row.agent.source().to_string())
            .collect();
        if names.is_empty() {
            AgentSetHooksAgents::Off
        } else {
            AgentSetHooksAgents::List(names)
        }
    }

    pub(crate) fn dismiss_label(&self) -> &'static str {
        self.mode.dismiss_label()
    }

    pub(crate) fn confirm_label(&self) -> String {
        self.mode.confirm_label(self.on_count())
    }

    /// Both buttons in render order, the dismissing one first — what
    /// `app.dialog_dump` reports and what the row draws.
    pub(crate) fn buttons(&self) -> Vec<String> {
        vec![self.dismiss_label().to_string(), self.confirm_label()]
    }

    /// Which button the ring is drawn around, if either.
    pub(super) fn button_ring(&self) -> super::host_dialog::ButtonRing {
        super::host_dialog::ButtonRing {
            cancel: self.focus == CardFocus::Cancel,
            confirm: self.focus == CardFocus::Confirm,
        }
    }
}

/// What a finished survey may do with its result.
///
/// Pure, and separate from the `App` method that acts on it, because
/// three of the four arms are races: the key answered by another process
/// while this launch was asking, another dialog taken the screen in the
/// meantime, a machine with nothing installed. None of them is
/// reproducible from outside the process, and all of them decide whether
/// Roost writes into somebody else's config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurveyVerdict {
    Raise,
    /// Nothing is installed here, so there is nothing to consent about.
    NoAgent,
    /// Somebody answered the key while this launch was asking.
    AlreadyAnswered,
    /// The user is already answering a different question.
    ScreenTaken,
}

pub(crate) fn survey_verdict(
    mode: CardMode,
    any_present: bool,
    key: &AgentHooks,
    dialog_open: bool,
) -> SurveyVerdict {
    if mode == CardMode::FirstRun {
        if !any_present {
            return SurveyVerdict::NoAgent;
        }
        if *key != AgentHooks::Ask {
            return SurveyVerdict::AlreadyAnswered;
        }
    }
    if dialog_open {
        return SurveyVerdict::ScreenTaken;
    }
    SurveyVerdict::Raise
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_agent_install::{Home, ALL_AGENTS};
    use std::path::PathBuf;

    fn status(agent: Agent) -> Status {
        Status {
            agent,
            present: true,
            wired: None,
            entries_on_disk: false,
            up_to_date: false,
            noticed: false,
            allowed: false,
            files: roost_agent_install::owned_files(&Home::rooted("/home/u"), agent),
            skipped: None,
            warnings: Vec::new(),
        }
    }

    fn all() -> Vec<Status> {
        ALL_AGENTS.into_iter().map(status).collect()
    }

    /// The five rows, in `ALL_AGENTS` order, whatever the key says.
    #[test]
    fn every_agent_gets_a_row_in_the_inventory_order() {
        let rows = rows(CardMode::FirstRun, &AgentHooks::Ask, &all());
        assert_eq!(
            rows.iter().map(|row| row.agent).collect::<Vec<_>>(),
            ALL_AGENTS.to_vec()
        );
        assert_eq!(
            rows.iter()
                .map(|row| display_name(row.agent))
                .collect::<Vec<_>>(),
            ["Claude Code", "Codex", "Grok", "Cursor", "OpenCode"]
        );
        // codex is the one agent with two files, and the paths are the
        // install engine's own rather than a second table here.
        let codex = rows.iter().find(|row| row.agent == Agent::Codex).unwrap();
        assert_eq!(codex.files.len(), 2, "{codex:?}");
        assert!(rows
            .iter()
            .all(|row| !row.files.is_empty() && row.status.is_none()));
    }

    /// Only two rows carry an extra sentence, and they are the two whose
    /// install is not "one hook entry in one file".
    #[test]
    fn the_two_rows_with_something_more_to_say_are_codex_and_opencode() {
        assert_eq!(note(Agent::Codex), Some(CODEX_NOTE));
        assert_eq!(note(Agent::Opencode), Some(OPENCODE_NOTE));
        for agent in [Agent::Claude, Agent::Grok, Agent::Cursor] {
            assert_eq!(note(agent), None, "{}", agent.source());
        }
    }

    /// First run has no key, so what is installed here is the fallback —
    /// and an agent that exists only on a host stays off (D2).
    #[test]
    fn an_unanswered_key_starts_the_installed_agents_on_and_the_rest_off() {
        let mut statuses = all();
        statuses[2].present = false;
        let rows = rows(CardMode::FirstRun, &AgentHooks::Ask, &statuses);
        assert_eq!(
            rows.iter().map(|row| row.on).collect::<Vec<_>>(),
            [true, true, false, true, true]
        );
        assert_eq!(rows[2].chip(), "not found here");
        assert_eq!(rows[0].chip(), "found here");
    }

    /// The prefill is the key, not the disk: an agent that is wired and
    /// not named starts OFF, so Apply removing it is something the user
    /// can see they are doing.
    #[test]
    fn a_wired_agent_the_key_does_not_name_starts_off() {
        let mut statuses = all();
        statuses[1].entries_on_disk = true;
        statuses[1].wired = Some(3);
        let key = AgentHooks::allow(["claude"]);
        statuses[0].allowed = true;
        let rows = rows(CardMode::Preferences, &key, &statuses);
        assert!(rows[0].on, "the key names claude");
        assert!(!rows[1].on, "codex is wired and unnamed: it starts off");
        assert_eq!(rows[1].status.as_deref(), Some("wired, not allowed"));
    }

    /// `off` is an answer, so every switch starts off — including for an
    /// agent whose entries are still on disk.
    #[test]
    fn off_starts_every_switch_off() {
        let rows = rows(CardMode::Preferences, &AgentHooks::Off, &all());
        assert!(rows.iter().all(|row| !row.on));
    }

    /// The five wordings, and the order they are decided in.
    #[test]
    fn the_status_line_reads_the_record_and_the_disk_apart() {
        let mut absent = status(Agent::Grok);
        absent.present = false;
        assert_eq!(row_status(&absent), "not found");

        let mut bare = status(Agent::Grok);
        bare.allowed = true;
        // A present agent with nothing of Roost's in its files plans no
        // edits only when it is unwired AND unnamed; here it is named,
        // so the honest answer is the disk's.
        assert_eq!(row_status(&bare), "found, not wired");

        let mut unnamed = status(Agent::Grok);
        unnamed.entries_on_disk = true;
        unnamed.up_to_date = true;
        assert_eq!(
            row_status(&unnamed),
            "wired, not allowed",
            "an unnamed agent plans no edits, which is not the same as being current"
        );

        let mut current = status(Agent::Grok);
        current.entries_on_disk = true;
        current.allowed = true;
        current.up_to_date = true;
        current.wired = Some(1);
        assert_eq!(
            row_status(&current),
            format!("wired v{}", roost_agent_install::INTEGRATION_VERSION),
            "the disk is current, whatever version the record remembers"
        );

        let mut stale = status(Agent::Grok);
        stale.entries_on_disk = true;
        stale.allowed = true;
        stale.wired = Some(2);
        assert_eq!(row_status(&stale), "wired v2, out of date");

        let mut forgotten = stale.clone();
        forgotten.wired = None;
        assert_eq!(row_status(&forgotten), "wired, out of date");
    }

    fn draft(mode: CardMode, on: [bool; 5]) -> AgentHooksDraft {
        let mut rows = rows(mode, &AgentHooks::Ask, &all());
        for (row, on) in rows.iter_mut().zip(on) {
            row.on = on;
        }
        AgentHooksDraft::new(mode, rows)
    }

    /// Both modes name the count on the primary, and both have their own
    /// spelling for zero — "Instrument 0" would hide that confirming
    /// writes `off`.
    #[test]
    fn the_buttons_say_what_confirming_will_do() {
        let card = draft(CardMode::FirstRun, [true, true, false, false, false]);
        assert_eq!(card.buttons(), ["Decide later", "Instrument 2"]);
        let card = draft(CardMode::FirstRun, [false; 5]);
        assert_eq!(card.buttons(), ["Decide later", "Turn off"]);

        let card = draft(CardMode::Preferences, [true, false, false, false, false]);
        assert_eq!(card.buttons(), ["Cancel", "Apply"]);
        let card = draft(CardMode::Preferences, [false; 5]);
        assert_eq!(card.buttons(), ["Cancel", "Apply: turn off"]);
    }

    /// Every switch off travels as the word `off`, never as an empty
    /// list — which the op refuses, and which would parse back as `Ask`.
    #[test]
    fn what_confirming_sends_is_the_switches_that_are_on() {
        let card = draft(CardMode::FirstRun, [true, false, true, false, false]);
        assert_eq!(
            card.wire_agents(),
            AgentSetHooksAgents::List(vec!["claude".to_string(), "grok".to_string()])
        );
        assert_eq!(
            draft(CardMode::FirstRun, [false; 5]).wire_agents(),
            AgentSetHooksAgents::Off
        );
    }

    /// A toggle by name is the harness's spelling; an agent no row
    /// carries is an error rather than a silent no-op.
    #[test]
    fn a_toggle_names_its_agent() {
        let mut card = draft(CardMode::FirstRun, [false; 5]);
        card.toggle_named("codex").unwrap();
        assert!(card.rows()[1].on);
        assert_eq!(
            card.focus(),
            CardFocus::Switch(1),
            "the ring follows the act"
        );
        card.toggle_named("codex").unwrap();
        assert!(!card.rows()[1].on, "a second toggle switches it back off");
        assert!(card.toggle_named("gemini").unwrap_err().contains("gemini"));
    }

    /// Five switches, then the primary, then the dismiss — and back.
    #[test]
    fn tab_walks_the_switches_then_the_buttons_and_wraps() {
        let mut at = CardFocus::Switch(0);
        let mut walked = vec![at];
        for _ in 0..7 {
            at = step(at, false, 5);
            walked.push(at);
        }
        assert_eq!(
            walked,
            vec![
                CardFocus::Switch(0),
                CardFocus::Switch(1),
                CardFocus::Switch(2),
                CardFocus::Switch(3),
                CardFocus::Switch(4),
                CardFocus::Confirm,
                CardFocus::Cancel,
                CardFocus::Switch(0),
            ]
        );
        assert_eq!(step(CardFocus::Switch(0), true, 5), CardFocus::Cancel);
        assert_eq!(step(CardFocus::Cancel, true, 5), CardFocus::Confirm);
    }

    fn press(key: Key, modifiers: keyboard::Modifiers) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Space),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        }
    }

    /// Space acts on whatever the ring is on; Enter is the dialog's
    /// primary action wherever it sits, which is what Enter means in a
    /// dialog — a switch is flipped with Space.
    #[test]
    fn space_acts_on_the_ring_and_enter_is_always_the_primary() {
        let bare = keyboard::Modifiers::default();
        assert_eq!(
            key_action(&press(Key::Named(Named::Space), bare), CardFocus::Switch(3)),
            CardKey::Toggle(3)
        );
        assert_eq!(
            key_action(&press(Key::Named(Named::Space), bare), CardFocus::Confirm),
            CardKey::Confirm
        );
        assert_eq!(
            key_action(&press(Key::Named(Named::Space), bare), CardFocus::Cancel),
            CardKey::Cancel
        );
        assert_eq!(
            key_action(&press(Key::Named(Named::Enter), bare), CardFocus::Switch(3)),
            CardKey::Confirm
        );
        assert_eq!(
            key_action(&press(Key::Named(Named::Enter), bare), CardFocus::Cancel),
            CardKey::Cancel
        );
        // Esc is "Decide later" on first run and Cancel in preferences —
        // one action, whichever word the button carries.
        for at in [CardFocus::Switch(0), CardFocus::Confirm, CardFocus::Cancel] {
            assert_eq!(
                key_action(&press(Key::Named(Named::Escape), bare), at),
                CardKey::Cancel
            );
        }
    }

    /// Space takes no modifier, for `host_dialog`'s reason.
    #[test]
    fn a_modified_space_acts_on_nothing() {
        for modifiers in [
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
        ] {
            assert_eq!(
                key_action(
                    &press(Key::Named(Named::Space), modifiers),
                    CardFocus::Switch(0)
                ),
                CardKey::Nothing,
                "{modifiers:?}+Space must not flip a switch"
            );
        }
    }

    /// The ring is drawn on at most one button, and on neither while it
    /// sits on a switch.
    #[test]
    fn the_button_ring_follows_the_focus() {
        let mut card = draft(CardMode::Preferences, [false; 5]);
        assert_eq!(
            card.button_ring(),
            super::super::host_dialog::ButtonRing::default(),
            "no button is ringed while the ring sits on a switch"
        );
        for _ in 0..5 {
            card.step_focus(false);
        }
        assert_eq!(card.focus(), CardFocus::Confirm);
        assert!(card.button_ring().confirm && !card.button_ring().cancel);
        card.step_focus(false);
        assert!(card.button_ring().cancel && !card.button_ring().confirm);
    }

    /// The files come from the install engine, so a row names the same
    /// paths an uninstall would touch.
    #[test]
    fn the_rows_name_the_files_the_install_engine_owns() {
        let rows = rows(CardMode::Preferences, &AgentHooks::Ask, &all());
        let home = Home::rooted("/home/u");
        for row in &rows {
            let owned: Vec<String> = roost_agent_install::owned_files(&home, row.agent)
                .iter()
                .map(|path: &PathBuf| path.display().to_string())
                .collect();
            assert_eq!(row.files, owned, "{}", row.agent.source());
        }
    }

    #[test]
    fn a_first_run_survey_stands_down_for_an_answer_given_while_it_ran() {
        let allow = AgentHooks::allow(["claude"]);
        for key in [AgentHooks::Off, allow] {
            assert_eq!(
                survey_verdict(CardMode::FirstRun, true, &key, false),
                SurveyVerdict::AlreadyAnswered,
                "{key:?}"
            );
        }
        assert_eq!(
            survey_verdict(CardMode::FirstRun, true, &AgentHooks::Ask, false),
            SurveyVerdict::Raise
        );
    }

    /// Preferences is a question the user just asked for, so an answered
    /// key is exactly what it is there to show.
    #[test]
    fn preferences_is_raised_over_any_key() {
        assert_eq!(
            survey_verdict(CardMode::Preferences, false, &AgentHooks::Off, false),
            SurveyVerdict::Raise
        );
    }

    #[test]
    fn nothing_installed_here_asks_nothing() {
        assert_eq!(
            survey_verdict(CardMode::FirstRun, false, &AgentHooks::Ask, false),
            SurveyVerdict::NoAgent
        );
    }

    /// Either mode stands down rather than replacing a dialog the user
    /// is already answering — including their half-filled Add Host form.
    #[test]
    fn a_survey_never_takes_the_screen_from_another_dialog() {
        for mode in [CardMode::FirstRun, CardMode::Preferences] {
            assert_eq!(
                survey_verdict(mode, true, &AgentHooks::Ask, true),
                SurveyVerdict::ScreenTaken,
                "{mode:?}"
            );
        }
    }
}
