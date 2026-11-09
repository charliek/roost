//! The "Agent integration" card (plan 064 §3.5, reshaped by the move
//! off config writes): one dialog, two modes.
//!
//! **First run** asks only what still needs an answer: whether Roost may
//! write a file agent's own config (today only Cursor's `hooks.json`).
//! It is raised once per process when neither agent key is answered and
//! such an agent is installed. Otherwise there is nothing to ask: Claude
//! Code and OpenCode connect through the tab environment and Codex and
//! Grok report over OSC 7501, none of which touches a user file, so they
//! are on by default with no card. **Preferences** is the same card
//! opened deliberately from `Agent Integration…` in the command palette,
//! with a switch and a status line per agent.
//!
//! Pure on purpose, like [`super::bootstrap`] and
//! [`super::host_notice`]: the prefill rule, the status wording and the
//! ring are the parts that have to be right for every combination, and
//! a table test is the only way to say that once. `app.rs` renders what
//! these answer and adds nothing. The strings live here rather than at
//! the widgets so `app.dialog_dump` and the card cannot disagree.

use iced::keyboard::key::Named;
use iced::keyboard::{self, Key};
use roost_agent::{Agent, ALL_AGENTS};
use roost_agent_install::{Delivery, Status};
use roost_ipc::messages::AgentSetHooksAgents;

pub(crate) const TITLE: &str = "Agent integration";

const INTRO: &str = "Roost shows what your coding agents are doing (working, needs you, done) \
     in the sidebar, the Agents panel and notifications.";

const ASK: &str = "One agent needs a change to its own settings file to report fully:";

const PREFERENCES_LEDE: &str = "Roost shows what your coding agents are doing in the sidebar, \
     the Agents panel and notifications. Only rows that say so change an agent's own settings.";

const FIRST_RUN_FOOTER: &str =
    "Change this, or switch any agent off, from Agent Integration… in the command palette.";

const PREFERENCES_FOOTER: &str = "These switches apply to this machine. Turning Cursor on \
     also asks the hosts you connect to for its hooks.";

/// The agents with no Roost row: they speak OSC 7501 and Roost has
/// nothing to switch for them.
pub(crate) const BUILT_IN_LINE: &str =
    "Also built in: crush and pi report their status on their own.";

const CLAUDE_BLOCKED: &str = "Skipped: blocked by your organization's Claude policy";

/// Which of the two cards this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CardMode {
    /// Nobody has answered yet and a file agent is installed, so the app
    /// asked unprompted.
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

    /// The primary button. First run records an answer whatever the one
    /// switch says, so it is "Continue"; preferences has its own spelling
    /// for zero, since a bare "Apply" would hide that it writes `off`.
    fn confirm_label(self, on: usize) -> String {
        match (self, on) {
            (Self::FirstRun, _) => "Continue".to_string(),
            (Self::Preferences, 0) => "Apply: turn off".to_string(),
            (Self::Preferences, _) => "Apply".to_string(),
        }
    }
}

/// One agent's row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentHooksRow {
    pub(crate) agent: Agent,
    pub(crate) on: bool,
    pub(crate) found: bool,
    pub(crate) delivery: Delivery,
    /// Rendered in preferences mode only, so `None` on first run — the
    /// dump reports what the card shows, not what it could have.
    pub(crate) status: Option<String>,
    /// The files Roost would write: only a file agent has any.
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

/// The sentence under each row: what connecting it costs, in files.
pub(crate) fn note(agent: Agent, mode: CardMode) -> &'static str {
    match (agent, mode) {
        (Agent::Claude, _) => {
            "Connected inside Roost tabs only. Nothing is written to ~/.claude. Needs Claude \
             Code 2.1.280 or newer."
        }
        (Agent::Opencode, _) => {
            "Connected inside Roost tabs only. Nothing is written to ~/.config/opencode."
        }
        (Agent::Codex, _) => "Built in. Codex reports its status to the terminal itself.",
        (Agent::Grok, _) => "Built in. Grok reports its status to the terminal itself.",
        (Agent::Cursor, CardMode::FirstRun) => {
            "Adds Roost's entries to the file below, because Cursor reads its turn-end hook \
             only from that file. Without it, Roost shows nothing for Cursor."
        }
        (Agent::Cursor, CardMode::Preferences) => {
            "Adds Roost's entries to the file below, because Cursor reads its turn-end hook \
             only from that file."
        }
    }
}

