// Pure-logic tests for the command palette: the fuzzy matcher and the
// `PaletteState` navigation/filter/selection machine. Mirrors the
// repo's pattern of testing extracted state (TabPillStateTests,
// InlineRenameStateTests) without standing up AppKit.

import Testing

@testable import Roost

// MARK: - Fuzzy matcher

@Test
func fuzzyMatchesSubsequence() {
    #expect(fuzzyMatch("drk", "Dracula Dark") != nil)
    #expect(fuzzyMatch("ac", "Dracula")?.ranges == [2..<4])  // "ac" → indices 2,3
    #expect(fuzzyMatch("xyz", "Dracula") == nil)
    #expect(fuzzyMatch("draculaa", "Dracula") == nil)  // longer than candidate
}

@Test
func fuzzyEmptyQueryMatchesWithNoRanges() {
    let m = fuzzyMatch("", "anything")
    #expect(m?.score == 0)
    #expect(m?.ranges.isEmpty == true)
}

@Test
func fuzzyExactBeatsPrefixBeatsScattered() {
    let exact = fuzzyMatch("dracula", "Dracula")!.score
    let prefix = fuzzyMatch("dracula", "Dracula+")!.score
    let scattered = fuzzyMatch("dca", "Dracula")!.score  // d..c..a, gappy
    #expect(exact > prefix)
    #expect(prefix > scattered)
}

@Test
func fuzzyRangesCollapseContiguousRuns() {
    // "dl" → d at 0, l at 5 in "dracula": two separate single-char runs.
    let ranges = fuzzyMatch("dl", "dracula")!.ranges
    #expect(ranges == [0..<1, 5..<6])
}

// MARK: - PaletteState filtering / selection

private func sampleRoot() -> PaletteFrame {
    PaletteFrame(
        id: "commands",
        placeholder: "Execute a command…",
        items: [
            PaletteItem(id: "select_theme", title: "Select Theme…"),
            PaletteItem(id: "new_tab", title: "New Tab", trailingText: "⌘T"),
            PaletteItem(id: "toggle_sidebar", title: "Toggle Sidebar", trailingText: "⌘B"),
        ]
    )
}

@Test
func emptyQueryReturnsAllInOrder() {
    let state = PaletteState(root: sampleRoot())
    #expect(state.matches.map(\.item.id) == ["select_theme", "new_tab", "toggle_sidebar"])
    #expect(state.selectedItem?.id == "select_theme")
}

@Test
func queryFiltersAndRanks() {
    var state = PaletteState(root: sampleRoot())
    state.setQuery("tab")
    let ids = state.matches.map(\.item.id)
    #expect(ids.contains("new_tab"))
    #expect(!ids.contains("toggle_sidebar"))  // no t-a-b subsequence
    #expect(state.selectedItem?.id == "new_tab")  // selection reset to top
}

@Test
func emptyResultsMakeSelectionNilSoEnterNoOps() {
    var state = PaletteState(root: sampleRoot())
    state.setQuery("zzzzz")
    #expect(state.matches.isEmpty)
    #expect(state.selectedItem == nil)  // panel treats nil as a no-op on Enter
}

@Test
func moveSelectionClampsWithoutWrapping() {
    var state = PaletteState(root: sampleRoot())  // 3 items
    state.moveSelection(by: -1)
    #expect(state.current.selection == 0)  // clamped at top
    state.moveSelection(by: 5)
    #expect(state.current.selection == 2)  // clamped at bottom
    state.moveSelection(by: -1)
    #expect(state.selectedItem?.id == "new_tab")
}

@Test
func setQueryResetsSelectionToTop() {
    var state = PaletteState(root: sampleRoot())
    state.moveSelection(by: 2)
    #expect(state.current.selection == 2)
    state.setQuery("")
    #expect(state.current.selection == 0)
}

// MARK: - PaletteState stack (drill-in / pop)

private func themeFrame() -> PaletteFrame {
    PaletteFrame(
        id: "themes",
        placeholder: "Select a theme…",
        items: [
            PaletteItem(id: "Dracula", title: "Dracula"),
            PaletteItem(id: "roost-dark", title: "roost-dark"),
        ]
    )
}

