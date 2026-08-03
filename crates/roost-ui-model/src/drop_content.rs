//! Toolkit-neutral normalization for content dropped onto a terminal.
//!
//! Native adapters own drag protocols and paste delivery. This module owns the
//! payload policy shared by GTK and Iced: safe local paths take precedence over
//! plain text, retain first-seen order, and become shell-safe terminal input.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::shell_escape;

/// Resolve local file paths and an optional plain-text fallback into terminal
/// input.
///
/// Paths are de-duplicated by their raw platform representation before UTF-8
/// conversion. Non-UTF-8 paths and paths bearing a control character no
/// filename may legitimately carry (`\n`, `\r`, ESC) are ignored rather than
/// repaired: a newline would split the join into bogus extra shell lines and an
/// ESC would smuggle a control sequence (e.g. a bracketed-paste marker) into
/// the PTY, while stripping either would silently turn the path into a
/// different filename. Rejecting keeps `shell_escape::escape` lossless.
/// If at least one safe path remains, paths take priority over `text` and are
/// newline-joined in first-seen order. Otherwise non-empty text is returned
/// verbatim.
pub fn resolve<I, P>(paths: I, text: Option<&str>) -> Option<String>
where
    I: IntoIterator<Item = P>,
    P: AsRef<Path>,
{
    let mut seen = HashSet::<PathBuf>::new();
    let escaped = paths
        .into_iter()
        .filter_map(|path| {
            let path = path.as_ref();
            if !seen.insert(path.to_path_buf()) {
                return None;
            }
            let path = path.to_str()?;
            (!path.contains(['\n', '\r', '\u{1b}'])).then(|| shell_escape::escape(path))
        })
        .collect::<Vec<_>>();
    if !escaped.is_empty() {
        return Some(escaped.join("\n"));
    }
    text.filter(|text| !text.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_file_is_escaped() {
        assert_eq!(
            resolve(["/tmp/My File.png"], None),
            Some("/tmp/My\\ File.png".to_string())
        );
    }

    #[test]
    fn multiple_files_keep_first_seen_order_and_newline_join() {
        assert_eq!(
            resolve(["/tmp/a b.png", "/tmp/c.png"], None),
            Some("/tmp/a\\ b.png\n/tmp/c.png".to_string())
        );
    }

    #[test]
    fn duplicate_raw_paths_are_collapsed() {
        assert_eq!(
            resolve(["/tmp/shot.png", "/tmp/shot.png"], None),
            Some("/tmp/shot.png".to_string())
        );
    }

    #[test]
    fn newline_and_carriage_return_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\nil.png", "/tmp/ev\ril.png"], None), None);
        assert_eq!(
            resolve(["/tmp/ev\nil.png", "/tmp/ok.png"], None),
            Some("/tmp/ok.png".to_string())
        );
    }

    /// Shared with the Swift `testControlBearingPathIsDropped` vector.
    #[test]
    fn escape_bearing_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\u{1b}[201~il.png"], None), None);
        assert_eq!(
            resolve(["/tmp/ev\u{1b}[201~il.png", "/tmp/ok.png"], None),
            Some("/tmp/ok.png".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_replacement() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = PathBuf::from(OsString::from_vec(b"/tmp/invalid-\xff".to_vec()));
        assert_eq!(resolve([invalid], None), None);
    }

    #[test]
    fn plain_text_is_verbatim_when_no_safe_file_remains() {
        assert_eq!(
            resolve(std::iter::empty::<&Path>(), Some("git status && ls")),
            Some("git status && ls".to_string())
        );
        assert_eq!(
            resolve(std::iter::empty::<&Path>(), Some("line one\nline two")),
            Some("line one\nline two".to_string())
        );
        assert_eq!(
            resolve(["/tmp/ev\nil.png"], Some("fallback")),
            Some("fallback".to_string())
        );
    }

    #[test]
    fn safe_files_take_priority_over_text() {
        assert_eq!(
            resolve(["/tmp/a.png"], Some("ignored")),
            Some("/tmp/a.png".to_string())
        );
    }

    #[test]
    fn empty_payload_is_none() {
        assert_eq!(resolve(std::iter::empty::<&Path>(), None), None);
        assert_eq!(resolve(std::iter::empty::<&Path>(), Some("")), None);
    }
}
