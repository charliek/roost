//! `roostctl` inside a local session tab talks to the window that owns the
//! session (plan 072 D3, #561).
//!
//! A local `roost-session` hands its tabs its own socket as `ROOST_SOCKET`,
//! which outranks every other way of finding a Roost. A session has no
//! palette, no screenshot and no window to move, so a verb run in such a
//! tab would reach the one process that cannot do what it asks. When the
//! socket came from the environment, nothing on the command line named a
//! target, and the socket is this machine's local session, [`follow`] asks
//! each running window which local session it owns and hands the verb to
//! the one that owns this one. Ids need no mapping: a bare id on a window's
//! socket under `session` already means the slot's.

use std::path::{Path, PathBuf};
use std::time::Duration;

use roost_ipc::session_launch::timeout_scale;
use roost_ipc::target::{TargetDiagnosis, TargetEnv, TargetOrigin, TargetSelector};
use roost_ipc::IpcClient;

use crate::error::CliError;

/// How long one window has to say which session it owns. One that hasn't
/// answered by then owns nothing, so a hung window cannot hang the verb.
const CLAIM_BUDGET: Duration = Duration::from_millis(500);

/// What target selection reads from the process besides the flags.
#[derive(Debug, Default, Clone)]
pub(crate) struct Environment {
    pub target: TargetEnv,
    pub windows: Windows,
}

impl Environment {
    pub fn from_process() -> Self {
        Self {
            target: TargetEnv::from_process(),
            windows: Windows::Profiles,
        }
    }
}

/// Where [`follow`] looks for the window that owns a session.
#[derive(Debug, Default, Clone)]
pub(crate) enum Windows {
    /// This machine's: the session a window owns is
    /// [`roost_ipc::session_socket_path`], and the windows are the UI
    /// sockets auto-detect probes. Resolved only when the socket is
    /// [`inherited`].
    #[default]
    Profiles,
    /// A test's.
    #[cfg(test)]
    Given {
        session: PathBuf,
        sockets: Vec<PathBuf>,
    },
}

/// More than one window claims the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ambiguous(pub Vec<PathBuf>);

impl std::fmt::Display for Ambiguous {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let sockets: Vec<String> = self.0.iter().map(|p| p.display().to_string()).collect();
        write!(
            f,
            "more than one Roost window uses this session ({}); pass --socket",
            sockets.join(", ")
        )
    }
}

/// Whether the ladder's socket is the calling tab's own `ROOST_SOCKET`,
/// with nothing on the command line naming another. `ROOST_SOCKET`
/// outranks `--target` on the ladder, so the winning origin alone cannot
/// say that `--target` was given.
pub(crate) fn inherited(selector: &TargetSelector, origin: TargetOrigin) -> bool {
    origin == TargetOrigin::SocketEnv
        && selector.socket_override.is_none()
        && selector.kind_override.is_none()
}

/// The socket a verb dials: where the ladder points, or — for a verb that
/// `follows_window` — the window that owns the local session it points at.
pub(crate) async fn route(
    selector: &TargetSelector,
    environment: &Environment,
    follows_window: bool,
) -> Result<PathBuf, CliError> {
    let diagnosis = selector.diagnose_in(&environment.target).await;
    if follows_window {
        let window = follow(selector, environment, &diagnosis)
            .await
            .map_err(|ambiguous| CliError::Usage(ambiguous.to_string()))?;
        if let Some(window) = window {
            return Ok(window);
        }
    }
    Ok(diagnosis.resolved?.socket_path)
}

/// The window that owns the session `diagnosis` resolved to, when the
/// socket is [`inherited`] and is this machine's local session; `None`
/// keeps the ladder's answer.
pub(crate) async fn follow(
    selector: &TargetSelector,
    environment: &Environment,
    diagnosis: &TargetDiagnosis,
) -> Result<Option<PathBuf>, Ambiguous> {
    let Ok(resolved) = &diagnosis.resolved else {
        return Ok(None);
    };
    if !inherited(selector, diagnosis.origin) {
        return Ok(None);
    }
    let (session, windows) = match &environment.windows {
        Windows::Profiles => {
            let Some(session) = roost_ipc::session_socket_path() else {
                return Ok(None);
            };
            let windows = diagnosis
                .candidates
                .iter()
                .map(|(_, socket)| socket.clone())
                .collect();
            (PathBuf::from(session), windows)
        }
        #[cfg(test)]
        Windows::Given { session, sockets } => (session.clone(), sockets.clone()),
    };
    if resolved.socket_path != session {
        return Ok(None);
    }
    let claims = claims(windows, CLAIM_BUDGET.mul_f64(timeout_scale())).await;
    choose_ui(&session, &claims)
}

/// The window that owns `session_socket`: the one candidate whose
/// `identify.local_session_socket` names it.
///
/// Each candidate is a window's socket and the session it claimed, `None`
/// when it isn't listening, didn't answer in time, or runs its tabs
/// in-process. No claimant keeps today's route.
pub(crate) fn choose_ui(
    session_socket: &Path,
    candidates: &[(PathBuf, Option<PathBuf>)],
) -> Result<Option<PathBuf>, Ambiguous> {
    let mut owners: Vec<PathBuf> = candidates
        .iter()
        .filter(|(_, claimed)| claimed.as_deref() == Some(session_socket))
        .map(|(window, _)| window.clone())
        .collect();
    match owners.len() {
        0 | 1 => Ok(owners.pop()),
        _ => Err(Ambiguous(owners)),
    }
}