/// "A", "A and B", "A, B and C".
fn and_list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The first-run lede, naming the installed agents already connected.
fn first_run_lede(connected: &[Agent]) -> String {
    if connected.is_empty() {
        return format!("{INTRO} {ASK}");
    }
    let names: Vec<&str> = connected.iter().map(|a| display_name(*a)).collect();
    format!(
        "{INTRO} {} {} already connected, with nothing written to their settings. {ASK}",
        and_list(&names),
        if names.len() == 1 { "is" } else { "are" }
    )
}

/// A file agent's status line: the record and the agent's own files are
/// independent sources ([`Status`] keeps them apart on purpose), and "is
/// there anything of Roost's in these files" is answered before any
/// version can be.
fn file_status(status: &Status) -> String {
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

/// The status line preferences mode shows under each row, then what the
/// retire left behind or what came back after it.
pub(crate) fn row_status(status: &Status, claude_blocked: bool) -> String {
    let delivery = Delivery::of(status.agent);
    let mut line = if !status.present {
        "not found".to_string()
    } else if !status.on {
        "Off".to_string()
    } else {
        match delivery {
            Delivery::File => file_status(status),
            Delivery::Env if status.agent == Agent::Claude && claude_blocked => {
                CLAUDE_BLOCKED.to_string()
            }
            Delivery::Env => "Connected · nothing written".to_string(),
            Delivery::Osc7501 => "Built in".to_string(),
        }
    };
    if status.came_back {
        line.push_str(&format!(
            " · an old Roost entry came back after Roost removed it (synced from an older \
             Roost?): roostctl agent uninstall {} removes it",
            status.agent.source()
        ));
    } else if let Some(left) = status.retired.as_ref().filter(|r| !r.leftovers.is_empty()) {
        let count = left.leftovers.len();
        line.push_str(&format!(
            " · {count} old Roost entr{} left in {} that Roost can't be sure it added: \
             roostctl agent status lists {}",
            if count == 1 { "y" } else { "ies" },
            left.leftovers[0].file,
            if count == 1 { "it" } else { "them" }
        ));
    }
    line
}

fn row(mode: CardMode, status: &Status, claude_blocked: bool) -> AgentHooksRow {
    let delivery = Delivery::of(status.agent);
    AgentHooksRow {
        agent: status.agent,
        // First run asks a question nobody has answered: the file agent
        // starts off, because it is the one row that writes a user file.
        on: mode == CardMode::Preferences && status.on,
        found: status.present,
        delivery,
        status: (mode == CardMode::Preferences).then(|| row_status(status, claude_blocked)),
        files: if delivery == Delivery::File {
            status
                .files
                .iter()
                .map(|path| path.display().to_string())
                .collect()
        } else {
            Vec::new()
        },
    }
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
    lede: String,
    rows: Vec<AgentHooksRow>,
    /// Agents that are on and have no row on this card: on first run,
    /// the defaults the answer records alongside the file agent's.
    kept_on: Vec<Agent>,
    focus: CardFocus,
}

impl AgentHooksDraft {
    /// The card for `statuses` (one per agent, in `ALL_AGENTS` order,
    /// from one `roost_agent_install::status` walk). First run carries
    /// only the installed file agents; preferences carries all five.
    pub(crate) fn new(mode: CardMode, statuses: &[Status], claude_blocked: bool) -> Self {
        let shown = |status: &Status| match mode {
            CardMode::FirstRun => Delivery::of(status.agent) == Delivery::File && status.present,
            CardMode::Preferences => true,
        };
        let rows = statuses
            .iter()
            .filter(|status| shown(status))
            .map(|status| row(mode, status, claude_blocked))
            .collect();
        let kept_on = statuses
            .iter()
            .filter(|status| !shown(status) && status.on)
            .map(|status| status.agent)
            .collect();
        let lede = match mode {
            CardMode::FirstRun => first_run_lede(
                &statuses
                    .iter()
                    .filter(|s| s.present && s.on && Delivery::of(s.agent) != Delivery::File)
                    .filter(|s| !(s.agent == Agent::Claude && claude_blocked))
                    .map(|s| s.agent)
                    .collect::<Vec<_>>(),
            ),
            CardMode::Preferences => PREFERENCES_LEDE.to_string(),
        };
        Self {
            mode,
            lede,
            rows,
            kept_on,
            focus: CardFocus::Switch(0),
        }
    }

    pub(crate) fn lede(&self) -> &str {
        &self.lede
    }

    /// Whether this card has a question on it: a first-run card with no
    /// row is never raised.
    pub(crate) fn asks(&self) -> bool {
        !self.rows.is_empty()
    }

    pub(crate) fn footer(&self) -> &'static str {
        match self.mode {
            CardMode::FirstRun => FIRST_RUN_FOOTER,
            CardMode::Preferences => PREFERENCES_FOOTER,
        }
    }

    /// Preferences names the agents that need no switch at all.
    pub(crate) fn built_in_line(&self) -> Option<&'static str> {
        (self.mode == CardMode::Preferences).then_some(BUILT_IN_LINE)
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

    /// What confirming sends to `agent.set_hooks`: every agent that is
    /// on, shown or not, in `ALL_AGENTS` order.
    ///
    /// Every agent off is the word `off`, not an empty list: the op
    /// refuses an empty `agents`, and `off` is the spelling that says
    /// "the user answered, and the answer was nothing".
    pub(crate) fn wire_agents(&self) -> AgentSetHooksAgents {
        let names: Vec<String> = ALL_AGENTS
            .into_iter()
            .filter(|agent| {
                self.kept_on.contains(agent)
                    || self.rows.iter().any(|row| row.agent == *agent && row.on)
            })
            .map(|agent| agent.source().to_string())
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
/// three of the four arms are races: the keys answered by another
/// process while this launch was asking, another dialog taken the screen
/// in the meantime, a machine with nothing to ask about. None of them is
/// reproducible from outside the process, and all of them decide whether
/// Roost asks to write into somebody else's config file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SurveyVerdict {
    Raise,
    /// No file agent is installed here, so there is nothing to ask:
    /// every other agent connects without a file and is on by default.
    NothingToAsk,
    /// Somebody answered while this launch was asking.
    AlreadyAnswered,
    /// The user is already answering a different question.
    ScreenTaken,
}

/// `asks` is whether the first-run card would carry a row; `answered` is
/// whether either agent key holds an answer on disk.
pub(crate) fn survey_verdict(
    mode: CardMode,
    asks: bool,
    answered: bool,
    dialog_open: bool,
) -> SurveyVerdict {
    if mode == CardMode::FirstRun {
        if !asks {
            return SurveyVerdict::NothingToAsk;
        }
        if answered {
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
    use roost_agent_install::retire::{Leftover, LeftoverReason, Retired};
    use roost_agent_install::Home;

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
            on: roost_ui_model::config::agent_default_on(agent),
            retired: None,
            came_back: false,
        }
    }

    fn all() -> Vec<Status> {
        ALL_AGENTS.into_iter().map(status).collect()
    }

    fn agents(card: &AgentHooksDraft) -> Vec<Agent> {
        card.rows().iter().map(|row| row.agent).collect()
    }

    /// First run asks only about the installed file agent, unchecked;
    /// with none installed there is no question at all.
    #[test]
    fn first_run_carries_only_an_installed_cursor_unchecked() {
        let card = AgentHooksDraft::new(CardMode::FirstRun, &all(), false);
        assert_eq!(agents(&card), [Agent::Cursor]);
        assert!(!card.rows()[0].on, "the one file-writing row starts off");
        assert!(card.asks());
        assert_eq!(card.rows()[0].status, None);
        assert_eq!(
            card.rows()[0].files,
            ["/home/u/.cursor/hooks.json"],
            "the row names the file it would write"
        );

        let mut statuses = all();
        statuses[3].present = false;
        let card = AgentHooksDraft::new(CardMode::FirstRun, &statuses, false);
        assert!(!card.asks(), "nothing to ask without Cursor");
    }

    /// Continue records the defaults with the file agent's answer, so the
    /// next launch has an answer and no card.
    #[test]
    fn continue_records_the_defaults_and_cursors_answer() {
        let mut card = AgentHooksDraft::new(CardMode::FirstRun, &all(), false);
        assert_eq!(card.buttons(), ["Decide later", "Continue"]);
        assert_eq!(
            card.wire_agents(),
            AgentSetHooksAgents::List(
                ["claude", "codex", "grok", "opencode"]
                    .map(String::from)
                    .to_vec()
            )
        );
        card.toggle_named("cursor").unwrap();
        assert_eq!(card.buttons(), ["Decide later", "Continue"]);
        assert_eq!(
            card.wire_agents(),
            AgentSetHooksAgents::List(
                ["claude", "codex", "grok", "cursor", "opencode"]
                    .map(String::from)
                    .to_vec()
            )
        );
        assert!(card.toggle_named("claude").is_err(), "no row for claude");
    }

    /// The lede names only the installed agents already connected, and
    /// never a Claude the policy keeps out.
    #[test]
    fn the_first_run_lede_names_what_is_already_connected() {
        let mut statuses = all();
        statuses[4].present = false;
        let card = AgentHooksDraft::new(CardMode::FirstRun, &statuses, false);
        assert!(
            card.lede()
                .contains("Claude Code, Codex and Grok are already connected"),
            "{}",
            card.lede()
        );
        let card = AgentHooksDraft::new(CardMode::FirstRun, &statuses, true);
        assert!(
            card.lede()
                .contains(" Codex and Grok are already connected"),
            "{}",
            card.lede()
        );
        for status in &mut statuses {
            status.present = status.agent == Agent::Cursor;
        }
        let card = AgentHooksDraft::new(CardMode::FirstRun, &statuses, false);
        assert!(
            !card.lede().contains("already connected"),
            "{}",
            card.lede()
        );
        assert!(card.lede().ends_with(ASK));
    }

    /// Preferences has every agent, prefilled from the switches, and only
    /// the file agent names a file.
    #[test]
    fn preferences_has_a_switch_per_agent_from_the_switches() {
        let mut statuses = all();
        statuses[4].on = false;
        let card = AgentHooksDraft::new(CardMode::Preferences, &statuses, false);
        assert_eq!(agents(&card), ALL_AGENTS.to_vec());
        assert_eq!(
            card.rows().iter().map(|row| row.on).collect::<Vec<_>>(),
            [true, true, true, false, false]
        );
        assert!(card
            .rows()
            .iter()
            .all(|row| row.files.is_empty() == (row.agent != Agent::Cursor)));
        assert_eq!(card.built_in_line(), Some(BUILT_IN_LINE));
        assert_eq!(card.buttons(), ["Cancel", "Apply"]);
        assert_eq!(
            card.wire_agents(),
            AgentSetHooksAgents::List(["claude", "codex", "grok"].map(String::from).to_vec())
        );
    }

    #[test]
    fn every_switch_off_is_the_word_off() {
        let mut statuses = all();
        for status in &mut statuses {
            status.on = false;
        }
        let card = AgentHooksDraft::new(CardMode::Preferences, &statuses, false);
        assert_eq!(card.buttons(), ["Cancel", "Apply: turn off"]);
        assert_eq!(card.wire_agents(), AgentSetHooksAgents::Off);
    }

    /// The status line says how each agent is connected.
    #[test]
    fn the_status_line_says_the_delivery() {
        let line = |status: &Status, blocked| row_status(status, blocked);
        assert_eq!(
            line(&status(Agent::Claude), false),
            "Connected · nothing written"
        );
        assert_eq!(line(&status(Agent::Claude), true), CLAUDE_BLOCKED);
        assert_eq!(
            line(&status(Agent::Opencode), true),
            "Connected · nothing written",
            "the policy is Claude's alone"
        );
        assert_eq!(line(&status(Agent::Codex), false), "Built in");
        let mut off = status(Agent::Grok);
        off.on = false;
        assert_eq!(line(&off, false), "Off");
        let mut absent = status(Agent::Grok);
        absent.present = false;
        assert_eq!(line(&absent, false), "not found");
        assert_eq!(line(&status(Agent::Cursor), false), "Off");
    }

    /// Cursor's line is still the file's: the record and the disk apart.
    #[test]
    fn a_file_agents_line_reads_the_record_and_the_disk_apart() {
        let cursor = |entries_on_disk, allowed, up_to_date, wired| Status {
            on: true,
            entries_on_disk,
            allowed,
            up_to_date,
            wired,
            ..status(Agent::Cursor)
        };
        assert_eq!(
            row_status(&cursor(false, true, false, None), false),
            "found, not wired"
        );
        assert_eq!(
            row_status(&cursor(true, false, true, None), false),
            "wired, not allowed"
        );
        assert_eq!(
            row_status(&cursor(true, true, true, Some(1)), false),
            format!("wired v{}", roost_agent_install::INTEGRATION_VERSION)
        );
        assert_eq!(
            row_status(&cursor(true, true, false, Some(2)), false),
            "wired v2, out of date"
        );
        assert_eq!(
            row_status(&cursor(true, true, false, None), false),
            "wired, out of date"
        );
    }

    /// What the retire left, or what came back after it, is on the line.
    #[test]
    fn leftovers_and_a_comeback_are_named() {
        let mut left = status(Agent::Claude);
        left.retired = Some(Retired {
            version: 4,
            at: 0,
            complete: true,
            misses: 0,
            changed: Vec::new(),
            backups: Vec::new(),
            leftovers: vec![Leftover {
                file: "/home/u/.claude/settings.json".into(),
                event: Some("Stop".into()),
                reason: LeftoverReason::Annotated,
            }],
            noticed: false,
        });
        let line = row_status(&left, false);
        assert!(
            line.starts_with(
                "Connected · nothing written · 1 old Roost entry left in \
                 /home/u/.claude/settings.json"
            ),
            "{line}"
        );
        let mut back = status(Agent::Claude);
        back.came_back = true;
        assert!(
            row_status(&back, false).contains("roostctl agent uninstall claude"),
            "{}",
            row_status(&back, false)
        );
    }

    fn draft(on: [bool; 5]) -> AgentHooksDraft {
        let mut statuses = all();
        for (status, on) in statuses.iter_mut().zip(on) {
            status.on = on;
        }
        AgentHooksDraft::new(CardMode::Preferences, &statuses, false)
    }

    /// A toggle by name is the harness's spelling; an agent no row
    /// carries is an error rather than a silent no-op.
    #[test]
    fn a_toggle_names_its_agent() {
        let mut card = draft([false; 5]);
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
        let mut card = draft([false; 5]);
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

    #[test]
    fn a_first_run_survey_stands_down_for_an_answer_given_while_it_ran() {
        assert_eq!(
            survey_verdict(CardMode::FirstRun, true, true, false),
            SurveyVerdict::AlreadyAnswered
        );
        assert_eq!(
            survey_verdict(CardMode::FirstRun, true, false, false),
            SurveyVerdict::Raise
        );
    }

    /// Preferences is a question the user just asked for, so an answered
    /// key is exactly what it is there to show.
    #[test]
    fn preferences_is_raised_over_any_answer() {
        assert_eq!(
            survey_verdict(CardMode::Preferences, true, true, false),
            SurveyVerdict::Raise
        );
    }

    #[test]
    fn nothing_to_ask_asks_nothing() {
        assert_eq!(
            survey_verdict(CardMode::FirstRun, false, false, false),
            SurveyVerdict::NothingToAsk
        );
    }

    /// Either mode stands down rather than replacing a dialog the user
    /// is already answering — including their half-filled Add Host form.
    #[test]
    fn a_survey_never_takes_the_screen_from_another_dialog() {
        for mode in [CardMode::FirstRun, CardMode::Preferences] {
            assert_eq!(
                survey_verdict(mode, true, false, true),
                SurveyVerdict::ScreenTaken,
                "{mode:?}"
            );
        }
    }
}
