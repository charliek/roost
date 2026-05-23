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

/// Two UI variants Roost ships. On macOS they coexist on the same
/// machine with distinct paths so a Swift `Roost.app` and a
/// `cargo run -p roost-linux` dev session don't fight over the same
/// socket / state directory. On Linux there is only one UI, so both
/// kinds resolve to the same XDG paths.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BundleProfileKind {
    /// The Swift `Roost.app`. App id: `ai.stridelabs.Roost`.
    Mac,
    /// The gtk4-rs `roost-linux` binary. App id:
    /// `ai.stridelabs.Roost.gtk`. Linux's only UI; on macOS this is
    /// the dev-mode side-by-side variant.
    Gtk,
}

impl BundleProfileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Gtk => "gtk",
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
    /// `"Roost"` for the Mac profile, `"Roost-gtk"` for the GTK
    /// profile. Linux ignores this — XDG paths use lowercase `roost/`
    /// regardless.
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
        };
        let (socket_path, state_dir, log_dir) = resolve_paths(app_label)?;
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

    /// Pick a profile with `ROOST_BUNDLE_PROFILE` overriding the
    /// caller's default. Unknown values silently fall through to the
    /// default.
    pub fn resolve(default: BundleProfileKind) -> anyhow::Result<BundleProfile> {
        let kind = match std::env::var("ROOST_BUNDLE_PROFILE")
            .ok()
            .as_deref()
            .map(str::trim)
        {
            Some("mac") => BundleProfileKind::Mac,
            Some("gtk") => BundleProfileKind::Gtk,
            _ => default,
        };
        Self::for_kind(kind)
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

#[cfg(target_os = "macos")]
fn resolve_paths(app_label: &str) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
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
fn resolve_paths(_app_label: &str) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let socket = match xdg_runtime_dir() {
        Some(dir) => dir.join("roost").join("roost.sock"),
        None => {
            let uid = libc_getuid();
            PathBuf::from(format!("/tmp/roost-{uid}")).join("roost.sock")
        }
    };
    let state = match xdg_data_home() {
        Some(dir) => dir.join("roost"),
        None => valid_home()?.join(".local/share/roost"),
    };
    let log = match xdg_state_home() {
        Some(dir) => dir.join("roost"),
        None => valid_home()?.join(".local/state/roost"),
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
        assert_eq!(mac.app_id, "ai.stridelabs.Roost");
        assert_eq!(gtk.app_id, "ai.stridelabs.Roost.gtk");
        assert_eq!(mac.app_label, "Roost");
        assert_eq!(gtk.app_label, "Roost-gtk");
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

    #[cfg(target_os = "macos")]
    #[test]
    fn mac_paths_are_distinct_from_gtk_on_mac() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        assert_ne!(mac.socket_path, gtk.socket_path);
        assert_ne!(mac.state_dir, gtk.state_dir);
        assert_ne!(mac.log_dir, gtk.log_dir);
        assert!(mac.socket_path.to_string_lossy().contains("/Roost/"));
        assert!(gtk.socket_path.to_string_lossy().contains("/Roost-gtk/"));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_paths_collapse_across_kinds() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        assert_eq!(mac.socket_path, gtk.socket_path);
        assert_eq!(mac.state_dir, gtk.state_dir);
        assert_eq!(mac.log_dir, gtk.log_dir);
    }
}
