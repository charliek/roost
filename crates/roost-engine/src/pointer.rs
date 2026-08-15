//! Toolkit-neutral pointer DTOs used by IPC and UI input adapters.

/// Pointer action accepted by a terminal mouse encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerAction {
    Press,
    Release,
    Motion,
}

/// Logical terminal mouse button. Wheel up/down map to buttons four/five.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PointerButton {
    Left,
    Right,
    Middle,
    Four,
    Five,
}

/// Per-terminal throttle for mode-1003 motion reports.
///
/// The state transition is deliberately split into a read-only decision and a
/// commit. UI adapters must commit only after the terminal encoder produced
/// bytes; otherwise a report declined under mode 1000 could suppress the first
/// report after mode 1003 is enabled.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MotionEmitter {
    last_cell: Option<(u32, u32)>,
    last_emit: Option<f64>,
}

impl MotionEmitter {
    /// Maximum report frequency for pointer motion without a button.
    pub const MIN_INTERVAL_SECONDS: f64 = 1.0 / 60.0;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn would_emit(&self, col: u32, row: u32, now_seconds: f64) -> bool {
        if self.last_cell == Some((col, row)) {
            return false;
        }
        if self
            .last_emit
            .is_some_and(|last| now_seconds - last < Self::MIN_INTERVAL_SECONDS)
        {
            return false;
        }
        true
    }

    pub fn commit(&mut self, col: u32, row: u32, now_seconds: f64) {
        self.last_cell = Some((col, row));
        self.last_emit = Some(now_seconds);
    }
}

/// Per-terminal gate for button-held motion ("drag") reports.
///
/// xterm reports button-held motion only when the pointer crosses into a new
/// cell. winit delivers sub-pixel `CursorMoved` events while a button is down,
/// so a stationary click would otherwise emit press + a same-cell drag +
/// release; crossterm reads that drag as the start of a new gesture and
/// double-click detection never fires.
///
/// This is a cell-crossing gate, not a time throttle — every crossing is
/// reported, including a return to the press cell. The read-only decision and
/// the commit are split for the same reason [`MotionEmitter`] splits them: a
/// report the terminal encoder declined must not advance the memory.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DragCellGate {
    active: Option<(PointerButton, (u32, u32))>,
}

impl DragCellGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether this report reaches the terminal. Only button-held motion at
    /// the cell already reported for that button is withheld; motion with no
    /// recorded press dispatches, as does every press and release.
    pub fn would_dispatch(
        &self,
        action: PointerAction,
        button: Option<PointerButton>,
        cell: (u32, u32),
    ) -> bool {
        match (action, button) {
            (PointerAction::Motion, Some(button)) => self.active != Some((button, cell)),
            _ => true,
        }
    }

    /// Record a report the encoder actually produced bytes for. Release is
    /// the exception the caller must apply unconditionally: the gesture is
    /// over whether or not the release encoded.
    pub fn commit_dispatched(
        &mut self,
        action: PointerAction,
        button: Option<PointerButton>,
        cell: (u32, u32),
    ) {
        match action {
            PointerAction::Press | PointerAction::Motion => {
                // Wheel pseudo-presses (buttons four/five) are not a gesture:
                // they leave whatever drag is live untouched and start none.
                if let Some(
                    button @ (PointerButton::Left | PointerButton::Right | PointerButton::Middle),
                ) = button
                {
                    self.active = Some((button, cell));
                }
            }
            PointerAction::Release => self.active = None,
        }
    }

    pub fn reset(&mut self) {
        self.active = None;
    }
}

#[cfg(test)]
mod tests {
    use super::{DragCellGate, MotionEmitter, PointerAction, PointerButton};

    const A: (u32, u32) = (5, 3);
    const B: (u32, u32) = (6, 3);

    fn pressed_at(cell: (u32, u32)) -> DragCellGate {
        let mut gate = DragCellGate::new();
        gate.commit_dispatched(PointerAction::Press, Some(PointerButton::Left), cell);
        gate
    }

    #[test]
    fn press_is_never_gated() {
        let gate = pressed_at(A);
        assert!(gate.would_dispatch(PointerAction::Press, Some(PointerButton::Left), A));
    }

