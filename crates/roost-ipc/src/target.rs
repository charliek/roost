//! CLI-side target selection for `roostctl`.
//!
//! `roostctl` can dial either the Mac Swift UI or the GTK UI. With
//! both running on the same Mac, the CLI needs to be told which to
//! talk to. Resolution order (highest precedence first):
//!
//! 1. `--socket <path>` (explicit path).
//! 2. `ROOST_SOCKET` env var.
//! 3. `--target {mac,gtk}` shortcut (resolves to that profile's
//!    canonical socket path).
//! 4. `ROOST_BUNDLE_PROFILE` env var (same effect as `--target`).
//! 5. Auto-detect: probe both known socket paths. If exactly one is
//!    listening, use it. If both are listening, return
//!    [`TargetError::Ambiguous`]. If neither, return
//!    [`TargetError::NoLiveTarget`].
//!
//! The auto-detect probe must be cheap and fast — it's on the hot
//! path for every CLI invocation. Implementation: `connect()` with a
//! short timeout (~50ms per profile) and immediately close on
//! success.

use std::path::PathBuf;
use std::time::Duration;

use tokio::net::UnixStream;
use tokio::time::timeout;

use crate::paths::{BundleProfile, BundleProfileKind};

/// CLI inputs to target resolution. All fields optional.
#[derive(Debug, Default, Clone)]
pub struct TargetSelector {
    /// `--socket <path>` value. Highest precedence.
    pub socket_override: Option<PathBuf>,
    /// `--target {mac,gtk}` value.
    pub kind_override: Option<BundleProfileKind>,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(
        "two Roost UIs are running (mac + gtk); pass --target mac|gtk or set \
         ROOST_BUNDLE_PROFILE"
    )]
    Ambiguous,
    #[error("no Roost UI is running (tried {tried:?})")]
    NoLiveTarget { tried: Vec<PathBuf> },
    #[error("path resolution failed: {0}")]
    Path(#[from] anyhow::Error),
    #[error("unknown ROOST_BUNDLE_PROFILE value {0:?} (expected `mac` or `gtk`)")]
    UnknownProfile(String),
}

/// Resolved target — a socket path plus the profile kind that
/// produced it (or `None` if the path came from `--socket` /
/// `ROOST_SOCKET` directly).
#[derive(Debug, Clone)]
pub struct ResolvedTarget {
    pub socket_path: PathBuf,
    pub kind: Option<BundleProfileKind>,
}

impl TargetSelector {
    /// Resolve to a socket path.
    ///
    /// `probe_alive` controls whether the auto-detect step actually
    /// dials the candidate sockets. Pass `true` for `roostctl`
    /// commands that need to actually talk to a UI; pass `false` for
    /// commands like `claude-hook session-start` that should exit 0
    /// even when no UI is running (the hook silently no-ops).
    pub async fn resolve(&self, probe_alive: bool) -> Result<ResolvedTarget, TargetError> {
        // 1. --socket
        if let Some(p) = &self.socket_override {
            return Ok(ResolvedTarget {
                socket_path: p.clone(),
                kind: None,
            });
        }

        // 2. ROOST_SOCKET
        if let Some(env) = std::env::var_os("ROOST_SOCKET") {
            let p = PathBuf::from(env);
            if !p.as_os_str().is_empty() {
                return Ok(ResolvedTarget {
                    socket_path: p,
                    kind: None,
                });
            }
        }

        // 3. --target
        if let Some(kind) = self.kind_override {
            let p = BundleProfile::for_kind(kind)?;
            return Ok(ResolvedTarget {
                socket_path: p.socket_path,
                kind: Some(kind),
            });
        }

        // 4. ROOST_BUNDLE_PROFILE
        if let Ok(raw) = std::env::var("ROOST_BUNDLE_PROFILE") {
            match raw.trim() {
                "mac" => {
                    let p = BundleProfile::for_kind(BundleProfileKind::Mac)?;
                    return Ok(ResolvedTarget {
                        socket_path: p.socket_path,
                        kind: Some(BundleProfileKind::Mac),
                    });
                }
                "gtk" => {
                    let p = BundleProfile::for_kind(BundleProfileKind::Gtk)?;
                    return Ok(ResolvedTarget {
                        socket_path: p.socket_path,
                        kind: Some(BundleProfileKind::Gtk),
                    });
                }
                "" => {
                    // Empty string is the launchd-inherited
                    // empty-env case; fall through to auto-detect
                    // so a sandboxed process with no profile set
                    // can still discover one.
                }
                other => {
                    return Err(TargetError::UnknownProfile(other.to_string()));
                }
            }
        }

        // 5. Auto-detect.
        resolve_auto_detect(probe_alive).await
    }
}

