// Command palette — the reusable Cmd+Shift+P overlay.
//
// This file holds the PURE, AppKit-free model: the items, the fuzzy
// matcher, and the `PaletteState` navigation/filter/selection machine.
// It's split out from the `NSPanel` (PalettePanel.swift) so the logic
// is unit-tested in isolation, the same way `TabPillState` /
// `InlineRenameState` are. Themes, commands, and any future picker are
// just different `PaletteFrame`s pushed onto the state.

import Foundation

// MARK: - Items

/// One row in the palette. `id` is the stable handle the panel maps
/// back to an action (a command id or a theme file name); `title` is
/// both what's shown and what the fuzzy matcher scores against, so
/// match ranges line up with the displayed text 1:1.
struct PaletteItem: Equatable {
    let id: String
    let title: String
    var subtitle: String?
    /// Right-aligned hint, e.g. a shortcut like "⌘T".
    var trailingText: String?

    init(id: String, title: String, subtitle: String? = nil, trailingText: String? = nil) {
        self.id = id
        self.title = title
        self.subtitle = subtitle
        self.trailingText = trailingText
    }
}

/// An item plus the title character offsets that matched the query, so
/// the panel can bold them. `ranges` is empty for an empty query.
struct PaletteMatch: Equatable {
    let item: PaletteItem
    let ranges: [Range<Int>]
}

// MARK: - Fuzzy matching

/// Case-insensitive subsequence match with light ranking. Returns nil
/// when `query` is not a subsequence of `candidate`. Higher score is a
/// better match; ties are broken by the caller (stable, by input
/// order). Offsets in the returned ranges are into `candidate`'s
/// Character array (callers map to NSRange for display).
///
/// Bonuses favor what feels right in a launcher: exact and prefix
/// matches win outright, consecutive runs and word-boundary hits score
/// higher, gaps cost a little.
func fuzzyMatch(_ query: String, _ candidate: String) -> (score: Int, ranges: [Range<Int>])? {
    let q = Array(query.lowercased())
    let c = Array(candidate.lowercased())
    if q.isEmpty { return (0, []) }
    if q.count > c.count { return nil }

    var matched: [Int] = []
    matched.reserveCapacity(q.count)
    var ci = 0
    for qc in q {
        var found = false
        while ci < c.count {
            if c[ci] == qc {
                matched.append(ci)
                ci += 1
                found = true
                break
            }
            ci += 1
        }
        if !found { return nil }
    }

    var score = 0
    if c == q {
        score += 1000  // exact
    } else if c.starts(with: q) {
        score += 100  // prefix
    }
    var prev = -2
    for idx in matched {
        if idx == prev + 1 {
            score += 10  // consecutive run
        } else if prev >= 0 {
            score -= min(idx - prev - 1, 5)  // small gap penalty
        }
        if idx == 0 || isBoundary(c[idx - 1]) {
            score += 15  // start-of-word
        }
        prev = idx
    }
    // Shorter candidates with the same hits read as tighter matches.
    score -= c.count / 10

    return (score, contiguousRanges(matched))
}

private func isBoundary(_ ch: Character) -> Bool {
    ch == " " || ch == "-" || ch == "_" || ch == "/" || ch == "."
}

/// Collapse sorted matched offsets into contiguous half-open ranges.
private func contiguousRanges(_ offsets: [Int]) -> [Range<Int>] {
    guard let first = offsets.first else { return [] }
    var ranges: [Range<Int>] = []
    var start = first
    var prev = first
    for idx in offsets.dropFirst() {
        if idx == prev + 1 {
            prev = idx
        } else {
            ranges.append(start..<(prev + 1))
            start = idx
            prev = idx
        }
    }
    ranges.append(start..<(prev + 1))
    return ranges
}

// MARK: - Command inventory

/// The first-cut command list, kept separate from RoostApp so its
/// alignment with the keybind namespace is unit-testable. Every spec id
/// is a `KeybindAction` id except `selectThemeID` (a palette-only
/// command that drills into the theme list rather than firing once).
enum PaletteCommands {
    static let selectThemeID = "select_theme"
    /// Palette-only drill-in into the monospace font family list.
    /// Same pattern as `selectThemeID`: not a `KeybindAction`, pushes
    /// a sub-frame with live preview + Esc-to-revert. Mirrors the
    /// Linux `PaletteCommands::SELECT_FONT_ID`.
    static let selectFontID = "select_font"

    /// Palette-only drill-in into the live notification inbox. Like
    /// `selectThemeID`, not a `KeybindAction` — built dynamically in
    /// `paletteCommandItems()` so its title can carry the live count.
    static let viewNotificationsID = "view_notifications"
    /// Palette-only command: empty the inbox + clear all pending dots.
    static let clearNotificationsID = "clear_notifications"

