//! Mouse-tracking routing decisions + W3C cursor-shape mapping +
//! mode 1003 motion throttle for the GTK UI.
//!
//! Sibling of `mac/Sources/Roost/MouseRouting.swift` (PR A). Same
//! contract: pure functions + a `MotionEmitter` struct usable
//! without a `gtk4::DrawingArea`, so unit tests can pin the routing
//! decision matrix and the 60 Hz throttle without spinning a GTK
//! event loop.
//!
//! libghostty-vt's `MouseEncoder` already gates on the negotiated
//! DEC mode (1000 / 1002 / 1003 / 1006 / 1015 / 1016) — an `encode`
//! call returns empty bytes when the mode declines. The GTK UI only
//! needs to decide whether to call the encoder at all, vs routing
//! the event to selection / paste / URL hover.

/// One of the three mouse actions libghostty-vt's encoder accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRoutingAction {
    Press,
    Release,
    Motion,
}

/// One of the five buttons the encoder reports on. Wheel up =
/// `Four`, wheel down = `Five`. `None` is used at call sites that
/// mean "no button" (motion-no-button under mode 1003).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRoutingButton {
    Left,
    Right,
    Middle,
    Four,
    Five,
}

/// What the call site should do with this mouse event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseRoutingDispatch {
    /// Forward the event to the encoder with these parameters. A
    /// `None` button means "clear the button on the event" — the
    /// motion-no-button path for mode 1003.
    Forward {
        action: MouseRoutingAction,
        button: Option<MouseRoutingButton>,
    },
    /// Don't forward to the encoder. Fall through to selection /
    /// paste / URL hover / whatever the legacy path was.
    PassThrough,
}

/// Decide whether a mouse event should be forwarded to the PTY via
/// the mouse encoder, or passed through to the existing
/// selection/paste/URL paths.
///
/// Precedence (highest → lowest):
/// 1. URL takes priority over mouse forwarding (`url_intercepts_click`
///    is true when Ctrl is held over a URL hit, the GTK equivalent
///    of macOS Cmd-hover). Matches ghostty.
/// 2. Mouse tracking off → pass through.
/// 3. Otherwise → forward to encoder.
pub fn compute_mouse_tracking_dispatch(
    event_kind: MouseRoutingAction,
    button: Option<MouseRoutingButton>,
    is_mouse_tracking_active: bool,
    url_intercepts_click: bool,
) -> MouseRoutingDispatch {
    if url_intercepts_click {
        return MouseRoutingDispatch::PassThrough;
    }
    if !is_mouse_tracking_active {
        return MouseRoutingDispatch::PassThrough;
    }
    MouseRoutingDispatch::Forward {
        action: event_kind,
        button,
    }
}

/// 60 Hz cap + per-cell dedup for mode 1003 motion-no-button
/// reports. Split into `would_emit` (read-only peek) + `commit`
/// (advance state) so the encoder-decline case doesn't lock
/// state — mirrors `mac/Sources/Roost/MouseRouting.swift`'s
/// `MotionEmitter`.
///
/// `now_seconds` is a monotonic-clock-style timestamp in seconds
/// (`std::time::Instant::elapsed`). Tests inject a hand-rolled
/// clock instead.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct MotionEmitter {
    /// Last cell we emitted a report for. `None` until the first
    /// commit. Private so the `would_emit` / `commit` invariants
    /// are the only way to advance state; tests within this crate
    /// access via the in-module `mod tests` rules.
    last_cell: Option<(u32, u32)>,
    /// Monotonic seconds of the last emit. `None` until the first
    /// commit.
    last_emit: Option<f64>,
}

impl MotionEmitter {
    /// 60 Hz cap: 16 ms minimum gap between emits.
    pub const MIN_INTERVAL_SECONDS: f64 = 1.0 / 60.0;

    pub fn new() -> Self {
        Self::default()
    }

