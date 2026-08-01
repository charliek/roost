// BundleProfile.swift — daemon-removal refactor M1.
//
// Swift companion to `roost_ipc::paths::BundleProfile` (Rust). Three
// variants — `mac` (Swift `Roost.app`), `gtk` (gtk4-rs
// `roost-linux`), and the Iced POC. Path resolution mirrors the Rust side so
// `roostctl` written in Rust and the Swift UI agree on where the
// Unix socket lives.
//
// The `defaultSocketPath` helper that the rest of the Mac codebase
// has been calling becomes a thin shim over the Mac profile (the
// Swift app never uses the GTK paths).

import Foundation

/// UI variants Roost ships or evaluates. On macOS they coexist on the same
/// machine with distinct paths so a Swift `Roost.app` and a
/// `cargo run -p roost-linux` dev session don't fight over the same
/// socket / state directory.
enum BundleProfileKind: String, Sendable {
    case mac
    case gtk
    case iced
}

struct BundleProfile: Sendable {
    let kind: BundleProfileKind
    /// `"Roost"`, `"Roost-gtk"`, or `"Roost-iced"`. Used as the directory component
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

    /// `roost.lock` lives next to the socket so the single-instance
    /// flock and the IPC socket share a parent directory — on **both**
    /// UIs (Linux passes `BundleProfile::lock_path()`, also socket-
    /// relative). Because it derives from `socketPath`, a
    /// `ROOST_STATE_DIR` override never moves the lock.
    var lockPath: String {
        let parent = (socketPath as NSString).deletingLastPathComponent
        return (parent as NSString).appendingPathComponent("roost.lock")
    }

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
            case .iced: return ("Roost-iced", "ai.stridelabs.Roost.iced")
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
            // Redirect ONLY the state dir when ROOST_STATE_DIR is set, so
            // tests (and side-by-side instances) get an isolated state.json
            // while socket/lock/log stay on the default path.
            stateDir: applyStateDirOverride(stateDir, env["ROOST_STATE_DIR"]),
            logDir: logDir
        )
    }

    /// Apply a `ROOST_STATE_DIR` override to `defaultDir`. Strict
    /// (**absolute** + non-empty) — NOT the permissive `ROOST_CONFIG`
    /// policy: a relative state dir resolves against the process CWD,
    /// which is nondeterministic. A set-but-invalid value (non-empty +
    /// non-absolute) is ignored with a warn; empty/unset falls back
    /// silently. KEEP IN SYNC with `paths.rs` `apply_state_dir_override`.
    private static func applyStateDirOverride(_ defaultDir: String, _ raw: String?) -> String {
        guard let raw, !raw.isEmpty else { return defaultDir }
        if raw.hasPrefix("/") { return raw }
        FileHandle.standardError.write(Data(
            "ROOST_STATE_DIR ignored: not an absolute path; using default state dir\n".utf8))
        return defaultDir
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

    /// Iced POC profile — isolated from both production UIs.
    static func iced(environment env: [String: String] = ProcessInfo.processInfo.environment)
        -> BundleProfile
    {
        resolve(kind: .iced, environment: env)
    }

    /// Pick a profile, letting `ROOST_BUNDLE_PROFILE=mac|gtk|iced` override
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
            case "iced": return .iced
            default: return fallback
            }
        }()
        return resolve(kind: kind, environment: env)
    }
}
