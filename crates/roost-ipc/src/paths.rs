//! Bundle profile + path resolution.
//!
//! `roost-ipc` is the canonical home of `BundleProfile`. `roost-common`
//! re-exports the type as a compatibility shim until the daemon
//! goes away in M7. The Swift companion at
//! `mac/Sources/Roost/BundleProfile.swift` mirrors this resolver
//! byte-for-byte; the two implementations are tested in lockstep.

use std::path::PathBuf;

#[cfg(not(target_os = "macos"))]
use anyhow::Context;

/// UI variants Roost ships or evaluates. On macOS all three coexist
/// with distinct paths. On Linux the established Mac/GTK profiles keep
/// sharing the production XDG paths, while the Iced POC uses its own
/// namespace so it can run beside GTK.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BundleProfileKind {
    /// The Swift `Roost.app`. App id: `ai.stridelabs.Roost`.
    Mac,
    /// The gtk4-rs `roost-linux` binary. App id:
    /// `ai.stridelabs.Roost.gtk`. The production Linux UI; on macOS
    /// this is the dev-mode side-by-side variant.
    Gtk,
    /// The Rust + Iced proof of concept. App id:
    /// `ai.stridelabs.Roost.iced`. Always isolated from the production
    /// Mac/GTK namespace, including on Linux.
    Iced,
}

impl BundleProfileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Gtk => "gtk",
            Self::Iced => "iced",
        }
    }
}

/// Resolved paths for one bundle profile.
///
/// `socket_path` is the Unix-domain-socket path the UI binds and any
/// CLI dials. `state_dir` is the directory containing persistent
/// state (`state.json` post-M3; the legacy `roost.db` pre-M7).
/// `log_dir` is the directory containing `roost.log`.
#[derive(Clone, Debug)]
pub struct BundleProfile {
    pub kind: BundleProfileKind,
    /// Human-readable label used in path components on macOS.
    /// `"Roost"` for Mac, `"Roost-gtk"` for GTK, and `"Roost-iced"`
    /// for Iced. Linux uses the established `roost/` namespace for Mac
    /// and GTK and the isolated `roost-iced/` namespace for Iced.
    pub app_label: &'static str,
    /// Reverse-DNS application identifier (`CFBundleIdentifier` on
    /// macOS, gtk `application_id` on Linux).
    pub app_id: &'static str,
    pub socket_path: PathBuf,
    pub state_dir: PathBuf,
    pub log_dir: PathBuf,
}

impl BundleProfile {
    /// Resolve a profile by kind.
    pub fn for_kind(kind: BundleProfileKind) -> anyhow::Result<BundleProfile> {
        let (app_label, app_id) = match kind {
            BundleProfileKind::Mac => ("Roost", "ai.stridelabs.Roost"),
            BundleProfileKind::Gtk => ("Roost-gtk", "ai.stridelabs.Roost.gtk"),
            BundleProfileKind::Iced => ("Roost-iced", "ai.stridelabs.Roost.iced"),
        };
        let (socket_path, state_dir, log_dir) = resolve_paths(kind, app_label)?;
        // Redirect ONLY the state dir when `ROOST_STATE_DIR` is set, so
        // tests (and side-by-side instances) get an isolated `state.json`
        // while the socket/lock/log stay on the default profile path — the
        // CLI + harness find the UI by the unchanged socket. See
        // `apply_state_dir_override` for the strict (absolute) policy.
        let state_dir = apply_state_dir_override(state_dir, std::env::var_os("ROOST_STATE_DIR"));
        Ok(BundleProfile {
            kind,
            app_label,
            app_id,
            socket_path,
            state_dir,
            log_dir,
        })
    }

    pub fn mac() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Mac)
    }

    pub fn gtk() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Gtk)
    }

    pub fn iced() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Iced)
    }

    /// Pick a profile with `ROOST_BUNDLE_PROFILE` overriding the
    /// caller's default. Unknown values silently fall through to the
    /// default.
    pub fn resolve(default: BundleProfileKind) -> anyhow::Result<BundleProfile> {
        let value = std::env::var("ROOST_BUNDLE_PROFILE").ok();
        Self::for_kind(kind_from_profile_env(default, value.as_deref()))
    }

    /// SQLite database path inside `state_dir`. Daemon-only; deleted
    /// in M7 along with the daemon.
    pub fn db_path(&self) -> PathBuf {
        self.state_dir.join("roost.db")
    }

    /// `state.json` path inside `state_dir`. Introduced in M3.
    pub fn state_json_path(&self) -> PathBuf {
        self.state_dir.join("state.json")
    }

    /// `roost.log` path inside `log_dir`.
    pub fn log_path(&self) -> PathBuf {
        self.log_dir.join("roost.log")
    }

    /// flock-based single-instance lock path. Lives next to the
    /// socket so cleanup logic only has to know one directory.
    pub fn lock_path(&self) -> PathBuf {
        // `socket_path` always lives in a directory we control.
        match self.socket_path.parent() {
            Some(parent) => parent.join("roost.lock"),
            // Should never happen: BundleProfile::for_kind always
            // joins at least one component onto an absolute root.
            // Fall back to a leaf-style filename to avoid a panic.
            None => PathBuf::from("roost.lock"),
        }
    }
}

