//! Who opened a tab (plan 085 D2, D6): the opener-set half of its
//! environment, the form `state.json` saves it in, and how a restore
//! decides it again.

use std::collections::BTreeMap;
use std::ffi::OsString;

use roost_ipc::opener::{
    check_local, check_remote, is_ssh_context, local_set_from, remote_value_from, OpenerValueError,
};
use serde::{Deserialize, Deserializer, Serialize};

/// What a tab was opened by. It decides the tab's opener-set variables
/// ([`roost_ipc::opener::OPENER_SET`]); everything else comes from the
/// baseline (`pty::tab_environment`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum TabOpener {
    /// No opener stated: the baseline alone.
    #[default]
    None,
    /// A local opener and the local-set values it brought. A name it lacks
    /// stays unset in the tab; an empty value is set empty.
    Local(BTreeMap<String, String>),
    /// A remote opener: its SSH identity, and nothing else of it.
    Remote {
        ssh_connection: Option<String>,
        ssh_client: Option<String>,
    },
}

impl TabOpener {
    /// The opener of a process started in `env`: remote with its SSH
    /// identity when `env` is an SSH context, else local with its
    /// local-set values. A value that [`local_set_from`] or
    /// [`remote_value_from`] refuses is left out.
    pub fn starter(env: &[(OsString, OsString)]) -> Self {
        if !is_ssh_context(env) {
            return Self::Local(local_set_from(env));
        }
        Self::Remote {
            ssh_connection: remote_value_from(env, "SSH_CONNECTION"),
            ssh_client: remote_value_from(env, "SSH_CLIENT"),
        }
    }

    /// The opener-set variables this opener puts in its tab's environment.
    pub fn vars(&self) -> Vec<(&str, &str)> {
        match self {
            Self::None => Vec::new(),
            Self::Local(values) => values
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect(),
            Self::Remote {
                ssh_connection,
                ssh_client,
            } => [
                ("SSH_CONNECTION", ssh_connection),
                ("SSH_CLIENT", ssh_client),
            ]
            .into_iter()
            .filter_map(|(name, value)| Some((name, value.as_deref()?)))
            .collect(),
        }
    }
}

/// A [`TabOpener`] as `state.json` saves it: `"none"`, `{"local":{…}}` or
/// `{"remote":{…}}`. Every tab is saved with one, so a tab with **no**
/// `opener` key was saved before there were openers (a *legacy* tab, which
/// [`restore_opener`] gives its caller's `legacy` opener).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PersistedOpener {
    None,
    Local(BTreeMap<String, String>),
    Remote(PersistedRemote),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PersistedRemote {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_connection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ssh_client: Option<String>,
}

impl PersistedOpener {
    /// D2's rules, which a saved entry is held to as a wire one is.
    pub fn check(&self) -> Result<(), OpenerValueError> {
        match self {
            Self::None => Ok(()),
            Self::Local(values) => values
                .iter()
                .try_for_each(|(name, value)| check_local(name, value)),
            Self::Remote(remote) => [
                ("ssh_connection", &remote.ssh_connection),
                ("ssh_client", &remote.ssh_client),
            ]
            .into_iter()
            .filter_map(|(name, value)| Some((name, value.as_deref()?)))
            .try_for_each(|(name, value)| check_remote(name, value)),
        }
    }
}

impl From<&TabOpener> for PersistedOpener {
    fn from(opener: &TabOpener) -> Self {
        match opener {
            TabOpener::None => Self::None,
            TabOpener::Local(values) => Self::Local(values.clone()),
            TabOpener::Remote {
                ssh_connection,
                ssh_client,
            } => Self::Remote(PersistedRemote {
                ssh_connection: ssh_connection.clone(),
                ssh_client: ssh_client.clone(),
            }),
        }
    }
}

/// `opener`'s `deserialize_with`: an entry that does not parse, or that
/// breaks [`PersistedOpener::check`], loads as `"none"` with a warning
/// rather than failing the whole file. An absent key (or `null`) stays
/// `None`: a legacy tab.
pub fn deserialize_saved<'de, D>(deserializer: D) -> Result<Option<PersistedOpener>, D::Error>
where
    D: Deserializer<'de>,
{
    let Some(raw) = Option::<serde_json::Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    let checked = serde_json::from_value::<PersistedOpener>(raw)
        .map_err(|error| error.to_string())
        .and_then(|opener| opener.check().map(|()| opener).map_err(|e| e.to_string()));
    Ok(Some(checked.unwrap_or_else(|error| {
        tracing::warn!(%error, "a saved tab opener is invalid; the tab restores with none");
        PersistedOpener::None
    })))
}

