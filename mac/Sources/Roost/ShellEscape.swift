// Shell-escaping for paths/URLs dropped onto the terminal (drag-and-drop).
//
// The set is copied byte-for-byte from Ghostty-Mac's
// `Ghostty.Shell.escapeCharacters` (vendored ghostty SHA pinned in
// third_party/ghostty/build.sh): backslash, space, and the shell
// metacharacters that would otherwise word-split or glob a dropped path at a
// raw shell prompt. Mirrors `crates/roost-linux/src/shell_escape.rs`
// (`shell_escape::escape`); the two implementations share unit-test vectors so
// they stay byte-identical.

import Foundation

enum ShellEscape {
    private static let characters: Set<Character> = [
        "\\", " ", "(", ")", "[", "]", "{", "}", "<", ">",
        "\"", "'", "`", "!", "#", "$", "&", ";", "|", "*", "?", "\t",
    ]

    /// Prefix each shell-sensitive character with a backslash. Single pass —
    /// unlike Ghostty's multi-pass `replacingOccurrences` (which only works
    /// because it escapes `\` first) — but produces identical output. ASCII-only:
    /// non-ASCII codepoints (e.g. the U+202F narrow-no-break space in a macOS
    /// screenshot filename) pass through unchanged, which modern shells handle as
    /// UTF-8 literals.
    static func escape(_ str: String) -> String {
        var out = String()
        out.reserveCapacity(str.count)
        for ch in str {
            if characters.contains(ch) {
                out.append("\\")
            }
            out.append(ch)
        }
        return out
    }
}
