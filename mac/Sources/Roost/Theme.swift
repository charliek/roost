// Roost theme — Phase 6a M6.
//
// Ports the theme system from the Go binary's `cmd/roost/theme.go` so
// users can keep using the same theme files (same names, same on-disk
// format) under the Rust+Swift port. The bundled theme files live at
// `Sources/Roost/Resources/themes/` and are copied verbatim from
// `cmd/roost/themes/`; new themes added on the Go side need to be
// mirrored here until the Phase 9 cutover collapses the two trees.
//
// File format mirrors Ghostty's `themes/` directory entries
// (e.g. `palette = 0=#1a1a1a`, `background = #1e1e1e`,
// `selection-background = #3f638b`, …). We deliberately don't parse
// every key Ghostty's renderer recognizes — only the ones Roost
// actually applies in `TerminalView.draw(_:)` and the window chrome.

import AppKit
import Foundation

/// Parsed terminal color scheme. Mirrors the Go binary's `Theme`
/// struct field-for-field so a future "share theme between Go and
/// Swift binaries" diagnostic stays trivial.
struct Theme: Sendable {
    var foreground: NSColor
    var background: NSColor
    var cursor: NSColor
    var selectionBackground: NSColor
    var selectionForeground: NSColor
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
        palette: Array(repeating: NSColor.gray, count: 256)
    )

    /// Look up a theme by bundled file name. Names match the
    /// embedded filenames verbatim (`"Dracula+"`, `"Catppuccin
    /// Mocha"`, `"roost-dark"`, …); no extension. Returns the
    /// fallback theme on any parse / resource-lookup error so the
    /// UI never goes color-less.
    @MainActor
    static func loadBundled(name: String) -> Theme {
        guard let url = Bundle.module.url(
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
        guard let urls = Bundle.module.urls(
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
        default:
            // Ghostty has many more keys (bold-color, cursor-text,
            // link-color, …). Drop them silently rather than erroring
            // so a user can keep an unchanged Ghostty theme file on
            // disk and have Roost honor what it can.
            continue
        }
    }
    theme.palette = palette
    return theme
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
