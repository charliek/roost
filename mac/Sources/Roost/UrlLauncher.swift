// `UrlLauncher` is the protocol seam that lets `TerminalView`'s
// Cmd-click handler open a URL via either `NSWorkspace.shared.open`
// in production or a test stub. AppKit's `NSWorkspace` is a singleton
// without an easy mocking surface; the seam is a one-method protocol
// the click handler holds a reference to.
//
// Defaulting `TerminalView.urlLauncher` to `WorkspaceUrlLauncher()`
// keeps production code call-site identical to a direct
// `NSWorkspace.shared.open`. The test stub lives next to the test
// cases in `mac/Tests/RoostTests/TerminalViewClickableLinksTests.swift`.

import AppKit
import Foundation

protocol UrlLauncher: AnyObject {
    /// Open `url` in whichever app the system associates with its
    /// scheme. Returns `true` if the launcher accepted the URL — the
    /// terminal view doesn't act on the return value, but recording
    /// stubs use it to assert "open was called once".
    @discardableResult
    func open(_ url: URL) -> Bool
}

/// Production launcher. Hands the URL to `NSWorkspace.shared.open`.
final class WorkspaceUrlLauncher: UrlLauncher {
    func open(_ url: URL) -> Bool {
        NSWorkspace.shared.open(url)
    }
}