/// Auto-detect step (resolution order #5): probe the known profile
/// socket paths and pick a live one. Split out of [`TargetSelector::resolve`]
/// so it can be unit-tested directly — `resolve` consults `ROOST_SOCKET`
/// / `ROOST_BUNDLE_PROFILE` first, so testing this branch through the
/// public entry point would depend on the ambient environment.
async fn resolve_auto_detect(probe_alive: bool) -> Result<ResolvedTarget, TargetError> {
    let mac = BundleProfile::mac()?;
    let gtk = BundleProfile::gtk()?;
    let mac_path = mac.socket_path.clone();
    let gtk_path = gtk.socket_path.clone();

    // On Linux both profiles resolve to the same XDG socket path —
    // there is only one UI, and `paths.rs` ignores `app_label` off
    // macOS. Probing that single path twice would report a phantom
    // "mac + gtk both running" ambiguity, so collapse to the lone
    // gtk target (Linux's only UI) before the probe. Keyed off the
    // resolved paths being equal rather than `cfg!(target_os)` so it
    // stays correct if the path policy ever changes.
    if mac_path == gtk_path {
        if !probe_alive || probe_socket(&gtk_path).await {
            return Ok(ResolvedTarget {
                socket_path: gtk_path,
                kind: Some(BundleProfileKind::Gtk),
            });
        }
        return Err(TargetError::NoLiveTarget {
            tried: vec![gtk_path],
        });
    }

    if !probe_alive {
        // Without a probe, prefer the Mac socket. Callers in
        // probe_alive=false mode (Claude hooks) tolerate
        // "no live target" silently — they just no-op when the
        // dial fails downstream.
        return Ok(ResolvedTarget {
            socket_path: mac_path,
            kind: Some(BundleProfileKind::Mac),
        });
    }

    // Probe both candidates in parallel so the cold-path cost
    // is one 50ms timeout, not two. `tokio::join!` polls both
    // futures concurrently on the current task — no extra
    // executor work.
    let (mac_alive, gtk_alive) = tokio::join!(probe_socket(&mac_path), probe_socket(&gtk_path));
    match (mac_alive, gtk_alive) {
        (true, false) => Ok(ResolvedTarget {
            socket_path: mac_path,
            kind: Some(BundleProfileKind::Mac),
        }),
        (false, true) => Ok(ResolvedTarget {
            socket_path: gtk_path,
            kind: Some(BundleProfileKind::Gtk),
        }),
        (true, true) => Err(TargetError::Ambiguous),
        (false, false) => Err(TargetError::NoLiveTarget {
            tried: vec![mac_path, gtk_path],
        }),
    }
}

/// Cheap liveness probe — `connect` with a short timeout.
async fn probe_socket(path: &std::path::Path) -> bool {
    matches!(
        timeout(Duration::from_millis(50), UnixStream::connect(path)).await,
        Ok(Ok(_))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn probe_returns_false_for_missing_socket() {
        // `/tmp/roost-ipc-test-missing-XXXX` is guaranteed to not
        // exist; the probe should return false within the timeout.
        let p = std::path::PathBuf::from(format!(
            "/tmp/roost-ipc-test-missing-{}",
            std::process::id()
        ));
        assert!(!probe_socket(&p).await);
    }

    #[tokio::test]
    async fn explicit_socket_path_short_circuits_resolution() {
        let sel = TargetSelector {
            socket_override: Some(PathBuf::from("/tmp/probe.sock")),
            kind_override: None,
        };
        let res = sel.resolve(false).await.expect("resolve");
        assert_eq!(res.socket_path, PathBuf::from("/tmp/probe.sock"));
        assert_eq!(res.kind, None);
    }

    // On non-macOS the two profiles intentionally share one socket path
    // (one UI, `app_label` ignored). The auto-detect picker must treat
    // that as a single gtk target — never as a mac+gtk ambiguity. Calls
    // `resolve_auto_detect` directly so the assertion is independent of
    // ambient `ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`, which the public
    // `resolve` consults ahead of auto-detect.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn linux_collapses_identical_profile_paths_to_gtk() {
        let mac = BundleProfile::mac().expect("mac profile");
        let gtk = BundleProfile::gtk().expect("gtk profile");
        assert_eq!(
            mac.socket_path, gtk.socket_path,
            "precondition: Linux profiles share one socket path"
        );

        // probe_alive=false skips the dial, so the result is independent
        // of whether a UI happens to be running on the test host.
        let res = resolve_auto_detect(false)
            .await
            .expect("resolve must not be ambiguous when paths collapse");
        assert_eq!(res.kind, Some(BundleProfileKind::Gtk));
        assert_eq!(res.socket_path, gtk.socket_path);
    }
}
