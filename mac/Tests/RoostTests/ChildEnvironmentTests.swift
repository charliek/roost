import Testing

@testable import Roost

@Suite struct ChildEnvironmentTests {
    private func env(base: [String: String], agentHook: String? = "/opt/roost/roostctl")
        -> [String: String]
    {
        childEnvironment(
            base: base,
            tabID: 7,
            socketPath: "/tmp/roost-test.sock",
            argv: ["/usr/bin/env"],
            resourcesDir: "/nonexistent-resources",
            version: "test",
            agentHook: agentHook
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

    /// `ROOST_LEASE` was stripped while a session had a driver lease to
    /// leak. Nothing mints one at protocol 5, so the strip is gone and
    /// the variable is just another one Roost passes through — which is
    /// what this pins, so the removal cannot quietly come back.
    @Test func passesThroughRoostLeaseNowThatNothingMintsOne() {
        let out = env(base: ["ROOST_LEASE": "9f2c1d7a4b6e08315c0d9a72e4f16b83", "HOME": "/Users/u"])
        #expect(out["ROOST_LEASE"] == "9f2c1d7a4b6e08315c0d9a72e4f16b83")
    }

    @Test func forcesTerminalIdentityAndRoostContract() {
        let out = env(base: ["TERM": "xterm-kitty", "HOME": "/Users/u"])
        #expect(out["TERM"] == "xterm-256color")
        #expect(out["COLORTERM"] == "truecolor")
        #expect(out["ROOST_TAB_ID"] == "7")
        #expect(out["ROOST_SOCKET"] == "/tmp/roost-test.sock")
        #expect(out["TERM_PROGRAM"] == "Roost")
    }

    @Test func carriesTheAgentHookEntrypoint() {
        let out = env(base: ["HOME": "/Users/u"])
        #expect(out["ROOST_AGENT_HOOK"] == "/opt/roost/roostctl")
    }

    /// A `swift run` build has no embedded CLI. The variable is then
    /// **omitted**, and an inherited one is removed rather than passed
    /// through: it must always describe *this* Roost, never an outer one
    /// this build happens to be nested inside.
    @Test func omitsTheAgentHookWhenNoCliIsBundled() {
        let out = env(
            base: ["HOME": "/Users/u", "ROOST_AGENT_HOOK": "/stale/outer/roostctl"],
            agentHook: nil)
        #expect(out["ROOST_AGENT_HOOK"] == nil)
    }
}
