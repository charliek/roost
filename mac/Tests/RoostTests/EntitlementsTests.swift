// Static guard on the Mac bundle's TCC surface. Programs hosted in a Roost
// tab (Claude Code /voice, ffmpeg, sox, osascript) reach the microphone /
// camera / Apple-events *through Roost* — macOS makes Roost the responsible
// app. Under the hardened runtime those requests are silently denied (no
// prompt, no Privacy-pane entry) unless Roost.app carries the matching
// entitlement AND the paired Info.plist purpose string. Both are one trivial
// edit away from being dropped or clobbered in a merge, and the failure is
// invisible at runtime, so we assert them here (pure file read, runs in the
// deterministic swift-mac CI job — no GUI). We also assert the *negatives*:
// the broad personal-information / jit / sandbox / network keys stay out
// (see the Roost.entitlements doc comment), and the roostctl helper does not
// inherit the capture entitlements.

import Foundation
import Testing

// The capture set Roost.app must carry so hosted programs can prompt for these
// resources — and which the roostctl helper must NOT inherit.
private let captureKeys = [
    "com.apple.security.device.audio-input",
    "com.apple.security.device.camera",
    "com.apple.security.automation.apple-events",
]

// Broad keys we deliberately keep OUT of Roost.app (see the Roost.entitlements
// doc comment): PIM data (no in-use indicator), JIT, sandbox, network.
private let broadKeysExcluded = [
    "com.apple.security.personal-information.addressbook",
    "com.apple.security.personal-information.calendars",
    "com.apple.security.personal-information.location",
    "com.apple.security.personal-information.photos-library",
    "com.apple.security.cs.allow-jit",
    "com.apple.security.cs.allow-unsigned-executable-memory",
    "com.apple.security.app-sandbox",
    "com.apple.security.network.client",
    "com.apple.security.network.server",
]

private let usageStringKeys = [
    "NSMicrophoneUsageDescription",
    "NSCameraUsageDescription",
    "NSAppleEventsUsageDescription",
]

private func macDir() throws -> URL {
    // `#filePath` is mac/Tests/RoostTests/EntitlementsTests.swift; mac/ is 3 up.
    var dir = URL(fileURLWithPath: #filePath)
    for _ in 0..<3 { dir.deleteLastPathComponent() }
    var isDir: ObjCBool = false
    let resources = dir.appendingPathComponent("Resources")
    guard FileManager.default.fileExists(atPath: resources.path, isDirectory: &isDir),
          isDir.boolValue
    else {
        throw NSError(
            domain: "EntitlementsTests", code: 1,
            userInfo: [NSLocalizedDescriptionKey:
                "mac/Resources not found at \(resources.path); did the repo layout change?"]
        )
    }
    return dir
}

private func plistDict(_ name: String) throws -> [String: Any] {
    let url = try macDir().appendingPathComponent("Resources").appendingPathComponent(name)
    let data = try Data(contentsOf: url)
    let obj = try PropertyListSerialization.propertyList(from: data, options: [], format: nil)
    guard let dict = obj as? [String: Any] else {
        throw NSError(
            domain: "EntitlementsTests", code: 2,
            userInfo: [NSLocalizedDescriptionKey: "\(name) did not parse as a plist dictionary"]
        )
    }
    return dict
}

@Test
func appEntitlementsDeclareCaptureSet() throws {
    let ent = try plistDict("Roost.entitlements")
    for key in captureKeys + ["com.apple.security.cs.disable-library-validation"] {
        #expect(
            ent[key] as? Bool == true,
            "Roost.entitlements must declare \(key)=true — hosted programs lose access silently otherwise"
        )
    }
}

@Test
func appEntitlementsExcludeBroadKeys() throws {
    let ent = try plistDict("Roost.entitlements")
    for key in broadKeysExcluded {
        #expect(
            ent[key] == nil,
            "Roost.entitlements unexpectedly declares \(key); see its doc comment for why this stays out"
        )
    }
}

@Test
func infoPlistTemplateHasUsageStrings() throws {
    let info = try plistDict("Info.plist.template")
    for key in usageStringKeys {
        let value = (info[key] as? String)?.trimmingCharacters(in: .whitespacesAndNewlines)
        #expect(
            value?.isEmpty == false,
            "Info.plist.template must carry a non-empty \(key) (paired with the capture entitlement)"
        )
    }
}

@Test
func roostctlHelperDropsCaptureEntitlements() throws {
    let ent = try plistDict("roostctl.entitlements")
    for key in captureKeys {
        #expect(
            ent[key] == nil,
            "roostctl.entitlements must not carry \(key) — the CLI never captures (least privilege)"
        )
    }
}
