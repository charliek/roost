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
func bundleProfileIcedIsDistinctFromMacAndGtk() {
    let mac = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    let gtk = BundleProfile.gtk(environment: ["HOME": "/Users/tester"])
    let iced = BundleProfile.iced(environment: ["HOME": "/Users/tester"])
    #expect(iced.appID == "ai.stridelabs.Roost.iced")
    #expect(iced.appLabel == "Roost-iced")
    #expect(iced.socketPath == "/Users/tester/Library/Caches/Roost-iced/roost.sock")
    #expect(iced.stateDir == "/Users/tester/Library/Application Support/Roost-iced")
    #expect(iced.logDir == "/Users/tester/Library/Logs/Roost-iced")
    #expect(iced.socketPath != mac.socketPath)
    #expect(iced.socketPath != gtk.socketPath)
    #expect(iced.socketLockPath != mac.socketLockPath)
    #expect(iced.socketLockPath != gtk.socketLockPath)
    #expect(iced.stateLockPath != mac.stateLockPath)
    #expect(iced.stateLockPath != gtk.stateLockPath)
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

@Test
func bundleProfileIcedEnvOverridesDefault() {
    let p = BundleProfile.currentForBinary(
        default: .mac,
        environment: [
            "HOME": "/Users/tester",
            "ROOST_BUNDLE_PROFILE": "iced",
        ]
    )
    #expect(p.kind == .iced)
    #expect(p.appID == "ai.stridelabs.Roost.iced")
}

// MARK: - ROOST_STATE_DIR override (lockstep with paths.rs apply_state_dir_override)

@Test
func stateDirOverrideMovesStateAndItsLockOnly() {
    let base = BundleProfile.mac(environment: ["HOME": "/Users/tester"])
    let p = BundleProfile.mac(environment: [
        "HOME": "/Users/tester",
        "ROOST_STATE_DIR": "/tmp/roost-isolated-state",
    ])
    #expect(p.stateDir == "/tmp/roost-isolated-state")
    #expect(p.stateJSONPath == "/tmp/roost-isolated-state/state.json")
    // The state lock follows the state it guards — that is the whole
    // point: two UIs on one state dir must contend even when their
    // socket directories differ.
    #expect(p.stateLockPath == "/tmp/roost-isolated-state/state.lock")
    #expect(p.stateLockPath != base.stateLockPath)
    // Invariant: socket, socket lock, and log stay on the default path.
    #expect(p.socketPath == base.socketPath)
    #expect(p.socketLockPath == base.socketLockPath)
    #expect(p.logPath == base.logPath)
}

/// R1. `stateDir` collapses onto the socket'"'"'s directory whenever HOME is
/// missing (the `/tmp/<appLabel>` fallback) or `ROOST_STATE_DIR` points
/// at the runtime dir. One shared lock filename would make the two
/// locks one file, and `flock` is per-open-file-description — the app
/// would contend with itself. Lockstep with `paths.rs`'"'"'s
/// `the_two_lock_filenames_differ_even_when_the_directories_collide`.
@Test
func theTwoLockFilenamesDifferWhenTheDirectoriesCollide() {
    let homeless = BundleProfile.mac(environment: [:])
    #expect(homeless.stateDir == "/tmp/Roost")
    #expect((homeless.socketPath as NSString).deletingLastPathComponent == homeless.stateDir)
    #expect(homeless.socketLockPath != homeless.stateLockPath)

    let aimed = BundleProfile.mac(environment: [
        "HOME": "/Users/tester",
        "ROOST_STATE_DIR": "/Users/tester/Library/Caches/Roost",
    ])
    #expect((aimed.socketPath as NSString).deletingLastPathComponent == aimed.stateDir)
    #expect(aimed.socketLockPath != aimed.stateLockPath)
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

// MARK: - Sidebar width clamp + persistence (sidebar.set_width)
//
// The wire contract (docs/reference/ipc.md) says a finite out-of-band
// width lands on the nearest bound rather than erroring, and that the
// op persists while the sidebar is collapsed. Both live in these two
// nonisolated helpers, so they're testable without AppKit.

@Test
func sidebarWidthClampsToBand() {
    #expect(RoostApp.clampSidebarWidth(260) == 260)
    // Below the floor / above the cap → nearest bound, never an error.
    #expect(RoostApp.clampSidebarWidth(90) == RoostApp.sidebarMinWidth)
    #expect(RoostApp.clampSidebarWidth(1000) == RoostApp.sidebarMaxWidth)
    // Exact bounds are in-band.
    #expect(RoostApp.clampSidebarWidth(RoostApp.sidebarMinWidth) == RoostApp.sidebarMinWidth)
    #expect(RoostApp.clampSidebarWidth(RoostApp.sidebarMaxWidth) == RoostApp.sidebarMaxWidth)
}

@Test
func sidebarWidthPersistsClampedValue() {
    let suite = "ai.stridelabs.Roost.test.\(UUID().uuidString)"
    let defaults = UserDefaults(suiteName: suite)!
    defer { defaults.removePersistentDomain(forName: suite) }
    // The collapsed arm of `setSidebarWidth` writes through here —
    // `splitViewDidResizeSubviews` skips zero-width layouts, so this is
    // the only thing that persists a width while the sidebar is hidden.
    #expect(RoostApp.persistSidebarWidth(300, in: defaults) == 300)
    #expect(defaults.double(forKey: "RoostSidebarWidth") == 300)
    // Out-of-band: the *clamped* width is what's stored, so a relaunch
    // reads back a value already inside the band.
    #expect(RoostApp.persistSidebarWidth(1000, in: defaults) == RoostApp.sidebarMaxWidth)
    #expect(defaults.double(forKey: "RoostSidebarWidth") == Double(RoostApp.sidebarMaxWidth))
}
