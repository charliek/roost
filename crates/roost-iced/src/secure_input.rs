//! Secure Keyboard Entry's state machine (plan 074 §D3), on Ghostty's
//! model: on while the app is active and either the remembered toggle is
//! on or the active tab is at a password prompt.
//!
//! Platform-neutral, so the truth table runs on every host; the Carbon
//! calls, the thread-local owner and the app-activation observers are
//! `macos::secure_input`.
//!
//! Two states are kept apart on purpose. *Desired* is what the inputs ask
//! for; *owned* is whether Roost holds an `EnableSecureEventInput` that
//! succeeded. macOS counts enables per process, so a Disable is only ever
//! sent for an enable Roost owns, and a failed call leaves `owned` where
//! the system left it.

use roost_ipc::messages::{Project, Tab};

/// macOS's `OSStatus`.
pub(crate) type OsStatus = i32;

/// `noErr`.
const NO_ERR: OsStatus = 0;

/// The Carbon calls, behind a seam a test can fail.
pub(crate) trait SecureEventInput {
    fn enable(&mut self) -> OsStatus;
    fn disable(&mut self) -> OsStatus;
    /// Whether any process holds secure input. Diagnostic only: it cannot
    /// say whose enable it is, so it never decides ownership.
    fn system_enabled(&self) -> bool;
}

/// What the app pushes in on every change.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Inputs {
    /// `macos-secure-keyboard-entry`.
    pub(crate) manual: bool,
    /// `macos-auto-secure-input`.
    pub(crate) auto: bool,
    /// The active tab's, whatever owns the keyboard: a palette or a rename
    /// editor in front of a password prompt does not take the prompt away.
    pub(crate) password_input: bool,
}

/// The formula. One place, because the owner, the test op's reader and the
/// e2e's assertion all mean the same thing by it.
pub(crate) fn desired(app_active: bool, inputs: Inputs) -> bool {
    app_active && (inputs.manual || (inputs.auto && inputs.password_input))
}

/// Whether the tab band draws the lock.
pub(crate) fn indicator(owned: bool, indication: bool) -> bool {
    owned && indication
}

/// The active tab's `password_input`, off the row its project lists.
///
/// The listed row rather than any live poll: the row is what the engine
/// (or a host's mirror, which a disconnect takes down) last reported, so a
/// background tab at a prompt, or a tab on a host nobody can reach, never
/// counts.
pub(crate) fn listed_password_input(project: Option<&Project>, tab: i64) -> bool {
    project
        .and_then(|project| project.tabs.iter().find(|row| row.id == tab))
        .is_some_and(|row: &Tab| row.password_input)
}

/// The owner's state, as the test op reports it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Status {
    pub(crate) inputs: Inputs,
    /// As read when the owner last applied.
    pub(crate) app_active: bool,
    pub(crate) desired: bool,
    pub(crate) owned: bool,
}

pub(crate) struct SecureInput<F> {
    ffi: F,
    status: Status,
    /// Set by [`Self::release`]; nothing enables after it.
    released: bool,
}

impl<F: SecureEventInput> SecureInput<F> {
    pub(crate) fn new(ffi: F) -> Self {
        Self {
            ffi,
            status: Status::default(),
            released: false,
        }
    }

    pub(crate) fn set_inputs(&mut self, inputs: Inputs) {
        self.status.inputs = inputs;
    }

    /// Bring what Roost owns to what the inputs and `app_active` ask for,
    /// with at most one Carbon call.
    ///
    /// A failed Enable leaves nothing owned, so no lock shows and no
    /// Disable follows. A failed Disable keeps the enable owned, and the
    /// next call retries it.
    pub(crate) fn apply(&mut self, app_active: bool) {
        self.status.app_active = app_active;
        self.status.desired = !self.released && desired(app_active, self.status.inputs);
        match (self.status.desired, self.status.owned) {
            (true, false) => {
                let status = self.ffi.enable();
                if status == NO_ERR {
                    self.status.owned = true;
                } else {
                    tracing::warn!(
                        status,
                        system_enabled = self.ffi.system_enabled(),
                        "EnableSecureEventInput failed; secure keyboard entry stays off"
                    );
                }
            }
            (false, true) => {
                let status = self.ffi.disable();
                if status == NO_ERR {
                    self.status.owned = false;
                } else {
                    tracing::warn!(
                        status,
                        system_enabled = self.ffi.system_enabled(),
                        "DisableSecureEventInput failed; retrying on the next change"
                    );
                }
            }
            _ => {}
        }
    }

