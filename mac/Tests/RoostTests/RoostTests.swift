// Smoke tests for the Mac executable. Kept tight on purpose — the real
// behavior coverage will live in Rust integration tests against
// `roost-core`. These exist mainly so `swift test` runs on macOS CI and
// catches gross packaging regressions, plus pin a few invariants that
// would silently break the daemon-discovery story if they regressed.

import Foundation
import Testing
@testable import Roost

@Test
func defaultSocketPathUsesHomeOnMac() {
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/Roost/roost.sock")
}

@Test
func defaultSocketPathIgnoresXdgRuntimeDirOnMac() {
    // The daemon's macOS branch is HOME-derived only, so the Mac
    // client must not chase XDG_RUNTIME_DIR even if a shell exports
    // it. Both sides agreeing matters more than mirroring Linux.
    let socket = RoostApp.defaultSocketPath(environment: [
        "XDG_RUNTIME_DIR": "/run/user/501",
        "HOME": "/Users/tester",
    ])
    #expect(socket == "/Users/tester/Library/Caches/Roost/roost.sock")
}

@Test
func defaultSocketPathFallsBackToTmpWhenHomeMissing() {
    let socket = RoostApp.defaultSocketPath(environment: [:])
    #expect(socket == "/tmp/Roost/roost.sock")
}

@Test
func defaultSocketPathSkipsEmptyHome() {
    // Sandboxed launchd processes can inherit HOME="" (set but empty).
    // The function must fall through to /tmp, not yield
    // "/Library/Caches/Roost/roost.sock".
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "",
    ])
    #expect(socket == "/tmp/Roost/roost.sock")
}

@Test
func defaultSocketPathSkipsRelativeHome() {
    // A relative HOME would yield an unusable socket path; fall
    // through to /tmp instead.
    let socket = RoostApp.defaultSocketPath(environment: [
        "HOME": "relative/path",
    ])
    #expect(socket == "/tmp/Roost/roost.sock")
}

@Test
func defaultSocketPathInvariants() {
    let socket = RoostApp.defaultSocketPath()
    #expect(!socket.isEmpty)
    #expect(socket.hasPrefix("/"))
    // Use case-insensitive match — capital `Roost` (M1) and any future
    // lowercase recurrence both pass; the substring check exists only
    // to catch the path going *somewhere else entirely*.
    #expect(socket.lowercased().contains("roost"))
}

// MARK: - BundleProfile parity

@Test
func bundleProfileMacUsesCapitalRoost() {
    let p = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    #expect(p.appID == "ai.stridelabs.Roost")
    #expect(p.appLabel == "Roost")
    #expect(p.socketPath == "/Users/tester/Library/Caches/Roost/roost.sock")
    #expect(p.stateDir == "/Users/tester/Library/Application Support/Roost")
    #expect(p.logDir == "/Users/tester/Library/Logs/Roost")
}

@Test
func bundleProfileGtkIsDistinctFromMac() {
    let mac = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    let gtk = BundleProfile.gtk(environment: ["HOME": "/Users/tester"])
    #expect(gtk.appID == "ai.stridelabs.Roost.gtk")
    #expect(gtk.appLabel == "Roost-gtk")
    #expect(mac.socketPath != gtk.socketPath)
    #expect(mac.stateDir != gtk.stateDir)
    #expect(mac.logDir != gtk.logDir)
}

@Test
func bundleProfileEnvOverridesDefault() {
    let p = BundleProfile.currentForBinary(
        default: .mac,
        environment: [
            "HOME": "/Users/tester",
            "ROOST_BUNDLE_PROFILE": "gtk",
        ]
    )
    #expect(p.kind == .gtk)
    #expect(p.appID == "ai.stridelabs.Roost.gtk")
}

// MARK: - ROOST_STATE_DIR override (lockstep with paths.rs apply_state_dir_override)

@Test
func stateDirOverrideAbsoluteMovesOnlyStateDir() {
    let base = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    let p = BundleProfile.mac(environment: [
        "HOME": "/Users/tester",
        "ROOST_STATE_DIR": "/tmp/roost-isolated-state",
    ])
    #expect(p.stateDir == "/tmp/roost-isolated-state")
    #expect(p.stateJSONPath == "/tmp/roost-isolated-state/state.json")
    // Invariant: socket, lock, log stay on the default profile path.
    #expect(p.socketPath == base.socketPath)
    #expect(p.lockPath == base.lockPath)
    #expect(p.logPath == base.logPath)
}

@Test
func stateDirOverrideUnsetKeepsDefault() {
    let p = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    #expect(p.stateDir == "/Users/tester/Library/Application Support/Roost")
}

@Test
func stateDirOverrideEmptyKeepsDefault() {
    let p = BundleProfile.mac(environment: [
        "HOME": "/Users/tester",
        "ROOST_STATE_DIR": "",
    ])
    #expect(p.stateDir == "/Users/tester/Library/Application Support/Roost")
}

@Test
func stateDirOverrideRelativeKeepsDefault() {
    let p = BundleProfile.mac(environment: [
        "HOME": "/Users/tester",
        "ROOST_STATE_DIR": "relative/state",
    ])
    #expect(p.stateDir == "/Users/tester/Library/Application Support/Roost")
}

// MARK: - Sidebar visibility persistence (UserDefaults)
//
// Mac analog of the Rust `sidebar_collapsed_persists_across_reopen` test —
// covers the regression class the CI-skipped relaunch e2e can't, since the
// Rust GTK state.json test doesn't exercise the Mac UserDefaults path.

@Test
func sidebarVisibleOnLaunchDefaultsToVisibleWhenUnset() {
    let suite = "ai.stridelabs.Roost.test.\(UUID().uuidString)"
    let defaults = UserDefaults(suiteName: suite)!
    defer { defaults.removePersistentDomain(forName: suite) }
    // Never toggled → sidebar starts visible.
    #expect(RoostApp.sidebarVisibleOnLaunch(defaults) == true)
}

@Test
func sidebarVisibleStateSurvivesReopen() {
    let suite = "ai.stridelabs.Roost.test.\(UUID().uuidString)"
    let defaults = UserDefaults(suiteName: suite)!
    defer { defaults.removePersistentDomain(forName: suite) }
    // User hides it → an explicit false must survive a "relaunch" (re-read).
    defaults.set(false, forKey: "RoostSidebarVisible")
    #expect(RoostApp.sidebarVisibleOnLaunch(defaults) == false)
    // User re-shows it → back to visible.
    defaults.set(true, forKey: "RoostSidebarVisible")
    #expect(RoostApp.sidebarVisibleOnLaunch(defaults) == true)
}