    /// Pure read-only check: would `commit(col, row, now)` actually
    /// advance the state and emit? Used by the production path to
    /// skip the encoder + scratch alloc when the answer is "no".
    /// The caller must follow up with `commit` after a successful
    /// encode so the throttle doesn't lock state on events that the
    /// encoder declined (e.g. mode 1003 toggling on between two
    /// same-cell motions).
    pub fn would_emit(&self, col: u32, row: u32, now_seconds: f64) -> bool {
        if let Some((lc, lr)) = self.last_cell {
            if lc == col && lr == row {
                return false;
            }
        }
        if let Some(last) = self.last_emit {
            if now_seconds - last < Self::MIN_INTERVAL_SECONDS {
                return false;
            }
        }
        true
    }

    /// Advance the throttle state. Call this only after the encoder
    /// successfully produced bytes — committing on a declined
    /// encode would silently suppress the next event the encoder
    /// would have accepted.
    pub fn commit(&mut self, col: u32, row: u32, now_seconds: f64) {
        self.last_cell = Some((col, row));
        self.last_emit = Some(now_seconds);
    }
}

/// Map a W3C CSS cursor name (as carried by OSC 22) to the matching
/// GTK cursor name accepted by `widget.set_cursor_from_name(...)`.
/// W3C and GTK share most names; this helper drops unknown names to
/// `"default"` (matches the Mac `nsCursorForW3CName` fallback) and
/// normalizes the empty payload to `"default"` so the GTK call
/// always succeeds.
pub fn gtk_cursor_name_for_w3c(name: &str) -> &'static str {
    match name {
        "" | "default" => "default",
        "pointer" => "pointer",
        "text" => "text",
        "crosshair" => "crosshair",
        "grab" => "grab",
        "grabbing" => "grabbing",
        "not-allowed" => "not-allowed",
        "col-resize" => "col-resize",
        "row-resize" => "row-resize",
        "e-resize" => "e-resize",
        "w-resize" => "w-resize",
        "n-resize" => "n-resize",
        "s-resize" => "s-resize",
        "ne-resize" => "ne-resize",
        "nw-resize" => "nw-resize",
        "se-resize" => "se-resize",
        "sw-resize" => "sw-resize",
        "wait" => "wait",
        "progress" => "progress",
        "help" => "help",
        "move" => "move",
        _ => "default",
    }
}

