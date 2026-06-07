// Roost theme — Phase 6a M6.
//
// The theme system — same theme files (same names, same on-disk format)
// as the Rust/Linux UI. The bundled theme files live at
// `Sources/Roost/Resources/themes/`; the source-of-truth copy is in the
// Rust crate at `crates/roost-linux/src/resources/themes/`, and the two
// trees are kept byte-identical by `make themes-check`. Add a new theme
// to both trees (and to `BUNDLED_THEMES` in `theme.rs`).
//
// File format mirrors Ghostty's `themes/` directory entries
// (e.g. `palette = 0=#1a1a1a`, `background = #1e1e1e`,
// `selection-background = #3f638b`, …). We deliberately don't parse
// every key Ghostty's renderer recognizes — only the ones Roost
// actually applies in `TerminalView.draw(_:)` and the window chrome.

import AppKit
import CGhosttyVT
import Foundation

extension Bundle {
    /// The bundle carrying Roost's themes (`themes/`). In a packaged
    /// `.app` it lives at `Contents/Resources/Roost_Roost.bundle`,
    /// resolved here via `Bundle.main` — deliberately NOT via SwiftPM's
    /// generated `Bundle.module`. That accessor searches exactly
    /// `Bundle.main.bundleURL/Roost_Roost.bundle` (the `.app` ROOT) plus
    /// a build-machine path baked in at compile time; the root can't be
    /// populated (nested bundles outside `Contents/` break codesigning)
    /// and the build path doesn't exist on a user's machine, so
    /// `Bundle.module` `fatalError`ed on every clean install while
    /// dev/CI launches — which have that build path present — passed.
    /// Falls back to `Bundle.module` for `swift run` / tests, where no
    /// `.app` exists and the build-tree bundle does resolve.
    static var roostResources: Bundle {
        // `resourceURL` is `Contents/Resources` for a normally-launched
        // `.app`; the explicit `bundleURL/Contents/Resources` candidate
        // covers direct-exec contexts (e.g. running the binary by path),
        // where `resourceURL` can resolve elsewhere. First hit wins.
        for candidate in [
            Bundle.main.resourceURL?.appendingPathComponent("Roost_Roost.bundle"),
            Bundle.main.bundleURL
                .appendingPathComponent("Contents/Resources/Roost_Roost.bundle"),
        ] {
            if let url = candidate, let bundle = Bundle(url: url) { return bundle }
        }
        return .module
    }
}

/// Parsed terminal color scheme. Mirrors the Go binary's `Theme`
/// struct field-for-field so a future "share theme between Go and
/// Swift binaries" diagnostic stays trivial.
struct Theme: Sendable {
    var foreground: NSColor
    var background: NSColor
    var cursor: NSColor
    var selectionBackground: NSColor
    var selectionForeground: NSColor
    /// Ghostty `bold-color` accent. `nil` until a theme opts in;
    /// bold cells without an explicit SGR fg adopt this color when
    /// set, matching the Linux `Theme.bold_color` shape. Defaulted
    /// here so existing memberwise-init callers (smoke tests, future
    /// fixtures) keep compiling without naming the field.
    var boldColor: NSColor? = nil
    var palette: [NSColor]  // exactly 256 entries

    /// Hard-coded fallback so the UI has a sane theme before
    /// `loadBundled(name:)` runs (or when it errors). Mirrors the
    /// "roost-dark" theme's headline colors.
    static let fallback: Theme = .init(
        foreground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        background: NSColor(srgbRed: 0.118, green: 0.118, blue: 0.118, alpha: 1),
        cursor: NSColor(srgbRed: 0.6, green: 0.6, blue: 0.6, alpha: 1),
        selectionBackground: NSColor(srgbRed: 0.247, green: 0.388, blue: 0.545, alpha: 1),
        selectionForeground: NSColor(srgbRed: 1, green: 1, blue: 1, alpha: 1),
        boldColor: nil,
        palette: Theme.standardXterm256Palette()
    )

