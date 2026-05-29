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
/// preference. `keybinds` is the ordered list of `keybind = …`
/// lines in the order they appear — `canonicalizeBindings`
/// applies them in order so later lines override earlier ones,
/// matching the Go binary's semantics.
/// Three-state `copy-on-select` config value matching Ghostty's
/// `off | true | clipboard` semantics. Mirrors
/// `crates/roost-linux/src/config.rs::CopyOnSelect` 1:1.
///
/// * `.off` — never auto-copy; the user must press the explicit copy
///   shortcut (`⌘C` on Mac, Ctrl+Shift+C on Linux).
/// * `.on` (default) — write the selection to the "selection
///   clipboard": a named per-app `NSPasteboard` on Mac (`PRIMARY` on
///   Linux). Middle-click pastes from that target. The system
///   clipboard (`⌘V` / Ctrl+Shift+V) is **not** touched.
/// * `.clipboard` — write to both the selection clipboard and the
///   system clipboard. Drag-and-paste-into-another-app works
///   without an explicit copy step.
enum CopyOnSelect: Sendable {
    case off
    case on
    case clipboard

    static let `default`: CopyOnSelect = .on

    /// Parse a config value. Accepts the Ghostty-compatible
    /// spellings; any other value returns `nil` so the caller can
    /// fall back to the default and log.
    static func parse(_ s: String) -> CopyOnSelect? {
        switch s.trimmingCharacters(in: .whitespaces).lowercased() {
        case "off", "false", "no": return .off
        case "true", "yes": return .on
        case "clipboard", "both": return .clipboard
        default: return nil
        }
    }
}

/// Two-state policy for OSC 52 program-initiated clipboard writes.
/// Matches the first two values of Ghostty's `clipboard-write`
/// (`allow | deny`); `ask` is deferred until the consent banner UI
/// lands. Default is `.allow` to match Ghostty's default.
enum ClipboardWrite: Sendable {
    case allow
    case deny

    static let `default`: ClipboardWrite = .allow

    /// Parse a config value. `allow | true | yes` → `.allow`,
    /// `deny | false | no` → `.deny`; any other value returns `nil`
    /// so the caller can fall back to the default and log.
    static func parse(_ s: String) -> ClipboardWrite? {
        switch s.trimmingCharacters(in: .whitespaces).lowercased() {
        case "allow", "true", "yes": return .allow
        case "deny", "false", "no": return .deny
        default: return nil
        }
    }
}

struct RoostConfig: Sendable {
    var themeName: String?
    var fontFamily: String?
    var fontSize: CGFloat?
    /// Round-4 R4: per-pill width bounds for the tab strip. `nil`
    /// falls back to the compiled-in defaults (80 / 220). A user
    /// who writes `tab-min-width = 0` or `tab-max-width = 0` in
    /// their config disables that bound — handy if you want the
    /// pre-round-4 behavior where pills grow to fit their title.
    var tabMinWidth: CGFloat?
    var tabMaxWidth: CGFloat?
    var keybinds: [Keybind] = []
    /// Launcher entries from repeated `command =` lines, in source
    /// order (= picker row order). A line missing `label`/`run` is
    /// skipped (see `parseCommandLine`).
    var commands: [CustomCommand] = []
    /// `copy-on-select` setting — controls what mouse-drag selections
    /// write to the clipboard on release. Defaults to `.on` (matches
    /// Ghostty's default on macOS).
    var copyOnSelect: CopyOnSelect = .default
    /// `clipboard-write` policy — controls whether programs running
    /// in the terminal can write the host clipboard via OSC 52.
    /// Defaults to `.allow` (matches Ghostty's default).
    var clipboardWrite: ClipboardWrite = .default
    /// `word-break-chars` setting — chars that count as word chars
    /// (beyond Unicode letters/digits) for double-click word
    /// expansion. Default matches Ghostty's `_-.+~/:@%`, keeping
    /// file paths + URLs whole on double-click. Despite the
    /// `-break-` name (kept for Ghostty compatibility) the value is
    /// the EXTRA word-char set, not the break-char set.
    var wordBreakChars: String = WordSelection.defaultWordChars

