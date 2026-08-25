// Strict-DECTCEM cursor decision tests. Historically had a Rust
// companion suite, `cursor_render_mode`, in the now-removed GTK UI
// that cross-checked this same truth table — mirroring Ghostty's
// `renderer/cursor.zig` `style()` priority chain (#246). iced resolves
// cursor visibility + visual style from libghostty's render state
// directly (`crates/roost-vt/src/render_state.rs`) rather than
// through an equivalent standalone decision function, so there is no
// current Rust peer to add cases to in lockstep.

import CGhosttyVT
import Testing

@testable import Roost

private let block = GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK
private let bar = GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BAR
private let underline = GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_UNDERLINE
private let blockHollow = GHOSTTY_RENDER_STATE_CURSOR_VISUAL_STYLE_BLOCK_HOLLOW

// MARK: - #246: hidden wins over focus

@Test
func cursorMode_hiddenFocused_none() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: false, blinking: true, hasFocus: true, blinkOn: true, visualStyle: block
        ) == nil,
        "DECTCEM-hidden cursor must not draw even when focused (#246)"
    )
}

@Test
func cursorMode_hiddenFocusedBlock_none() {
    // The Linux glyph-skip trap: a hidden focused block must resolve to
    // `nil` so the skip predicate never blanks the underlying glyph.
    #expect(
        TerminalView.cursorRenderMode(
            visible: false, blinking: false, hasFocus: true, blinkOn: true, visualStyle: block
        ) == nil
    )
}

@Test
func cursorMode_hiddenUnfocused_none() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: false, blinking: true, hasFocus: false, blinkOn: true, visualStyle: block
        ) == nil
    )
}

// MARK: - Unfocused: hollow outline, blink-independent

@Test
func cursorMode_visibleUnfocused_outline() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: false, blinkOn: true, visualStyle: block
        ) == .outline
    )
}

@Test
func cursorMode_visibleUnfocusedBlinkOff_stillOutline() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: false, blinkOn: false, visualStyle: bar
        ) == .outline,
        "unfocused outline is blink-independent"
    )
}

// MARK: - Focused blink gating

@Test
func cursorMode_focusedBlinkingBlinkOff_none() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: true, blinkOn: false, visualStyle: block
        ) == nil,
        "blinking cursor in the blink-off phase draws nothing"
    )
}

@Test
func cursorMode_focusedSteadyBlinkOff_drawn() {
    // The pre-existing steady-cursor fix: a non-blinking style
    // (DECSCUSR 2/4/6) ignores the blink phase (Ghostty parity).
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: false, hasFocus: true, blinkOn: false, visualStyle: block
        ) == .block,
        "steady cursor must not flash off (blinking=false ignores blinkOn)"
    )
}

// MARK: - Focused visible: visual style maps to shape

@Test
func cursorMode_focusedBlock_block() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: true, blinkOn: true, visualStyle: block
        ) == .block
    )
}

@Test
func cursorMode_focusedBar_bar() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: true, blinkOn: true, visualStyle: bar
        ) == .bar
    )
}

@Test
func cursorMode_focusedUnderline_underline() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: true, blinkOn: true, visualStyle: underline
        ) == .underline
    )
}

@Test
func cursorMode_focusedBlockHollow_outline() {
    #expect(
        TerminalView.cursorRenderMode(
            visible: true, blinking: true, hasFocus: true, blinkOn: true, visualStyle: blockHollow
        ) == .outline,
        "DECSCUSR block-hollow routes to the same outline path as blurred"
    )
}
