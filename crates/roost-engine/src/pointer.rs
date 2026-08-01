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

#[cfg(test)]
mod tests {
    use super::MotionEmitter;

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
