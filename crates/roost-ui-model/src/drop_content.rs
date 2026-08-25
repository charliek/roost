//! Toolkit-neutral normalization for content dropped onto a terminal.
//!
//! Native adapters own drag protocols and paste delivery. This module owns the
//! payload policy for the Rust UI adapter (Iced), mirrored by Swift's
//! `TerminalView.swift`: safe local paths take precedence over plain text,
//! retain first-seen order, and become shell-safe terminal input.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::shell_escape;

/// Characters no dropped path or URL may legitimately carry — aligned with
/// Swift's `Character.isNewline` scalar classes plus ESC. Swift classifies
/// grapheme clusters where this classifies scalars, so Rust is strictly
/// stricter on pathological clusters; that asymmetry only ever rejects more.
const REJECTED_DROP_CONTROLS: [char; 8] = [
    '\n',       // LF
    '\u{0b}',   // VT
    '\u{0c}',   // FF
    '\r',       // CR
    '\u{0085}', // NEL
    '\u{2028}', // LS
    '\u{2029}', // PS
    '\u{1b}',   // ESC
];

/// Resolve local file paths, an optional dragged URL, and an optional
/// plain-text fallback into terminal input.
///
/// Paths are de-duplicated by their raw platform representation before UTF-8
/// conversion. Non-UTF-8 paths, and paths or URLs bearing one of
/// [`REJECTED_DROP_CONTROLS`], are ignored rather than repaired: a line break
/// would split the join into bogus extra shell lines and an ESC would smuggle a
/// control sequence (e.g. a bracketed-paste marker) into the PTY, while
/// stripping either would silently turn the path into a different filename.
/// Rejecting keeps `shell_escape::escape` lossless.
///
/// Priority is paths → `url` → `text`, mirroring the Mac's
/// `TerminalView.dropContentString`. Surviving paths are newline-joined in
/// first-seen order and a surviving URL is shell-escaped; `text` is returned
/// verbatim and deliberately unfiltered (it may be a command the user wants to
/// run — the bracketed-paste mitigations own that boundary).
///
/// `url` exists for cross-UI parity with the Mac's dragged-URL branch. No Rust
/// toolkit surfaces a distinct URL drop payload yet, so both call sites pass
/// `None` and this branch is production-dead until one does — the tests are its
/// only exercise.
pub fn resolve<I, P>(paths: I, url: Option<&str>, text: Option<&str>) -> Option<String>
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
            (!path.contains(REJECTED_DROP_CONTROLS)).then(|| shell_escape::escape(path))
        })
        .collect::<Vec<_>>();
    if !escaped.is_empty() {
        return Some(escaped.join("\n"));
    }
    if let Some(url) = url.filter(|url| !url.is_empty() && !url.contains(REJECTED_DROP_CONTROLS)) {
        return Some(shell_escape::escape(url));
    }
    text.filter(|text| !text.is_empty()).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_paths() -> std::iter::Empty<&'static Path> {
        std::iter::empty::<&Path>()
    }

    #[test]
    fn single_file_is_escaped() {
        assert_eq!(
            resolve(["/tmp/My File.png"], None, None),
            Some("/tmp/My\\ File.png".to_string())
        );
    }

    #[test]
    fn multiple_files_keep_first_seen_order_and_newline_join() {
        assert_eq!(
            resolve(["/tmp/a b.png", "/tmp/c.png"], None, None),
            Some("/tmp/a\\ b.png\n/tmp/c.png".to_string())
        );
    }

    #[test]
    fn duplicate_raw_paths_are_collapsed() {
        assert_eq!(
            resolve(["/tmp/shot.png", "/tmp/shot.png"], None, None),
            Some("/tmp/shot.png".to_string())
        );
    }

    /// Shared with the Swift `testNewlineBearingPathIsDropped` vector.
    #[test]
    fn newline_and_carriage_return_paths_are_rejected() {
        assert_eq!(
            resolve(["/tmp/ev\nil.png", "/tmp/ev\ril.png"], None, None),
            None
        );
        assert_eq!(
            resolve(["/tmp/ev\nil.png", "/tmp/ok.png"], None, None),
            Some("/tmp/ok.png".to_string())
        );
    }

    /// Shared with the Swift `testVerticalTabBearingPathIsDropped` vector.
    #[test]
    fn vertical_tab_bearing_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\u{0b}il.png"], None, None), None);
        assert_eq!(
            resolve(["/tmp/ev\u{0b}il.png", "/tmp/ok.png"], None, None),
            Some("/tmp/ok.png".to_string())
        );
    }

    /// Shared with the Swift `testFormFeedBearingPathIsDropped` vector.
    #[test]
    fn form_feed_bearing_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\u{0c}il.png"], None, None), None);
        assert_eq!(
            resolve(["/tmp/ev\u{0c}il.png", "/tmp/ok.png"], None, None),
            Some("/tmp/ok.png".to_string())
        );
    }

    /// Shared with the Swift `testUnicodeNewlineBearingPathIsDropped` vector.
    #[test]
    fn unicode_newline_bearing_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\u{0085}il.png"], None, None), None);
        assert_eq!(resolve(["/tmp/ev\u{2028}il.png"], None, None), None);
        assert_eq!(resolve(["/tmp/ev\u{2029}il.png"], None, None), None);
        assert_eq!(
            resolve(
                [
                    "/tmp/ev\u{0085}il.png",
                    "/tmp/ev\u{2028}il.png",
                    "/tmp/ev\u{2029}il.png",
                    "/tmp/ok.png",
                ],
                None,
                None
            ),
            Some("/tmp/ok.png".to_string())
        );
    }

    /// Shared with the Swift `testControlBearingPathIsDropped` vector.
    #[test]
    fn escape_bearing_paths_are_rejected() {
        assert_eq!(resolve(["/tmp/ev\u{1b}[201~il.png"], None, None), None);
        assert_eq!(
            resolve(["/tmp/ev\u{1b}[201~il.png", "/tmp/ok.png"], None, None),
            Some("/tmp/ok.png".to_string())
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_replacement() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;

        let invalid = PathBuf::from(OsString::from_vec(b"/tmp/invalid-\xff".to_vec()));
        assert_eq!(resolve([invalid], None, None), None);
    }

    /// Shared with the Swift `testWebURLIsEscapedWhenNoFiles` vector.
    #[test]
    fn url_is_escaped_when_no_safe_path_remains() {
        assert_eq!(
            resolve(
                no_paths(),
                Some("https://example.com/a?b=c&d=e"),
                Some("ignored")
            ),
            Some("https://example.com/a\\?b=c\\&d=e".to_string())
        );
        assert_eq!(
            resolve(
                ["/tmp/ev\nil.png"],
                Some("https://example.com/x"),
                Some("ignored")
            ),
            Some("https://example.com/x".to_string())
        );
    }

    #[test]
    fn safe_paths_take_priority_over_url() {
        assert_eq!(
            resolve(["/tmp/a.png"], Some("https://example.com/x"), None),
            Some("/tmp/a.png".to_string())
        );
    }

    /// Shared with the Swift `testControlBearingURLFallsThroughToString`
    /// vector: a rejected URL is absent, not stripped, so the unfiltered text
    /// fallback answers instead.
    #[test]
    fn control_bearing_url_falls_through_to_text() {
        for control in REJECTED_DROP_CONTROLS {
            let url = format!("https://example.com/{control}evil");
            assert_eq!(
                resolve(no_paths(), Some(&url), Some("fallback")),
                Some("fallback".to_string()),
                "url bearing U+{:04X} should fall through",
                control as u32
            );
        }
    }

    /// Shared with the Swift
    /// `testControlBearingURLAndStringYieldsRawString` vector. Documents the
    /// accepted #282 baseline: when a drag carries the same control-bearing
    /// text on both the URL and the text side, the rejected URL falls through
    /// and the raw text reaches the PTY unchanged. That is the plain-text
    /// boundary #280's bracketed-paste mitigations own — reject-don't-strip
    /// means this arm must not launder the payload into an escaped URL either.
    #[test]
    fn control_bearing_url_and_text_yields_raw_text() {
        let payload = "https://example.com/\u{1b}[201~evil";
        assert_eq!(
            resolve(no_paths(), Some(payload), Some(payload)),
            Some(payload.to_string())
        );
    }

    /// Shared with the Swift `testControlBearingURLWithoutStringIsNil` vector.
    #[test]
    fn control_bearing_url_without_text_is_none() {
        assert_eq!(
            resolve(
                no_paths(),
                Some("https://example.com/\u{1b}[201~evil"),
                None
            ),
            None
        );
    }

    #[test]
    fn plain_text_is_verbatim_when_no_safe_file_remains() {
        assert_eq!(
            resolve(no_paths(), None, Some("git status && ls")),
            Some("git status && ls".to_string())
        );
        assert_eq!(
            resolve(no_paths(), None, Some("line one\nline two")),
            Some("line one\nline two".to_string())
        );
        assert_eq!(
            resolve(["/tmp/ev\nil.png"], None, Some("fallback")),
            Some("fallback".to_string())
        );
    }

    #[test]
    fn safe_files_take_priority_over_text() {
        assert_eq!(
            resolve(["/tmp/a.png"], None, Some("ignored")),
            Some("/tmp/a.png".to_string())
        );
    }

    #[test]
    fn empty_payload_is_none() {
        assert_eq!(resolve(no_paths(), None, None), None);
        assert_eq!(resolve(no_paths(), Some(""), Some("")), None);
    }
}