/// The opener a restored tab is re-opened with (D6).
///
/// A saved remote or `"none"` is kept. A saved local one takes `starter`'s
/// values when `starter` is local with at least one — a desk start knows
/// the desk's current displays and agent — and keeps its own otherwise: a
/// remote starter's describe a laptop, and a value-less local start (a
/// TTY, a headless `roostctl session start`) knows nothing of the desk. A
/// legacy tab (no saved opener) gets `legacy`.
pub fn restore_opener(
    saved: Option<&PersistedOpener>,
    starter: &TabOpener,
    legacy: &TabOpener,
) -> TabOpener {
    match saved {
        None => legacy.clone(),
        Some(PersistedOpener::None) => TabOpener::None,
        Some(PersistedOpener::Remote(remote)) => TabOpener::Remote {
            ssh_connection: remote.ssh_connection.clone(),
            ssh_client: remote.ssh_client.clone(),
        },
        Some(PersistedOpener::Local(saved)) => match starter {
            TabOpener::Local(values) if !values.is_empty() => starter.clone(),
            _ => TabOpener::Local(saved.clone()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    }

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn remote(connection: &str, client: &str) -> TabOpener {
        TabOpener::Remote {
            ssh_connection: Some(connection.into()),
            ssh_client: Some(client.into()),
        }
    }

    #[test]
    fn a_desk_started_process_is_a_local_starter_with_its_values() {
        let starter = TabOpener::starter(&env(&[
            ("DISPLAY", ":0"),
            ("SSH_AUTH_SOCK", "/run/agent"),
            ("PATH", "/bin"),
        ]));
        assert_eq!(
            starter,
            TabOpener::Local(map(&[("DISPLAY", ":0"), ("SSH_AUTH_SOCK", "/run/agent")]))
        );
    }

    #[test]
    fn an_ssh_started_process_is_a_remote_starter_with_its_identity_only() {
        let starter = TabOpener::starter(&env(&[
            ("SSH_CONNECTION", "10.0.0.2 51000 10.0.0.1 22"),
            ("SSH_CLIENT", "10.0.0.2 51000 22"),
            ("SSH_TTY", "/dev/pts/3"),
            ("SSH_AUTH_SOCK", "/tmp/ssh-x/agent.1"),
            ("DISPLAY", "localhost:10.0"),
        ]));
        assert_eq!(
            starter,
            remote("10.0.0.2 51000 10.0.0.1 22", "10.0.0.2 51000 22")
        );
    }

    #[test]
    fn ssh_tty_alone_makes_a_remote_starter_with_no_values() {
        let starter = TabOpener::starter(&env(&[("SSH_TTY", "/dev/pts/3"), ("DISPLAY", ":0")]));
        assert_eq!(
            starter,
            TabOpener::Remote {
                ssh_connection: None,
                ssh_client: None
            }
        );
    }

    #[test]
    fn an_oversized_remote_value_is_left_out_of_the_starter() {
        let long = "x".repeat(roost_ipc::opener::MAX_ORIGIN_VALUE + 1);
        let starter = TabOpener::starter(&env(&[
            ("SSH_CONNECTION", &long),
            ("SSH_CLIENT", "10.0.0.2 51000 22"),
        ]));
        assert_eq!(
            starter,
            TabOpener::Remote {
                ssh_connection: None,
                ssh_client: Some("10.0.0.2 51000 22".into())
            }
        );
    }

    #[test]
    fn each_opener_brings_exactly_its_own_variables() {
        assert_eq!(TabOpener::None.vars(), Vec::<(&str, &str)>::new());
        let local = TabOpener::Local(map(&[("DISPLAY", ":1"), ("SSH_AUTH_SOCK", "")]));
        assert_eq!(local.vars(), [("DISPLAY", ":1"), ("SSH_AUTH_SOCK", "")]);
        let half = TabOpener::Remote {
            ssh_connection: Some("a b c d".into()),
            ssh_client: None,
        };
        assert_eq!(half.vars(), [("SSH_CONNECTION", "a b c d")]);
        assert_eq!(
            remote("a b c d", "a b c").vars(),
            [("SSH_CONNECTION", "a b c d"), ("SSH_CLIENT", "a b c")]
        );
    }

    #[test]
    fn the_three_classes_save_in_their_documented_shapes() {
        let local = TabOpener::Local(map(&[("DISPLAY", ":1")]));
        let half = TabOpener::Remote {
            ssh_connection: Some("a b c d".into()),
            ssh_client: None,
        };
        for (opener, json) in [
            (TabOpener::None, serde_json::json!("none")),
            (local, serde_json::json!({"local": {"DISPLAY": ":1"}})),
            (
                remote("a b c d", "a b c"),
                serde_json::json!({"remote": {"ssh_connection": "a b c d", "ssh_client": "a b c"}}),
            ),
            (
                half,
                serde_json::json!({"remote": {"ssh_connection": "a b c d"}}),
            ),
        ] {
            let saved = PersistedOpener::from(&opener);
            assert_eq!(serde_json::to_value(&saved).unwrap(), json, "{opener:?}");
            let back: PersistedOpener = serde_json::from_value(json).unwrap();
            assert_eq!(back, saved);
            assert_eq!(
                restore_opener(Some(&back), &TabOpener::None, &TabOpener::None),
                opener,
                "a saved {opener:?} restores as itself under a starter that refreshes nothing"
            );
        }
    }

    #[derive(Deserialize)]
    struct Saved {
        #[serde(default, deserialize_with = "deserialize_saved")]
        opener: Option<PersistedOpener>,
    }

    fn load(json: &str) -> Option<PersistedOpener> {
        serde_json::from_str::<Saved>(json).unwrap().opener
    }

    #[test]
    fn no_key_is_a_legacy_tab() {
        assert_eq!(load("{}"), None);
        assert_eq!(load(r#"{"opener": null}"#), None);
    }

    #[test]
    fn an_invalid_saved_entry_loads_as_none() {
        let long = "x".repeat(roost_ipc::opener::MAX_ORIGIN_VALUE + 1);
        let too_long = format!(r#"{{"opener": {{"remote": {{"ssh_client": "{long}"}}}}}}"#);
        for json in [
            r#"{"opener": {"local": {"PATH": "/bin"}}}"#,
            r#"{"opener": {"local": {"SSH_CONNECTION": "a b c d"}}}"#,
            r#"{"opener": {"local": {"DISPLAY": ":0\u0000"}}}"#,
            r#"{"opener": {"remote": {"ssh_connection": "a", "ssh_tty": "/dev/pts/1"}}}"#,
            &too_long,
            r#"{"opener": "local"}"#,
            r#"{"opener": {"elsewhere": {}}}"#,
            r#"{"opener": 7}"#,
        ] {
            assert_eq!(load(json), Some(PersistedOpener::None), "{json}");
        }
        assert_eq!(
            load(r#"{"opener": {"local": {"DISPLAY": ":0"}}}"#),
            Some(PersistedOpener::Local(map(&[("DISPLAY", ":0")]))),
            "the control: a valid entry loads as itself"
        );
    }

    #[test]
    fn a_saved_remote_is_kept_under_every_starter() {
        let saved = PersistedOpener::from(&remote("a b c d", "a b c"));
        for starter in [
            TabOpener::None,
            TabOpener::Local(map(&[("DISPLAY", ":9")])),
            remote("e f g h", "e f g"),
        ] {
            assert_eq!(
                restore_opener(Some(&saved), &starter, &TabOpener::None),
                remote("a b c d", "a b c"),
                "{starter:?}"
            );
        }
    }

    #[test]
    fn a_saved_none_is_kept_whatever_legacy_says() {
        let desk = TabOpener::Local(map(&[("DISPLAY", ":9")]));
        for legacy in [TabOpener::None, desk.clone()] {
            assert_eq!(
                restore_opener(Some(&PersistedOpener::None), &desk, &legacy),
                TabOpener::None,
                "{legacy:?}"
            );
        }
    }

    #[test]
    fn a_legacy_tab_gets_the_legacy_opener() {
        let desk = TabOpener::Local(map(&[("DISPLAY", ":9")]));
        assert_eq!(
            restore_opener(None, &desk, &TabOpener::None),
            TabOpener::None
        );
        assert_eq!(restore_opener(None, &TabOpener::None, &desk), desk);
    }

    #[test]
    fn a_saved_local_takes_a_local_starters_values_when_it_has_any() {
        let saved = PersistedOpener::Local(map(&[("DISPLAY", ":1"), ("SSH_AUTH_SOCK", "/old")]));
        let desk = TabOpener::Local(map(&[("WAYLAND_DISPLAY", "/run/user/1/wayland-0")]));
        assert_eq!(
            restore_opener(Some(&saved), &desk, &TabOpener::None),
            desk,
            "the whole set is replaced, not merged"
        );
    }

    #[test]
    fn a_saved_local_keeps_its_values_under_a_remote_or_value_less_starter() {
        let values = map(&[("DISPLAY", ":1"), ("SSH_AUTH_SOCK", "/old")]);
        let saved = PersistedOpener::Local(values.clone());
        for starter in [
            remote("a b c d", "a b c"),
            TabOpener::Local(BTreeMap::new()),
            TabOpener::None,
        ] {
            assert_eq!(
                restore_opener(Some(&saved), &starter, &TabOpener::None),
                TabOpener::Local(values.clone()),
                "{starter:?}"
            );
        }
    }
}