    /// The standard xterm 256-color palette: 16 ANSI colors (0–15), the
    /// 6×6×6 color cube (16–231), and the 24-step grayscale ramp
    /// (232–255). Used as the palette BASE that theme files override on
    /// top of — a theme file only specifies the 16 ANSI slots, so without
    /// this base every 256-color index (`SGR 48;5;N` / `38;5;N`) fell back
    /// to a flat placeholder and rendered the same wrong color. 256-color
    /// TUIs (vim/htop/lazygit, and opencode over SSH where COLORTERM is
    /// unset) depend on the cube + ramp being correct. Matches libghostty's
    /// and xterm's computed values.
    static func standardXterm256Palette() -> [NSColor] {
        func srgb(_ r: Int, _ g: Int, _ b: Int) -> NSColor {
            NSColor(srgbRed: CGFloat(r) / 255, green: CGFloat(g) / 255, blue: CGFloat(b) / 255, alpha: 1)
        }
        var p: [NSColor] = []
        p.reserveCapacity(256)
        // 0–15: standard ANSI (normal + bright).
        let ansi = [
            (0, 0, 0), (128, 0, 0), (0, 128, 0), (128, 128, 0),
            (0, 0, 128), (128, 0, 128), (0, 128, 128), (192, 192, 192),
            (128, 128, 128), (255, 0, 0), (0, 255, 0), (255, 255, 0),
            (0, 0, 255), (255, 0, 255), (0, 255, 255), (255, 255, 255),
        ]
        for (r, g, b) in ansi { p.append(srgb(r, g, b)) }
        // 16–231: 6×6×6 color cube. Channel levels are 0, then 95+40·n.
        let levels = [0, 95, 135, 175, 215, 255]
        for i in 0..<216 {
            p.append(srgb(levels[(i / 36) % 6], levels[(i / 6) % 6], levels[i % 6]))
        }
        // 232–255: 24-step grayscale ramp, 8 + 10·n.
        for i in 0..<24 {
            let v = 8 + i * 10
            p.append(srgb(v, v, v))
        }
        return p
    }

    /// Look up a theme by bundled file name. Names match the
    /// embedded filenames verbatim (`"Dracula+"`, `"Catppuccin
    /// Mocha"`, `"roost-dark"`, …); no extension. Returns the
    /// fallback theme on any parse / resource-lookup error so the
    /// UI never goes color-less.
    @MainActor
    static func loadBundled(name: String) -> Theme {
        guard let url = Bundle.roostResources.url(
            forResource: name,
            withExtension: nil,
            subdirectory: "themes"
        ) else {
            NSLog("roost-mac: theme %@ not found in bundle; using fallback", name)
            return .fallback
        }
        do {
            let data = try String(contentsOf: url, encoding: .utf8)
            return try parse(data)
        } catch {
            NSLog("roost-mac: theme %@ parse failed: %@", name, "\(error)")
            return .fallback
        }
    }

    /// Returns the list of bundled theme names (sorted). Useful for
    /// future "themes" picker UI; M6 doesn't use it but the
    /// equivalent `BundledThemeNames()` lives on the Go side and
    /// keeps the surface parallel.
    @MainActor
    static func bundledNames() -> [String] {
        guard let urls = Bundle.roostResources.urls(
            forResourcesWithExtension: nil,
            subdirectory: "themes"
        ) else { return [] }
        return urls.map { $0.lastPathComponent }.sorted()
    }
}

