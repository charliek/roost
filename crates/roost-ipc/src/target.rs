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

/// Which step of the precedence order above decided the target.
///
/// [`ResolvedTarget`] cannot answer this — `kind: None` means step 1
/// *or* 2, and `Some(k)` means step 3, 4, *or* 5 — so a diagnostic that
/// wants to explain the choice would otherwise have to re-derive the
/// precedence rule and become a second source of truth for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOrigin {
    SocketFlag,
    SocketEnv,
    TargetFlag,
    ProfileEnv,
    AutoDetect,
}

impl TargetOrigin {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::SocketFlag => "--socket",
            Self::SocketEnv => "ROOST_SOCKET",
            Self::TargetFlag => "--target",
            Self::ProfileEnv => "ROOST_BUNDLE_PROFILE",
            Self::AutoDetect => "auto-detect",
        }
    }
}

/// [`TargetSelector::resolve`]'s outcome plus the inputs that produced
/// it. Non-wire, diagnostic-only.
#[derive(Debug)]
pub struct TargetDiagnosis {
    pub origin: TargetOrigin,
    /// The paths auto-detect would probe, each labelled with its profile
    /// — both on macOS, the single collapsed path elsewhere. Populated
    /// whatever the origin is, so a caller can still report per profile
    /// when resolution found nothing live. Labelled rather than
    /// positional so callers don't re-derive "index 0 means mac".
    pub candidates: Vec<(BundleProfileKind, PathBuf)>,
    pub resolved: Result<ResolvedTarget, TargetError>,
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
        self.resolve_inner(probe_alive).await.1
    }

    /// [`Self::resolve`] with `probe_alive = true`, plus which input won
    /// and what auto-detect would have tried.
    ///
    /// Additive: `resolve`'s behavior is untouched and no existing caller
    /// changes.
    pub async fn diagnose(&self) -> TargetDiagnosis {
        let (origin, resolved) = self.resolve_inner(true).await;
        TargetDiagnosis {
            origin,
            // Best-effort: when the profile paths themselves fail to
            // resolve, `resolved` already carries that error.
            candidates: auto_detect_candidates().unwrap_or_default(),
            resolved,
        }
    }

    /// Read the environment, run it through [`classify`], and act on the
    /// rung that fired.
    ///
    /// [`TargetOrigin`] exists so a diagnostic doesn't have to re-derive
    /// the precedence rule; deriving it a second time *here* would be no
    /// better, so `resolve` and `diagnose` share this and cannot drift
    /// about which input won.
    async fn resolve_inner(
        &self,
        probe_alive: bool,
    ) -> (TargetOrigin, Result<ResolvedTarget, TargetError>) {
        let socket_env = std::env::var_os("ROOST_SOCKET");
        let profile_env = std::env::var("ROOST_BUNDLE_PROFILE").ok();
        match classify(
            self.socket_override.as_deref(),
            socket_env.as_deref(),
            self.kind_override,
            profile_env.as_deref(),
        ) {
            Step::Socket(socket_path, origin) => (
                origin,
                Ok(ResolvedTarget {
                    socket_path,
                    kind: None,
                }),
            ),
            Step::Profile(kind, origin) => (origin, for_kind(kind)),
            Step::UnknownProfile(raw) => (
                TargetOrigin::ProfileEnv,
                Err(TargetError::UnknownProfile(raw)),
            ),
            Step::AutoDetect => (
                TargetOrigin::AutoDetect,
                resolve_auto_detect(probe_alive).await,
            ),
        }
    }
}

/// Which rung of the precedence ladder fired, and what it decided.
#[derive(Debug, PartialEq, Eq)]
enum Step {
    Socket(PathBuf, TargetOrigin),
    Profile(BundleProfileKind, TargetOrigin),
    UnknownProfile(String),
    AutoDetect,
}

