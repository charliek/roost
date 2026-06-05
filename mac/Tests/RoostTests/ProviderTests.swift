// Pure-logic tests for dynamic command providers: the `provider =`
// parser, directory-entry builder, subprocess invocation (argv/env/
// stdin), and stdout parsing. Mirrors the Rust peer in
// `crates/roost-linux/src/provider.rs` (same cases, same expectations).

import Foundation
import Testing

@testable import Roost

private func parseProv(_ value: String) -> Provider {
    guard let p = parseProviderLine(value) else {
        Issue.record("expected a valid provider for: \(value)")
        return Provider(label: "", run: "", title: "", timeoutSecs: 0, limit: 0, shellInterpret: true)
    }
    return p
}

// MARK: - parseProviderLine

@Test
func parsesLabelAndRunWithDefaults() {
    let p = parseProv(#"label="Open shed" run="~/.config/roost/providers/shed.sh""#)
    #expect(p.label == "Open shed")
    #expect(p.run == "~/.config/roost/providers/shed.sh")
    #expect(p.title == "Open shed")  // defaults to label
    #expect(p.timeoutSecs == providerDefaultTimeoutSecs)
    #expect(p.limit == providerDefaultLimit)
}

@Test
func explicitTitleTimeoutLimit() {
    let p = parseProv(#"label="Shed" run="shed.sh" title="Pick service" timeout=8 limit=25"#)
    #expect(p.title == "Pick service")
    #expect(p.timeoutSecs == 8)
    #expect(p.limit == 25)
}

@Test
func timeoutAndLimitClampAndFallBack() {
    let big = parseProv(#"label="a" run="b" timeout=9999 limit=999999"#)
    #expect(big.timeoutSecs == 60)
    #expect(big.limit == 1000)
    let bad = parseProv(#"label="a" run="b" timeout=soon limit=many"#)
    #expect(bad.timeoutSecs == providerDefaultTimeoutSecs)
    #expect(bad.limit == providerDefaultLimit)
}

@Test
func missingLabelOrRunIsNil() {
    #expect(parseProviderLine(#"run="shed.sh""#) == nil)
    #expect(parseProviderLine(#"label="Shed""#) == nil)
    #expect(parseProviderLine(#"label="Shed" run="""#) == nil)
}

@Test
func unknownKeysIgnored() {
    let p = parseProv(#"label="a" run="b" icon="star" mode="x""#)
    #expect(p.label == "a")
    #expect(p.run == "b")
}

// MARK: - providerFromFile (directory discovery)

@Test
func providerFromFileHumanizesFilename() {
    let p = providerFromFile(path: "/x/shed-open_logs.sh", filename: "shed-open_logs.sh", header: "")
    #expect(p.run == "/x/shed-open_logs.sh")  // raw path, exec'd directly
    #expect(!p.shellInterpret)
    #expect(p.label == "shed open logs")
    #expect(p.title == "shed open logs")
}

@Test
func providerFromFileKeepsRawPathWithSpaces() {
    let p = providerFromFile(path: "/x/my shed.sh", filename: "my shed.sh", header: "")
    #expect(p.run == "/x/my shed.sh")  // raw path (no quoting); direct exec is space-safe
    #expect(!p.shellInterpret)
}

@Test
func providerFromFileReadsHeaderMetadata() {
    let header = "#!/usr/bin/env bash\n# @roost.label: Open shed\n# @roost.title: Pick a service\nset -e\n"
    let p = providerFromFile(path: "/x/shed.sh", filename: "shed.sh", header: header)
    #expect(p.label == "Open shed")
    #expect(p.title == "Pick a service")
}

@Test
func headerScanStopsAtFirstRealLine() {
    let header = "echo hi\n# @roost.label: Nope"
    let p = providerFromFile(path: "/x/foo.sh", filename: "foo.sh", header: header)
    #expect(p.label == "foo")  // humanized filename, not "Nope"
}

// MARK: - invocation (argv / env / stdin)

@Test
func invocationArgvShellVsDirect() {
    // Config provider (shellInterpret = true): wrapped in `sh -c`.
    #expect(providerInvocationArgv(shell: "/bin/zsh", run: "shed.sh", shellInterpret: true, phase: .list) == ["/bin/zsh", "-c", "shed.sh list"])
    #expect(providerInvocationArgv(shell: "/bin/sh", run: "python3 shed.py", shellInterpret: true, phase: .activate)[2] == "python3 shed.py activate")
    // Discovered provider (shellInterpret = false): exec'd directly — a
    // path with spaces is one argv element.
    #expect(providerInvocationArgv(shell: "/bin/sh", run: "/x/my shed.sh", shellInterpret: false, phase: .list) == ["/x/my shed.sh", "list"])
}

@Test
func invocationEnvCarriesContext() {
    var ctx = ProviderContext()
    ctx.socket = "/tmp/roost.sock"
    ctx.query = "ap"
    ctx.selectedID = "api"
    ctx.activeTabID = 7
    ctx.activeProjectID = 3
    ctx.activeCwd = "/repo"
    ctx.roostctl = "/usr/bin/roostctl"
    let env = providerInvocationEnv(phase: .activate, ctx: ctx)
    func get(_ k: String) -> String? { env.first { $0.0 == k }?.1 }
    #expect(get("ROOST_PROVIDER_PHASE") == "activate")
    #expect(get("ROOST_SOCKET") == "/tmp/roost.sock")
    #expect(get("ROOST_SELECTED_ID") == "api")
    #expect(get("ROOST_ACTIVE_TAB_ID") == "7")
    #expect(get("ROOST_ACTIVE_PROJECT_ID") == "3")
    #expect(get("ROOST_ACTIVE_CWD") == "/repo")
    #expect(get("ROOST_ROOSTCTL") == "/usr/bin/roostctl")
}

@Test
func invocationEnvOmitsAbsentOptionals() {
    var ctx = ProviderContext()
    ctx.socket = "/s"
    let env = providerInvocationEnv(phase: .list, ctx: ctx)
    #expect(!env.contains { $0.0 == "ROOST_SELECTED_ID" })
    #expect(!env.contains { $0.0 == "ROOST_ACTIVE_TAB_ID" })
    #expect(!env.contains { $0.0 == "ROOST_ROOSTCTL" })  // nil ⇒ omitted
}

@Test
func invocationStdinIsValidJSONWithContext() throws {
    var ctx = ProviderContext()
    ctx.socket = "/s"
    ctx.query = "q"
    ctx.selectedID = "api"
    ctx.activeTabID = 7
    ctx.activeProjectID = 3
    ctx.activeCwd = "/repo"
    let json = providerInvocationStdin(phase: .activate, ctx: ctx)
    let obj = try JSONSerialization.jsonObject(with: Data(json.utf8)) as! [String: Any]
    #expect(obj["v"] as? Int == 1)
    #expect(obj["phase"] as? String == "activate")
    #expect(obj["selected_id"] as? String == "api")
    let tab = obj["active_tab"] as! [String: Any]
    #expect(tab["id"] as? Int == 7)
    #expect(tab["project_id"] as? Int == 3)
    #expect(tab["cwd"] as? String == "/repo")
    #expect(obj["socket"] as? String == "/s")
}

@Test
func invocationStdinListOmitsSelectedID() {
    let json = providerInvocationStdin(phase: .list, ctx: ProviderContext())
    #expect(!json.contains("selected_id"))
}

// MARK: - parseProviderOutput

@Test
func parseOutputObjectForm() throws {
    let out = try parseProviderOutput(#"{"placeholder":"pick","items":[{"id":"web","title":"Web"}]}"#)
    #expect(out.placeholder == "pick")
    #expect(out.items.count == 1)
    #expect(out.items[0].id == "web")
    #expect(out.items[0].subtitle == nil)
}

@Test
func parseOutputBareArrayForm() throws {
    let out = try parseProviderOutput(#"[{"id":"web","title":"Web","subtitle":"../web"},{"id":"api","title":"Api"}]"#)
    #expect(out.items.count == 2)
    #expect(out.items[1].id == "api")
    #expect(out.items[0].subtitle == "../web")
}

@Test
func parseOutputEmptyIsEmpty() throws {
    #expect(try parseProviderOutput("") == ProviderOutput())
    #expect(try parseProviderOutput("   \n ") == ProviderOutput())
}

@Test
func parseOutputMalformedThrows() {
    #expect(throws: (any Error).self) { try parseProviderOutput("not json") }
    #expect(throws: (any Error).self) { try parseProviderOutput(#"{"items":[{"title":"no id"}]}"#) }
}

// MARK: - palette row builders

@Test
func providerItemsMapAndIndexRoundTrip() {
    let providers = [
        parseProv(#"label="Open shed" run="shed.sh""#),
        parseProv(#"label="Worktrees" run="wt.sh""#),
    ]
    let items = providerItems(providers)
    #expect(items.count == 2)
    #expect(items[0].id == "provider:0")
    #expect(items[0].title == "Open shed")
    #expect(items[0].subtitle == "shed.sh")
    #expect(providerIndex(items[0].id) == 0)
    #expect(providerIndex(items[1].id) == 1)
}

@Test
func providerItemsEmptyShowsSentinel() {
    let items = providerItems([])
    #expect(items.count == 1)
    #expect(items[0].id == "provider:none")
    #expect(providerIndex(items[0].id) == nil)
    #expect(!items[0].actionable)  // the sentinel is non-selectable
}

@Test
func outputPaletteItemsCapsAndHints() {
    let out = ProviderOutput(
        placeholder: "",
        items: (0..<5).map {
            ProviderOutputItem(id: "i\($0)", title: "Item \($0)", subtitle: nil, actionable: nil)
        })
    let rows = providerOutputPaletteItems(out, limit: 3)
    #expect(rows.count == 4)  // 3 real + 1 overflow hint
    #expect(rows[2].id == "i2")
    #expect(rows[2].actionable)  // absent ⇒ actionable
    #expect(rows[3].id == providerOverflowID)
    #expect(rows[3].title == "… 2 more")
    #expect(!rows[3].actionable)  // overflow hint is non-actionable
}

@Test
func outputPaletteItemsUnderLimitHasNoHint() {
    let out = ProviderOutput(
        placeholder: "",
        items: [ProviderOutputItem(id: "a", title: "A", subtitle: "sub", actionable: nil)])
    let rows = providerOutputPaletteItems(out, limit: 100)
    #expect(rows.count == 1)
    #expect(rows[0].subtitle == "sub")
    #expect(!rows.contains { $0.id == providerOverflowID })
}

@Test
func actionableParsesAndCarriesThrough() {
    let out = try! parseProviderOutput(
        #"{"items":[{"id":"x","title":"X","actionable":false},{"id":"y","title":"Y"}]}"#)
    #expect(out.items[0].actionable == false)
    #expect(out.items[1].actionable == nil)
    let rows = providerOutputPaletteItems(out, limit: 100)
    #expect(!rows[0].actionable)
    #expect(rows[1].actionable)
}

@Test
func paletteItemActionableDefaultsTrue() {
    #expect(PaletteItem(id: "a", title: "A").actionable)
    #expect(!PaletteItem(id: "a", title: "A", actionable: false).actionable)
}