    /// Latch off for good, and give back whatever Roost holds.
    pub(crate) fn release(&mut self) {
        self.released = true;
        self.apply(self.status.app_active);
    }

    pub(crate) fn status(&self) -> Status {
        self.status
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use roost_ipc::messages::TabState;

    use super::*;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Call {
        Enable,
        Disable,
    }

    /// Records every call, and answers each with the next queued status
    /// (`noErr` once the queue is empty).
    #[derive(Default)]
    struct Fake {
        calls: Vec<Call>,
        enable: VecDeque<OsStatus>,
        disable: VecDeque<OsStatus>,
        /// Another process's enable, which [`SecureEventInput::system_enabled`]
        /// reports and nothing else may act on.
        foreign: bool,
    }

    impl SecureEventInput for Fake {
        fn enable(&mut self) -> OsStatus {
            self.calls.push(Call::Enable);
            self.enable.pop_front().unwrap_or(NO_ERR)
        }

        fn disable(&mut self) -> OsStatus {
            self.calls.push(Call::Disable);
            self.disable.pop_front().unwrap_or(NO_ERR)
        }

        fn system_enabled(&self) -> bool {
            self.foreign
        }
    }

    const MANUAL: Inputs = Inputs {
        manual: true,
        auto: true,
        password_input: false,
    };
    const AT_PROMPT: Inputs = Inputs {
        manual: false,
        auto: true,
        password_input: true,
    };
    const IDLE: Inputs = Inputs {
        manual: false,
        auto: true,
        password_input: false,
    };

    fn owner(inputs: Inputs) -> SecureInput<Fake> {
        let mut owner = SecureInput::new(Fake::default());
        owner.set_inputs(inputs);
        owner
    }

    /// Every input combination, with the app active and not.
    #[test]
    fn the_formula_is_ghosttys() {
        for app_active in [false, true] {
            for manual in [false, true] {
                for auto in [false, true] {
                    for password_input in [false, true] {
                        let inputs = Inputs {
                            manual,
                            auto,
                            password_input,
                        };
                        let want = app_active && (manual || (auto && password_input));
                        assert_eq!(desired(app_active, inputs), want, "{app_active} {inputs:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_lock_shows_only_for_an_owned_enable_under_indication() {
        assert!(indicator(true, true));
        assert!(!indicator(true, false));
        assert!(!indicator(false, true));
        assert!(!indicator(false, false));
    }

    /// Become, become, resign, resign, become, resign: one call per edge,
    /// and every Enable balanced by a Disable.
    #[test]
    fn repeated_activation_changes_stay_balanced() {
        let mut owner = owner(MANUAL);
        for app_active in [true, true, false, false, true, false] {
            owner.apply(app_active);
            assert_eq!(owner.status().owned, app_active);
        }
        assert_eq!(
            owner.ffi.calls,
            [Call::Enable, Call::Disable, Call::Enable, Call::Disable]
        );
    }

    #[test]
    fn a_failed_enable_owns_nothing_and_is_never_disabled() {
        let mut owner = owner(MANUAL);
        owner.ffi.enable.push_back(-50);
        owner.apply(true);
        let status = owner.status();
        assert!(status.desired);
        assert!(!status.owned, "a failed Enable must not count as owned");

        owner.apply(false);
        assert_eq!(
            owner.ffi.calls,
            [Call::Enable],
            "nothing was owned, so the resign must not Disable"
        );
    }

    #[test]
    fn a_failed_disable_stays_owned_and_is_retried_on_the_next_change() {
        let mut owner = owner(MANUAL);
        owner.ffi.disable.push_back(-50);
        owner.apply(true);
        owner.apply(false);
        assert!(owner.status().owned, "the enable is still Roost's");
        assert!(!owner.status().desired);

        owner.set_inputs(IDLE);
        owner.apply(false);
        assert!(!owner.status().owned);
        assert_eq!(
            owner.ffi.calls,
            [Call::Enable, Call::Disable, Call::Disable]
        );
    }

    /// The toggle and a prompt overlapping hold one enable between them:
    /// it lasts until neither asks for it.
    #[test]
    fn manual_and_auto_overlapping_hold_one_enable() {
        let mut owner = owner(AT_PROMPT);
        owner.apply(true);
        owner.set_inputs(Inputs {
            manual: true,
            ..AT_PROMPT
        });
        owner.apply(true);
        owner.set_inputs(MANUAL);
        owner.apply(true);
        assert_eq!(
            owner.ffi.calls,
            [Call::Enable],
            "still wanted by the toggle"
        );

        owner.set_inputs(IDLE);
        owner.apply(true);
        assert_eq!(owner.ffi.calls, [Call::Enable, Call::Disable]);
    }

    #[test]
    fn auto_off_ignores_a_prompt() {
        let mut owner = owner(Inputs {
            auto: false,
            ..AT_PROMPT
        });
        owner.apply(true);
        assert!(!owner.status().desired);
        assert!(owner.ffi.calls.is_empty());
    }

    fn tab(id: i64, password_input: bool) -> Tab {
        Tab {
            id,
            project_id: 1,
            title: format!("tab-{id}"),
            cwd: "/tmp".into(),
            state: TabState::None,
            has_notification: false,
            is_active: false,
            user_titled: false,
            position: 0,
            created_at: 0,
            last_active: 0,
            hook_active: false,
            shell_state: Default::default(),
            agent_lifecycle: Default::default(),
            ownership: None,
            password_input,
        }
    }

    fn project(tabs: Vec<Tab>) -> Project {
        Project {
            id: 1,
            name: "p".into(),
            cwd: "/tmp".into(),
            position: 0,
            created_at: 0,
            tabs,
        }
    }

    /// Only the active tab's prompt counts: a background tab at one turns
    /// nothing on, and switching to it does.
    #[test]
    fn only_the_active_tabs_prompt_counts() {
        let listed = project(vec![tab(10, false), tab(11, true)]);
        assert!(!listed_password_input(Some(&listed), 10));
        assert!(listed_password_input(Some(&listed), 11));
        assert!(
            !listed_password_input(Some(&listed), 12),
            "a tab that is gone"
        );
        assert!(!listed_password_input(None, 11), "a project that is gone");

        let mut owner = owner(Inputs {
            password_input: listed_password_input(Some(&listed), 10),
            ..IDLE
        });
        owner.apply(true);
        assert!(
            owner.ffi.calls.is_empty(),
            "the prompt is in a background tab"
        );
        owner.set_inputs(Inputs {
            password_input: listed_password_input(Some(&listed), 11),
            ..IDLE
        });
        owner.apply(true);
        owner.set_inputs(Inputs {
            password_input: listed_password_input(Some(&listed), 10),
            ..IDLE
        });
        owner.apply(true);
        assert_eq!(owner.ffi.calls, [Call::Enable, Call::Disable]);
    }

    /// A host disconnect takes its tabs' prompts down (the mirror's
    /// suspend), and the flag clearing gives the enable back.
    #[test]
    fn a_disconnect_that_clears_the_flag_disables() {
        let mut owner = owner(AT_PROMPT);
        owner.apply(true);
        let mut listed = project(vec![tab(10, true)]);
        listed.tabs[0].password_input = false;
        owner.set_inputs(Inputs {
            password_input: listed_password_input(Some(&listed), 10),
            ..IDLE
        });
        owner.apply(true);
        assert!(!owner.status().owned);
        assert_eq!(owner.ffi.calls, [Call::Enable, Call::Disable]);
    }

    /// Quit releases, and `Drop` releases again: one Disable, and nothing
    /// afterwards can enable — not an input change, not a become-active.
    #[test]
    fn release_latches_off() {
        let mut owner = owner(MANUAL);
        owner.apply(true);
        owner.release();
        owner.release();
        owner.set_inputs(Inputs {
            manual: true,
            ..AT_PROMPT
        });
        owner.apply(true);
        assert!(!owner.status().owned);
        assert!(!owner.status().desired);
        assert_eq!(owner.ffi.calls, [Call::Enable, Call::Disable]);
    }

    /// Another process's secure input is not Roost's to give back.
    #[test]
    fn an_enable_roost_does_not_own_is_never_disabled() {
        let mut owner = owner(IDLE);
        owner.ffi.foreign = true;
        owner.apply(true);
        owner.apply(false);
        owner.release();
        assert!(owner.ffi.calls.is_empty());
    }
}
