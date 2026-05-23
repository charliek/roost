//! Per-project sidebar rollup state machine.
//!
//! Aggregates each tab's [`TabState`] + `hook_active` flag into a
//! single [`RollupState`] for the project's sidebar row. The CSS
//! stripe lives in `resources/style.css` (`.roost-rollup-*`); this
//! module picks which one to apply.
//!
//! Mirrors the Go binary's per-project state rollup in
//! `cmd/roost/app.go` so users moving between the two UIs see the
//! same precedence (needs-input wins, hook-active suppresses noise).

/// Per-tab agent state as exposed by the daemon's `TabStateChangedEvent`.
/// The proto numeric mapping is:
///
/// * 0 / `Unspecified` — no state set yet (treated as `None`).
/// * 1 / `None` — no agent activity.
/// * 2 / `Running` — agent is working.
/// * 3 / `NeedsInput` — agent is waiting on the user.
/// * 4 / `Idle` — agent finished, no pending input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabState {
    None,
    Running,
    NeedsInput,
    Idle,
}

impl TabState {
    /// Map the roost-ipc `TabState` enum to our local enum. The
    /// workspace is the source of truth post-M3b; the legacy proto
    /// integer mapping (`from_proto`) is gone with the daemon.
    pub fn from_ipc(value: roost_ipc::messages::TabState) -> Self {
        use roost_ipc::messages::TabState as Ipc;
        match value {
            Ipc::Running => Self::Running,
            Ipc::NeedsInput => Self::NeedsInput,
            Ipc::Idle => Self::Idle,
            Ipc::None => Self::None,
        }
    }
}

/// Project-level rollup as drawn by the sidebar CSS class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollupState {
    None,
    Running,
    NeedsInput,
    Idle,
}

impl RollupState {
    /// CSS class to apply to the sidebar row, or `None` when the
    /// rollup is `None` (no class = no stripe).
    pub fn css_class(self) -> Option<&'static str> {
        match self {
            Self::None => None,
            Self::Running => Some("roost-rollup-running"),
            Self::NeedsInput => Some("roost-rollup-needs-input"),
            Self::Idle => Some("roost-rollup-idle"),
        }
    }

    /// All possible class names, in order — used to clear stale
    /// classes before applying the current one. Returned as a slice
    /// so callers can iterate without owning a Vec.
    pub const fn all_classes() -> &'static [&'static str] {
        &[
            "roost-rollup-running",
            "roost-rollup-needs-input",
            "roost-rollup-idle",
        ]
    }
}

/// Compute the project rollup from a list of `(state, hook_active)`
/// pairs. Priority: `NeedsInput > Running > Idle > None`. When a tab
/// has `hook_active = true` its state is suppressed (the Claude hook
/// owns the notification surface; promoting the rollup color would
/// duplicate the urgency signal). Empty list → `None`.
///
/// Pure function — no GTK, no env, no allocation. Used by [`crate::app`]
/// when applying rollup CSS classes; tested directly without spinning
/// up the GTK runtime.
pub fn project_rollup(tabs: &[(TabState, bool)]) -> RollupState {
    let mut needs_input = false;
    let mut running = false;
    let mut idle = false;
    for (state, hook_active) in tabs {
        if *hook_active {
            continue;
        }
        match state {
            TabState::NeedsInput => needs_input = true,
            TabState::Running => running = true,
            TabState::Idle => idle = true,
            TabState::None => {}
        }
    }
    if needs_input {
        RollupState::NeedsInput
    } else if running {
        RollupState::Running
    } else if idle {
        RollupState::Idle
    } else {
        RollupState::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_list_is_none() {
        assert_eq!(project_rollup(&[]), RollupState::None);
    }

    #[test]
    fn all_none_is_none() {
        let tabs = [(TabState::None, false), (TabState::None, false)];
        assert_eq!(project_rollup(&tabs), RollupState::None);
    }

    #[test]
    fn single_running() {
        assert_eq!(
            project_rollup(&[(TabState::Running, false)]),
            RollupState::Running
        );
    }

    #[test]
    fn needs_input_outranks_running() {
        let tabs = [(TabState::Running, false), (TabState::NeedsInput, false)];
        assert_eq!(project_rollup(&tabs), RollupState::NeedsInput);
    }

    #[test]
    fn running_outranks_idle() {
        let tabs = [(TabState::Idle, false), (TabState::Running, false)];
        assert_eq!(project_rollup(&tabs), RollupState::Running);
    }

    #[test]
    fn idle_outranks_none() {
        let tabs = [(TabState::None, false), (TabState::Idle, false)];
        assert_eq!(project_rollup(&tabs), RollupState::Idle);
    }

    #[test]
    fn hook_active_suppresses_needs_input() {
        // If the only NeedsInput tab has its hook active, the rollup
        // falls back to whatever the other tabs say.
        let tabs = [
            (TabState::NeedsInput, true), // hook-active → suppressed
            (TabState::Running, false),
        ];
        assert_eq!(project_rollup(&tabs), RollupState::Running);
    }

    #[test]
    fn hook_active_suppresses_running() {
        let tabs = [
            (TabState::Running, true), // hook-active → suppressed
            (TabState::Idle, false),
        ];
        assert_eq!(project_rollup(&tabs), RollupState::Idle);
    }

    #[test]
    fn hook_active_on_all_falls_back_to_none() {
        let tabs = [(TabState::Running, true), (TabState::NeedsInput, true)];
        assert_eq!(project_rollup(&tabs), RollupState::None);
    }

    #[test]
    fn from_ipc_maps_correctly() {
        use roost_ipc::messages::TabState as Ipc;
        assert_eq!(TabState::from_ipc(Ipc::None), TabState::None);
        assert_eq!(TabState::from_ipc(Ipc::Running), TabState::Running);
        assert_eq!(TabState::from_ipc(Ipc::NeedsInput), TabState::NeedsInput);
        assert_eq!(TabState::from_ipc(Ipc::Idle), TabState::Idle);
    }

    #[test]
    fn css_class_mapping_round_trip() {
        // Every rollup state except None must report exactly one of
        // `all_classes()` so the M7 sidebar-row update doesn't try to
        // apply a class M3's CSS doesn't define.
        let all: std::collections::HashSet<_> =
            RollupState::all_classes().iter().copied().collect();
        for state in [
            RollupState::Running,
            RollupState::NeedsInput,
            RollupState::Idle,
        ] {
            let cls = state.css_class().expect("non-None rollup has a class");
            assert!(all.contains(cls), "class {cls} not in all_classes()");
        }
        assert!(RollupState::None.css_class().is_none());
    }
}
