//! How a `roostctl` verb fails: one type, one exit code per kind, and the
//! one place a failure becomes bytes on stderr.
//!
//! The `code` strings and exit codes are a contract scripts and the agent
//! skill branch on — `docs/reference/cli.md#exit-codes` is their table, and
//! the tests below pin every row of it.

use roost_ipc::target::TargetError;
use roost_ipc::ClientError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// A bad command line — including a mutating verb given no tab.
    Usage(String),
    /// Auto-detect found nothing listening at any known socket.
    NoTarget(String),
    /// Auto-detect found several UIs.
    AmbiguousTarget(String),
    /// A dial, read or write failed, the stream dropped, or a call went
    /// unanswered for `wait`'s per-call ceiling.
    Connection(String),
    /// The server refused; `code` is the server's own, verbatim.
    Server { code: String, message: String },
    /// The server does not serve an op the verb needs — `open` and
    /// `project ensure` when `identify.ops` lacks `project.ensure`
    /// (plan 066 §3.2).
    Unsupported(String),
    /// `doctor` found a failing check. The report is on stdout.
    ChecksFailed(String),
    /// `session status` found no session running.
    NotRunning(String),
    /// `wait`'s condition did not hold in time.
    Timeout(String),
    /// Something on this machine, outside the wire: a file that could not
    /// be written, a binary that could not be found, an unset `$HOME`.
    Failed(String),
}

impl CliError {
    pub fn exit_code(&self) -> i32 {
        match self {
            Self::Usage(_) => 2,
            Self::NotRunning(_) => crate::session::STATUS_NOT_RUNNING_EXIT,
            Self::Timeout(_) => 4,
            Self::NoTarget(_)
            | Self::AmbiguousTarget(_)
            | Self::Connection(_)
            | Self::Server { .. }
            | Self::Unsupported(_)
            | Self::ChecksFailed(_)
            | Self::Failed(_) => 1,
        }
    }

    pub fn code(&self) -> &str {
        match self {
            Self::Usage(_) => "usage",
            Self::NoTarget(_) => "no-target",
            Self::AmbiguousTarget(_) => "ambiguous-target",
            Self::Connection(_) => "connection",
            Self::Server { code, .. } => code,
            Self::Unsupported(_) => "unsupported",
            Self::ChecksFailed(_) => "checks-failed",
            Self::NotRunning(_) => "not-running",
            Self::Timeout(_) => "timeout",
            Self::Failed(_) => "failed",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Usage(message)
            | Self::NoTarget(message)
            | Self::AmbiguousTarget(message)
            | Self::Connection(message)
            | Self::Server { message, .. }
            | Self::Unsupported(message)
            | Self::ChecksFailed(message)
            | Self::NotRunning(message)
            | Self::Timeout(message)
            | Self::Failed(message) => message,
        }
    }

    /// Exactly what `roostctl` writes to stderr for this failure.
    pub fn render(&self, json: bool) -> String {
        if json {
            let envelope = serde_json::json!({
                "error": { "code": self.code(), "message": self.message() }
            });
            format!("{envelope}\n")
        } else {
            format!("roostctl: {}: {}\n", self.code(), self.message())
        }
    }

    /// A command line clap refused. The message keeps clap's usage text:
    /// for a parser error that text *is* the help the reader needs.
    pub fn from_clap(error: &clap::Error) -> Self {
        let rendered = error.render().to_string();
        let message = rendered.strip_prefix("error: ").unwrap_or(&rendered);
        Self::Usage(message.trim_end().to_string())
    }

    /// `{:#}` so an `anyhow::Error` keeps its cause chain; every other
    /// `Display` renders the same either way.
    pub fn failed(error: impl std::fmt::Display) -> Self {
        Self::Failed(format!("{error:#}"))
    }

    pub fn connection(error: impl std::fmt::Display) -> Self {
        Self::Connection(format!("{error:#}"))
    }
}

impl From<ClientError> for CliError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Server { code, message } => Self::Server { code, message },
            // The answer arrived and did not decode: a skewed server, not
            // a dropped one, so retrying the connection cannot fix it.
            ClientError::Protocol(_) => Self::Failed(error.to_string()),
            ClientError::Io(_)
            | ClientError::IdMismatch { .. }
            | ClientError::Disconnected
            | ClientError::RevisionGap { .. } => Self::Connection(error.to_string()),
        }
    }
}

