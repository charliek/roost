// Roost keybind system — Phase 6a P1.
//
// Ports `cmd/roost/shortcuts.go` (the Ghostty-style trigger parser +
// the defaults/user-overrides merger) to Swift. Lets users keep
// using the same `keybind = trigger = action` lines in
// `~/.config/roost/config.conf` they were using on the Go binary —
// no migration; same action namespace; same alias rules.
//
// Action namespace (matches the Go binary verbatim):
//   new_tab close_tab rename_tab new_project rename_project
//   cycle_tab_prev cycle_tab_next paste copy
//   font_increase font_decrease font_reset
//   toggle_sidebar
//   switch_project_1..9 switch_tab_1..9
//   unbind  (special — removes the matching default)
//
// The trigger parser accepts Ghostty's alias set:
//   super / cmd / command  → ⌘ Command
//   ctrl  / control        → ⌃ Control
//   alt   / opt / option   → ⌥ Option
//   shift                  → ⇧ Shift
// The key segment is the last `+`-separated token (e.g. "t", "0",
// "plus", "equal", "minus", "bracketleft", "braceright").

import AppKit
import Foundation

// MARK: - Action names

/// Canonical action identifiers. Keep this enum in lockstep with
/// `cmd/roost/shortcuts.go`'s `Action*` constants so a user's
/// existing config keeps working under the Swift binary.
enum KeybindAction {
    static let newTab        = "new_tab"
    static let closeTab      = "close_tab"
    static let renameTab     = "rename_tab"
    static let cycleTabPrev  = "cycle_tab_prev"
    static let cycleTabNext  = "cycle_tab_next"
    static let paste         = "paste"
    static let copy          = "copy"
    static let newProject    = "new_project"
    static let renameProject = "rename_project"
    /// Round-4 R3: ⌘⇧W (Mac) / Alt+Shift+W (Linux) close the active
    /// project; confirms with an NSAlert when the project has 2+ tabs.
    static let closeProject  = "close_project"
    static let fontIncrease  = "font_increase"
    static let fontDecrease  = "font_decrease"
    static let fontReset     = "font_reset"
    static let toggleSidebar = "toggle_sidebar"
    /// Phase 6a P7: jump to the next unread (notified) tab. New
    /// action; not present on the Go binary (cmux-inspired
    /// ⌘⇧U convention).
    static let jumpToUnread  = "jump_to_unread"
    /// Cmd+Shift+P command palette. Mac-only for now — there's no
    /// Go/Linux counterpart yet, so this breaks the otherwise-lockstep
    /// namespace deliberately; add the peer action when Linux grows a
    /// palette.
    static let commandPalette = "command_palette"
    static let unbind        = "unbind"

    /// `switch_project_N` (1..9). Defined as a function rather than
    /// a constant per index because the digit varies.
    static func switchProject(_ n: Int) -> String { "switch_project_\(n)" }
    static func switchTab(_ n: Int) -> String { "switch_tab_\(n)" }

    /// All recognized non-numeric action names. The Go binary calls
    /// this `knownActions`; used by `canonicalizeBindings` to warn
    /// on typos in user config (so `keybind = cmd+t = nwe_tab`
    /// preserves the default rather than silently dropping it).
    static let knownStaticActions: Set<String> = [
        newTab, closeTab, renameTab, cycleTabPrev, cycleTabNext,
        paste, copy, newProject, renameProject, closeProject,
        fontIncrease, fontDecrease, fontReset, toggleSidebar,
        jumpToUnread, commandPalette,
    ]

    /// True if `action` is a recognized name (including the
    /// generated switch_project_N / switch_tab_N forms).
    static func isKnown(_ action: String) -> Bool {
        if action == unbind { return true }
        if knownStaticActions.contains(action) { return true }
        for n in 1...9 {
            if action == switchProject(n) || action == switchTab(n) {
                return true
            }
        }
        return false
    }
}

// MARK: - Keybind value

/// One user keybind line (`keybind = trigger = action`). Pure data;
/// the parser in `Config.swift` produces these and the merger in
/// `canonicalizeBindings` consumes them.
struct Keybind: Equatable {
    var trigger: String
    var action: String
}

