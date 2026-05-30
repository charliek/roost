// MouseRoutingTests — pure-helper tests for the
// `computeMouseTrackingDispatch` decision matrix.
//
// The Mac `TerminalView` calls into this helper from
// `mouseDown` / `mouseUp` / `mouseDragged` / `mouseMoved` /
// `rightMouseDown` / `rightMouseUp` to decide whether to forward
// an event to the mouse encoder or pass through to the existing
// selection / paste / URL paths. URL precedence wins over
// mouse forwarding (matches ghostty). Mouse tracking off → pass
// through. Otherwise → forward with the (action, button) parameters
// the caller supplied.
//
// The encoder's own gating on the negotiated DEC mode (1000 / 1002
// / 1003 / 1006 / 1015 / 1016) is libghostty-vt's job and is
// covered by the Rust mouse_encoder tests. This file only checks
// the routing layer above it.

import Foundation
import Testing

@testable import Roost

@Suite("Mouse routing dispatch")
struct MouseRoutingTests {

    // MARK: tracking off → pass through

    @Test("Press LEFT with tracking off → pass through (selection anchor)")
    func pressLeftTrackingOff() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .left,
            isMouseTrackingActive: false,
            urlInterceptsClick: false
        ) == .passThrough)
    }

    @Test("Release LEFT with tracking off → pass through")
    func releaseLeftTrackingOff() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .release,
            button: .left,
            isMouseTrackingActive: false,
            urlInterceptsClick: false
        ) == .passThrough)
    }

    @Test("Motion with tracking off → pass through")
    func motionTrackingOff() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .motion,
            button: nil,
            isMouseTrackingActive: false,
            urlInterceptsClick: false
        ) == .passThrough)
    }

    // MARK: URL precedence beats mouse tracking

    @Test("Cmd-click on URL → pass through even when tracking on")
    func cmdClickOnUrlAlwaysPassThrough() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .left,
            isMouseTrackingActive: true,
            urlInterceptsClick: true
        ) == .passThrough)
    }

    @Test("Cmd-click on URL with tracking off → pass through")
    func cmdClickOnUrlTrackingOff() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .left,
            isMouseTrackingActive: false,
            urlInterceptsClick: true
        ) == .passThrough)
    }

    // MARK: tracking on, no URL precedence → forward

    @Test("Press LEFT, tracking on, no URL → forward(press, left)")
    func pressLeftTrackingOn() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .left,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .press, button: .left))
    }

    @Test("Release LEFT, tracking on → forward(release, left)")
    func releaseLeftTrackingOn() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .release,
            button: .left,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .release, button: .left))
    }

    @Test("Motion + LEFT (drag), tracking on → forward(motion, left)")
    func motionDragTrackingOn() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .motion,
            button: .left,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .motion, button: .left))
    }

    @Test("Motion no-button, tracking on → forward(motion, nil)")
    func motionNoButtonTrackingOn() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .motion,
            button: nil,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .motion, button: nil))
    }

    @Test("Press RIGHT, tracking on → forward(press, right)")
    func pressRightTrackingOn() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .right,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .press, button: .right))
    }

    @Test("Press RIGHT, tracking off → pass through (no-op upstream)")
    func pressRightTrackingOff() {
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .right,
            isMouseTrackingActive: false,
            urlInterceptsClick: false
        ) == .passThrough)
    }

    @Test("Press MIDDLE, tracking on → forward(press, middle)")
    func pressMiddleTrackingOn() {
        // Middle-button forwarding to PTY is technically Tier 3
        // scope (conflicts with paste-on-middle), but the routing
        // helper itself doesn't filter — the caller decides. Lock
        // in that the helper doesn't drop middle silently.
        #expect(computeMouseTrackingDispatch(
            eventKind: .press,
            button: .middle,
            isMouseTrackingActive: true,
            urlInterceptsClick: false
        ) == .forward(action: .press, button: .middle))
    }
}