fn kind_from_profile_env(default: BundleProfileKind, value: Option<&str>) -> BundleProfileKind {
    match value.map(str::trim) {
        Some("mac") => BundleProfileKind::Mac,
        Some("gtk") => BundleProfileKind::Gtk,
        Some("iced") => BundleProfileKind::Iced,
        _ => default,
    }
}

/// Apply a `ROOST_STATE_DIR` override to the resolved state dir. The env
/// value is passed in (not read here) so the policy is unit-testable
/// without mutating process-global env. Redirects **only** the state dir;
/// the caller leaves socket/lock/log untouched.
///
/// Validation follows the strict `valid_home`/XDG style (**absolute** +
/// non-empty), NOT the permissive `ROOST_CONFIG` policy: a relative state
/// dir would resolve against the process CWD — nondeterministic, and a
/// likely way to scribble state somewhere unexpected. A set-but-invalid
/// value (non-empty + non-absolute) is ignored with a warn; empty/unset
/// falls back silently (mirrors the HOME handling in `resolve_paths`).
/// Existence isn't checked — like the default dir, it's created on first
/// write. KEEP IN SYNC with `BundleProfile.swift`'s override.
fn apply_state_dir_override(default: PathBuf, raw: Option<std::ffi::OsString>) -> PathBuf {
    let Some(raw) = raw.filter(|v| !v.is_empty()) else {
        return default;
    };
    let p = PathBuf::from(&raw);
    if p.is_absolute() {
        p
    } else {
        tracing::warn!(
            value = ?raw,
            "ROOST_STATE_DIR ignored: not an absolute path; using default state dir"
        );
        default
    }
}

#[cfg(target_os = "macos")]
fn resolve_paths(
    _kind: BundleProfileKind,
    app_label: &str,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    // Sandboxed launchd processes can inherit `HOME=""` (set but
    // empty) or no HOME at all. Mirror the Swift companion's
    // `/tmp/<appLabel>/...` fallback rather than erroring — the
    // alternative is the process refusing to launch at all in that
    // environment. The two implementations are tested against each
    // other to stay in lockstep.
    if let Some(raw) = std::env::var_os("HOME") {
        let home = PathBuf::from(raw);
        if !home.as_os_str().is_empty() && home.is_absolute() {
            let socket = home
                .join("Library/Caches")
                .join(app_label)
                .join("roost.sock");
            let state = home.join("Library/Application Support").join(app_label);
            let log = home.join("Library/Logs").join(app_label);
            return Ok((socket, state, log));
        }
    }
    let tmp = PathBuf::from("/tmp").join(app_label);
    Ok((tmp.join("roost.sock"), tmp.clone(), tmp))
}

#[cfg(not(target_os = "macos"))]
fn resolve_paths(
    kind: BundleProfileKind,
    _app_label: &str,
) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let namespace = match kind {
        BundleProfileKind::Mac | BundleProfileKind::Gtk => "roost",
        BundleProfileKind::Iced => "roost-iced",
    };
    let socket = match xdg_runtime_dir() {
        Some(dir) => dir.join(namespace).join("roost.sock"),
        None => {
            let uid = libc_getuid();
            PathBuf::from(format!("/tmp/{namespace}-{uid}")).join("roost.sock")
        }
    };
    let state = match xdg_data_home() {
        Some(dir) => dir.join(namespace),
        None => valid_home()?.join(".local/share").join(namespace),
    };
    let log = match xdg_state_home() {
        Some(dir) => dir.join(namespace),
        None => valid_home()?.join(".local/state").join(namespace),
    };
    Ok((socket, state, log))
}

/// Read `$HOME` and ensure it's non-empty and absolute. The plain
/// `std::env::var_os` would silently yield `""` or a relative path
/// from a misconfigured launchd / container env, producing unusable
/// paths like `.local/share/roost` (no leading slash).
#[cfg(not(target_os = "macos"))]
fn valid_home() -> anyhow::Result<PathBuf> {
    let raw = std::env::var_os("HOME").context("$HOME not set")?;
    let p = PathBuf::from(&raw);
    if p.as_os_str().is_empty() || !p.is_absolute() {
        anyhow::bail!("$HOME is not an absolute non-empty path (got {:?})", raw);
    }
    Ok(p)
}

#[cfg(not(target_os = "macos"))]
fn xdg_runtime_dir() -> Option<PathBuf> {
    let raw = std::env::var_os("XDG_RUNTIME_DIR")?;
    let p = PathBuf::from(raw);
    (!p.as_os_str().is_empty() && p.is_absolute()).then_some(p)
}

#[cfg(not(target_os = "macos"))]
fn xdg_data_home() -> Option<PathBuf> {
    let raw = std::env::var_os("XDG_DATA_HOME")?;
    let p = PathBuf::from(raw);
    (!p.as_os_str().is_empty() && p.is_absolute()).then_some(p)
}

