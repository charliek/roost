//! Shell-escaping for paths/URLs dropped onto the terminal (drag-and-drop).
//!
//! The set is copied byte-for-byte from Ghostty-Mac's
//! `Ghostty.Shell.escapeCharacters` (vendored ghostty SHA pinned in
//! third_party/ghostty/build.sh): backslash, space, and the shell
//! metacharacters that would otherwise word-split or glob a dropped path at a
//! raw shell prompt. Mirrors `mac/Sources/Roost/ShellEscape.swift`
//! (`ShellEscape.escape`); the two implementations share unit-test vectors so
//! they stay byte-identical.

/// Prefix each shell-sensitive character with a backslash. ASCII-only:
/// non-ASCII codepoints (e.g. the U+202F narrow-no-break space in a macOS
/// screenshot filename) pass through unchanged, which modern shells handle as
/// UTF-8 literals.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if matches!(
            ch,
            '\\' | ' '
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '<'
                | '>'
                | '"'
                | '\''
                | '`'
                | '!'
                | '#'
                | '$'
                | '&'
                | ';'
                | '|'
                | '*'
                | '?'
                | '\t'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_string_is_unchanged() {
        assert_eq!(escape(""), "");
    }

    #[test]
    fn plain_path_is_unchanged() {
        assert_eq!(
            escape("/Users/me/screenshots/img.png"),
            "/Users/me/screenshots/img.png"
        );
    }

    #[test]
    fn spaces_are_escaped() {
        assert_eq!(escape("/Users/me/My File.png"), "/Users/me/My\\ File.png");
    }

    /// The headline case: a real macOS screenshot filename has regular spaces
    /// (escaped) and a U+202F narrow-no-break space before "PM" (passes
    /// through, being non-ASCII). Shared verbatim with the Swift
    /// `realScreenshotFilename` test.
    #[test]
    fn real_screenshot_filename() {
        let input = "/Users/me/Desktop/Screenshot 2026-06-28 at 3.45.12\u{202f}PM.png";
        let expected = "/Users/me/Desktop/Screenshot\\ 2026-06-28\\ at\\ 3.45.12\u{202f}PM.png";
        assert_eq!(escape(input), expected);
    }

    #[test]
    fn backslash_is_doubled() {
        assert_eq!(escape("a\\b"), "a\\\\b");
    }

    #[test]
    fn tab_is_escaped() {
        assert_eq!(escape("a\tb"), "a\\\tb");
    }

    #[test]
    fn quotes_are_escaped() {
        assert_eq!(escape("a\"b'c`d"), "a\\\"b\\'c\\`d");
    }

    #[test]
    fn shell_metacharacters_are_escaped() {
        assert_eq!(
            escape("$&;|*?(){}[]<>!#"),
            "\\$\\&\\;\\|\\*\\?\\(\\)\\{\\}\\[\\]\\<\\>\\!\\#"
        );
    }

    #[test]
    fn already_backslashed_space_gets_both_escaped() {
        assert_eq!(escape("\\ "), "\\\\\\ ");
    }

    #[test]
    fn non_ascii_passes_through() {
        assert_eq!(escape("/tmp/图 片.png"), "/tmp/图\\ 片.png");
    }
}
