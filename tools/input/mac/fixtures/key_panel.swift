// A foreign app for the real-input guard's desktop controls (plan 075 §D2.4),
// built per run with `swiftc` and quit by pid.
//
//   key_panel panel <trigger-file>   a non-activating panel that takes the
//                                    keyboard once <trigger-file> exists
//   key_panel claim                  an ordinary window that claims, through
//                                    Accessibility, to be focused while the
//                                    app is inactive (what a background GPUI
//                                    or winit app does, #604)
//
// Prints `ready <pid>` once it is up (in claim mode, once its window is on
// screen), then in panel mode `key <window number> <is key>` once the panel
// has been made key.
//
// Both modes answer Accessibility on purpose: a stock AppKit app reports no
// focused window for a non-activating panel while it is inactive, and the
// helper's guard can only judge a claim that is made.

import AppKit

final class FixtureApp: NSApplication {
    var claimed: NSWindow?

    override func accessibilityFocusedWindow() -> Any? {
        keyWindow ?? claimed ?? super.accessibilityFocusedWindow()
    }
}

final class ClaimingWindow: NSWindow {
    override func isAccessibilityFocused() -> Bool { true }
}

final class KeyPanel: NSPanel {
    override func isAccessibilityFocused() -> Bool { isKeyWindow }
}

func say(_ line: String) {
    print(line)
    fflush(stdout)
}

/// Reports every edit of the panel's field, so a test can tell a key that
/// reached the panel from one that reached nothing.
final class TextReporter: NSObject, NSTextFieldDelegate {
    func controlTextDidChange(_ notification: Notification) {
        say("text \((notification.object as? NSTextField)?.stringValue ?? "")")
    }
}

let reporter = TextReporter()

// The subclass must be the shared application before anything touches NSApp.
let app = FixtureApp.shared as! FixtureApp
app.setActivationPolicy(.accessory)
let frame = NSRect(x: 120, y: 200, width: 360, height: 160)
let arguments = CommandLine.arguments

switch arguments.dropFirst().first {
case "claim":
    let window = ClaimingWindow(
        contentRect: frame, styleMask: [.titled], backing: .buffered, defer: false)
    window.title = "roost key_panel claim"
    app.claimed = window
    window.orderFrontRegardless()
    // Said from the run loop, once the window server has the window: said
    // before it, the test's bring-Roost-to-front can land first and leave
    // this window ahead of Roost's.
    DispatchQueue.main.async { say("ready \(getpid())") }
case "panel" where arguments.count == 3:
    let trigger = arguments[2]
    let panel = KeyPanel(
        contentRect: frame, styleMask: [.nonactivatingPanel, .titled], backing: .buffered,
        defer: false)
    panel.title = "roost key_panel panel"
    panel.level = .floating
    panel.isFloatingPanel = true
    panel.hidesOnDeactivate = false
    let field = NSTextField(frame: NSRect(x: 20, y: 60, width: 320, height: 24))
    field.delegate = reporter
    panel.contentView?.addSubview(field)
    panel.initialFirstResponder = field
    say("ready \(getpid())")
    Timer.scheduledTimer(withTimeInterval: 0.05, repeats: true) { timer in
        guard FileManager.default.fileExists(atPath: trigger) else { return }
        timer.invalidate()
        panel.makeKeyAndOrderFront(nil)
        panel.makeFirstResponder(field)
        say("key \(panel.windowNumber) \(panel.isKeyWindow)")
    }
default:
    FileHandle.standardError.write("usage: key_panel claim | key_panel panel <trigger-file>\n".data(using: .utf8)!)
    exit(2)
}

app.run()