    static let specs: [(id: String, title: String)] = [
        (selectThemeID, "Select Theme…"),
        (selectFontID, "Select Font…"),
        (KeybindAction.newTab, "New Tab"),
        (KeybindAction.closeTab, "Close Tab"),
        (KeybindAction.renameTab, "Rename Tab"),
        (KeybindAction.cycleTabNext, "Next Tab"),
        (KeybindAction.cycleTabPrev, "Previous Tab"),
        (KeybindAction.newProject, "New Project"),
        (KeybindAction.renameProject, "Rename Project"),
        (KeybindAction.closeProject, "Close Project"),
        (KeybindAction.toggleSidebar, "Toggle Sidebar"),
        (KeybindAction.jumpToUnread, "Jump to Unread"),
        (KeybindAction.fontIncrease, "Increase Font Size"),
        (KeybindAction.fontDecrease, "Decrease Font Size"),
        (KeybindAction.fontReset, "Reset Font Size"),
    ]
}

// MARK: - State machine

/// One screen of the palette: a titled list with its own query +
/// selection. Pushing a sub-list (e.g. Select Theme…) starts fresh so
/// the parent's query doesn't carry in and filter everything away;
/// popping restores the parent's preserved query.
struct PaletteFrame {
    let id: String
    let placeholder: String
    let items: [PaletteItem]
    var query: String = ""
    var selection: Int = 0

    init(id: String, placeholder: String, items: [PaletteItem], query: String = "", selection: Int = 0) {
        self.id = id
        self.placeholder = placeholder
        self.items = items
        self.query = query
        self.selection = selection
    }
}

/// Pure navigation/filter/selection over a stack of frames. No AppKit,
/// no callbacks, no side effects — the panel reads `matches` /
/// `selectedItem` and drives transitions; effects (preview, run,
/// revert) live in the panel keyed off frame/item ids.
struct PaletteState {
    private(set) var stack: [PaletteFrame]

    init(root: PaletteFrame) {
        stack = [root]
    }

    var current: PaletteFrame { stack[stack.count - 1] }
    var isRoot: Bool { stack.count == 1 }

    /// Filtered + ranked rows for the current frame's query. Empty
    /// query returns every item in input order (no highlight ranges).
    var matches: [PaletteMatch] {
        let frame = current
        let query = frame.query.trimmingCharacters(in: .whitespaces)
        if query.isEmpty {
            return frame.items.map { PaletteMatch(item: $0, ranges: []) }
        }
        let scored: [(offset: Int, score: Int, match: PaletteMatch)] =
            frame.items.enumerated().compactMap { offset, item in
                guard let (score, ranges) = fuzzyMatch(query, item.title) else { return nil }
                return (offset, score, PaletteMatch(item: item, ranges: ranges))
            }
        // Higher score first; stable by original order on ties.
        return scored
            .sorted { $0.score != $1.score ? $0.score > $1.score : $0.offset < $1.offset }
            .map(\.match)
    }

    /// The highlighted item, or nil when the filter yields nothing.
    var selectedItem: PaletteItem? {
        let m = matches
        guard m.indices.contains(current.selection) else { return nil }
        return m[current.selection].item
    }

    /// Replace the current frame's query; reset selection to the top
    /// match (the best-ranked row).
    mutating func setQuery(_ query: String) {
        stack[stack.count - 1].query = query
        stack[stack.count - 1].selection = 0
    }

    /// Set the highlight to an explicit row (a mouse click), clamped to
    /// the result bounds.
    mutating func setSelection(_ index: Int) {
        let count = matches.count
        guard count > 0 else { return }
        stack[stack.count - 1].selection = min(max(index, 0), count - 1)
    }

    /// Move the highlight, clamped to the result bounds (no wrap).
    mutating func moveSelection(by delta: Int) {
        let count = matches.count
        guard count > 0 else {
            stack[stack.count - 1].selection = 0
            return
        }
        let next = current.selection + delta
        stack[stack.count - 1].selection = min(max(next, 0), count - 1)
    }

    /// Drill into a sub-list (starts with an empty query).
    mutating func push(_ frame: PaletteFrame) {
        stack.append(frame)
    }

    /// Pop back to the parent frame, returning the frame that was
    /// removed (so the panel can fire its cancel/revert exactly once).
    /// Returns nil at the root.
    @discardableResult
    mutating func pop() -> PaletteFrame? {
        guard !isRoot else { return nil }
        return stack.removeLast()
    }
}
