// User config — Phase 6a M6.
//
// Reads `~/.config/roost/config.conf` and surfaces the subset of
// settings the Swift UI honors. The file format mirrors the Go
// binary's spec (XDG-style path even on macOS — see CLAUDE.md and
// docs/development/spec.md for the rationale): one
// `key = value` per line, `#`-prefixed comments allowed,
// whitespace forgiving.
//
// M6 ships the read path for `theme`, `font-family`, `font-size`
// (which the UI applies on launch). Keybind overrides — the larger
// chunk of the Go-side config surface in `cmd/roost/shortcuts.go` —
// land as a follow-up slice; defaults already match the Go binary
// on Mac.

import Foundation

/// Resolved user config. All fields optional so the caller can
/// fall back to compiled-in defaults when the user hasn't set a
/// preference.
struct RoostConfig: Sendable {
    var themeName: String?
    var fontFamily: String?
    var fontSize: CGFloat?

    static let empty = RoostConfig(themeName: nil, fontFamily: nil, fontSize: nil)

    /// Read `~/.config/roost/config.conf`. Returns `.empty` when
    /// the file doesn't exist, is empty, or fails to parse — a
    /// missing config is the common path, not an error.
    static func load(from path: URL = defaultPath()) -> RoostConfig {
        guard let text = try? String(contentsOf: path, encoding: .utf8) else {
            return .empty
        }
        return parse(text)
    }

    /// `~/.config/roost/config.conf` — XDG-style even on macOS, by
    /// deliberate divergence from Apple HIG (matches Ghostty / nvim
    /// / fish / the Go binary's behavior).
    static func defaultPath() -> URL {
        let home = FileManager.default.homeDirectoryForCurrentUser
        return home
            .appendingPathComponent(".config")
            .appendingPathComponent("roost")
            .appendingPathComponent("config.conf")
    }
}

/// Parse a config-file body. Public for tests; never throws — any
/// parse error is dropped and the affected key stays at its default.
func parse(_ text: String) -> RoostConfig {
    var cfg = RoostConfig.empty
    for raw in text.split(separator: "\n", omittingEmptySubsequences: false) {
        let line = raw.trimmingCharacters(in: .whitespaces)
        if line.isEmpty || line.hasPrefix("#") { continue }
        guard let eqIdx = line.firstIndex(of: "=") else { continue }
        let key = line[..<eqIdx].trimmingCharacters(in: .whitespaces)
        let value = line[line.index(after: eqIdx)...]
            .trimmingCharacters(in: .whitespaces)
            // Strip surrounding quotes so a user can write either
            // `font-family = "JetBrains Mono"` or
            // `font-family = JetBrains Mono` and both work.
            .trimmingCharacters(in: CharacterSet(charactersIn: "\"'"))
        switch key {
        case "theme":
            cfg.themeName = value
        case "font-family":
            cfg.fontFamily = value
        case "font-size":
            if let n = Double(value), n > 0 {
                cfg.fontSize = CGFloat(n)
            }
        default:
            // Many other keys are valid in the Go binary's config
            // (keybind, font-style, …); silently drop the ones M6
            // doesn't yet consume.
            continue
        }
    }
    return cfg
}
