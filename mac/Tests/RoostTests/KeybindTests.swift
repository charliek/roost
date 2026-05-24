// Keybind defaults + canonicalization, focused on the command palette
// action added for Cmd+Shift+P. No keybind test existed before this.

import Testing

@testable import Roost

private func noWarn(_: String, _: String, _: String) {}

@Test
func commandPaletteBoundToSuperShiftPByDefault() throws {
    let table = canonicalizeBindings(defaults: defaultBindingsMac(), user: [], warn: noWarn)
    let accel = try #require(triggerToAccel("super+shift+p"))
    #expect(table[accel] == KeybindAction.commandPalette)
}

@Test
func userCanUnbindCommandPalette() throws {
    let table = canonicalizeBindings(
        defaults: defaultBindingsMac(),
        user: [Keybind(trigger: "super+shift+p", action: KeybindAction.unbind)],
        warn: noWarn
    )
    let accel = try #require(triggerToAccel("super+shift+p"))
    #expect(table[accel] == nil)
}

@Test
func unknownActionTypoKeepsTheDefault() throws {
    // A user typo must not erase the default it collides with.
    let table = canonicalizeBindings(
        defaults: defaultBindingsMac(),
        user: [Keybind(trigger: "super+t", action: "nwe_tab")],
        warn: noWarn
    )
    let accel = try #require(triggerToAccel("super+t"))
    #expect(table[accel] == KeybindAction.newTab)
}

@Test
func paletteCommandsStayInKeybindNamespace() {
    // Every palette command id (except the theme-drill sentinel) must
    // be a real keybind action, so `runCommand`'s switch + the shortcut
    // hint can't silently drift from the namespace.
    for spec in PaletteCommands.specs where spec.id != PaletteCommands.selectThemeID {
        #expect(KeybindAction.isKnown(spec.id), "\(spec.id) is not a known keybind action")
        #expect(!spec.title.isEmpty)
    }
    #expect(!KeybindAction.isKnown(PaletteCommands.selectThemeID))
}