// MARK: - Trigger parser

/// Canonicalized key equivalent ready for `NSMenuItem`. Manual
/// `Hashable` because `NSEvent.ModifierFlags` is an `OptionSet`
/// (UInt rawValue) which isn't automatically Hashable in the
/// synthesized sense — hash through the rawValue.
struct Accel: Hashable {
    /// Key string for `NSMenuItem.keyEquivalent`. Lowercased single
    /// character for letters; verbatim for digits; special tokens
    /// for `+` (uses `=`), `-`, `[`, `]`, `{`, `}`.
    var key: String
    /// Modifier mask for `NSMenuItem.keyEquivalentModifierMask`.
    var modifiers: NSEvent.ModifierFlags

    static func == (lhs: Accel, rhs: Accel) -> Bool {
        lhs.key == rhs.key && lhs.modifiers.rawValue == rhs.modifiers.rawValue
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(key)
        hasher.combine(modifiers.rawValue)
    }
}

/// Convert a Ghostty-style trigger string ("super+shift+t",
/// "ctrl+1") to a Swift `Accel`. Returns nil on:
///   * empty input,
///   * empty key segment,
///   * unknown modifier alias,
///   * unknown special key name.
/// Caller should `NSLog` and skip — mirrors the Go binary's
/// `triggerToAccel` returning ok=false.
func triggerToAccel(_ trigger: String) -> Accel? {
    let raw = trigger.trimmingCharacters(in: .whitespaces)
    if raw.isEmpty { return nil }
    let parts = raw.split(separator: "+", omittingEmptySubsequences: false).map {
        String($0).trimmingCharacters(in: .whitespaces)
    }
    if parts.isEmpty { return nil }
    let keyToken = parts.last ?? ""
    if keyToken.isEmpty { return nil }

    var mask: NSEvent.ModifierFlags = []
    for m in parts.dropLast() {
        switch m.lowercased() {
        case "shift":
            mask.insert(.shift)
        case "ctrl", "control":
            mask.insert(.control)
        case "alt", "opt", "option":
            mask.insert(.option)
        case "super", "cmd", "command":
            mask.insert(.command)
        default:
            return nil
        }
    }

    guard let key = keyEquivalentForToken(keyToken) else { return nil }
    return Accel(key: key, modifiers: mask)
}

/// Map a Ghostty key-segment token to the NSMenuItem `keyEquivalent`
/// string. Single characters pass through lowercased; named special
/// keys map to the macOS conventions. Returns nil for an unknown
/// special name.
private func keyEquivalentForToken(_ token: String) -> String? {
    // Single character (letter, digit, punctuation).
    if token.count == 1 {
        return token.lowercased()
    }
    switch token.lowercased() {
    case "plus":         return "+"
    case "equal":        return "="
    case "minus":        return "-"
    case "bracketleft":  return "["
    case "bracketright": return "]"
    case "braceleft":    return "{"
    case "braceright":   return "}"
    case "comma":        return ","
    case "period":       return "."
    case "slash":        return "/"
    case "backslash":    return "\\"
    case "semicolon":    return ";"
    case "apostrophe":   return "'"
    case "grave":        return "`"
    case "space":        return " "
    case "return", "enter": return "\r"
    case "tab":          return "\t"
    case "escape":       return "\u{1b}"
    case "backspace":    return "\u{7f}"
    default:             return nil
    }
}

// MARK: - Default bindings

