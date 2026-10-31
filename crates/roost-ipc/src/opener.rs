//! The variables a tab's opener decides (plan 085 D2), and the rules every
//! value of one is held to: the engine reads them to build a tab's
//! environment, a session to derive its starter opener, and a client to
//! say what it brings.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::path::Path;

/// Every variable a tab gets from its opener and never from the
/// environment of the process serving it.
pub const OPENER_SET: [&str; 7] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
    "SSH_CONNECTION",
    "SSH_CLIENT",
    "SSH_TTY",
];

/// What a local opener may bring: the desk's displays, bus and agent.
pub const LOCAL_SET: [&str; 4] = [
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "DBUS_SESSION_BUS_ADDRESS",
    "SSH_AUTH_SOCK",
];

/// Any of these set makes an environment an SSH context.
const SSH_CONTEXT: [&str; 3] = ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"];

/// The longest local value accepted, in bytes.
pub const MAX_LOCAL_VALUE: usize = 4096;

/// The longest remote (`SSH_*`) value accepted, in bytes — also the most
/// of each a bridge preamble carries, so its line always fits the
/// session's [`crate::messages::MAX_BRIDGE_PREAMBLE`] even when every
/// byte has to be escaped.
pub const MAX_ORIGIN_VALUE: usize = 256;

/// Why an opener's name or value was refused. Names the variable, never
/// its value: a value is a socket path or a client address.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OpenerValueError {
    #[error("`{0}` is not a local opener variable ({})", LOCAL_SET.join(", "))]
    NotLocal(String),
    #[error("`{0}` contains a NUL byte")]
    Nul(String),
    #[error("`{name}` is {len} bytes, over the {max}-byte limit")]
    TooLong {
        name: String,
        len: usize,
        max: usize,
    },
}

/// One local opener entry: a [`LOCAL_SET`] name, and a value with no NUL
/// within [`MAX_LOCAL_VALUE`] bytes.
pub fn check_local(name: &str, value: &str) -> Result<(), OpenerValueError> {
    if !LOCAL_SET.contains(&name) {
        return Err(OpenerValueError::NotLocal(name.to_owned()));
    }
    check_value(name, value, MAX_LOCAL_VALUE)
}

/// One remote value, `name` saying which: no NUL, within
/// [`MAX_ORIGIN_VALUE`] bytes.
pub fn check_remote(name: &str, value: &str) -> Result<(), OpenerValueError> {
    check_value(name, value, MAX_ORIGIN_VALUE)
}

fn check_value(name: &str, value: &str, max: usize) -> Result<(), OpenerValueError> {
    if value.contains('\0') {
        return Err(OpenerValueError::Nul(name.to_owned()));
    }
    if value.len() > max {
        return Err(OpenerValueError::TooLong {
            name: name.to_owned(),
            len: value.len(),
            max,
        });
    }
    Ok(())
}

/// Whether `env` is an SSH login's: any of `SSH_CONNECTION`, `SSH_CLIENT`
/// or `SSH_TTY` set. The one answer to "is this caller remote?".
pub fn is_ssh_context(env: &[(OsString, OsString)]) -> bool {
    SSH_CONTEXT.iter().any(|name| lookup(env, name).is_some())
}

/// The [`LOCAL_SET`] values `env` carries. A value that is not UTF-8, or
/// that [`check_local`] refuses, is skipped. A relative `WAYLAND_DISPLAY`
/// is made absolute against `env`'s own `XDG_RUNTIME_DIR`: a tab's runtime
/// dir is derived again (D3), so a relative name would resolve against
/// another directory than the opener's.
pub fn local_set_from(env: &[(OsString, OsString)]) -> BTreeMap<String, String> {
    LOCAL_SET
        .iter()
        .filter_map(|&name| {
            let value = lookup(env, name)?.to_str()?.to_owned();
            let value = if name == "WAYLAND_DISPLAY" {
                absolute_wayland_display(value, lookup(env, "XDG_RUNTIME_DIR"))
            } else {
                value
            };
            check_local(name, &value).ok()?;
            Some((name.to_owned(), value))
        })
        .collect()
}

/// `env`'s `name`, as a remote value: absent when it is not UTF-8 or
/// [`check_remote`] refuses it.
pub fn remote_value_from(env: &[(OsString, OsString)], name: &str) -> Option<String> {
    let value = lookup(env, name)?.to_str()?;
    check_remote(name, value).ok()?;
    Some(value.to_owned())
}

fn absolute_wayland_display(display: String, runtime_dir: Option<&OsStr>) -> String {
    if display.is_empty() || Path::new(&display).is_absolute() {
        return display;
    }
    match runtime_dir.map(Path::new).filter(|dir| dir.is_absolute()) {
        Some(dir) => dir
            .join(&display)
            .into_os_string()
            .into_string()
            .unwrap_or(display),
        None => display,
    }
}

fn lookup<'a>(env: &'a [(OsString, OsString)], name: &str) -> Option<&'a OsStr> {
    env.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.as_os_str())
}

#[cfg(test)]
mod tests {
    use std::os::unix::ffi::OsStringExt;

    use super::*;