/// Parse a Ghostty-format theme file. Same line-per-key shape as
/// `cmd/roost/theme.go::parseTheme`:
///   palette = N=#RRGGBB
///   background = #RRGGBB
///   foreground = #RRGGBB
///   cursor-color = #RRGGBB
///   selection-background = #RRGGBB
///   selection-foreground = #RRGGBB
/// Unknown keys are dropped (forward-compat with Ghostty additions).
/// Blank lines + `#` comments are skipped. Throws on syntactically
/// broken color literals.
private func parse(_ text: String) throws -> Theme {
    var theme = Theme.fallback
    var palette = theme.palette
    for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if line.isEmpty || line.hasPrefix("#") { continue }
        guard let eqIdx = line.firstIndex(of: "=") else { continue }
        let key = line[..<eqIdx].trimmingCharacters(in: .whitespaces)
        let rest = line[line.index(after: eqIdx)...]
            .trimmingCharacters(in: .whitespaces)
        switch key {
        case "palette":
            // `palette = N=#RRGGBB`
            guard let inner = rest.firstIndex(of: "=") else { continue }
            let nStr = rest[..<inner].trimmingCharacters(in: .whitespaces)
            let cStr = rest[rest.index(after: inner)...]
                .trimmingCharacters(in: .whitespaces)
            if let n = Int(nStr), n >= 0, n < 256,
               let c = parseHexColor(cStr)
            {
                palette[n] = c
            }
        case "background":
            if let c = parseHexColor(rest) { theme.background = c }
        case "foreground":
            if let c = parseHexColor(rest) { theme.foreground = c }
        case "cursor-color":
            if let c = parseHexColor(rest) { theme.cursor = c }
        case "selection-background":
            if let c = parseHexColor(rest) { theme.selectionBackground = c }
        case "selection-foreground":
            if let c = parseHexColor(rest) { theme.selectionForeground = c }
        case "bold-color":
            if let c = parseHexColor(rest) { theme.boldColor = c }
        default:
            // Ghostty has many more keys (cursor-text, link-color, …).
            // Drop them silently rather than erroring so a user can
            // keep an unchanged Ghostty theme file on disk and have
            // Roost honor what it can.
            continue
        }
    }
    theme.palette = palette
    return theme
}

// MARK: - libghostty-vt application (Phase 6a P3)

extension Theme {
    /// Colors pre-converted to libghostty's RGB structs. Converting the
    /// 256-entry palette runs `NSColor.usingColorSpace(.sRGB)` 256 times;
    /// the command palette's live theme preview broadcasts one theme to
    /// every open terminal on each arrow keypress, so we resolve once
    /// (`resolved()`) and reuse the result per terminal rather than
    /// re-converting N times.
    struct Resolved {
        var foreground: GhosttyColorRgb
        var background: GhosttyColorRgb
        var cursor: GhosttyColorRgb
        var palette: [GhosttyColorRgb]  // exactly 256 entries
    }

    @MainActor
    func resolved() -> Resolved {
        var palette = [GhosttyColorRgb](repeating: ghosttyColor(.black), count: 256)
        for i in 0..<min(self.palette.count, 256) {
            palette[i] = ghosttyColor(self.palette[i])
        }
        return Resolved(
            foreground: ghosttyColor(foreground),
            background: ghosttyColor(background),
            cursor: ghosttyColor(cursor),
            palette: palette
        )
    }

