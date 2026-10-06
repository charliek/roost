//! Build identity for the two leaf binaries, `roost-session` and
//! `roost-iced` (plan 076 D2): whether the release workflow produced
//! this build, and the commit it came from.
//!
//! One file shared by both `build.rs` through `#[path]`, and compiled
//! into `roost-session`'s unit tests the same way, because a build
//! script's own `#[cfg(test)]` code never runs. Only those two crates
//! use it: a commit then rebuilds two leaf binaries and nothing below
//! them.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RELEASE_ENV: &str = "ROOST_RELEASE_BUILD";

/// What git said about the checkout this build came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Git {
    pub head_sha: String,
    /// Files whose change means HEAD moved. Only paths that exist: cargo
    /// treats a missing `rerun-if-changed` path as always stale.
    pub watch: Vec<PathBuf>,
}

/// The `build.rs` entry point.
pub fn emit() {
    let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .unwrap_or_default();
    let release = std::env::var(RELEASE_ENV).ok();
    let git = read_git(OsStr::new("git"), &manifest_dir);
    for line in directives(release.as_deref(), git.as_ref()) {
        println!("{line}");
    }
}

/// Every cargo directive for one build, decided from its inputs alone.
///
/// Both env values are always set, so a crate reads them with `env!`
/// and an ambient variable of the same name can never leak in. With no
/// git there is no `rerun-if-changed` at all: the env directive alone
/// stops cargo's default of rerunning on every package change.
pub fn directives(release: Option<&str>, git: Option<&Git>) -> Vec<String> {
    let dev = release != Some("1");
    let sha = git
        .and_then(|git| short_sha(&git.head_sha))
        .unwrap_or_default();
    let mut out = vec![
        format!("cargo:rerun-if-env-changed={RELEASE_ENV}"),
        format!("cargo:rustc-env=ROOST_BUILD_DEV={}", u8::from(dev)),
        format!("cargo:rustc-env=ROOST_BUILD_SHA={sha}"),
    ];
    if let Some(git) = git {
        out.extend(
            git.watch
                .iter()
                .map(|path| format!("cargo:rerun-if-changed={}", path.display())),
        );
    }
    out
}

/// The first 7 characters of a full commit id, lowercased, or `None`
/// for anything that is not one.
pub fn short_sha(head_sha: &str) -> Option<String> {
    let head_sha = head_sha.trim();
    (head_sha.len() >= 7 && head_sha.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| head_sha[..7].to_ascii_lowercase())
}

/// The ref a `HEAD` file points at, `None` when HEAD is detached.
pub fn head_ref(contents: &str) -> Option<&str> {
    contents
        .trim()
        .strip_prefix("ref:")
        .map(str::trim)
        .filter(|name| !name.is_empty())
}

/// HEAD's commit and the files to watch for it moving, via
/// `git rev-parse` so a worktree (where `.git` is a file) resolves the
/// same as a plain checkout. `None` when git is missing or any step
/// fails.
pub fn read_git(git: &OsStr, dir: &Path) -> Option<Git> {
    let rev_parse = |args: &[&str]| -> Option<String> {
        let output = Command::new(git)
            .arg("-C")
            .arg(dir)
            .arg("rev-parse")
            .args(args)
            .output()
            .ok()?;
        let text = String::from_utf8(output.stdout).ok()?;
        let text = text.trim();
        (output.status.success() && !text.is_empty()).then(|| text.to_string())
    };
    let absolute = |path: String| {
        let path = PathBuf::from(path);
        if path.is_absolute() {
            path
        } else {
            dir.join(path)
        }
    };

    let head_sha = rev_parse(&["HEAD"])?;
    let head_path = absolute(rev_parse(&["--git-path", "HEAD"])?);
    let common_dir = absolute(rev_parse(&["--git-common-dir"])?)
        .canonicalize()
        .ok()?;
    let head_contents = std::fs::read_to_string(&head_path).ok()?;

    let mut watch = vec![head_path];
    if let Some(name) = head_ref(&head_contents) {
        let ref_path = absolute(rev_parse(&["--git-path", name])?);
        // A ref that lives only in packed-refs has no loose file yet;
        // the next commit creates one, so watch the directory it will
        // appear in — but never above `refs/`, where a directory watch
        // would fire on every fetch.
        let refs_root = common_dir.join("refs");
        let nearest = ref_path
            .ancestors()
            .find(|path| path.exists())
            .and_then(|path| path.canonicalize().ok())
            .filter(|path| path.starts_with(&refs_root));
        watch.extend(nearest);
    }
    let packed = common_dir.join("packed-refs");
    if packed.exists() {
        watch.push(packed);
    }
    Some(Git { head_sha, watch })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git() -> Git {
        Git {
            head_sha: "A1B2C3D4e5f60718293a4b5c6d7e8f9012345678".into(),
            watch: vec![PathBuf::from("/repo/.git/HEAD")],
        }
    }

    #[test]
    fn only_the_release_flag_set_to_one_is_a_release_build() {
        for (flag, dev) in [
            (Some("1"), "0"),
            (None, "1"),
            (Some("0"), "1"),
            (Some(""), "1"),
        ] {
            let lines = directives(flag, Some(&git()));
            assert!(
                lines.contains(&format!("cargo:rustc-env=ROOST_BUILD_DEV={dev}")),
                "{flag:?}: {lines:?}"
            );
            assert!(lines.contains(&"cargo:rerun-if-env-changed=ROOST_RELEASE_BUILD".to_string()));
        }
    }

    #[test]
    fn a_readable_checkout_gives_the_short_sha_and_watches_head() {
        let lines = directives(None, Some(&git()));
        assert!(lines.contains(&"cargo:rustc-env=ROOST_BUILD_SHA=a1b2c3d".to_string()));
        assert!(lines.contains(&"cargo:rerun-if-changed=/repo/.git/HEAD".to_string()));
    }

    #[test]
    fn no_git_means_no_sha_and_no_watched_path() {
        for release in [None, Some("1")] {
            let lines = directives(release, None);
            assert!(lines.contains(&"cargo:rustc-env=ROOST_BUILD_SHA=".to_string()));
            assert!(
                !lines
                    .iter()
                    .any(|line| line.starts_with("cargo:rerun-if-changed")),
                "{lines:?}"
            );
        }
    }

    #[test]
    fn a_missing_git_binary_or_a_non_checkout_reads_as_no_git() {
        let here = Path::new(env!("CARGO_MANIFEST_DIR"));
        assert_eq!(read_git(OsStr::new("roost-no-such-git-binary"), here), None);
        assert_eq!(
            read_git(
                OsStr::new("git"),
                Path::new("/nonexistent/roost-build-info")
            ),
            None
        );
    }

    #[test]
    fn short_sha_takes_seven_hex_characters_or_nothing() {
        assert_eq!(short_sha("ABCDEF0123\n").as_deref(), Some("abcdef0"));
        assert_eq!(short_sha("abc"), None);
        assert_eq!(short_sha("not-a-sha-at-all"), None);
        assert_eq!(short_sha(""), None);
    }

    #[test]
    fn head_names_its_ref_unless_detached() {
        assert_eq!(
            head_ref("ref: refs/heads/feature/x\n"),
            Some("refs/heads/feature/x")
        );
        assert_eq!(head_ref("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\n"), None);
        assert_eq!(head_ref("ref: \n"), None);
    }
}
