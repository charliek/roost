//! A creation on a host, parked until the mirror lists it (plan 037
//! §3.9), and outranked by the user's own focus (plan 071 §D13).
//!
//! Every creation dispatch captures the window's focus generation, which
//! every user focus bumps. A click while the request is in flight means
//! the reply arms nothing, and a click while the selection waits drops
//! it: either way, what the user chose stays on screen.

use std::time::Instant;

use roost_ui_model::keys::TabKey;

use super::PENDING_HOST_SELECTION_DEADLINE;

/// What the creation made, for the status an expired wait leaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Creation {
    Tab,
    Project,
}

#[derive(Debug, Clone, Copy)]
pub(super) struct PendingHostSelection {
    pub(super) tab: TabKey,
    pub(super) creation: Creation,
    /// When the wait started.
    ///
    /// The wait has to be bounded, because "the row will appear" is an
    /// assumption and not a guarantee: a tab whose command exits the
    /// instant it spawns is closed again before any batch lists it, and
    /// a creation that fails after its intent was enqueued never
    /// produces one at all. Neither ends the connection, so nothing else
    /// here would ever clear the entry.
    armed: Instant,
    /// The focus generation its dispatch captured.
    generation: u64,
}

/// What one reconcile makes of a pending selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PendingStep {
    Wait,
    Select,
    /// The user focused something since, or the host is gone: silently.
    Drop,
    /// The mirror never listed it: dropped, and the status says so.
    Expired,
}

/// The selection a creation's reply owes, unless the user focused
/// something while the request was in flight (`dispatched` is the
/// generation the dispatch captured, `generation` the current one).
pub(super) fn arm(
    tab: TabKey,
    creation: Creation,
    dispatched: u64,
    generation: u64,
    now: Instant,
) -> Option<PendingHostSelection> {
    if tab.is_local() || dispatched != generation {
        return None;
    }
    Some(PendingHostSelection {
        tab,
        creation,
        armed: now,
        generation,
    })
}

/// Listed wins over the deadline: a row that is there is the answer,
/// however late the reconcile that saw it, and "never appeared" would
/// then be untrue.
pub(super) fn pending_step(
    pending: &PendingHostSelection,
    connected: bool,
    listed: bool,
    now: Instant,
    generation: u64,
) -> PendingStep {
    if pending.generation != generation || !connected {
        PendingStep::Drop
    } else if listed {
        PendingStep::Select
    } else if now.saturating_duration_since(pending.armed) >= PENDING_HOST_SELECTION_DEADLINE {
        PendingStep::Expired
    } else {
        PendingStep::Wait
    }
}

/// The status an expired wait leaves, naming the slot as the local
/// session (plan 071 §D8) and any other host by its label.
pub(super) fn never_appeared(creation: Creation, on_slot: bool, label: &str) -> String {
    let what = match creation {
        Creation::Tab => "tab",
        Creation::Project => "project",
    };
    let place = if on_slot { "the local session" } else { label };
    format!("the new {what} never appeared on {place}")
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use roost_ui_model::keys::HostId;

    use super::super::PENDING_SELECTION_TICK_INTERVAL;
    use super::*;

    fn tab() -> TabKey {
        TabKey::new(HostId::new(3), 7)
    }

    fn pending(now: Instant, generation: u64) -> PendingHostSelection {
        arm(tab(), Creation::Tab, generation, generation, now).expect("armed")
    }

    #[test]
    fn the_step_table() {
        let armed = Instant::now();
        let late = armed + PENDING_HOST_SELECTION_DEADLINE;
        let early = late - Duration::from_millis(1);
        let waiting = pending(armed, 4);
        let step = |connected, listed, now, generation| {
            pending_step(&waiting, connected, listed, now, generation)
        };

        assert_eq!(step(true, false, armed, 4), PendingStep::Wait);
        assert_eq!(step(true, false, early, 4), PendingStep::Wait);
        assert_eq!(step(true, true, early, 4), PendingStep::Select);
        assert_eq!(step(true, false, late, 4), PendingStep::Expired);
        assert_eq!(
            step(true, true, late, 4),
            PendingStep::Select,
            "a listed row is the answer however late"
        );
        assert_eq!(step(false, false, early, 4), PendingStep::Drop);
        assert_eq!(step(false, true, early, 4), PendingStep::Drop);
        assert_eq!(
            step(false, false, late, 4),
            PendingStep::Drop,
            "a host that went is not a row that never appeared"
        );
        assert_eq!(
            step(true, false, armed - Duration::from_secs(1), 4),
            PendingStep::Wait,
            "a clock that reads backwards waits rather than abandoning"
        );
    }

    #[test]
    fn a_stale_generation_does_not_arm() {
        let now = Instant::now();
        assert!(
            arm(tab(), Creation::Tab, 4, 5, now).is_none(),
            "a focus while the request was in flight wins"
        );
        assert!(arm(tab(), Creation::Project, 4, 5, now).is_none());
        assert!(arm(tab(), Creation::Tab, 5, 5, now).is_some());
    }

    #[test]
    fn a_local_tab_never_arms() {
        let local = TabKey::new(HostId::LOCAL, 7);
        assert!(arm(local, Creation::Tab, 4, 4, Instant::now()).is_none());
    }

    #[test]
    fn a_user_focus_clears_the_pending_selection() {
        let armed = Instant::now();
        let waiting = pending(armed, 4);
        assert_eq!(
            pending_step(&waiting, true, false, armed, 5),
            PendingStep::Drop
        );
        assert_eq!(
            pending_step(&waiting, true, true, armed, 5),
            PendingStep::Drop,
            "even once listed: the click is on screen and stays"
        );
        assert_eq!(
            pending_step(
                &waiting,
                true,
                false,
                armed + PENDING_HOST_SELECTION_DEADLINE,
                5
            ),
            PendingStep::Drop,
            "and a wait the user moved past never reports itself lost"
        );
    }

    #[test]
    fn the_wake_alone_expires_a_wait_nothing_answers() {
        let armed = Instant::now();
        let waiting = pending(armed, 4);
        let mut now = armed;
        let step = loop {
            match pending_step(&waiting, true, false, now, 4) {
                PendingStep::Wait => now += PENDING_SELECTION_TICK_INTERVAL,
                step => break step,
            }
        };
        assert_eq!(step, PendingStep::Expired);
        assert!(now < armed + PENDING_HOST_SELECTION_DEADLINE + PENDING_SELECTION_TICK_INTERVAL);
    }

    #[test]
    fn expiry_names_what_never_appeared_and_where() {
        assert_eq!(
            never_appeared(Creation::Tab, true, "localhost"),
            "the new tab never appeared on the local session"
        );
        assert_eq!(
            never_appeared(Creation::Project, true, "localhost"),
            "the new project never appeared on the local session"
        );
        assert_eq!(
            never_appeared(Creation::Tab, false, "devbox"),
            "the new tab never appeared on devbox"
        );
        assert_eq!(
            never_appeared(Creation::Project, false, "devbox"),
            "the new project never appeared on devbox"
        );
    }
}