    /// Push fg / bg / cursor + 256-color palette into a libghostty-vt
    /// terminal. Mirrors the Go binary's
    /// `internal/ghostty/terminal.go::SetTheme`.
    ///
    /// Safe to call at any point in a terminal's life, including after
    /// `ghostty_terminal_vt_write` — the command palette's live theme
    /// preview relies on this, and `themeAppliesAfterVtWrite` pins it.
    /// The palette set preserves any per-index OSC overrides a program
    /// applied, so OSC-overridden / truecolor cells keep their colors.
    ///
    /// libghostty-vt's `GHOSTTY_TERMINAL_OPT_COLOR_*` options take a
    /// pointer to a `GhosttyColorRgb` (or `[256]GhosttyColorRgb` for the
    /// palette), read synchronously inside the call — the local storage
    /// doesn't need to outlive the call.
    @MainActor
    static func apply(_ colors: Resolved, to terminal: GhosttyTerminal) {
        func set(_ option: GhosttyTerminalOption, _ value: UnsafeMutableRawPointer, _ what: StaticString) {
            let rc = ghostty_terminal_set(terminal, option, value)
            if rc.rawValue != 0 {
                NSLog("roost-mac: ghostty_terminal_set(%@) failed rc=%d", "\(what)", rc.rawValue)
            }
        }
        var fg = colors.foreground
        set(GHOSTTY_TERMINAL_OPT_COLOR_FOREGROUND, &fg, "foreground")
        var bg = colors.background
        set(GHOSTTY_TERMINAL_OPT_COLOR_BACKGROUND, &bg, "background")
        var cursor = colors.cursor
        set(GHOSTTY_TERMINAL_OPT_COLOR_CURSOR, &cursor, "cursor")
        // The palette is a contiguous 256-entry C array; pass its base
        // pointer. The 256 count matches libghostty's signature exactly;
        // sending fewer is undefined behaviour per the header docs.
        var palette = colors.palette
        palette.withUnsafeMutableBufferPointer { buf in
            guard let base = buf.baseAddress else { return }
            set(GHOSTTY_TERMINAL_OPT_COLOR_PALETTE, UnsafeMutableRawPointer(base), "palette")
        }
    }

    /// Convenience for the single-terminal path (terminal init): resolve
    /// the theme's colors and apply them in one step.
    @MainActor
    static func apply(_ theme: Theme, to terminal: GhosttyTerminal) {
        apply(theme.resolved(), to: terminal)
    }
}

/// Convert an NSColor (in any color space) to libghostty's RGB struct.
/// Converts to sRGB first to match the bit-exact values the Go binary
/// pushes; `usingColorSpace(.sRGB)` returns nil for color spaces that
/// can't be represented in sRGB (rare for the named colors we use),
/// in which case we fall back to a black pixel rather than corrupting
/// the FFI struct with NaN-derived bytes.
@MainActor
private func ghosttyColor(_ color: NSColor) -> GhosttyColorRgb {
    let srgb = color.usingColorSpace(.sRGB) ?? .black
    let r = UInt8((srgb.redComponent * 255.0).rounded().clamped(to: 0...255))
    let g = UInt8((srgb.greenComponent * 255.0).rounded().clamped(to: 0...255))
    let b = UInt8((srgb.blueComponent * 255.0).rounded().clamped(to: 0...255))
    return GhosttyColorRgb(r: r, g: g, b: b)
}

private extension Comparable {
    /// Clamp `self` into a closed range. Generic helper because Swift
    /// stdlib's only built-in clamp lives on `Strideable` ranges, not
    /// the closed-range arithmetic the color conversion above wants.
    func clamped(to range: ClosedRange<Self>) -> Self {
        min(max(self, range.lowerBound), range.upperBound)
    }
}

/// Parse `#RRGGBB` or `#RGB`. Lenient: returns nil for invalid
/// strings rather than throwing, so a stray field doesn't kill the
/// whole theme load.
private func parseHexColor(_ s: String) -> NSColor? {
    var hex = s
    if hex.hasPrefix("#") { hex.removeFirst() }
    guard let v = UInt32(hex, radix: 16) else { return nil }
    switch hex.count {
    case 6:
        let r = CGFloat((v >> 16) & 0xff) / 255.0
        let g = CGFloat((v >> 8) & 0xff) / 255.0
        let b = CGFloat(v & 0xff) / 255.0
        return NSColor(srgbRed: r, green: g, blue: b, alpha: 1)
    case 3:
        // RGB short form — each nibble expands to a pair.
        let r = CGFloat(((v >> 8) & 0xf) * 0x11) / 255.0
        let g = CGFloat(((v >> 4) & 0xf) * 0x11) / 255.0
        let b = CGFloat((v & 0xf) * 0x11) / 255.0
        return NSColor(srgbRed: r, green: g, blue: b, alpha: 1)
    default:
        return nil
    }
}
