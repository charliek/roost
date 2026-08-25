// BundleProfile.swift — daemon-removal refactor M1.
//
// Swift companion to `roost_ipc::paths::BundleProfile` (Rust). Three
// variants — `mac` (Swift `Roost.app`), `linux` (the packaged Linux
// `roost`), and `iced` (the Rust + iced build). Path resolution mirrors
// the Rust side so `roostctl` written in Rust and the Swift UI agree on
// where the Unix socket lives.
//
// The `defaultSocketPath` helper that the rest of the Mac codebase
// has been calling becomes a thin shim over the Mac profile (the
// Swift app never uses the Linux paths).

import Foundation

/// UI variants Roost ships or evaluates. On macOS they coexist on the same
/// machine with distinct paths so a Swift `Roost.app` and a
/// `cargo run -p roost-iced` dev session don't fight over the same
/// socket / state directory. `linux` never launches on macOS; it stays
/// modeled here so this mirror covers every kind the Rust enum has.
enum BundleProfileKind: String, Sendable {
    case mac
    case linux
    case iced
}

struct BundleProfile: Sendable {
    let kind: BundleProfileKind
    /// `"Roost"`, `"Roost-linux"`, or `"Roost-iced"`. Used as the directory component
    /// under `~/Library/{Caches,Application Support,Logs}/`.
    let appLabel: String
    /// `CFBundleIdentifier` (Mac) / desktop-entry id (Linux).
    let appID: String
    let socketPath: String
    let stateDir: String
    let logDir: String

    /// `state.json` path inside `stateDir`. Introduced in M3 (M1 just
    /// publishes the helper).
    var stateJSONPath: String { (stateDir as NSString).appendingPathComponent("state.json") }

    /// `roost.log` path inside `logDir`.
    var logPath: String { (logDir as NSString).appendingPathComponent("roost.log") }

    /// flock guarding the IPC socket: the probe→unlink→bind sequence
    /// and the bound socket's lifetime. `roost.lock` lives next to the
    /// socket, so it never moves with a `ROOST_STATE_DIR` override.
    /// Mirrors `BundleProfile::socket_lock_path()` in `paths.rs`.
    ///
    /// The name stays `roost.lock` for compatibility with tooling that
    /// already knows it (`tools/roosttest`, `docs/reference/paths.md`).
    var socketLockPath: String {
        let parent = (socketPath as NSString).deletingLastPathComponent
        return (parent as NSString).appendingPathComponent("roost.lock")
    }

    /// flock guarding `state.json`. Lives next to it, so it follows
    /// `ROOST_STATE_DIR` — two UIs pointed at one state dir contend
    /// even when their socket directories differ. Mirrors
    /// `BundleProfile::state_lock_path()` in `paths.rs`.
    ///
    /// The filename differs from `socketLockPath`'s on purpose:
    /// `stateDir` can equal the socket's directory (the HOME-less
    /// `/tmp/<appLabel>` fallback below, or a `ROOST_STATE_DIR` aimed
    /// at it). One shared name would make the two locks one file, and
    /// `flock` is per-open-file-description — the app would contend
    /// with itself and refuse to start.
    var stateLockPath: String {
        (stateDir as NSString).appendingPathComponent("state.lock")
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
        // Matches Rust's `paths.rs` on macOS, the only OS this runs on.
        // There the Linux id is platform-resolved and collapses onto
        // `ai.stridelabs.Roost` on Linux; `.linux` is the macOS answer.
        let (appLabel, appID): (String, String) = {
            switch kind {
            case .mac: return ("Roost", "ai.stridelabs.Roost")
            case .linux: return ("Roost-linux", "ai.stridelabs.Roost.linux")
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
            // — and, with it, the state lock — while the socket, the socket
            // lock, and the log stay on the default path.
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

    /// Linux profile — what the packaged `/usr/bin/roost` uses. Kept
    /// resolvable on macOS (nothing launches it there) so this mirror
    /// stays total against the Rust enum.
    static func linux(environment env: [String: String] = ProcessInfo.processInfo.environment)
        -> BundleProfile
    {
        resolve(kind: .linux, environment: env)
    }

    /// Iced profile — isolated from both production UIs.
    static func iced(environment env: [String: String] = ProcessInfo.processInfo.environment)
        -> BundleProfile
    {
        resolve(kind: .iced, environment: env)
    }

    /// Pick a profile, letting `ROOST_BUNDLE_PROFILE=mac|linux|iced` override
    /// the caller's preferred default. Unknown values silently fall
    /// through to the default — same policy as Rust.
    static func currentForBinary(
        default fallback: BundleProfileKind,
        environment env: [String: String] = ProcessInfo.processInfo.environment
    ) -> BundleProfile {
        let kind: BundleProfileKind = {
            switch env["ROOST_BUNDLE_PROFILE"]?.trimmingCharacters(in: .whitespaces) {
            case "mac": return .mac
            case "linux": return .linux
            case "iced": return .iced
            default: return fallback
            }
        }()
        return resolve(kind: kind, environment: env)
    }
}
