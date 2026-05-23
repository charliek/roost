// BundleProfile.swift — daemon-removal refactor M1.
//
// Swift companion to `roost_common::BundleProfile` (Rust). Two
// variants — `mac` (Swift `Roost.app`) and `gtk` (gtk4-rs
// `roost-linux`, used in dev mode on macOS side-by-side with the
// Swift app). Path resolution mirrors the Rust side byte-for-byte so
// `roostctl` written in Rust and the Swift UI agree on where the
// Unix socket lives.
//
// The `defaultSocketPath` helper that the rest of the Mac codebase
// has been calling becomes a thin shim over the Mac profile (the
// Swift app never uses the GTK paths).

import Foundation

/// Two UI variants Roost ships. On macOS they coexist on the same
/// machine with distinct paths so a Swift `Roost.app` and a
/// `cargo run -p roost-linux` dev session don't fight over the same
/// socket / state directory.
enum BundleProfileKind: String, Sendable {
    case mac
    case gtk
}

struct BundleProfile: Sendable {
    let kind: BundleProfileKind
    /// `"Roost"` or `"Roost-gtk"`. Used as the directory component
    /// under `~/Library/{Caches,Application Support,Logs}/`.
    let appLabel: String
    /// `CFBundleIdentifier` (Mac) / GApplication application id (GTK).
    let appID: String
    let socketPath: String
    let stateDir: String
    let logDir: String

    /// `state.json` path inside `stateDir`. Introduced in M3 (M1 just
    /// publishes the helper).
    var stateJSONPath: String { (stateDir as NSString).appendingPathComponent("state.json") }

    /// `roost.log` path inside `logDir`.
    var logPath: String { (logDir as NSString).appendingPathComponent("roost.log") }

    /// Resolve a profile by kind using the host's environment.
    ///
    /// Falls back to `/tmp/<appLabel>/...` when `HOME` is missing or
    /// not absolute — mirrors the Rust side's defensive defaults so
    /// the Swift and Rust derivations stay in lockstep.
    static func resolve(
        kind: BundleProfileKind,
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> BundleProfile {
        let (appLabel, appID): (String, String) = {
            switch kind {
            case .mac: return ("Roost", "ai.stridelabs.Roost")
            case .gtk: return ("Roost-gtk", "ai.stridelabs.Roost.gtk")
            }
        }()

        let home: String? = {
            guard let h = env["HOME"], !h.isEmpty, h.hasPrefix("/") else { return nil }
            return h
        }()

        let socket: String
        let stateDir: String
        let logDir: String
        if let home {
            socket = "\(home)/Library/Caches/\(appLabel)/roost.sock"
            stateDir = "\(home)/Library/Application Support/\(appLabel)"
            logDir = "\(home)/Library/Logs/\(appLabel)"
        } else {
            // Mirror the Rust side: HOME-less is a degraded mode but
            // shouldn't crash — refactor branch users hitting this
            // are likely in a test or a sandboxed launchd env.
            socket = "/tmp/\(appLabel)/roost.sock"
            stateDir = "/tmp/\(appLabel)"
            logDir = "/tmp/\(appLabel)"
        }

        return BundleProfile(
            kind: kind,
            appLabel: appLabel,
            appID: appID,
            socketPath: socket,
            stateDir: stateDir,
            logDir: logDir
        )
    }

    /// Mac profile — what the Swift `Roost.app` uses.
    static func mac(environment env: [String: String] = ProcessInfo.processInfo.environment)
        -> BundleProfile
    {
        resolve(kind: .mac, environment: env)
    }

    /// GTK profile — what `roost-linux` uses (Linux always, macOS dev).
    static func gtk(environment env: [String: String] = ProcessInfo.processInfo.environment)
        -> BundleProfile
    {
        resolve(kind: .gtk, environment: env)
    }

    /// Pick a profile, letting `ROOST_BUNDLE_PROFILE=mac|gtk` override
    /// the caller's preferred default. Unknown values silently fall
    /// through to the default — same policy as Rust.
    static func currentForBinary(
        default fallback: BundleProfileKind,
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> BundleProfile {
        let kind: BundleProfileKind = {
            switch env["ROOST_BUNDLE_PROFILE"]?.trimmingCharacters(in: .whitespaces) {
            case "mac": return .mac
            case "gtk": return .gtk
            default: return fallback
            }
        }()
        return resolve(kind: kind, environment: env)
    }
}