    fn env(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    #[test]
    fn the_local_set_is_the_opener_sets_first_four() {
        assert_eq!(LOCAL_SET, OPENER_SET[..4]);
        assert_eq!(SSH_CONTEXT, OPENER_SET[4..]);
    }

    #[test]
    fn each_ssh_variable_alone_makes_an_ssh_context() {
        for name in ["SSH_CONNECTION", "SSH_CLIENT", "SSH_TTY"] {
            assert!(
                is_ssh_context(&env(&[("PATH", "/bin"), (name, "x")])),
                "{name} alone"
            );
        }
    }

    #[test]
    fn no_ssh_variable_is_no_ssh_context() {
        let desk = env(&[
            ("DISPLAY", ":0"),
            ("SSH_AUTH_SOCK", "/run/agent"),
            ("SSH_TTYX", "/dev/pts/1"),
        ]);
        assert!(!is_ssh_context(&desk), "SSH_AUTH_SOCK is a desk agent");
        assert!(!is_ssh_context(&[]));
    }

    #[test]
    fn the_local_set_takes_its_four_names_and_nothing_else() {
        let found = local_set_from(&env(&[
            ("DISPLAY", ":1"),
            ("DBUS_SESSION_BUS_ADDRESS", ""),
            ("SSH_CONNECTION", "1 2 3 4"),
            ("PATH", "/bin"),
        ]));
        let expected: BTreeMap<String, String> =
            [("DISPLAY", ":1"), ("DBUS_SESSION_BUS_ADDRESS", "")]
                .map(|(name, value)| (name.to_owned(), value.to_owned()))
                .into();
        assert_eq!(
            found, expected,
            "an empty value is kept, an unset one stays absent"
        );
    }

    #[test]
    fn a_relative_wayland_display_is_made_absolute_against_its_runtime_dir() {
        let found = local_set_from(&env(&[
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XDG_RUNTIME_DIR", "/run/user/4242"),
        ]));
        assert_eq!(found["WAYLAND_DISPLAY"], "/run/user/4242/wayland-1");
    }

    #[test]
    fn a_wayland_display_with_nothing_to_resolve_against_is_left_as_is() {
        for (pairs, expected) in [
            (
                &[("WAYLAND_DISPLAY", "/elsewhere/wayland-2")][..],
                "/elsewhere/wayland-2",
            ),
            (&[("WAYLAND_DISPLAY", "wayland-1")][..], "wayland-1"),
            (
                &[
                    ("WAYLAND_DISPLAY", "wayland-1"),
                    ("XDG_RUNTIME_DIR", "relative"),
                ][..],
                "wayland-1",
            ),
            (
                &[("WAYLAND_DISPLAY", ""), ("XDG_RUNTIME_DIR", "/run/user/1")][..],
                "",
            ),
        ] {
            assert_eq!(
                local_set_from(&env(pairs))["WAYLAND_DISPLAY"],
                expected,
                "{pairs:?}"
            );
        }
    }

    #[test]
    fn a_value_that_is_not_utf8_is_skipped() {
        let found = local_set_from(&[
            (
                OsString::from("DISPLAY"),
                OsString::from_vec(vec![b':', 0xff]),
            ),
            (
                OsString::from("SSH_AUTH_SOCK"),
                OsString::from("/run/agent"),
            ),
        ]);
        assert_eq!(found.keys().collect::<Vec<_>>(), ["SSH_AUTH_SOCK"]);
    }

    #[test]
    fn an_oversized_local_value_is_skipped() {
        let long = "x".repeat(MAX_LOCAL_VALUE + 1);
        assert!(local_set_from(&env(&[("DISPLAY", &long)])).is_empty());
    }

    #[test]
    fn a_remote_value_is_taken_only_when_it_passes_the_remote_rules() {
        let long = "x".repeat(MAX_ORIGIN_VALUE + 1);
        let found = [
            (
                OsString::from("SSH_CLIENT"),
                OsString::from("10.0.0.2 51000 22"),
            ),
            (OsString::from("SSH_CONNECTION"), OsString::from(&long)),
            (OsString::from("SSH_TTY"), OsString::from_vec(vec![0xff])),
        ];
        assert_eq!(
            remote_value_from(&found, "SSH_CLIENT").as_deref(),
            Some("10.0.0.2 51000 22")
        );
        assert_eq!(remote_value_from(&found, "SSH_CONNECTION"), None);
        assert_eq!(remote_value_from(&found, "SSH_TTY"), None);
        assert_eq!(remote_value_from(&found, "SSH_AUTH_SOCK"), None);
    }

    #[test]
    fn a_local_entry_needs_a_local_name() {
        for name in ["SSH_CONNECTION", "PATH", "SSH_AUTH_SOCKX", "display"] {
            assert_eq!(
                check_local(name, "x"),
                Err(OpenerValueError::NotLocal(name.into()))
            );
        }
        assert_eq!(check_local("SSH_AUTH_SOCK", ""), Ok(()));
    }

    #[test]
    fn values_refuse_a_nul_and_their_kinds_limit() {
        assert_eq!(
            check_local("DISPLAY", ":0\0x"),
            Err(OpenerValueError::Nul("DISPLAY".into()))
        );
        assert_eq!(
            check_remote("ssh_client", "a\0"),
            Err(OpenerValueError::Nul("ssh_client".into()))
        );
        assert_eq!(check_local("DISPLAY", &"x".repeat(MAX_LOCAL_VALUE)), Ok(()));
        assert_eq!(
            check_local("DISPLAY", &"x".repeat(MAX_LOCAL_VALUE + 1)),
            Err(OpenerValueError::TooLong {
                name: "DISPLAY".into(),
                len: MAX_LOCAL_VALUE + 1,
                max: MAX_LOCAL_VALUE,
            })
        );
        assert_eq!(
            check_remote("ssh_connection", &"x".repeat(MAX_ORIGIN_VALUE)),
            Ok(())
        );
        assert!(matches!(
            check_remote("ssh_connection", &"x".repeat(MAX_ORIGIN_VALUE + 1)),
            Err(OpenerValueError::TooLong {
                max: MAX_ORIGIN_VALUE,
                ..
            })
        ));
    }
}
