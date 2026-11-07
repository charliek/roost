//! `[user@]host[:port]`: the one spelling a saved mobile host has.

use std::fmt;

pub const DEFAULT_PORT: u16 = 22;

/// Where a dialer logs in. `host` never carries IPv6 brackets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostTarget {
    pub user: String,
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TargetError {
    #[error("the host is empty")]
    Empty,
    #[error("{0:?} has no user before the '@'")]
    EmptyUser(String),
    #[error("{0:?} names no user, and there is no default user to log in as")]
    MissingUser(String),
    #[error("{0:?} has no host name")]
    EmptyHost(String),
    #[error("{0:?} has a space in it")]
    Whitespace(String),
    #[error("{0:?} has an unclosed '[' around an IPv6 address")]
    UnclosedBracket(String),
    #[error("{0:?} has something after the ']' that is not a :port")]
    AfterBracket(String),
    #[error("{0:?} has a port that is not a number from 1 to 65535")]
    BadPort(String),
}

impl HostTarget {
    /// Parse `[user@]host[:port]`, logging in as `default_user` when the
    /// target names nobody. An IPv6 literal takes a port only inside
    /// brackets (`[::1]:2222`); a bare one (`fd7a::1`) is all host.
    pub fn parse(target: &str, default_user: &str) -> Result<Self, TargetError> {
        let trimmed = target.trim();
        if trimmed.is_empty() {
            return Err(TargetError::Empty);
        }
        if trimmed.chars().any(char::is_whitespace) {
            return Err(TargetError::Whitespace(trimmed.to_string()));
        }
        let (user, authority) = match trimmed.rsplit_once('@') {
            Some(("", _)) => return Err(TargetError::EmptyUser(trimmed.to_string())),
            Some((user, authority)) => (user.to_string(), authority),
            None if default_user.trim().is_empty() => {
                return Err(TargetError::MissingUser(trimmed.to_string()))
            }
            None => (default_user.trim().to_string(), trimmed),
        };
        let (host, port) = split_host_port(authority, trimmed)?;
        if host.is_empty() {
            return Err(TargetError::EmptyHost(trimmed.to_string()));
        }
        Ok(Self {
            user,
            host: host.to_string(),
            port,
        })
    }

    /// `host`, or `host:port` off the default port: what a host key is
    /// pinned against, and how a message names the machine.
    pub fn host_port(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == DEFAULT_PORT {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

impl fmt::Display for HostTarget {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.user, self.host_port())
    }
}

fn split_host_port<'a>(authority: &'a str, whole: &str) -> Result<(&'a str, u16), TargetError> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, tail) = rest
            .split_once(']')
            .ok_or_else(|| TargetError::UnclosedBracket(whole.to_string()))?;
        return match tail {
            "" => Ok((host, DEFAULT_PORT)),
            _ => match tail.strip_prefix(':') {
                Some(port) => Ok((host, parse_port(port, whole)?)),
                None => Err(TargetError::AfterBracket(whole.to_string())),
            },
        };
    }
    match authority.split_once(':') {
        None => Ok((authority, DEFAULT_PORT)),
        Some((host, port)) if !port.contains(':') => Ok((host, parse_port(port, whole)?)),
        Some(_) => Ok((authority, DEFAULT_PORT)),
    }
}

fn parse_port(port: &str, whole: &str) -> Result<u16, TargetError> {
    match port.parse::<u16>() {
        Ok(0) | Err(_) => Err(TargetError::BadPort(whole.to_string())),
        Ok(port) => Ok(port),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(target: &str) -> Result<HostTarget, TargetError> {
        HostTarget::parse(target, "phone")
    }

    fn target(user: &str, host: &str, port: u16) -> HostTarget {
        HostTarget {
            user: user.into(),
            host: host.into(),
            port,
        }
    }

    #[test]
    fn user_host_and_port_are_split() {
        assert_eq!(parse("charliek@popos"), Ok(target("charliek", "popos", 22)));
        assert_eq!(
            parse("charliek@popos:2222"),
            Ok(target("charliek", "popos", 2222))
        );
        assert_eq!(
            parse(" popos.tail1234.ts.net "),
            Ok(target("phone", "popos.tail1234.ts.net", 22))
        );
        assert_eq!(
            parse("100.82.128.123:22"),
            Ok(target("phone", "100.82.128.123", 22))
        );
    }

    #[test]
    fn ipv6_takes_a_port_only_in_brackets() {
        assert_eq!(parse("[::1]:2222"), Ok(target("phone", "::1", 2222)));
        assert_eq!(parse("me@[::1]"), Ok(target("me", "::1", 22)));
        assert_eq!(
            parse("fd7a:115c::1"),
            Ok(target("phone", "fd7a:115c::1", 22))
        );
        assert_eq!(
            parse("[::1"),
            Err(TargetError::UnclosedBracket("[::1".into()))
        );
        assert_eq!(
            parse("[::1]22"),
            Err(TargetError::AfterBracket("[::1]22".into()))
        );
    }

    #[test]
    fn the_rightmost_at_ends_the_user() {
        assert_eq!(parse("a@b@host"), Ok(target("a@b", "host", 22)));
    }

    #[test]
    fn malformed_targets_are_refused() {
        assert_eq!(parse("  "), Err(TargetError::Empty));
        assert_eq!(parse("@host"), Err(TargetError::EmptyUser("@host".into())));
        assert_eq!(parse("me@"), Err(TargetError::EmptyHost("me@".into())));
        assert_eq!(parse(":22"), Err(TargetError::EmptyHost(":22".into())));
        assert_eq!(
            parse("pop os"),
            Err(TargetError::Whitespace("pop os".into()))
        );
        for bad in ["host:", "host:0", "host:65536", "host:ssh", "[::1]:"] {
            assert_eq!(parse(bad), Err(TargetError::BadPort(bad.into())), "{bad}");
        }
    }

    #[test]
    fn a_missing_user_needs_a_default() {
        assert_eq!(
            HostTarget::parse("popos", " "),
            Err(TargetError::MissingUser("popos".into()))
        );
        assert_eq!(
            HostTarget::parse("root@popos", ""),
            Ok(target("root", "popos", 22))
        );
    }

    #[test]
    fn display_omits_the_default_port_and_brackets_ipv6() {
        assert_eq!(target("me", "popos", 22).to_string(), "me@popos");
        assert_eq!(target("me", "popos", 2222).to_string(), "me@popos:2222");
        assert_eq!(target("me", "::1", 22).host_port(), "[::1]");
        assert_eq!(target("me", "::1", 2222).host_port(), "[::1]:2222");
    }
}