/// Every window's claim, asked concurrently, each within `budget`.
async fn claims(windows: Vec<PathBuf>, budget: Duration) -> Vec<(PathBuf, Option<PathBuf>)> {
    let asked: Vec<_> = windows
        .into_iter()
        .map(|window| {
            tokio::spawn(async move {
                let claimed = tokio::time::timeout(budget, claim(&window))
                    .await
                    .ok()
                    .flatten();
                (window, claimed)
            })
        })
        .collect();
    let mut claims = Vec::with_capacity(asked.len());
    for answer in asked {
        if let Ok(claim) = answer.await {
            claims.push(claim);
        }
    }
    claims
}

async fn claim(window: &Path) -> Option<PathBuf> {
    let mut client = IpcClient::connect(window).await.ok()?;
    crate::identify(&mut client)
        .await
        .ok()?
        .local_session_socket
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "/run/roost-session/roost.sock";

    fn claimed(window: &str, session: Option<&str>) -> (PathBuf, Option<PathBuf>) {
        (PathBuf::from(window), session.map(PathBuf::from))
    }

    #[test]
    fn the_one_window_that_claims_the_session_is_chosen() {
        let candidates = [
            claimed("/run/roost/roost.sock", None),
            claimed("/run/roost-iced/roost.sock", Some(SESSION)),
        ];
        assert_eq!(
            choose_ui(Path::new(SESSION), &candidates),
            Ok(Some(PathBuf::from("/run/roost-iced/roost.sock")))
        );
    }

    #[test]
    fn no_claimant_keeps_the_route() {
        assert_eq!(choose_ui(Path::new(SESSION), &[]), Ok(None));
        let silent = [
            claimed("/run/roost/roost.sock", None),
            claimed("/run/roost-iced/roost.sock", None),
        ];
        assert_eq!(choose_ui(Path::new(SESSION), &silent), Ok(None));
    }

    #[test]
    fn two_claimants_are_ambiguous_and_named() {
        let candidates = [
            claimed("/run/roost/roost.sock", Some(SESSION)),
            claimed("/run/roost-iced/roost.sock", Some(SESSION)),
        ];
        let ambiguous = choose_ui(Path::new(SESSION), &candidates).expect_err("two claim it");
        assert_eq!(
            ambiguous,
            Ambiguous(vec![
                PathBuf::from("/run/roost/roost.sock"),
                PathBuf::from("/run/roost-iced/roost.sock"),
            ])
        );
        assert_eq!(
            ambiguous.to_string(),
            "more than one Roost window uses this session \
             (/run/roost/roost.sock, /run/roost-iced/roost.sock); pass --socket"
        );
    }

    /// A window on another session is running, answering, and on
    /// `session` — just not this one.
    #[test]
    fn a_window_claiming_another_session_is_a_decoy() {
        let candidates = [
            claimed("/run/roost/roost.sock", Some("/elsewhere/roost.sock")),
            claimed("/run/roost-iced/roost.sock", Some(SESSION)),
        ];
        assert_eq!(
            choose_ui(Path::new(SESSION), &candidates),
            Ok(Some(PathBuf::from("/run/roost-iced/roost.sock")))
        );
        let only_the_decoy = [claimed(
            "/run/roost/roost.sock",
            Some("/elsewhere/roost.sock"),
        )];
        assert_eq!(choose_ui(Path::new(SESSION), &only_the_decoy), Ok(None));
    }

    /// A window that accepts and never answers claims nothing once its
    /// budget is out, and costs no more than that budget: the window that
    /// did answer is chosen alone.
    #[tokio::test]
    async fn a_window_that_does_not_answer_in_time_claims_nothing() {
        let (hung, holding) = crate::tests::hung_window("hung-claim");
        let owner = crate::events::fake::Fake::ui("owner-claim");
        owner.with(|world| world.identify["local_session_socket"] = SESSION.into());

        let budget = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let answers = claims(vec![hung.clone(), owner.socket.clone()], budget).await;
        let took = started.elapsed();
        holding.abort();

        assert_eq!(
            answers,
            [
                (hung, None),
                (owner.socket.clone(), Some(PathBuf::from(SESSION)))
            ]
        );
        assert!(took < budget * 10, "{took:?}");
        assert_eq!(
            choose_ui(Path::new(SESSION), &answers),
            Ok(Some(owner.socket.clone()))
        );
    }

    #[test]
    fn only_the_tabs_own_socket_with_no_selector_is_inherited() {
        let bare = TargetSelector::default();
        assert!(inherited(&bare, TargetOrigin::SocketEnv));
        for origin in [
            TargetOrigin::SocketFlag,
            TargetOrigin::TargetFlag,
            TargetOrigin::ProfileEnv,
            TargetOrigin::AutoDetect,
        ] {
            assert!(!inherited(&bare, origin), "{origin:?}");
        }
        let with_target = TargetSelector {
            socket_override: None,
            kind_override: Some(roost_ipc::paths::BundleProfileKind::Session),
        };
        assert!(!inherited(&with_target, TargetOrigin::SocketEnv));
        let with_socket = TargetSelector {
            socket_override: Some(PathBuf::from("/tmp/named.sock")),
            kind_override: None,
        };
        assert!(!inherited(&with_socket, TargetOrigin::SocketEnv));
    }
}