/// The precedence order of the module doc, over inputs already read.
///
/// Pure — the env values arrive as arguments — so the policy can be
/// pinned across all five origins without a test mutating process-global
/// state that every other test in this binary also reads.
fn classify(
    socket_override: Option<&std::path::Path>,
    socket_env: Option<&std::ffi::OsStr>,
    kind_override: Option<BundleProfileKind>,
    profile_env: Option<&str>,
) -> Step {
    // 1. --socket
    if let Some(p) = socket_override {
        return Step::Socket(p.to_path_buf(), TargetOrigin::SocketFlag);
    }
    // 2. ROOST_SOCKET
    if let Some(env) = socket_env.filter(|v| !v.is_empty()) {
        return Step::Socket(PathBuf::from(env), TargetOrigin::SocketEnv);
    }
    // 3. --target
    if let Some(kind) = kind_override {
        return Step::Profile(kind, TargetOrigin::TargetFlag);
    }
    // 4. ROOST_BUNDLE_PROFILE
    match profile_env.map(str::trim) {
        Some("mac") => Step::Profile(BundleProfileKind::Mac, TargetOrigin::ProfileEnv),
        Some("gtk") => Step::Profile(BundleProfileKind::Gtk, TargetOrigin::ProfileEnv),
        // Empty string is the launchd-inherited empty-env case; fall
        // through to auto-detect so a sandboxed process with no profile
        // set can still discover one.
        Some("") | None => Step::AutoDetect,
        Some(other) => Step::UnknownProfile(other.to_string()),
    }
}

fn for_kind(kind: BundleProfileKind) -> Result<ResolvedTarget, TargetError> {
    Ok(ResolvedTarget {
        socket_path: BundleProfile::for_kind(kind)?.socket_path,
        kind: Some(kind),
    })
}

/// The socket paths auto-detect probes, each labelled with the profile
/// it belongs to, in precedence order.
///
/// Off macOS both profiles resolve to the same XDG path — there is only
/// one UI and `paths.rs` ignores `app_label` there — so the pair
/// collapses to the lone gtk target. Keyed off the resolved paths being
/// equal rather than `cfg!(target_os)` so it stays correct if the path
/// policy ever changes.
///
/// This is the *only* encoding of that set: [`resolve_auto_detect`]
/// consumes it and [`TargetDiagnosis`] reports it, so a diagnostic can
/// never disagree with the resolver about what would have been tried.
fn auto_detect_candidates() -> Result<Vec<(BundleProfileKind, PathBuf)>, TargetError> {
    let mac = BundleProfile::mac()?;
    let gtk = BundleProfile::gtk()?;
    if mac.socket_path == gtk.socket_path {
        return Ok(vec![(BundleProfileKind::Gtk, gtk.socket_path)]);
    }
    Ok(vec![
        (BundleProfileKind::Mac, mac.socket_path),
        (BundleProfileKind::Gtk, gtk.socket_path),
    ])
}

