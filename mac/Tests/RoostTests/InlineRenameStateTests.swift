// Race-guard tests for the M5 inline rename state on
// `ProjectRowCellView`. Linux M9's regression test
// (`crates/roost-linux/src/app.rs::server_driven_marker_drains_on_first_check`)
// is structurally similar: assert that a sibling-driven mutation
// arriving while the user is mid-edit does NOT clobber the typing
// buffer. The Mac equivalent lives on `isEditing` — `configure(...)`
// must short-circuit when it's true.

import AppKit
import Foundation
import Testing
@testable import Roost

@MainActor
@Test
func configure_skipsStringValueWhileEditing() {
    let cell = ProjectRowCellView()
    cell.configure(
        with: ProjectSnapshot(id: 42, name: "Original", cwd: "/tmp"),
        notifying: false
    )
    #expect(cell.textField?.stringValue == "Original")

    // Simulate the user opening the inline edit and typing "user-edit".
    // beginEdit's `makeFirstResponder` call is a silent no-op without
    // a window, which is fine — we're testing the state machine, not
    // AppKit focus.
    cell.beginEdit(initial: "Original", onCommit: { _ in }, onCancel: {})
    cell.textField?.stringValue = "user-edit"
    #expect(cell.isEditing == true)

    // Sibling client renames the project to "fromCli" — model updates,
    // the row's configure is called as part of `rebuildSidebar()`.
    // The typing buffer ("user-edit") must NOT be overwritten.
    cell.configure(
        with: ProjectSnapshot(id: 42, name: "fromCli", cwd: "/tmp"),
        notifying: false
    )
    #expect(cell.textField?.stringValue == "user-edit")
    #expect(cell.isEditing == true)
}

@MainActor
@Test
func configure_resyncsStringValueAfterEdit() {
    let cell = ProjectRowCellView()
    cell.configure(
        with: ProjectSnapshot(id: 1, name: "Alpha", cwd: ""),
        notifying: false
    )
    cell.beginEdit(initial: "Alpha", onCommit: { _ in }, onCancel: {})
    cell.textField?.stringValue = "in-progress"

    // Simulate AppKit calling controlTextDidEndEditing (focus loss /
    // commit) — the cell's own delegate would normally fire `endEdit`.
    // We don't have a real NSWindow here, so trigger the public
    // surface: post the notification the delegate listens for.
    let notif = Notification(
        name: NSControl.textDidEndEditingNotification,
        object: cell.textField
    )
    cell.controlTextDidEndEditing(notif)
    #expect(cell.isEditing == false)

    // Now the model arrives with the post-rename name; configure
    // should pick it up.
    cell.configure(
        with: ProjectSnapshot(id: 1, name: "Beta", cwd: ""),
        notifying: false
    )
    #expect(cell.textField?.stringValue == "Beta")
}

@MainActor
@Test
func configure_reapplyDuringEditDoesNotClobber() {
    let cell = ProjectRowCellView()
    cell.configure(
        with: ProjectSnapshot(id: 7, name: "P7", cwd: ""),
        notifying: false
    )
    cell.beginEdit(initial: "P7", onCommit: { _ in }, onCancel: {})
    cell.textField?.stringValue = "still-typing"

    // A reconfigure (e.g. a notification toggling) arrives via a sidebar
    // rebuild mid-edit; the typing buffer is preserved.
    cell.configure(
        with: ProjectSnapshot(id: 7, name: "P7", cwd: ""),
        notifying: true
    )
    #expect(cell.textField?.stringValue == "still-typing")
}
