// Pure-logic tests for the custom command launcher: the config-line
// parser, the login-shell argv builder, the shell quoter, and the
// launcher palette-row builder. Mirrors the repo's pattern of testing
// extracted state (PaletteStateTests, NotificationInboxTests) without
// standing up AppKit, and the Rust peer in
// `crates/roost-linux/src/custom_command.rs`.

import Testing

@testable import Roost

private func parseCmd(_ value: String) -> CustomCommand {
    guard let c = parseCommandLine(value) else {
        Issue.record("expected a valid command for: \(value)")
        return CustomCommand(label: "", run: "", title: "", env: [], hold: false)
    }
    return c
}

// MARK: - parseCommandLine

@Test
func parsesSimpleLabelAndRun() {
    let c = parseCmd(#"label="Claude" run="claude --resume""#)
    #expect(c.label == "Claude")
    #expect(c.run == "claude --resume")
    #expect(c.title == "Claude")  // title defaults to label
    #expect(c.env.isEmpty)
    #expect(c.hold == false)
}

@Test
func quotedValuesKeepInternalSpaces() {
    let c = parseCmd(#"label="Logs" run="docker compose logs -f""#)
    #expect(c.run == "docker compose logs -f")
}

@Test
func unquotedValuesParse() {
    let c = parseCmd("label=Build run=make")
    #expect(c.label == "Build")
    #expect(c.run == "make")
}

@Test
func explicitTitleOverridesLabel() {
    let c = parseCmd(#"label="Logs" run="lazygit" title="git""#)
    #expect(c.title == "git")
}

@Test
func holdTrueBareAndFalse() {
    #expect(parseCmd(#"label="a" run="b" hold=true"#).hold == true)
    #expect(parseCmd(#"label="a" run="b" hold"#).hold == true)
    #expect(parseCmd(#"label="a" run="b" hold=false"#).hold == false)
    #expect(parseCmd(#"label="a" run="b""#).hold == false)  // absent ⇒ false
}

@Test
func singleEnvPair() {
    let c = parseCmd(#"label="a" run="b" env="RUST_LOG=debug""#)
    #expect(c.env.count == 1)
    #expect(c.env.first?.0 == "RUST_LOG")
    #expect(c.env.first?.1 == "debug")
}

@Test
func multipleEnvPairs() {
    let c = parseCmd(#"label="a" run="b" env="A=1 B=2""#)
    #expect(c.env.count == 2)
    #expect(c.env[0] == ("A", "1"))
    #expect(c.env[1] == ("B", "2"))
}

@Test
func envKeyMustBeIdentifier() {
    // A key that isn't a valid identifier is dropped (it would otherwise
    // splice into `export K=...` verbatim and inject shell).
    let c = parseCmd(#"label="a" run="b" env="GOOD=1 bad-key=2 A;rm=3 OK_2=4""#)
    #expect(c.env.count == 2)
    #expect(c.env[0] == ("GOOD", "1"))
    #expect(c.env[1] == ("OK_2", "4"))
}

@Test
func unknownKeyIsIgnored() {
    let c = parseCmd(#"label="a" run="b" icon="star" quickkey="1""#)
    #expect(c.label == "a")
    #expect(c.run == "b")
}

@Test
func missingLabelIsNil() {
    #expect(parseCommandLine(#"run="claude""#) == nil)
}

@Test
func missingRunIsNil() {
    #expect(parseCommandLine(#"label="Claude""#) == nil)
}

@Test
func emptyRunIsNil() {
    #expect(parseCommandLine(#"label="Claude" run="""#) == nil)
}

// MARK: - launchArgv

@Test
func launchArgvNonHold() {
    let c = parseCmd(#"label="a" run="echo hi""#)
    #expect(launchArgv(shell: "/bin/zsh", command: c) == ["/bin/zsh", "-i", "-c", "echo hi"])
}

@Test
func launchArgvHoldAppendsExecShell() {
    let c = parseCmd(#"label="a" run="make" hold=true"#)
    let argv = launchArgv(shell: "/bin/zsh", command: c)
    #expect(argv[3].hasSuffix("; exec /bin/zsh -i"))
    #expect(argv[3].hasPrefix("make"))
}

@Test
func launchArgvEnvExportsBeforeRun() {
    let c = parseCmd(#"label="a" run="echo $K" env="K=v""#)
    let argv = launchArgv(shell: "/bin/sh", command: c)
    #expect(argv[3] == "export K='v'; echo $K")
}

@Test
func launchArgvEnvAndHoldCombine() {
    let c = parseCmd(#"label="a" run="cmd" env="A=1 B=2" hold=true"#)
    let argv = launchArgv(shell: "/bin/sh", command: c)
    #expect(argv[3] == "export A='1'; export B='2'; cmd; exec /bin/sh -i")
}

@Test
func shellSingleQuoteEscapesEmbeddedQuote() {
    #expect(shellSingleQuote("plain") == "'plain'")
    #expect(shellSingleQuote("a'b") == #"'a'\''b'"#)
}

// MARK: - launcherItems / launchIndex

@Test
func launcherItemsMapCommands() {
    let commands = [
        parseCmd(#"label="Claude" run="claude --resume""#),
        parseCmd(#"label="Build" run="make" hold=true"#),
    ]
    let items = launcherItems(commands)
    #expect(items.count == 2)
    #expect(items[0].id == "launch:0")
    #expect(items[0].title == "Claude")
    #expect(items[0].subtitle == "claude --resume")
    #expect(items[0].trailingText == nil)
    #expect(items[1].id == "launch:1")
    #expect(items[1].trailingText == "hold")
}

@Test
func launcherItemsEmptyShowsSentinel() {
    let items = launcherItems([])
    #expect(items.count == 1)
    #expect(items[0].id == "launch:none")
    #expect(items[0].title == "No commands configured")
    #expect(items[0].subtitle != nil)
}

@Test
func launchIndexParsesAndRejects() {
    #expect(launchIndex("launch:0") == 0)
    #expect(launchIndex("launch:12") == 12)
    #expect(launchIndex("launch:none") == nil)
    #expect(launchIndex("notif:3") == nil)
    #expect(launchIndex("launch:") == nil)
}