/// Auto-detect step (resolution order #5): probe the known profile
/// socket paths and pick a live one. Split out of [`TargetSelector::resolve`]
/// so it can be unit-tested directly — `resolve` consults `ROOST_SOCKET`
/// / `ROOST_BUNDLE_PROFILE` first, so testing this branch through the
/// public entry point would depend on the ambient environment.
async fn resolve_auto_detect(probe_alive: bool) -> Result<ResolvedTarget, TargetError> {
    let candidates = auto_detect_candidates()?;

    // One candidate is the collapsed off-macOS case, not a partial list
    // — probing the single shared path twice would report a phantom
    // "mac + gtk both running" ambiguity.
    let [(mac_kind, mac_path), (gtk_kind, gtk_path)] = candidates.as_slice() else {
        // `auto_detect_candidates` never returns empty, but this sits
        // under `doctor::collect`, which promises totality — an index
        // panic there would take out the diagnostic instead of being one.
        let Some((kind, path)) = candidates.first() else {
            return Err(TargetError::NoLiveTarget { tried: Vec::new() });
        };
        if !probe_alive || probe_socket(path).await {
            return Ok(ResolvedTarget {
                socket_path: path.clone(),
                kind: Some(*kind),
            });
        }
        return Err(TargetError::NoLiveTarget {
            tried: vec![path.clone()],
        });
    };

    if !probe_alive {
        // Without a probe, prefer the Mac socket. Callers in
        // probe_alive=false mode (Claude hooks) tolerate
        // "no live target" silently — they just no-op when the
        // dial fails downstream.
        return Ok(ResolvedTarget {
            socket_path: mac_path.clone(),
            kind: Some(*mac_kind),
        });
    }

    // Probe both candidates in parallel so the cold-path cost
    // is one 50ms timeout, not two. `tokio::join!` polls both
    // futures concurrently on the current task — no extra
    // executor work.
    let (mac_alive, gtk_alive) = tokio::join!(probe_socket(mac_path), probe_socket(gtk_path));
    match (mac_alive, gtk_alive) {
        (true, false) => Ok(ResolvedTarget {
            socket_path: mac_path.clone(),
            kind: Some(*mac_kind),
        }),
        (false, true) => Ok(ResolvedTarget {
            socket_path: gtk_path.clone(),
            kind: Some(*gtk_kind),
        }),
        (true, true) => Err(TargetError::Ambiguous),
        (false, false) => Err(TargetError::NoLiveTarget {
            tried: vec![mac_path.clone(), gtk_path.clone()],
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

    /// The precedence ladder itself, over every origin `TargetOrigin`
    /// can report plus the two values that must *not* decide anything:
    /// an empty `ROOST_SOCKET` and an empty `ROOST_BUNDLE_PROFILE`.
    /// Driven through `classify` because `resolve` reads process-global
    /// env, which no test in this binary may mutate safely.
    #[test]
    fn origin_classification_pins_the_precedence_ladder() {
        use std::ffi::OsStr;
        let sock = std::path::Path::new("/tmp/flag.sock");
        let env_sock = OsStr::new("/tmp/env.sock");

        // 1. --socket outranks everything, including a set env and flag.
        assert_eq!(
            classify(
                Some(sock),
                Some(env_sock),
                Some(BundleProfileKind::Gtk),
                Some("mac")
            ),
            Step::Socket(sock.to_path_buf(), TargetOrigin::SocketFlag)
        );
        // 2. ROOST_SOCKET outranks --target and ROOST_BUNDLE_PROFILE.
        assert_eq!(
            classify(
                None,
                Some(env_sock),
                Some(BundleProfileKind::Gtk),
                Some("mac")
            ),
            Step::Socket(PathBuf::from(env_sock), TargetOrigin::SocketEnv)
        );
        // …but an empty one does not: it falls through to the next rung.
        assert_eq!(
            classify(
                None,
                Some(OsStr::new("")),
                Some(BundleProfileKind::Gtk),
                None
            ),
            Step::Profile(BundleProfileKind::Gtk, TargetOrigin::TargetFlag)
        );
        // 3. --target outranks ROOST_BUNDLE_PROFILE.
        assert_eq!(
            classify(None, None, Some(BundleProfileKind::Mac), Some("gtk")),
            Step::Profile(BundleProfileKind::Mac, TargetOrigin::TargetFlag)
        );
        // 4. ROOST_BUNDLE_PROFILE, both spellings, whitespace tolerated.
        assert_eq!(
            classify(None, None, None, Some("mac")),
            Step::Profile(BundleProfileKind::Mac, TargetOrigin::ProfileEnv)
        );
        assert_eq!(
            classify(None, None, None, Some(" gtk ")),
            Step::Profile(BundleProfileKind::Gtk, TargetOrigin::ProfileEnv)
        );
        // An unrecognized profile is an error, not a fall-through — the
        // user named a target and got it wrong.
        assert_eq!(
            classify(None, None, None, Some("bogus")),
            Step::UnknownProfile("bogus".into())
        );
        // 5. Nothing set, and the empty-profile (launchd) case, both
        // reach auto-detect.
        assert_eq!(classify(None, None, None, None), Step::AutoDetect);
        assert_eq!(classify(None, None, None, Some("  ")), Step::AutoDetect);
        assert_eq!(
            classify(None, Some(OsStr::new("")), None, Some("")),
            Step::AutoDetect
        );
    }

    /// `diagnose` is additive public surface over the same algorithm.
    /// `resolve_inner` makes agreement structural, so this only has to
    /// pin the wiring: `--socket` wins ahead of every env var, so this
    /// case is deterministic whatever the ambient environment holds.
    #[tokio::test]
    async fn diagnose_reports_the_origin_and_the_candidates() {
        let selector = TargetSelector {
            socket_override: Some(PathBuf::from("/tmp/diagnose-probe.sock")),
            kind_override: None,
        };
        let diagnosis = selector.diagnose().await;
        assert_eq!(diagnosis.origin, TargetOrigin::SocketFlag);
        assert_eq!(
            diagnosis.resolved.expect("resolved").socket_path,
            selector.resolve(true).await.expect("resolve").socket_path
        );
        assert!(
            !diagnosis.candidates.is_empty(),
            "auto-detect candidates must be reported whatever the origin"
        );
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