    #[test]
    fn same_cell_drag_after_press_is_suppressed() {
        let gate = pressed_at(A);
        assert!(!gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn declined_press_encode_does_not_arm_the_gate() {
        // The UI commits only when the encoder produced bytes, so a press the
        // terminal declined leaves the gate open for the first motion.
        let gate = DragCellGate::new();
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn release_clears_the_memory() {
        let mut gate = pressed_at(A);
        gate.commit_dispatched(PointerAction::Release, Some(PointerButton::Left), A);
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn release_is_never_gated() {
        let gate = pressed_at(A);
        assert!(gate.would_dispatch(PointerAction::Release, Some(PointerButton::Left), A));
    }

    #[test]
    fn reset_opens_the_gate() {
        let mut gate = pressed_at(A);
        gate.reset();
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn new_press_replaces_stale_state() {
        let mut gate = pressed_at(A);
        gate.commit_dispatched(PointerAction::Press, Some(PointerButton::Left), B);
        assert!(!gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), B));
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn wheel_pseudo_press_leaves_no_drag_active() {
        let mut gate = DragCellGate::new();
        gate.commit_dispatched(PointerAction::Press, Some(PointerButton::Four), A);
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Four), A));
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn wheel_pseudo_press_does_not_disturb_a_live_drag() {
        let mut gate = pressed_at(A);
        gate.commit_dispatched(PointerAction::Press, Some(PointerButton::Five), B);
        assert!(!gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
    }

    #[test]
    fn motion_without_a_recorded_press_dispatches() {
        let gate = DragCellGate::new();
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), A));
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Right), B));
    }

    #[test]
    fn motion_from_another_button_is_not_gated() {
        let gate = pressed_at(A);
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Right), A));
    }

    #[test]
    fn buttonless_motion_is_left_to_the_motion_emitter() {
        let gate = pressed_at(A);
        assert!(gate.would_dispatch(PointerAction::Motion, None, A));
    }

    #[test]
    fn cross_cell_drag_reports_every_crossing() {
        let mut gate = pressed_at(A);
        for cell in [(6, 3), (7, 3), (8, 3), (8, 4)] {
            assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), cell));
            gate.commit_dispatched(PointerAction::Motion, Some(PointerButton::Left), cell);
            assert!(!gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), cell));
        }
    }

    #[test]
    fn return_to_origin_reports_the_second_visit() {
        // press A -> motion A (suppressed) -> motion B -> motion A -> release.
        let mut gate = DragCellGate::new();
        let mut dispatched = Vec::new();
        let script = [
            (PointerAction::Press, A),
            (PointerAction::Motion, A),
            (PointerAction::Motion, B),
            (PointerAction::Motion, A),
            (PointerAction::Release, A),
        ];
        for (action, cell) in script {
            if !gate.would_dispatch(action, Some(PointerButton::Left), cell) {
                continue;
            }
            dispatched.push((action, cell));
            gate.commit_dispatched(action, Some(PointerButton::Left), cell);
        }
        assert_eq!(
            dispatched,
            vec![
                (PointerAction::Press, A),
                (PointerAction::Motion, B),
                (PointerAction::Motion, A),
                (PointerAction::Release, A),
            ]
        );
    }

    #[test]
    fn declined_motion_encode_does_not_advance_state() {
        let mut gate = pressed_at(A);
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), B));
        // No commit — the encoder declined; the same crossing still reports.
        assert!(gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), B));
        gate.commit_dispatched(PointerAction::Motion, Some(PointerButton::Left), B);
        assert!(!gate.would_dispatch(PointerAction::Motion, Some(PointerButton::Left), B));
    }

    #[test]
    fn first_motion_emits() {
        assert!(MotionEmitter::new().would_emit(5, 3, 0.0));
    }

    #[test]
    fn same_cell_is_deduplicated_after_interval() {
        let mut emitter = MotionEmitter::new();
        emitter.commit(5, 3, 0.0);
        assert!(!emitter.would_emit(5, 3, 0.100));
    }

    #[test]
    fn different_cell_obeys_sixty_hz_cap() {
        let mut emitter = MotionEmitter::new();
        emitter.commit(5, 3, 0.0);
        assert!(!emitter.would_emit(6, 3, 0.010));
        assert!(emitter.would_emit(6, 3, 0.020));
    }

    #[test]
    fn declined_encode_does_not_advance_state() {
        let emitter = MotionEmitter::new();
        assert!(emitter.would_emit(5, 3, 0.0));
        assert!(emitter.would_emit(5, 3, 0.050));
    }

    #[test]
    fn sustained_motion_is_capped_near_sixty_hz() {
        let mut emitter = MotionEmitter::new();
        let mut emits = 0;
        for ms in 0..1000 {
            let now = f64::from(ms) / 1000.0;
            let col = (ms as u32) % 80;
            if emitter.would_emit(col, 5, now) {
                emitter.commit(col, 5, now);
                emits += 1;
            }
        }
        assert!(
            (55..=70).contains(&emits),
            "expected ~60 emits, got {emits}"
        );
    }
}
