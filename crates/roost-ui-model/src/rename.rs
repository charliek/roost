//! Toolkit-neutral inline-rename presentation policy shared by the Rust UIs.

/// Returns the exact label a native editor should send to the authoritative
/// workspace, or `None` when the submitted value is empty after trimming.
///
/// The engine continues to accept its serialized command contract verbatim;
/// this helper only keeps GTK and Iced's native editing behavior identical.
pub fn committed_label(draft: &str) -> Option<String> {
    let trimmed = draft.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commit_trims_once_and_rejects_empty_values() {
        assert_eq!(
            committed_label("  renamed project  "),
            Some("renamed project".into())
        );
        assert_eq!(committed_label("\t\n"), None);
        assert_eq!(committed_label(""), None);
    }

    #[test]
    fn commit_preserves_internal_and_unicode_content() {
        assert_eq!(committed_label("  two  words  "), Some("two  words".into()));
        assert_eq!(committed_label("  日本語 🐦  "), Some("日本語 🐦".into()));
    }
}