/// Canonical form of an OSC 22 W3C name for the `app.cursor_shape`
/// IPC op. Maps the empty reset form to `"default"` so test
/// clients can always assert against a non-empty name. Unknown
/// names pass through verbatim — the renderer falls back to
/// "default" via `gtk_cursor_name_for_w3c`, but the canonical name
/// is still the raw payload, so tests can pin "I asked for X; got
/// X back" without depending on the mapping. Mirrors
/// `mac/Sources/Roost/MouseRouting.swift::canonicalCursorShape`.
pub fn canonical_cursor_shape(name: &str) -> String {
    if name.is_empty() {
        "default".to_string()
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- compute_mouse_tracking_dispatch ----

    #[test]
    fn press_left_tracking_off_passes_through() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Left),
                false,
                false,
            ),
            MouseRoutingDispatch::PassThrough
        );
    }

    #[test]
    fn release_left_tracking_off_passes_through() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Release,
                Some(MouseRoutingButton::Left),
                false,
                false,
            ),
            MouseRoutingDispatch::PassThrough
        );
    }

    #[test]
    fn motion_tracking_off_passes_through() {
        assert_eq!(
            compute_mouse_tracking_dispatch(MouseRoutingAction::Motion, None, false, false,),
            MouseRoutingDispatch::PassThrough
        );
    }

    #[test]
    fn ctrl_click_on_url_passes_through_even_when_tracking_on() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Left),
                true,
                true,
            ),
            MouseRoutingDispatch::PassThrough
        );
    }

    #[test]
    fn press_left_tracking_on_forwards() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Left),
                true,
                false,
            ),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Press,
                button: Some(MouseRoutingButton::Left),
            }
        );
    }

    #[test]
    fn release_left_tracking_on_forwards() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Release,
                Some(MouseRoutingButton::Left),
                true,
                false,
            ),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Release,
                button: Some(MouseRoutingButton::Left),
            }
        );
    }

    #[test]
    fn drag_motion_forwards() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Motion,
                Some(MouseRoutingButton::Left),
                true,
                false,
            ),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Motion,
                button: Some(MouseRoutingButton::Left),
            }
        );
    }

    #[test]
    fn motion_no_button_forwards_with_none() {
        assert_eq!(
            compute_mouse_tracking_dispatch(MouseRoutingAction::Motion, None, true, false,),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Motion,
                button: None,
            }
        );
    }

    #[test]
    fn right_press_tracking_on_forwards() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Right),
                true,
                false,
            ),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Press,
                button: Some(MouseRoutingButton::Right),
            }
        );
    }

    #[test]
    fn right_press_tracking_off_passes_through() {
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Right),
                false,
                false,
            ),
            MouseRoutingDispatch::PassThrough
        );
    }

    #[test]
    fn middle_press_routing_helper_does_not_filter() {
        // The helper passes middle through; the call site (GTK
        // middle-click handler) decides whether to route to paste vs
        // PTY. Locks in that the helper itself doesn't drop middle.
        assert_eq!(
            compute_mouse_tracking_dispatch(
                MouseRoutingAction::Press,
                Some(MouseRoutingButton::Middle),
                true,
                false,
            ),
            MouseRoutingDispatch::Forward {
                action: MouseRoutingAction::Press,
                button: Some(MouseRoutingButton::Middle),
            }
        );
    }

    // ---- MotionEmitter ----

    #[test]
    fn motion_first_emit_passes() {
        let e = MotionEmitter::new();
        assert!(e.would_emit(5, 3, 0.0));
    }

    #[test]
    fn motion_same_cell_within_min_interval_suppresses() {
        let mut e = MotionEmitter::new();
        e.commit(5, 3, 0.0);
        assert!(!e.would_emit(5, 3, 0.005));
    }

    #[test]
    fn motion_same_cell_after_100ms_still_suppresses() {
        // Per-cell dedup beats the 16 ms rate cap.
        let mut e = MotionEmitter::new();
        e.commit(5, 3, 0.0);
        assert!(!e.would_emit(5, 3, 0.100));
    }

    #[test]
    fn motion_different_cell_within_min_interval_suppresses() {
        let mut e = MotionEmitter::new();
        e.commit(5, 3, 0.0);
        assert!(!e.would_emit(6, 3, 0.010));
    }

    #[test]
    fn motion_different_cell_after_min_interval_emits() {
        let mut e = MotionEmitter::new();
        e.commit(5, 3, 0.0);
        assert!(e.would_emit(6, 3, 0.020));
    }

    #[test]
    fn motion_peek_does_not_advance_state() {
        // Production contract: a `would_emit` peek must NOT mutate
        // state, so a declined encode (encoder returned empty) can
        // retry on the next event with the same throttle window.
        let mut e = MotionEmitter::new();
        e.commit(5, 3, 0.0);
        let before = e;
        let _ = e.would_emit(6, 3, 0.020);
        assert_eq!(e, before);
    }

    #[test]
    fn motion_commit_after_declined_peek_advances_correctly() {
        // Mode 1000 only: peek at cell A says emit (first call).
        // Encoder declines (no motion in mode 1000). We do NOT
        // commit. Mode 1003 toggles on. Same cell A → peek STILL
        // says emit (state didn't advance), encoder emits this
        // time, we commit. Bug Mac's pytest caught during PR A.
        let mut e = MotionEmitter::new();
        assert!(e.would_emit(5, 3, 0.0));
        // Encoder declined — no commit.
        assert!(e.would_emit(5, 3, 0.050));
        e.commit(5, 3, 0.050);
        assert_eq!(e.last_cell, Some((5, 3)));
    }

    #[test]
    fn motion_sixty_hz_cap() {
        let mut e = MotionEmitter::new();
        let mut emits = 0;
        for ms in 0..1000 {
            let now = ms as f64 / 1000.0;
            let col = (ms as u32) % 80;
            if e.would_emit(col, 5, now) {
                e.commit(col, 5, now);
                emits += 1;
            }
        }
        assert!(
            (55..=70).contains(&emits),
            "expected ~60 emits, got {emits}"
        );
    }

    // ---- gtk_cursor_name_for_w3c ----

    #[test]
    fn cursor_empty_maps_to_default() {
        assert_eq!(gtk_cursor_name_for_w3c(""), "default");
    }

    #[test]
    fn cursor_default_passes_through() {
        assert_eq!(gtk_cursor_name_for_w3c("default"), "default");
    }

    #[test]
    fn cursor_pointer_passes_through() {
        // Strix's divider-grab cursor.
        assert_eq!(gtk_cursor_name_for_w3c("pointer"), "pointer");
    }

    #[test]
    fn cursor_text_passes_through() {
        assert_eq!(gtk_cursor_name_for_w3c("text"), "text");
    }

    #[test]
    fn cursor_grabbing_passes_through() {
        assert_eq!(gtk_cursor_name_for_w3c("grabbing"), "grabbing");
    }

    #[test]
    fn cursor_resize_variants_pass_through() {
        for name in [
            "col-resize",
            "row-resize",
            "n-resize",
            "s-resize",
            "e-resize",
            "w-resize",
            "ne-resize",
            "nw-resize",
            "se-resize",
            "sw-resize",
        ] {
            assert_eq!(gtk_cursor_name_for_w3c(name), name);
        }
    }

    #[test]
    fn cursor_unknown_falls_back_to_default() {
        // Silently ignore unknowns — matches ghostty.
        assert_eq!(gtk_cursor_name_for_w3c("not_a_real_shape"), "default");
        assert_eq!(gtk_cursor_name_for_w3c("zoom-in"), "default");
    }

    // ---- canonical_cursor_shape ----

    #[test]
    fn canonical_empty_to_default() {
        assert_eq!(canonical_cursor_shape(""), "default");
    }

    #[test]
    fn canonical_pointer_passthrough() {
        assert_eq!(canonical_cursor_shape("pointer"), "pointer");
    }

    #[test]
    fn canonical_unknown_passthrough() {
        // Canonical pins the RAW payload so tests can assert "I
        // asked for X; UI received X" independently of the
        // platform mapping.
        assert_eq!(
            canonical_cursor_shape("not_a_real_shape"),
            "not_a_real_shape"
        );
    }

    // ---- encode_focus_bytes ----

    #[test]
    fn focus_in_bytes_match_csi_i() {
        // CSI I = ESC [ I — the xterm focus-gained sequence per
        // mode 1004. A regression that swapped CSI I / CSI O would
        // silently invert focus state on every TUI; pin both here
        // with their exact byte values.
        assert_eq!(encode_focus_bytes(true), vec![0x1B, 0x5B, 0x49]);
    }

    #[test]
    fn focus_out_bytes_match_csi_o() {
        assert_eq!(encode_focus_bytes(false), vec![0x1B, 0x5B, 0x4F]);
    }

    #[test]
    fn focus_in_and_out_differ_by_one_byte() {
        let g = encode_focus_bytes(true);
        let l = encode_focus_bytes(false);
        assert_eq!(g.len(), 3);
        assert_eq!(l.len(), 3);
        assert_eq!(g[0], l[0]);
        assert_eq!(g[1], l[1]);
        assert_ne!(g[2], l[2]);
    }
}

