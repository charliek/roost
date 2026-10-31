pub mod mirror;
pub mod queue;
pub mod reconnect;
pub mod state;
pub mod task;
pub mod upload;

/// Why reaching a host failed: the classified family when the far side
/// is what refused, and the line the band and the toast render.
///
/// The pair travels together on purpose. The classifier
/// ([`roost_ipc::ssh::classify_ssh_failure`]) exists precisely so a
/// caller can route on "the binary is not installed over there" without
/// matching substrings of user-facing copy — but the copy is what the
/// UI shows, so collapsing to one or the other loses something. The
/// bootstrap offer and the retry ladder both branch on
/// [`Self::family`] — the ladder on [`Self::truncated`] as well, which
/// is why the two travel together — while the band and the toast render
/// [`Self::message`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFailure {
    /// `None` when the failure was this side's — a scratch directory
    /// that could not be made, a target string that means nothing, a
    /// socket that would not answer — which has no family and no remedy
    /// on the far host.
    pub family: Option<roost_ipc::ssh::SshFailure>,
    /// Whether the stderr [`Self::family`] was classified from was
    /// incomplete — a drain that ran out of time, or a byte cap that
    /// discarded the leading bytes. `false` for a failure whose verdict
    /// was not read out of a tail at all (this side's own failure, an
    /// establish that timed out). It is here so a retry policy can
    /// refuse to act on evidence it did not fully see.
    pub truncated: bool,
    pub message: String,
}

impl ConnectFailure {
    /// A failure with no far-side family: this client's own, or a
    /// transport that has no classifier.
    pub fn unclassified(message: impl Into<String>) -> Self {
        Self {
            family: None,
            truncated: false,
            message: message.into(),
        }
    }

    /// A classified far-side failure, rendered for `target`.
    pub fn classified(target: &str, failure: roost_ipc::ssh::SshFailure) -> Self {
        Self {
            message: failure.message(target),
            family: Some(failure),
            truncated: false,
        }
    }
}

/// A bare message with no family — what a panicked or cancelled engine
/// op reports (`spawn_engine_op`), and the shape every other op's error
/// already has.
impl From<String> for ConnectFailure {
    fn from(message: String) -> Self {
        Self::unclassified(message)
    }
}

impl From<roost_ipc::ssh::SshTunnelError> for ConnectFailure {
    fn from(error: roost_ipc::ssh::SshTunnelError) -> Self {
        Self {
            family: error.failure().cloned(),
            truncated: error.truncated(),
            message: error.to_string(),
        }
    }
}

impl From<roost_ipc::ssh::VerifyError> for ConnectFailure {
    fn from(error: roost_ipc::ssh::VerifyError) -> Self {
        Self {
            family: error.failure().cloned(),
            truncated: error.truncated(),
            // `{:#}` is what both callers printed before this type
            // existed — the anyhow chain on the socket arm, the ssh
            // error's own message on the other.
            message: format!("{error:#}"),
        }
    }
}
