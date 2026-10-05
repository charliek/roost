//! Which of two `roost-session` builds is newer (plan 076 D1).
//!
//! Hand-rolled rather than a semver crate: Roost's versions are plain
//! `MAJOR.MINOR.PATCH`, and everything else — a prerelease suffix, a
//! malformed string — answers [`VersionOrder::Unordered`], which a
//! semver library would order instead. Refusing to order is the point:
//! the never-downgrade rule refuses only an `Older` target, and a build
//! nobody can place is driven by hand.

use crate::messages::{SessionBinaryIdentity, SessionIdentify};

/// One build of `roost-session` (or of the client), as the update logic
/// compares them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BuildId {
    pub version: String,
    pub protocol: u32,
    pub libghostty_build: String,
    /// Not produced by the release workflow — any local build, debug or
    /// release profile, and every prerelease tag.
    pub dev: bool,
    pub sha: Option<String>,
}

/// Where `a` stands relative to `b` in [`order`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VersionOrder {
    Older,
    Same,
    Newer,
    Unordered,
}

impl From<&SessionBinaryIdentity> for BuildId {
    fn from(identity: &SessionBinaryIdentity) -> Self {
        Self {
            version: identity.app_version.clone(),
            protocol: identity.session_protocol,
            libghostty_build: identity.libghostty_build.clone(),
            dev: identity.dev,
            sha: identity.git_sha.clone(),
        }
    }
}

impl From<&SessionIdentify> for BuildId {
    fn from(identity: &SessionIdentify) -> Self {
        Self {
            version: identity.app_version.clone(),
            protocol: identity.session_protocol,
            libghostty_build: identity.libghostty_build.clone(),
            dev: identity.dev,
            sha: identity.git_sha.clone(),
        }
    }
}

/// Whether `a` is older or newer than `b`.
///
/// A different version decides the order even when either side is dev.
/// At an equal version, two release builds are `Same`, and so are two
/// dev builds whose shas are present and equal; anything else at an
/// equal version is `Unordered`, as is any version this cannot parse.
pub fn order(a: &BuildId, b: &BuildId) -> VersionOrder {
    let (Some(left), Some(right)) = (parse(&a.version), parse(&b.version)) else {
        return VersionOrder::Unordered;
    };
    match left.cmp(&right) {
        std::cmp::Ordering::Less => VersionOrder::Older,
        std::cmp::Ordering::Greater => VersionOrder::Newer,
        std::cmp::Ordering::Equal => match (a.dev, b.dev) {
            (false, false) => VersionOrder::Same,
            (true, true) => match (&a.sha, &b.sha) {
                (Some(left), Some(right)) if left == right => VersionOrder::Same,
                _ => VersionOrder::Unordered,
            },
            _ => VersionOrder::Unordered,
        },
    }
}

fn parse(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.');
    let mut out = [0u64; 3];
    for slot in &mut out {
        let part = parts.next()?;
        if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return None;
        }
        *slot = part.parse().ok()?;
    }
    parts.next().is_none().then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use VersionOrder::{Newer, Older, Same, Unordered};

    fn release(version: &str) -> BuildId {
        BuildId {
            version: version.into(),
            ..BuildId::default()
        }
    }

    fn dev(version: &str, sha: Option<&str>) -> BuildId {
        BuildId {
            version: version.into(),
            dev: true,
            sha: sha.map(Into::into),
            ..BuildId::default()
        }
    }

    #[test]
    fn each_component_orders_numerically() {
        for (a, b, want) in [
            ("0.0.21", "0.0.22", Older),
            ("0.0.22", "0.0.21", Newer),
            ("0.1.0", "0.0.99", Newer),
            ("0.0.99", "0.1.0", Older),
            ("1.0.0", "0.99.99", Newer),
            ("0.99.99", "1.0.0", Older),
            ("0.0.10", "0.0.9", Newer),
            ("0.0.22", "0.0.22", Same),
        ] {
            assert_eq!(order(&release(a), &release(b)), want, "{a} vs {b}");
        }
    }

    #[test]
    fn equal_dev_builds_are_same_only_with_present_equal_shas() {
        let cases = [
            (Some("a1b2c3d"), Some("a1b2c3d"), Same),
            (Some("a1b2c3d"), Some("f00ba12"), Unordered),
            (Some("a1b2c3d"), None, Unordered),
            (None, Some("a1b2c3d"), Unordered),
            (None, None, Unordered),
        ];
        for (left, right, want) in cases {
            assert_eq!(
                order(&dev("0.0.22", left), &dev("0.0.22", right)),
                want,
                "{left:?} vs {right:?}"
            );
        }
    }

    #[test]
    fn dev_against_release_is_unordered_only_at_an_equal_version() {
        let sha = Some("a1b2c3d");
        assert_eq!(order(&dev("0.0.22", sha), &release("0.0.22")), Unordered);
        assert_eq!(order(&release("0.0.22"), &dev("0.0.22", sha)), Unordered);
        assert_eq!(order(&dev("0.0.23", sha), &release("0.0.22")), Newer);
        assert_eq!(order(&release("0.0.22"), &dev("0.0.23", sha)), Older);
        assert_eq!(order(&dev("0.0.21", None), &release("0.0.22")), Older);
    }

    #[test]
    fn unparseable_versions_are_unordered() {
        for bad in [
            "0.0.23-rc1",
            "0.0",
            "0.0.22.1",
            "0.x.22",
            "0..22",
            "",
            "+1.0.0",
            " 0.0.22",
            "0.0.18446744073709551616",
        ] {
            assert_eq!(order(&release(bad), &release("0.0.22")), Unordered, "{bad}");
            assert_eq!(order(&release("0.0.22"), &release(bad)), Unordered, "{bad}");
        }
        assert_eq!(
            order(
                &release("0.0.18446744073709551615"),
                &release("0.0.18446744073709551614")
            ),
            Newer,
            "u64::MAX itself still parses"
        );
    }

    #[test]
    fn both_identity_shapes_carry_every_field() {
        let binary = SessionBinaryIdentity {
            app_version: "0.0.22".into(),
            session_protocol: 7,
            libghostty_build: "g".into(),
            dev: true,
            git_sha: Some("a1b2c3d".into()),
        };
        let running = SessionIdentify {
            app_version: "0.0.22".into(),
            session_protocol: 7,
            libghostty_build: "g".into(),
            dev: true,
            git_sha: Some("a1b2c3d".into()),
            ..SessionIdentify::default()
        };
        let want = BuildId {
            version: "0.0.22".into(),
            protocol: 7,
            libghostty_build: "g".into(),
            dev: true,
            sha: Some("a1b2c3d".into()),
        };
        assert_eq!(BuildId::from(&binary), want);
        assert_eq!(BuildId::from(&running), want);
    }
}
