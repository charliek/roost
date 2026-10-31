//! Who `tab open` and `open` say opened their tab (plan 085 D7): this
//! roostctl, read from its own environment, so the tab gets the caller's
//! display and agent, or its SSH identity, rather than the server's.

use std::collections::BTreeMap;
use std::ffi::OsString;

use roost_ipc::codes;
use roost_ipc::messages::{TabOpenParams, TabOrigin};
use roost_ipc::opener::{is_ssh_context, local_set_from, remote_value_from};

/// roostctl as a tab's opener.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Opener {
    /// Run at the machine: the display, bus and agent it has, stated
    /// even when it has none.
    Local(BTreeMap<String, String>),
    /// Run in an SSH login, through whatever socket it dials: that
    /// login's identity.
    Remote(TabOrigin),
}

impl Default for Opener {
    fn default() -> Self {
        Self::Local(BTreeMap::new())
    }
}

impl Opener {
    pub(crate) fn from_env(env: &[(OsString, OsString)]) -> Self {
        if is_ssh_context(env) {
            return Self::Remote(TabOrigin {
                ssh_connection: remote_value_from(env, "SSH_CONNECTION"),
                ssh_client: remote_value_from(env, "SSH_CLIENT"),
            });
        }
        Self::Local(local_set_from(env))
    }

    /// State this opener on `params`: `env` for a local one, `origin`
    /// for a remote one.
    pub(crate) fn state(&self, params: &mut TabOpenParams) {
        match self {
            Self::Local(values) => params.env = Some(values.clone()),
            Self::Remote(origin) => params.origin = Some(origin.clone()),
        }
    }
}

/// Whether a `tab.open` refusal is a server that does not know `env` or
/// `origin` — Roost.app reached by `--socket`, or a session from before
/// either field. Decided by the field the server names as refused: Swift
/// lists every unknown key (`unknown params: a, env`), serde names the
/// first (``unknown field `env`, expected one of …``) and then lists the
/// fields it knows, `origin` among them, so a substring match would also
/// take a refusal of a key *inside* `origin` (``unknown field `ssh_tty` ``)
/// or of any other field for this one.
pub(crate) fn refuses_opener_fields(code: &str, message: &str) -> bool {
    if code != codes::UNKNOWN_FIELD {
        return false;
    }
    let opener_field = |name: &str| name == "env" || name == "origin";
    if let Some(names) = message.strip_prefix("unknown params: ") {
        return names.split(", ").any(opener_field);
    }
    message
        .strip_prefix("unknown field `")
        .and_then(|rest| rest.split_once('`'))
        .is_some_and(|(name, _)| opener_field(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn at_the_machine_roostctl_is_a_local_opener_with_its_values() {
        let opener = Opener::from_env(&env(&[
            ("DISPLAY", ":1"),
            ("WAYLAND_DISPLAY", "wayland-2"),
            ("XDG_RUNTIME_DIR", "/run/user/7"),
            ("SSH_AUTH_SOCK", ""),
            ("PATH", "/bin"),
        ]));
        let expected: BTreeMap<String, String> = [
            ("DISPLAY", ":1"),
            ("WAYLAND_DISPLAY", "/run/user/7/wayland-2"),
            ("SSH_AUTH_SOCK", ""),
        ]
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .into();
        assert_eq!(opener, Opener::Local(expected));
        assert_eq!(
            Opener::from_env(&[]),
            Opener::Local(BTreeMap::new()),
            "an environment with none of the values is still a local opener"
        );
    }

    #[test]
    fn in_an_ssh_login_roostctl_is_a_remote_opener_with_its_identity() {
        let opener = Opener::from_env(&env(&[
            ("SSH_CONNECTION", "10.0.0.2 5 10.0.0.1 22"),
            ("SSH_CLIENT", "10.0.0.2 5 22"),
            ("SSH_TTY", "/dev/pts/3"),
            ("DISPLAY", "localhost:10.0"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"),
        ]));
        assert_eq!(
            opener,
            Opener::Remote(TabOrigin {
                ssh_connection: Some("10.0.0.2 5 10.0.0.1 22".into()),
                ssh_client: Some("10.0.0.2 5 22".into()),
            })
        );
        assert_eq!(
            Opener::from_env(&env(&[("SSH_TTY", "/dev/pts/3"), ("DISPLAY", ":0")])),
            Opener::Remote(TabOrigin::default()),
            "SSH_TTY alone makes an SSH login, whose identity is then unknown"
        );
    }

    #[test]
    fn stating_an_opener_sets_exactly_its_own_field() {
        let mut local = TabOpenParams::default();
        Opener::default().state(&mut local);
        assert_eq!((local.env, local.origin), (Some(BTreeMap::new()), None));
        let mut remote = TabOpenParams::default();
        Opener::Remote(TabOrigin::default()).state(&mut remote);
        assert_eq!(
            (remote.env, remote.origin),
            (None, Some(TabOrigin::default()))
        );
    }

    #[test]
    fn only_a_refusal_naming_env_or_origin_is_about_the_opener() {
        let refused = [
            "unknown params: env",
            "unknown params: origin",
            "unknown params: activate, env",
            "unknown field `env`, expected one of `project_id`, `cwd`, `origin`",
            "unknown field `origin`, expected one of `project_id`, `env`",
        ];
        for message in refused {
            assert!(
                refuses_opener_fields(codes::UNKNOWN_FIELD, message),
                "{message}"
            );
        }
        let other = [
            "unknown params: activate",
            "unknown params: environment",
            "unknown field `activate`, expected one of `project_id`, `env`, `origin`",
            "unknown field `ssh_tty`, expected `ssh_connection` or `ssh_client`",
            "unknown field `envy`, expected one of `env`",
        ];
        for message in other {
            assert!(
                !refuses_opener_fields(codes::UNKNOWN_FIELD, message),
                "{message}"
            );
        }
        assert!(
            !refuses_opener_fields(codes::INVALID_PARAM, "unknown params: env"),
            "only an unknown-field refusal"
        );
    }
}
