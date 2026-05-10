// Swift wrapper around libghostty-vt's render-state surface.
//
// Phase 5.4b scope: lifecycle (new/update/free) + default-colors
// readback. Per-cell walking lands in 5.4c.
//
// libghostty-vt's render state is a pull model: you call
// `ghostty_render_state_update` once per frame to snapshot the
// terminal, then read whatever you need (default colors, row + cell
// iterators, etc.). Snapshots are cheap; the expensive thing is
// walking 80x24 cells, which 5.4c will do.
//
// The Go cgo binding solved the same problem in
// `internal/ghostty/render.go`; this is a Swift port of just the
// pieces we need so far. Field-name parity with the upstream C
// header (size, foreground, background) is checked at compile time
// by the Swift importer.

import AppKit
import CGhosttyVT

final class RenderState {
    /// Opaque libghostty-vt render-state handle. Allocated on
    /// `init`, freed in `deinit`. Same `nonisolated(unsafe)`
    /// concurrency story as TerminalView.terminal — see comment there.
    nonisolated(unsafe) private var rs: GhosttyRenderState?

    init() {
        var handle: GhosttyRenderState?
        let rc = ghostty_render_state_new(nil, &handle)
        if rc.rawValue != 0 || handle == nil {
            fatalError("ghostty_render_state_new failed (rc.rawValue=\(rc.rawValue))")
        }
        self.rs = handle
    }

    deinit {
        if let rs {
            ghostty_render_state_free(rs)
        }
    }

    /// Snapshot the terminal's current state into this render state.
    /// Call once per frame before reading colors / walking cells.
    /// Must be called on the same thread as the terminal (main).
    func update(terminal: GhosttyTerminal) {
        guard let rs else { return }
        let rc = ghostty_render_state_update(rs, terminal)
        if rc.rawValue != 0 {
            // Update failures are unexpected at this stage of the
            // refactor — bail loudly so we notice in dev. Promotes to
            // a real error path once the renderer has a recovery
            // story (probably "skip this frame").
            fatalError("ghostty_render_state_update failed (rc.rawValue=\(rc.rawValue))")
        }
    }

    /// The terminal's current default foreground + background colors.
    /// `GhosttyRenderStateColors` is a sized struct (the C header's
    /// versioning convention); we set `size` to `MemoryLayout.size`
    /// before each call so libghostty-vt knows how much of the
    /// struct we understand.
    func defaultColors() -> (foreground: NSColor, background: NSColor) {
        guard let rs else {
            return (.white, .black)
        }
        var colors = GhosttyRenderStateColors()
        colors.size = MemoryLayout<GhosttyRenderStateColors>.size
        let rc = ghostty_render_state_colors_get(rs, &colors)
        if rc.rawValue != 0 {
            return (.white, .black)
        }
        return (
            foreground: nsColor(colors.foreground),
            background: nsColor(colors.background)
        )
    }
}

/// Convert a libghostty-vt RGB triple into an NSColor in sRGB. The
/// C struct uses `uint8_t` per channel; NSColor wants 0..1 floats.
private func nsColor(_ c: GhosttyColorRgb) -> NSColor {
    NSColor(
        srgbRed: CGFloat(c.r) / 255.0,
        green: CGFloat(c.g) / 255.0,
        blue: CGFloat(c.b) / 255.0,
        alpha: 1.0
    )
}