impl From<TargetError> for CliError {
    fn from(error: TargetError) -> Self {
        match error {
            TargetError::NoLiveTarget { .. } => Self::NoTarget(error.to_string()),
            TargetError::Ambiguous { .. } => Self::AmbiguousTarget(error.to_string()),
            TargetError::UnknownProfile(_) => Self::Usage(error.to_string()),
            TargetError::Path(_) => Self::Failed(error.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::paths::BundleProfileKind;
    use roost_ipc::target::LiveProfiles;

    /// One row of `docs/reference/cli.md#exit-codes`, checked in both
    /// renderings: the exit code, the code, and that the message rides
    /// along unchanged.
    fn assert_row(error: &CliError, exit: i32, code: &str) {
        assert_eq!(error.exit_code(), exit, "{error:?}");
        assert_eq!(error.code(), code, "{error:?}");

        let text = error.render(false);
        assert_eq!(
            text,
            format!("roostctl: {code}: {}\n", error.message()),
            "{error:?}"
        );
        assert!(
            !text.contains(&format!("{error:?}")),
            "the Debug dump leaked into the human form: {text}"
        );

        let json = error.render(true);
        assert!(
            json.ends_with('\n') && json.matches('\n').count() == 1,
            "{json:?}"
        );
        let parsed: serde_json::Value = serde_json::from_str(&json).expect("one JSON document");
        assert_eq!(
            parsed,
            serde_json::json!({"error": {"code": code, "message": error.message()}})
        );
    }

    #[test]
    fn usage_exits_2() {
        assert_row(&CliError::Usage("bad".into()), 2, "usage");
    }

    #[test]
    fn a_clap_error_is_usage_and_keeps_its_usage_text() {
        let error = clap::Command::new("roostctl")
            .subcommand(clap::Command::new("identify"))
            .try_get_matches_from(["roostctl", "identify", "--bogus"])
            .expect_err("an unknown flag is refused");
        let cli = CliError::from_clap(&error);
        assert_row(&cli, 2, "usage");
        assert!(
            cli.message().starts_with("unexpected argument '--bogus'"),
            "{cli:?}"
        );
        assert!(
            cli.message().contains("Usage: roostctl identify"),
            "{cli:?}"
        );
    }

    #[test]
    fn no_live_target_is_no_target() {
        let error = CliError::from(TargetError::NoLiveTarget {
            tried: vec!["/tmp/nothing.sock".into()],
        });
        assert_row(&error, 1, "no-target");
        assert!(error.message().contains("/tmp/nothing.sock"), "{error:?}");
    }

    #[test]
    fn several_live_targets_is_ambiguous_target() {
        let error = CliError::from(TargetError::Ambiguous {
            live: LiveProfiles(vec![BundleProfileKind::Mac, BundleProfileKind::Iced]),
        });
        assert_row(&error, 1, "ambiguous-target");
        assert!(error.message().contains("--target"), "{error:?}");
    }

    #[test]
    fn a_transport_failure_is_connection() {
        for client in [
            ClientError::Disconnected,
            ClientError::Io(roost_ipc::Error::Io(std::io::Error::from(
                std::io::ErrorKind::ConnectionRefused,
            ))),
            ClientError::IdMismatch {
                expected: 1,
                got: 2,
            },
            ClientError::RevisionGap {
                expected: 3,
                got: 5,
            },
        ] {
            assert_row(&CliError::from(client), 1, "connection");
        }
    }

    #[test]
    fn a_server_refusal_keeps_its_code_and_message_verbatim() {
        let error = CliError::from(ClientError::Server {
            code: "not-found".into(),
            message: "no tab 99".into(),
        });
        assert_eq!(
            error,
            CliError::Server {
                code: "not-found".into(),
                message: "no tab 99".into()
            }
        );
        assert_row(&error, 1, "not-found");
        assert_eq!(error.render(false), "roostctl: not-found: no tab 99\n");
    }

    #[test]
    fn unsupported_exits_1() {
        assert_row(
            &CliError::Unsupported("no project.ensure".into()),
            1,
            "unsupported",
        );
    }

    #[test]
    fn checks_failed_exits_1() {
        assert_row(
            &CliError::ChecksFailed("2 checks failed".into()),
            1,
            "checks-failed",
        );
    }

    #[test]
    fn not_running_exits_with_the_session_status_code() {
        assert_row(&CliError::NotRunning("no session".into()), 3, "not-running");
        assert_eq!(crate::session::STATUS_NOT_RUNNING_EXIT, 3);
    }

    #[test]
    fn timeout_exits_4() {
        assert_row(&CliError::Timeout("5s".into()), 4, "timeout");
    }

    #[test]
    fn a_local_failure_is_failed() {
        assert_row(&CliError::failed("$HOME not set"), 1, "failed");
        let undecodable = serde_json::from_str::<u8>("x").unwrap_err();
        assert_row(
            &CliError::from(ClientError::Protocol(roost_ipc::Error::Parse(undecodable))),
            1,
            "failed",
        );
        assert_row(
            &CliError::from(TargetError::Path(anyhow::anyhow!("no runtime dir"))),
            1,
            "failed",
        );
    }

    #[test]
    fn an_unknown_profile_is_usage() {
        assert_row(
            &CliError::from(TargetError::UnknownProfile("session".into())),
            2,
            "usage",
        );
    }
}
