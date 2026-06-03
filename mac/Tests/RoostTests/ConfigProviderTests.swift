// `provider =` config parsing + providers/ directory discovery. Mirrors
// the Rust peer's config tests in `crates/roost-linux/src/config.rs`.

import Foundation
import Testing

@testable import Roost

@Test
func parsesProviderEntriesInOrder() {
    let cfg = parse(
        """
        provider = label="Open shed" run="shed.sh"
        provider = label="Worktrees" run="wt.sh" timeout=8 limit=20
        """)
    #expect(cfg.providers.count == 2)
    #expect(cfg.providers[0].label == "Open shed")
    #expect(cfg.providers[0].run == "shed.sh")
    #expect(cfg.providers[1].timeoutSecs == 8)
    #expect(cfg.providers[1].limit == 20)
}

@Test
func malformedProviderSkipped() {
    let cfg = parse(#"provider = label="NoRun""#)
    #expect(cfg.providers.isEmpty)
}

@Test
func discoversExecutableProvidersFromDir() throws {
    let fm = FileManager.default
    let tmp = fm.temporaryDirectory.appendingPathComponent("roost-prov-\(UUID().uuidString)")
    try fm.createDirectory(at: tmp, withIntermediateDirectories: true)
    defer { try? fm.removeItem(at: tmp) }

    let cfgPath = tmp.appendingPathComponent("config.conf")
    try "theme = roost-dark\nprovider = label=\"Configured\" run=\"c.sh\"\n"
        .write(to: cfgPath, atomically: true, encoding: .utf8)

    let pdir = tmp.appendingPathComponent("providers")
    try fm.createDirectory(at: pdir, withIntermediateDirectories: true)
    let script = pdir.appendingPathComponent("shed.sh")
    try "#!/bin/sh\n# @roost.label: Open shed\necho '{}'\n".write(to: script, atomically: true, encoding: .utf8)
    try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: script.path)
    // A non-executable file is ignored.
    try "ignore me".write(to: pdir.appendingPathComponent("notes.txt"), atomically: true, encoding: .utf8)

    let cfg = RoostConfig.load(from: cfgPath)
    // Config provider first (source order), discovered script after.
    #expect(cfg.providers.count == 2)
    #expect(cfg.providers[0].label == "Configured")
    #expect(cfg.providers[1].label == "Open shed")  // from header metadata
    // Discovered providers keep the raw path + exec directly; config entries are shell-interpreted.
    #expect(cfg.providers[1].run == script.path)
    #expect(!cfg.providers[1].shellInterpret)
    #expect(cfg.providers[0].shellInterpret)
}