/// Encode the xterm focus-tracking sequence via libghostty-vt's
/// `ghostty_focus_encode`. CSI I for focus-gained, CSI O for
/// focus-lost. Returns an empty vector on any FFI hiccup; callers
/// guard on non-empty before pushing to the PTY.
///
/// Sibling of `TerminalView.encodeFocusBytes` on the Mac side. Both
/// route through the same C API so a regression in libghostty-vt's
/// encoding lands on both UIs at once.
pub fn encode_focus_bytes(focused: bool) -> Vec<u8> {
    use roost_vt::ffi;
    let event = if focused {
        ffi::GhosttyFocusEvent_GHOSTTY_FOCUS_GAINED
    } else {
        ffi::GhosttyFocusEvent_GHOSTTY_FOCUS_LOST
    };
    let mut buf = [0u8; 8];
    let mut written: usize = 0;
    // SAFETY: buf is a real local with capacity 8; written is a
    // real local. ghostty_focus_encode writes at most 8 bytes for
    // either focus event (CSI I / CSI O — 3 bytes each).
    let rc = unsafe {
        ffi::ghostty_focus_encode(event, buf.as_mut_ptr() as *mut _, buf.len(), &mut written)
    };
    if rc != 0 || written == 0 {
        return Vec::new();
    }
    buf[..written].to_vec()
}