    static let empty = RoostConfig(
        themeName: nil,
        fontFamily: nil,
        fontSize: nil,
        tabMinWidth: nil,
        tabMaxWidth: nil,
        keybinds: [],
        commands: [],
        copyOnSelect: .default,
        clipboardWrite: .default,
        wordBreakChars: WordSelection.defaultWordChars
    )

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
    /// / fish / the Go binary's behavior). `ROOST_CONFIG` overrides it
    /// with an absolute file — used by the E2E harness to drive the
    /// command launcher off a seeded config (mirrors the GTK side).
    static func defaultPath() -> URL {
        if let override = ProcessInfo.processInfo.environment["ROOST_CONFIG"],
           !override.isEmpty {
            return URL(fileURLWithPath: override)
        }
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
        // Value with whitespace trimmed but quotes intact — the
        // `command` parser does its own quote-aware tokenizing, and
        // the unconditional quote-strip below would lop the closing
        // quote off a value like `run="…"`.
        let rawValue = line[line.index(after: eqIdx)...]
            .trimmingCharacters(in: .whitespaces)
        let value =
            rawValue
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
        case "tab-min-width":
            // Negative values are nonsense; 0 means "no floor".
            // Round-5 (CR on #73): reject min > max to avoid an
            // unsatisfiable Auto Layout constraint pair. `0` on
            // either side means "unbounded", so 0 max never
            // conflicts with any min.
            if let n = Double(value), n >= 0 {
                if let max = cfg.tabMaxWidth, max > 0, CGFloat(n) > max {
                    // Skip the parse — keep the existing max as the
                    // tighter bound; user's max wins because writing
                    // them in this order suggests they wanted that
                    // cap to be authoritative.
                    NSLog(
                        "roost-mac: ignoring tab-min-width=%@ (>tab-max-width=%@)",
                        "\(n)",
                        "\(max)"
                    )
                    continue
                }
                cfg.tabMinWidth = CGFloat(n)
            }
        case "tab-max-width":
            // 0 means "no cap" — pills grow to fit their title
            // (pre-round-4 behavior).
            // Round-5 (CR on #73): reject max < min for the same
            // reason. Same rule: skip the parse so the existing
            // floor stays authoritative.
            if let n = Double(value), n >= 0 {
                if n > 0, let min = cfg.tabMinWidth, CGFloat(n) < min {
                    NSLog(
                        "roost-mac: ignoring tab-max-width=%@ (<tab-min-width=%@)",
                        "\(n)",
                        "\(min)"
                    )
                    continue
                }
                cfg.tabMaxWidth = CGFloat(n)
            }
        case "keybind":
            // `keybind = <trigger> = <action>`. The outer split on
            // the first `=` already gave us `value = "<trigger> =
            // <action>"`; split again on `=` to separate trigger
            // from action. Lenient: drop malformed lines silently
            // (matches the Go binary's tolerance for editor saves
            // mid-edit).
            //
            // Note: `value` was unconditionally quote-stripped
            // above for `theme` / `font-family`; that's safe for
            // keybinds too since Ghostty triggers don't include
            // matching quote characters at the ends.
            if let inner = value.firstIndex(of: "=") {
                let t = value[..<inner].trimmingCharacters(in: .whitespaces)
                let a = value[value.index(after: inner)...]
                    .trimmingCharacters(in: .whitespaces)
                if !t.isEmpty && !a.isEmpty {
                    cfg.keybinds.append(Keybind(trigger: t, action: a))
                }
            }
        case "copy-on-select":
            if let v = CopyOnSelect.parse(value) {
                cfg.copyOnSelect = v
            } else {
                NSLog(
                    "roost-mac: unknown copy-on-select value '%@'; keeping default 'true'",
                    value
                )
            }
        case "clipboard-write":
            if let v = ClipboardWrite.parse(value) {
                cfg.clipboardWrite = v
            } else {
                NSLog(
                    "roost-mac: unknown clipboard-write value '%@'; keeping default 'allow'",
                    value
                )
            }
        case "word-break-chars":
            // Empty value is "no extras" — Unicode letters/digits
            // only. Distinct from "missing" (which falls back to the
            // default). Trimming has already happened; the empty
            // string maps to the Unicode-only set.
            cfg.wordBreakChars = value
        case "command":
            // Launcher entry: `command = label="…" run="…" …`. Parse
            // the RAW value (quotes intact) since the tokenizer in
            // `parseCommandLine` handles quoting; a line missing
            // label/run is skipped, not fatal.
            if let c = parseCommandLine(rawValue) {
                cfg.commands.append(c)
            } else {
                NSLog("roost-mac: skipping malformed `command =` line (needs label + run)")
            }
        default:
            // Many other keys are valid in the Go binary's config
            // (font-style, …); silently drop the ones M6/P1 don't
            // yet consume.
            continue
        }
    }
    return cfg
}