#[cfg(not(target_os = "macos"))]
fn xdg_state_home() -> Option<PathBuf> {
    let raw = std::env::var_os("XDG_STATE_HOME")?;
    let p = PathBuf::from(raw);
    (!p.as_os_str().is_empty() && p.is_absolute()).then_some(p)
}

#[cfg(not(target_os = "macos"))]
#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

#[cfg(not(target_os = "macos"))]
fn libc_getuid() -> u32 {
    #[cfg(unix)]
    unsafe {
        getuid()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_ids_are_stable() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_eq!(mac.app_id, "ai.stridelabs.Roost");
        assert_eq!(gtk.app_id, "ai.stridelabs.Roost.gtk");
        assert_eq!(iced.app_id, "ai.stridelabs.Roost.iced");
        assert_eq!(mac.app_label, "Roost");
        assert_eq!(gtk.app_label, "Roost-gtk");
        assert_eq!(iced.app_label, "Roost-iced");
    }

    #[test]
    fn lock_path_is_next_to_socket() {
        let p = BundleProfile::mac().expect("mac profile");
        assert_eq!(
            p.lock_path().parent(),
            p.socket_path.parent(),
            "lock and socket must share a directory"
        );
    }

    #[test]
    fn profile_env_parser_accepts_all_targets_and_preserves_fallback_policy() {
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Mac, Some(" iced ")),
            BundleProfileKind::Iced
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("gtk")),
            BundleProfileKind::Gtk
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, Some("mac")),
            BundleProfileKind::Mac
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Gtk, Some("unknown")),
            BundleProfileKind::Gtk
        );
        assert_eq!(
            kind_from_profile_env(BundleProfileKind::Iced, None),
            BundleProfileKind::Iced
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn all_profile_paths_are_distinct_on_mac() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_ne!(mac.socket_path, gtk.socket_path);
        assert_ne!(mac.socket_path, iced.socket_path);
        assert_ne!(gtk.socket_path, iced.socket_path);
        assert_ne!(mac.state_dir, gtk.state_dir);
        assert_ne!(mac.state_dir, iced.state_dir);
        assert_ne!(gtk.state_dir, iced.state_dir);
        assert_ne!(mac.log_dir, gtk.log_dir);
        assert_ne!(mac.log_dir, iced.log_dir);
        assert_ne!(gtk.log_dir, iced.log_dir);
        assert!(mac.socket_path.to_string_lossy().contains("/Roost/"));
        assert!(gtk.socket_path.to_string_lossy().contains("/Roost-gtk/"));
        assert!(iced.socket_path.to_string_lossy().contains("/Roost-iced/"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_keeps_iced_distinct_from_collapsed_production_profiles() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_eq!(mac.socket_path, gtk.socket_path);
        assert_eq!(mac.state_dir, gtk.state_dir);
        assert_eq!(mac.log_dir, gtk.log_dir);
        assert_ne!(gtk.socket_path, iced.socket_path);
        assert_ne!(gtk.state_dir, iced.state_dir);
        assert_ne!(gtk.log_dir, iced.log_dir);
        assert!(iced.socket_path.to_string_lossy().contains("roost-iced"));
    }

    // ROOST_STATE_DIR override policy (pure helper — no env mutation).
    // Mirrored by BundleProfileTests.swift; kept in lockstep.

    #[test]
    fn state_dir_override_absolute_replaces() {
        let got = apply_state_dir_override(
            PathBuf::from("/default/state"),
            Some("/tmp/throwaway".into()),
        );
        assert_eq!(got, PathBuf::from("/tmp/throwaway"));
    }

    #[test]
    fn state_dir_override_unset_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(apply_state_dir_override(default.clone(), None), default);
    }

    #[test]
    fn state_dir_override_empty_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(
            apply_state_dir_override(default.clone(), Some("".into())),
            default
        );
    }

    #[test]
    fn state_dir_override_relative_keeps_default() {
        let default = PathBuf::from("/default/state");
        assert_eq!(
            apply_state_dir_override(default.clone(), Some("relative/state".into())),
            default
        );
    }

    #[test]
    fn state_dir_override_moves_only_state_dir() {
        // The lockstep invariant: redirecting state_dir must leave the
        // socket, lock, and log paths byte-identical.
        let base = BundleProfile::gtk().expect("gtk profile");
        let overridden = BundleProfile {
            state_dir: apply_state_dir_override(
                base.state_dir.clone(),
                Some("/tmp/roost-isolated-state".into()),
            ),
            ..base.clone()
        };
        assert_eq!(
            overridden.state_json_path(),
            PathBuf::from("/tmp/roost-isolated-state/state.json")
        );
        assert_eq!(overridden.socket_path, base.socket_path);
        assert_eq!(overridden.lock_path(), base.lock_path());
        assert_eq!(overridden.log_path(), base.log_path());
    }
}
