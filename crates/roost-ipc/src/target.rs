//! CLI-side target selection for `roostctl`.
//!
//! `roostctl` can dial the Mac, Linux, or Iced UI. With multiple UIs
//! running, the CLI needs to be told which to
//! talk to. Resolution order (highest precedence first):
//!
//! 1. `--socket <path>` (explicit path).
//! 2. `ROOST_SOCKET` env var.
//! 3. `--target {mac,linux,iced}` shortcut (resolves to that profile's
//!    canonical socket path).
//! 4. `ROOST_BUNDLE_PROFILE` env var (same effect as `--target`).
//! 5. Auto-detect: probe every distinct known socket path. If exactly
//!    one is listening, use it. If multiple are listening, return
//!    [`TargetError::Ambiguous`]. If none, return
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
    /// `--target {mac,linux,iced}` value.
    pub kind_override: Option<BundleProfileKind>,
}

#[derive(Debug, thiserror::Error)]
pub enum TargetError {
    #[error(
        "multiple Roost UIs are running ({live}); pass --target mac|linux|iced or set \
         ROOST_BUNDLE_PROFILE"
    )]
    Ambiguous { live: LiveProfiles },
    #[error("no Roost UI is running (tried {tried:?})")]
    NoLiveTarget { tried: Vec<PathBuf> },
    #[error("path resolution failed: {0}")]
    Path(#[from] anyhow::Error),
    #[error("unknown ROOST_BUNDLE_PROFILE value {0:?} (expected `mac`, `linux`, or `iced`)")]
    UnknownProfile(String),
}

/// Deterministically ordered live profiles included in ambiguity errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveProfiles(pub Vec<BundleProfileKind>);

impl std::fmt::Display for LiveProfiles {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (index, kind) in self.0.iter().enumerate() {
            if index != 0 {
                f.write_str(" + ")?;
            }
            f.write_str(kind.as_str())?;
        }
        Ok(())
    }
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
        Some("linux") => Step::Profile(BundleProfileKind::Linux, TargetOrigin::ProfileEnv),
        Some("iced") => Step::Profile(BundleProfileKind::Iced, TargetOrigin::ProfileEnv),
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
/// Profiles that resolve to the same path collapse to the later entry.
/// Today that means Mac + Linux become the lone production target on
/// Linux, while Iced remains independently addressable.
///
/// This is the *only* encoding of that set: [`resolve_auto_detect`]
/// consumes it and [`TargetDiagnosis`] reports it, so a diagnostic can
/// never disagree with the resolver about what would have been tried.
fn auto_detect_candidates() -> Result<Vec<(BundleProfileKind, PathBuf)>, TargetError> {
    let profiles = [
        BundleProfile::mac()?,
        BundleProfile::linux()?,
        BundleProfile::iced()?,
    ];
    let mut candidates: Vec<(BundleProfileKind, PathBuf)> = Vec::with_capacity(profiles.len());
    for profile in profiles {
        if let Some(existing) = candidates
            .iter_mut()
            .find(|(_, path)| *path == profile.socket_path)
        {
            existing.0 = profile.kind;
        } else {
            candidates.push((profile.kind, profile.socket_path));
        }
    }
    Ok(candidates)
}

/// Auto-detect step (resolution order #5): probe the known profile
/// socket paths and pick a live one. Split out of [`TargetSelector::resolve`]
/// so it can be unit-tested directly — `resolve` consults `ROOST_SOCKET`
/// / `ROOST_BUNDLE_PROFILE` first, so testing this branch through the
/// public entry point would depend on the ambient environment.
async fn resolve_auto_detect(probe_alive: bool) -> Result<ResolvedTarget, TargetError> {
    let candidates = auto_detect_candidates()?;

    if !probe_alive {
        // Without a probe, prefer the first canonical socket (Mac on
        // macOS, the production Linux profile on Linux). Callers in
        // probe_alive=false mode (Claude hooks) tolerate
        // "no live target" silently — they just no-op when the
        // dial fails downstream.
        return candidates
            .first()
            .map(|(kind, path)| ResolvedTarget {
                socket_path: path.clone(),
                kind: Some(*kind),
            })
            .ok_or(TargetError::NoLiveTarget { tried: Vec::new() });
    }

    // There are at most three known profiles. Spell out the bounded
    // fan-out so probes remain concurrent without adding an executor or
    // a futures utility dependency to the IPC contract crate.
    let alive = match candidates.as_slice() {
        [] => Vec::new(),
        [(_, one)] => vec![probe_socket(one).await],
        [(_, one), (_, two)] => {
            let (one, two) = tokio::join!(probe_socket(one), probe_socket(two));
            vec![one, two]
        }
        [(_, one), (_, two), (_, three)] => {
            let (one, two, three) =
                tokio::join!(probe_socket(one), probe_socket(two), probe_socket(three));
            vec![one, two, three]
        }
        _ => unreachable!("Roost has only three known profiles"),
    };
    choose_live_candidate(&candidates, &alive)
}

