//! Per-project sidebar rollup state machine.
//!
//! Reduces a project's tabs to the one CSS stripe its sidebar row wears.
//! The stripe itself lives in `resources/style.css` (`.roost-rollup-*`);
//! this module picks which one.
//!
//! Both halves are shared, not local: the per-tab value is
//! [`agent::effective_lifecycle`] (the same value the tab's own dot and
//! indicator icon render) and the ordering is [`agent::rank`] (the same
//! function the future agent overview sorts by). So the sidebar can't
//! disagree with the rest of the UI about which tab is loudest.
//!
//! Hook-driven state **participates**. Until plan 002 this module
//! skipped every tab with `hook_active` set, so a project whose only
//! blocked tab was a Claude session showed no stripe at all — the
//! differentiating case, hidden. Kept at parity with the Mac UI's
//! `ProjectRollup.swift`.

use roost_ipc::agent::{self, AgentLifecycle};

/// Every `roost-rollup-*` class, for clearing stale ones before
/// applying the current stripe.
pub const ROLLUP_CLASSES: [&str; 4] = [
    "roost-rollup-running",
    "roost-rollup-needs-input",
    "roost-rollup-idle",
    "roost-rollup-failed",
];

/// Sidebar stripe for a rollup, or `None` for `Inactive` (no class = no
/// stripe).
pub fn rollup_css_class(lifecycle: AgentLifecycle) -> Option<&'static str> {
    match lifecycle {
        AgentLifecycle::Inactive => None,
        AgentLifecycle::Working => Some("roost-rollup-running"),
        AgentLifecycle::Waiting => Some("roost-rollup-needs-input"),
        AgentLifecycle::Finished => Some("roost-rollup-idle"),
        AgentLifecycle::Failed => Some("roost-rollup-failed"),
    }
}

/// Compute the project rollup: the highest-[`agent::rank`] tab wins.
/// No tabs → `Inactive`.
///
/// Pure function — no GTK, no env, no allocation. Used by
/// [`crate::app`] when applying rollup CSS classes; tested directly
/// without spinning up the GTK runtime.
pub fn project_rollup(tabs: impl IntoIterator<Item = AgentLifecycle>) -> AgentLifecycle {
    tabs.into_iter()
        .max_by_key(|l| agent::rank(*l))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::agent::{AgentTabState, Ownership, ShellState};

    /// A tab an agent owns, mid-turn.
    fn owned(lifecycle: AgentLifecycle) -> AgentTabState {
        AgentTabState {
            shell: ShellState::AtPrompt,
            lifecycle,
            ownership: Some(Ownership {
                source: "claude".into(),
                session_id: "s1".into(),
                ..Ownership::default()
            }),
        }
    }

    /// A plain shell with no agent.
    fn shell(shell: ShellState) -> AgentTabState {
        AgentTabState {
            shell,
            ..AgentTabState::default()
        }
    }

    /// The production call shape: tabs in, presented lifecycles out.
    fn rollup(tabs: &[AgentTabState]) -> AgentLifecycle {
        project_rollup(tabs.iter().map(agent::effective_lifecycle))
    }

    #[test]
    fn empty_list_is_none() {
        assert_eq!(rollup(&[]), AgentLifecycle::Inactive);
    }

    #[test]
    fn all_idle_shells_is_none() {
        let tabs = [shell(ShellState::AtPrompt), shell(ShellState::Unknown)];
        assert_eq!(rollup(&tabs), AgentLifecycle::Inactive);
    }

    #[test]
    fn foreground_process_is_running() {
        assert_eq!(
            rollup(&[shell(ShellState::ForegroundProcess)]),
            AgentLifecycle::Working
        );
    }

    #[test]
    fn needs_input_outranks_running() {
        let tabs = [
            owned(AgentLifecycle::Working),
            owned(AgentLifecycle::Waiting),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Waiting);
    }

    #[test]
    fn running_outranks_idle() {
        let tabs = [
            owned(AgentLifecycle::Finished),
            shell(ShellState::ForegroundProcess),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Working);
    }

    #[test]
    fn idle_outranks_none() {
        let tabs = [shell(ShellState::AtPrompt), owned(AgentLifecycle::Finished)];
        assert_eq!(rollup(&tabs), AgentLifecycle::Finished);
    }

    /// `failed` sits above `waiting` — the whole reason the rollup
    /// ranks on the agent axis instead of the legacy `TabState`, which
    /// collapses the two.
    #[test]
    fn failed_outranks_needs_input() {
        let tabs = [
            owned(AgentLifecycle::Waiting),
            owned(AgentLifecycle::Failed),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Failed);
    }

    // The three tests below replace `hook_active_suppresses_needs_input`
    // / `hook_active_suppresses_running` / `hook_active_on_all_falls_
    // back_to_none`, which pinned the behavior plan 002 §2.2(a) reverses:
    // an agent-owned tab used to be dropped from the rollup entirely.

    #[test]
    fn agent_owned_needs_input_participates() {
        let tabs = [
            owned(AgentLifecycle::Waiting),
            shell(ShellState::ForegroundProcess),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Waiting);
    }

    #[test]
    fn agent_owned_running_participates() {
        let tabs = [
            owned(AgentLifecycle::Working),
            owned(AgentLifecycle::Finished),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Working);
    }

    #[test]
    fn all_tabs_agent_owned_still_ranks() {
        let tabs = [
            owned(AgentLifecycle::Working),
            owned(AgentLifecycle::Waiting),
        ];
        assert_eq!(rollup(&tabs), AgentLifecycle::Waiting);
    }

    /// Ownership without a live lifecycle falls through to the shell
    /// axis — the `D`/`A` failsafe, seen from the sidebar.
    #[test]
    fn inactive_owner_falls_through_to_shell() {
        let mut tab = owned(AgentLifecycle::Inactive);
        tab.shell = ShellState::ForegroundProcess;
        assert_eq!(rollup(&[tab]), AgentLifecycle::Working);
    }

    /// A `Failed` lifecycle left behind on a tab whose owner is gone
    /// must not outrank live tabs.
    #[test]
    fn failed_without_an_owner_falls_through_to_shell() {
        let tab = AgentTabState {
            shell: ShellState::AtPrompt,
            lifecycle: AgentLifecycle::Failed,
            ownership: None,
        };
        assert_eq!(rollup(&[tab]), AgentLifecycle::Inactive);
    }

    #[test]
    fn css_class_mapping_round_trip() {
        // Every lifecycle except Inactive must report exactly one of
        // `ROLLUP_CLASSES` so the sidebar-row update doesn't try to
        // apply a class the CSS doesn't define.
        for lifecycle in [
            AgentLifecycle::Working,
            AgentLifecycle::Waiting,
            AgentLifecycle::Finished,
            AgentLifecycle::Failed,
        ] {
            let cls = rollup_css_class(lifecycle).expect("non-Inactive rollup has a class");
            assert!(
                ROLLUP_CLASSES.contains(&cls),
                "class {cls} not in ROLLUP_CLASSES"
            );
        }
        assert!(rollup_css_class(AgentLifecycle::Inactive).is_none());
    }
}
