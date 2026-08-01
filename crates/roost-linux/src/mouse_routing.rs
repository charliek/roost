//! GTK mouse-routing decisions and W3C cursor-shape mapping.
//!
//! Sibling of `mac/Sources/Roost/MouseRouting.swift` (PR A). Same
//! The toolkit-neutral pointer DTOs and motion throttle live in
//! `roost-engine`; this adapter retains the established GTK aliases and the
//! local URL/cursor precedence rules.
//!
//! libghostty-vt's `MouseEncoder` already gates on the negotiated
//! DEC mode (1000 / 1002 / 1003 / 1006 / 1015 / 1016) — an `encode`
//! call returns empty bytes when the mode declines. The GTK UI only
//! needs to decide whether to call the encoder at all, vs routing
//! the event to selection / paste / URL hover.

/// Engine-owned pointer DTOs retain the GTK adapter's established aliases.
pub use roost_engine::pointer::{
    MotionEmitter, PointerAction as MouseRoutingAction, PointerButton as MouseRoutingButton,
};

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
///    is true when the link modifier is held over a URL hit — Cmd on
///    macOS, Alt on Linux by default, `link-modifier`-configurable).
///    Matches ghostty.
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

    fn encode_focus_bytes(focused: bool) -> Vec<u8> {
        let mut terminal = roost_vt::Terminal::new(roost_vt::TerminalOptions {
            cols: 1,
            rows: 1,
            max_scrollback: 0,
        })
        .expect("allocate focus encoder test terminal");
        terminal.vt_write(b"\x1b[?1004h");
        terminal.encode_focus(focused)
    }

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
        assert!(!e.would_emit(5, 3, 0.100));
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
