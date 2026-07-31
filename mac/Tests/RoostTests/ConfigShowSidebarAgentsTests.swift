// Config parser tests for the `show-sidebar-agents` setting (plan
// 007 §3.7). Mirrors `crates/roost-linux/src/config.rs::tests`
// (`show_sidebar_agents_*`) 1:1 so the two UIs agree on parsing
// semantics.

import Testing

@testable import Roost

@Suite("RoostConfig show-sidebar-agents parsing")
struct ConfigShowSidebarAgentsTests {
    @Test func defaultsToTrue() {
        let cfg = parse("")
        #expect(cfg.showSidebarAgents == true)
    }

    @Test func parsesTrueAndFalse() {
        #expect(parse("show-sidebar-agents = true").showSidebarAgents == true)
        #expect(parse("show-sidebar-agents = yes").showSidebarAgents == true)
        #expect(parse("show-sidebar-agents = false").showSidebarAgents == false)
        #expect(parse("show-sidebar-agents = no").showSidebarAgents == false)
    }

    // Quoted and CRLF forms must agree with the Rust mirror, whose
    // shared parse loop strips quotes only for `font-family`.
    @Test func parsesQuotedAndCRLFValues() {
        #expect(parse("show-sidebar-agents = \"false\"").showSidebarAgents == false)
        #expect(parse("show-sidebar-agents = 'false'").showSidebarAgents == false)
        #expect(parse("show-sidebar-agents = false\r\n").showSidebarAgents == false)
        #expect(parse("show-sidebar-agents = \"false\"\r\n").showSidebarAgents == false)
    }

    @Test func unknownValueKeepsDefault() {
        let cfg = parse("show-sidebar-agents = pancakes")
        #expect(cfg.showSidebarAgents == true)
    }
}
