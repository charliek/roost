//! Shared helpers for the Rust side of Roost.
//!
//! Single source of truth for:
//!   * Bundle-profile path resolution (socket / state / log directories).
//!   * The unix-socket-aware `tonic` Channel constructor that all
//!     daemon-era clients (roost-cli-rs, roost-smoke, roost-linux) need.
//!
//! `BundleProfile` is the new path-resolution surface (introduced in
//! the daemon-removal refactor M1). Each Roost binary picks a default
//! profile — daemon + Mac UI + CLI default to `Mac`; the Linux UI
//! defaults to `Gtk`. The `ROOST_BUNDLE_PROFILE=mac|gtk` env var
//! overrides at runtime for dev / smoke scenarios.
//!
//! The Mac UI implements the same logic in Swift (it doesn't link Rust
//! crates); the algorithm is intentionally simple and identical.
//!
//! The free-function helpers `default_socket_path` / `default_db_path`
//! that earlier callers consume remain as thin compat shims around
//! the Mac profile — the daemon (only consumer of `default_db_path`)
//! defaults to Mac, and the legacy callers of `default_socket_path`
//! either go through this path or get migrated in subsequent
//! milestones.

#![deny(unsafe_op_in_unsafe_fn)]

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tonic::transport::{Channel, Endpoint};
use tower::service_fn;

// ============================================================================
// Bundle profile
// ============================================================================

/// The two UI variants Roost ships. On macOS they coexist on the same
/// machine with distinct paths so a Swift `Roost.app` and a
/// `cargo run -p roost-linux` dev session don't fight over the same
/// socket / state directory. On Linux only the `Gtk` variant runs in
/// production; on Linux both kinds resolve to the same XDG paths
/// because there is no second native UI.
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
    /// String form used by `ROOST_BUNDLE_PROFILE` and a few log lines.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Mac => "mac",
            Self::Gtk => "gtk",
        }
    }
}

/// Resolved paths for a particular bundle profile.
///
/// `socket_path` is the Unix-domain-socket path the UI binds and any
/// CLI dials. `state_dir` is the directory containing persistent
/// state — the legacy `roost.db` (kept until the daemon goes away in
/// M7) and the M3-introduced `state.json`. `log_dir` is the directory
/// for `roost.log`.
#[derive(Clone, Debug)]
pub struct BundleProfile {
    pub kind: BundleProfileKind,
    /// Human-readable label used in path components on macOS.
    /// `"Roost"` for the Mac profile, `"Roost-gtk"` for the GTK
    /// profile. Linux ignores this — XDG paths use lowercase `roost/`
    /// regardless.
    pub app_label: &'static str,
    /// Reverse-DNS application identifier (Mac `CFBundleIdentifier`,
    /// gtk `application_id`).
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

    /// Shortcut for `BundleProfile::for_kind(BundleProfileKind::Mac)`.
    pub fn mac() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Mac)
    }

    /// Shortcut for `BundleProfile::for_kind(BundleProfileKind::Gtk)`.
    pub fn gtk() -> anyhow::Result<BundleProfile> {
        Self::for_kind(BundleProfileKind::Gtk)
    }

    /// Pick a profile with `ROOST_BUNDLE_PROFILE` overriding the
    /// binary's default. Unknown env values fall through silently to
    /// the default — callers asserting strict env should validate
    /// before calling.
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

    /// SQLite database path inside `state_dir`. The daemon's only
    /// consumer; deleted in M7 along with the daemon.
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
}

#[cfg(target_os = "macos")]
fn resolve_paths(app_label: &str) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    let home = std::env::var_os("HOME").context("$HOME not set")?;
    let home = PathBuf::from(home);
    let socket = home
        .join("Library/Caches")
        .join(app_label)
        .join("roost.sock");
    let state = home.join("Library/Application Support").join(app_label);
    let log = home.join("Library/Logs").join(app_label);
    Ok((socket, state, log))
}

