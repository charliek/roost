import Testing

@testable import Roost

@Suite struct ChildEnvironmentTests {
    private func env(base: [String: String]) -> [String: String] {
        childEnvironment(
            base: base,
            tabID: 7,
            socketPath: "/tmp/roost-test.sock",
            argv: ["/usr/bin/env"],
            resourcesDir: "/nonexistent-resources",
            version: "test"
        )
    }

    @Test func stripsInheritedTerminfo() {
        // Roost forces TERM=xterm-256color, so an inherited TERMINFO
        // (the launching terminal's private DB, e.g. Ghostty's without an
        // xterm-256color entry) would point strict $TERMINFO readers at a
        // DB lacking the advertised TERM.
        let out = env(base: [
            "TERMINFO": "/Applications/Ghostty.app/Contents/Resources/terminfo",
            "HOME": "/Users/u",
        ])
        #expect(out["TERMINFO"] == nil)
    }

    @Test func forcesTerminalIdentityAndRoostContract() {
        let out = env(base: ["TERM": "xterm-kitty", "HOME": "/Users/u"])
        #expect(out["TERM"] == "xterm-256color")
        #expect(out["COLORTERM"] == "truecolor")
        #expect(out["ROOST_TAB_ID"] == "7")
        #expect(out["ROOST_SOCKET"] == "/tmp/roost-test.sock")
        #expect(out["TERM_PROGRAM"] == "Roost")
    }
}