fn choose_live_candidate(
    candidates: &[(BundleProfileKind, PathBuf)],
    alive: &[bool],
) -> Result<ResolvedTarget, TargetError> {
    debug_assert_eq!(candidates.len(), alive.len());
    let live: Vec<_> = candidates
        .iter()
        .zip(alive)
        .filter_map(|((kind, path), alive)| alive.then_some((*kind, path)))
        .collect();
    match live.as_slice() {
        [(kind, path)] => Ok(ResolvedTarget {
            socket_path: (*path).clone(),
            kind: Some(*kind),
        }),
        [] => Err(TargetError::NoLiveTarget {
            tried: candidates.iter().map(|(_, path)| path.clone()).collect(),
        }),
        _ => Err(TargetError::Ambiguous {
            live: LiveProfiles(live.iter().map(|(kind, _)| *kind).collect()),
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
                Some(BundleProfileKind::Linux),
                Some("mac")
            ),
            Step::Socket(sock.to_path_buf(), TargetOrigin::SocketFlag)
        );
        // 2. ROOST_SOCKET outranks --target and ROOST_BUNDLE_PROFILE.
        assert_eq!(
            classify(
                None,
                Some(env_sock),
                Some(BundleProfileKind::Linux),
                Some("mac")
            ),
            Step::Socket(PathBuf::from(env_sock), TargetOrigin::SocketEnv)
        );
        // …but an empty one does not: it falls through to the next rung.
        assert_eq!(
            classify(
                None,
                Some(OsStr::new("")),
                Some(BundleProfileKind::Linux),
                None
            ),
            Step::Profile(BundleProfileKind::Linux, TargetOrigin::TargetFlag)
        );
        // 3. --target outranks ROOST_BUNDLE_PROFILE.
        assert_eq!(
            classify(None, None, Some(BundleProfileKind::Mac), Some("linux")),
            Step::Profile(BundleProfileKind::Mac, TargetOrigin::TargetFlag)
        );
        // 4. ROOST_BUNDLE_PROFILE, both spellings, whitespace tolerated.
        assert_eq!(
            classify(None, None, None, Some("mac")),
            Step::Profile(BundleProfileKind::Mac, TargetOrigin::ProfileEnv)
        );
        assert_eq!(
            classify(None, None, None, Some(" linux ")),
            Step::Profile(BundleProfileKind::Linux, TargetOrigin::ProfileEnv)
        );
        assert_eq!(
            classify(None, None, None, Some("iced")),
            Step::Profile(BundleProfileKind::Iced, TargetOrigin::ProfileEnv)
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

    /// The CLI half of the two divergent stale-value policies. A
    /// `ROOST_BUNDLE_PROFILE=gtk` left over from before the profile was
    /// renamed to `linux` is now just an unknown value — and here that
    /// is a hard error naming the accepted set, which is how a stale env
    /// gets noticed and fixed. (The UI-side resolver in `paths.rs` warns
    /// and keeps its default instead; tab environments never hit either
    /// path because the UI injects `ROOST_SOCKET`.)
    #[test]
    fn a_stale_gtk_profile_value_is_a_hard_error_with_the_accepted_set() {
        assert_eq!(
            classify(None, None, None, Some("gtk")),
            Step::UnknownProfile("gtk".into())
        );
        let message = TargetError::UnknownProfile("gtk".into()).to_string();
        assert!(
            message.contains("expected `mac`, `linux`, or `iced`"),
            "{message}"
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

    // On non-macOS the production profiles intentionally share one socket
    // path while Iced has its own. The auto-detect picker must treat Mac +
    // Linux as a single production candidate — never as a phantom
    // ambiguity. Calls
    // `resolve_auto_detect` directly so the assertion is independent of
    // ambient `ROOST_SOCKET` / `ROOST_BUNDLE_PROFILE`, which the public
    // `resolve` consults ahead of auto-detect.
    #[cfg(not(target_os = "macos"))]
    #[tokio::test]
    async fn linux_collapses_production_paths_but_keeps_iced_candidate() {
        let mac = BundleProfile::mac().expect("mac profile");
        let linux = BundleProfile::linux().expect("linux profile");
        let iced = BundleProfile::iced().expect("iced profile");
        assert_eq!(
            mac.socket_path, linux.socket_path,
            "precondition: Linux profiles share one socket path"
        );
        assert_ne!(linux.socket_path, iced.socket_path);

        let candidates = auto_detect_candidates().expect("candidates");
        assert_eq!(
            candidates,
            vec![
                (BundleProfileKind::Linux, linux.socket_path.clone()),
                (BundleProfileKind::Iced, iced.socket_path),
            ]
        );

        // probe_alive=false skips the dial, so the result is independent
        // of whether a UI happens to be running on the test host.
        let res = resolve_auto_detect(false)
            .await
            .expect("resolve must not be ambiguous when paths collapse");
        assert_eq!(res.kind, Some(BundleProfileKind::Linux));
        assert_eq!(res.socket_path, linux.socket_path);
    }

    #[test]
    fn chooser_names_every_live_candidate_in_stable_order() {
        let candidates = vec![
            (BundleProfileKind::Mac, PathBuf::from("/tmp/mac.sock")),
            (BundleProfileKind::Linux, PathBuf::from("/tmp/linux.sock")),
            (BundleProfileKind::Iced, PathBuf::from("/tmp/iced.sock")),
        ];
        let error = choose_live_candidate(&candidates, &[true, false, true])
            .expect_err("two live profiles must be ambiguous");
        match error {
            TargetError::Ambiguous { live } => {
                assert_eq!(
                    live,
                    LiveProfiles(vec![BundleProfileKind::Mac, BundleProfileKind::Iced])
                );
                assert_eq!(live.to_string(), "mac + iced");
            }
            other => panic!("expected ambiguity, got {other}"),
        }
    }

    #[test]
    fn chooser_reports_all_tried_paths_when_none_is_live() {
        let candidates = vec![
            (BundleProfileKind::Linux, PathBuf::from("/tmp/linux.sock")),
            (BundleProfileKind::Iced, PathBuf::from("/tmp/iced.sock")),
        ];
        let error = choose_live_candidate(&candidates, &[false, false])
            .expect_err("no live profiles must fail");
        match error {
            TargetError::NoLiveTarget { tried } => assert_eq!(
                tried,
                vec![
                    PathBuf::from("/tmp/linux.sock"),
                    PathBuf::from("/tmp/iced.sock")
                ]
            ),
            other => panic!("expected no-live-target, got {other}"),
        }
    }
}