#[cfg(not(target_os = "macos"))]
fn resolve_paths(_app_label: &str) -> anyhow::Result<(PathBuf, PathBuf, PathBuf)> {
    // Linux (and other unixes): XDG. On Linux the Mac/Gtk kinds
    // collapse to the same paths because there is no Swift app.
    let socket = match xdg_runtime_dir() {
        Some(dir) => dir.join("roost").join("roost.sock"),
        None => {
            let uid = libc_getuid();
            PathBuf::from(format!("/tmp/roost-{uid}")).join("roost.sock")
        }
    };
    let state = match xdg_data_home() {
        Some(dir) => dir.join("roost"),
        None => {
            let home = std::env::var_os("HOME").context("$HOME not set")?;
            PathBuf::from(home).join(".local/share/roost")
        }
    };
    let log = match xdg_state_home() {
        Some(dir) => dir.join("roost"),
        None => {
            let home = std::env::var_os("HOME").context("$HOME not set")?;
            PathBuf::from(home).join(".local/state/roost")
        }
    };
    Ok((socket, state, log))
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

// ============================================================================
// Legacy compat helpers
// ============================================================================
//
// The two free functions below carry the same semantics as the
// pre-M1 helpers, except they now resolve via the Mac bundle profile.
// On macOS that means the directory component flips from lowercase
// `roost/` to capital `Roost/` — refactor-branch users lose continuity
// with stale state in the lowercase directory. This is intentional
// (the no-migration policy documented in `docs/reference/paths.md`
// and in M1's commit message). Callers that want the GTK profile must
// build a `BundleProfile::gtk()` explicitly.

/// Resolve the default `roost-core` Unix-domain-socket path under the
/// Mac bundle profile.
///
/// * **macOS:** `~/Library/Caches/Roost/roost.sock`
/// * **Linux (XDG):** `$XDG_RUNTIME_DIR/roost/roost.sock`
/// * **Linux (fallback):** `/tmp/roost-<uid>/roost.sock`
pub fn default_socket_path() -> anyhow::Result<PathBuf> {
    Ok(BundleProfile::mac()?.socket_path)
}

/// Resolve the default SQLite database path under the Mac bundle
/// profile (still used by the daemon until M7).
///
/// * **macOS:** `~/Library/Application Support/Roost/roost.db`
/// * **Linux (XDG):** `$XDG_DATA_HOME/roost/roost.db`
/// * **Linux (fallback):** `$HOME/.local/share/roost/roost.db`
pub fn default_db_path() -> anyhow::Result<PathBuf> {
    Ok(BundleProfile::mac()?.db_path())
}

// ============================================================================
// libc::getuid wrapper
// ============================================================================
//
// Avoiding the libc dependency for one symbol. The Linux fallback path
// uses `/tmp/roost-<uid>` so two users on the same host don't collide
// on a default socket. Returns 0 on non-unix targets (which we don't
// actually ship to, but cfg-gating keeps the workspace cross-target).

#[cfg(unix)]
extern "C" {
    fn getuid() -> u32;
}

/// User-id wrapper. Safe to call.
pub fn libc_getuid() -> u32 {
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
// gRPC client channel over a Unix domain socket
// ============================================================================

/// Build a `tonic` Channel that talks to `roost-core` over the given
/// Unix-domain-socket path.
///
/// `tonic`'s `Endpoint::connect_with_connector` lets us plug in a
/// custom service that returns a Tokio `UnixStream` instead of a TCP
/// one. The URL passed to `Endpoint::from_static` is irrelevant —
/// tonic only uses it for HTTP/2 pseudo-header routing — but it must
/// be a syntactically valid http URI.
pub async fn connect_uds(path: PathBuf) -> anyhow::Result<Channel> {
    let path = Arc::new(path);
    let display_path = path.display().to_string();
    let endpoint = Endpoint::from_static("http://[::]:0");
    let channel = endpoint
        .connect_with_connector(service_fn(move |_| {
            let path = path.clone();
            async move {
                let stream = tokio::net::UnixStream::connect(&*path).await?;
                let io = hyper_util::rt::TokioIo::new(stream);
                Ok::<_, std::io::Error>(io)
            }
        }))
        .await
        .with_context(|| format!("connect uds at {display_path}"))?;
    Ok(channel)
}

/// Convenience: connect to whatever path resolves through the
/// daemon's defaults. Useful for one-shot CLI calls that don't want
/// to plumb explicit socket-path handling.
#[allow(dead_code)] // Provided for downstream binaries; not all use it directly.
pub async fn connect_default() -> anyhow::Result<Channel> {
    connect_uds(default_socket_path()?).await
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    /// Both paths must always be absolute and non-empty regardless of
    /// the host's environment. The exact value depends on env vars
    /// outside our control, so we don't assert on it.
    fn assert_path_well_formed(p: &Path) {
        assert!(p.is_absolute(), "{} should be absolute", p.display());
        let lossy = p.to_string_lossy();
        assert!(
            lossy.contains("roost") || lossy.contains("Roost"),
            "{} should reference roost",
            p.display()
        );
    }

    #[test]
    fn socket_path_is_well_formed() {
        let p = default_socket_path().expect("socket path resolves");
        assert_path_well_formed(&p);
        assert!(
            p.to_string_lossy().ends_with(".sock"),
            "{} should end in .sock",
            p.display()
        );
    }

    #[test]
    fn db_path_is_well_formed() {
        let p = default_db_path().expect("db path resolves");
        assert_path_well_formed(&p);
        assert!(
            p.to_string_lossy().ends_with(".db"),
            "{} should end in .db",
            p.display()
        );
    }

    #[test]
    fn libc_getuid_is_callable() {
        let _ = libc_getuid();
    }

    #[test]
    fn mac_profile_has_distinct_paths_from_gtk_on_macos_only() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        assert_eq!(mac.app_id, "ai.stridelabs.Roost");
        assert_eq!(gtk.app_id, "ai.stridelabs.Roost.gtk");
        // On macOS the directories differ (Roost vs Roost-gtk); on
        // Linux they collapse to the same XDG layout. Either way the
        // app ids are different so callers can tell the variants
        // apart in logs.
        #[cfg(target_os = "macos")]
        {
            assert_ne!(mac.socket_path, gtk.socket_path);
            assert_ne!(mac.state_dir, gtk.state_dir);
            assert_ne!(mac.log_dir, gtk.log_dir);
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert_eq!(mac.socket_path, gtk.socket_path);
            assert_eq!(mac.state_dir, gtk.state_dir);
            assert_eq!(mac.log_dir, gtk.log_dir);
        }
    }

    #[test]
    fn resolve_falls_back_to_default_kind_for_unknown_env() {
        // `ROOST_BUNDLE_PROFILE` is a global env var; setting it from
        // a test would race with other tests. Instead exercise the
        // default path by ensuring callers without the env still get
        // their chosen default.
        let p = BundleProfile::resolve(BundleProfileKind::Mac).expect("resolve mac");
        assert_eq!(p.kind, BundleProfileKind::Mac);
    }
}