/// Default action → [trigger] table for macOS. Mirrors the Go
/// binary's `defaultBindings()` with `runtime.GOOS == "darwin"`
/// branch — primary/projectMod/clipboardMod all = "super" (⌘).
func defaultBindingsMac() -> [String: [String]] {
    let primary = "super"
    let projectMod = "super"
    let clipboardMod = "super"

    var m: [String: [String]] = [
        KeybindAction.newTab:    ["\(primary)+t"],
        KeybindAction.closeTab:  ["\(primary)+w"],
        KeybindAction.renameTab: ["\(projectMod)+r"],
        // Shift-[ produces braceleft on US layouts; bracketleft on
        // layouts that don't transform. Keep both. (Go binary
        // semantics; preserved verbatim.)
        KeybindAction.cycleTabPrev: [
            "\(primary)+shift+braceleft",
            "\(primary)+shift+bracketleft",
        ],
        KeybindAction.cycleTabNext: [
            "\(primary)+shift+braceright",
            "\(primary)+shift+bracketright",
        ],
        // Round-2 F3 fix: Mac uses ⌘C / ⌘V exclusively for the
        // system clipboard. The pre-fix `ctrl+shift+v` / `ctrl+shift+c`
        // entries (Linux conventions inherited verbatim) caused the
        // Edit menu's keyEquivalent to land on the wrong accel
        // depending on dictionary iteration order, breaking ⌘V/⌘C
        // for the user. Mac users don't expect those triggers; drop
        // them. Users who want them can still bind via config.
        KeybindAction.paste:         ["\(clipboardMod)+v"],
        KeybindAction.copy:          ["\(clipboardMod)+c"],
        KeybindAction.newProject:    ["\(projectMod)+n"],
        KeybindAction.renameProject: ["\(projectMod)+shift+r"],
        KeybindAction.closeProject:  ["\(projectMod)+shift+w"],
        KeybindAction.toggleSidebar: ["\(projectMod)+b"],
        // ⌘⇧U — cmux's "jump to latest unread" convention.
        KeybindAction.jumpToUnread:  ["\(primary)+shift+u"],
        // ⌘⇧P — VS Code / Zed command-palette convention.
        KeybindAction.commandPalette: ["\(primary)+shift+p"],
        // Browser-style font sizing. + and = both bind because
        // cmd-+ on US layouts is really cmd-shift-=, and many
        // users hit cmd-= without the shift.
        KeybindAction.fontIncrease: ["\(primary)+plus", "\(primary)+equal"],
        KeybindAction.fontDecrease: ["\(primary)+minus"],
        KeybindAction.fontReset:    ["\(primary)+0"],
    ]
    for i in 1...9 {
        m[KeybindAction.switchProject(i)] = ["\(projectMod)+\(i)"]
        m[KeybindAction.switchTab(i)] = ["ctrl+\(i)"]
    }
    return m
}

// MARK: - Canonicalization (defaults + user overrides → accel table)

/// Merge default + user keybinds into a final `Accel → action`
/// table. Pure data; no NSMenu calls. Mirrors the Go binary's
/// `canonicalizeBindings`:
///
///  1. Alias collapse: `cmd+t` / `super+t` / `command+t` all map
///     to the same Accel, so `keybind = cmd+t = unbind` correctly
///     removes a default seeded as `super+t`.
///  2. Action validation: user bindings whose action isn't in
///     `KeybindAction.isKnown` get warned + skipped — a typo can't
///     erase the default.
///  3. Trigger validation: unparseable user triggers warn + skip;
///     unparseable default triggers warn but still skip so the
///     rest of the table installs.
///
/// `warn` is a callback so tests can capture without a logger
/// dependency. Production passes an `NSLog` shim.
func canonicalizeBindings(
    defaults: [String: [String]],
    user: [Keybind],
    warn: (_ message: String, _ trigger: String, _ action: String) -> Void
) -> [Accel: String] {
    var table: [Accel: String] = [:]
    for (action, triggers) in defaults {
        for t in triggers {
            guard let accel = triggerToAccel(t) else {
                warn("unparseable default trigger", t, action)
                continue
            }
            table[accel] = action
        }
    }
    for kb in user {
        guard let accel = triggerToAccel(kb.trigger) else {
            warn("unparseable trigger (default kept)", kb.trigger, kb.action)
            continue
        }
        if kb.action == KeybindAction.unbind {
            table.removeValue(forKey: accel)
            continue
        }
        if !KeybindAction.isKnown(kb.action) {
            warn("unknown action (default kept)", kb.trigger, kb.action)
            continue
        }
        table[accel] = kb.action
    }
    return table
}