@Test
func pushStartsEmptyQueryAndPopRestoresParent() {
    var state = PaletteState(root: sampleRoot())
    state.setQuery("theme")
    #expect(state.isRoot)

    state.push(themeFrame())
    #expect(!state.isRoot)
    #expect(state.current.id == "themes")
    #expect(state.current.query == "")  // sub-list starts fresh
    #expect(state.matches.count == 2)   // not filtered by parent's "theme"

    let popped = state.pop()
    #expect(popped?.id == "themes")
    #expect(state.isRoot)
    #expect(state.current.query == "theme")  // parent query preserved
}

@Test
func popAtRootReturnsNil() {
    var state = PaletteState(root: sampleRoot())
    #expect(state.pop() == nil)
    #expect(state.isRoot)
}

// MARK: - In-place row refresh (updateItems)
//
// The agents frame rebuilds on every workspace event while it's open
// (plan 005 §3.8), so the refresh must not disturb the query or the
// user's highlight. Mirrors the GTK `update_items` cases in
// `crates/roost-linux/src/palette.rs`.

@Test
func updateItemsKeepsTheHighlightedRowByID() {
    var state = PaletteState(root: sampleRoot())
    state.moveSelection(by: 2)
    #expect(state.selectedItem?.id == "toggle_sidebar")
    // Rebuild with the rows re-ranked and one dropped: the same row
    // stays highlighted at its new position.
    let refreshed = state.updateItems(
        frameID: "commands",
        items: [
            PaletteItem(id: "toggle_sidebar", title: "Toggle Sidebar"),
            PaletteItem(id: "new_tab", title: "New Tab"),
        ])
    #expect(refreshed)
    #expect(state.current.selection == 0)
    #expect(state.selectedItem?.id == "toggle_sidebar")
}

@Test
func updateItemsLeavesTheQueryAloneAndClampsAVanishedRow() {
    var state = PaletteState(root: sampleRoot())
    state.setQuery("ta")
    #expect(state.matches.count == 2)
    let survivor = state.matches[0].item
    state.moveSelection(by: 1)
    #expect(state.selectedItem?.id != survivor.id)
    state.updateItems(frameID: "commands", items: [survivor])
    // Query survives the rebuild; the vanished row falls back to the old
    // index, clamped to the new match count.
    #expect(state.current.query == "ta")
    #expect(state.current.selection == 0)
    #expect(state.selectedItem?.id == survivor.id)
}

@Test
func updateItemsReachesAFrameUnderTheTopOne() {
    var state = PaletteState(root: sampleRoot())
    state.push(themeFrame())
    let refreshed = state.updateItems(
        frameID: "commands", items: [PaletteItem(id: "only", title: "Only")])
    #expect(refreshed)
    // The visible frame is untouched…
    #expect(state.matches.count == 2)
    #expect(state.selectedItem?.id == "Dracula")
    // …and the parent carries the new rows once popped back to.
    state.pop()
    #expect(state.selectedItem?.id == "only")
}

@Test
func updateItemsReAnchorsACoveredFramesSelectionByID() {
    var state = PaletteState(root: sampleRoot())
    state.moveSelection(by: 2)
    #expect(state.selectedItem?.id == "toggle_sidebar")
    state.push(themeFrame())
    // Refresh the covered frame with its rows re-ranked and one dropped:
    // the remembered selection must follow the row id, not point at
    // whatever now sits at the old index.
    let refreshed = state.updateItems(
        frameID: "commands",
        items: [
            PaletteItem(id: "toggle_sidebar", title: "Toggle Sidebar"),
            PaletteItem(id: "new_tab", title: "New Tab"),
        ])
    #expect(refreshed)
    state.pop()
    #expect(state.selectedItem?.id == "toggle_sidebar")
    #expect(state.current.selection == 0)
}

@Test
func updateItemsUnknownFrameIsANoOp() {
    var state = PaletteState(root: sampleRoot())
    let refreshed = state.updateItems(frameID: "agents", items: [PaletteItem(id: "x", title: "X")])
    #expect(!refreshed)
    #expect(state.matches.count == 3)
}

@Test
func footerHintsDefaultToNil() {
    #expect(sampleRoot().footerHints == nil)
    let hinted = PaletteFrame(
        id: "agents", placeholder: "Go to agent…", items: [], footerHints: "esc close")
    #expect(hinted.footerHints == "esc close")
}
