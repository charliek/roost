//! Window-chrome title composition shared by the Rust UI adapters.
//!
//! Ports `mac/Sources/Roost/PathDisplay.swift` and the Mac's
//! `updateWindowTitle` (App.swift) so the iced window title reads the same
//! as the shipped Swift app. The Mac splits the string across
//! `NSWindow.title` + `.subtitle`; toolkits with a single title string
//! join them with a spaced en dash, which is how AppKit renders the pair.
//!
//! Pure functions — the caller supplies `$HOME` so these stay testable and
//! free of process state. GTK's `tilde_abbreviate` is home-collapse-only and
//! could adopt [`abbreviate_path`] later.

use unicode_segmentation::UnicodeSegmentation;

/// Fallback title when no project is active or its name is empty.
pub const DEFAULT_WINDOW_TITLE: &str = "Roost";

/// Separator between the project name and the abbreviated cwd. Matches the
/// spaced en dash AppKit puts between a window's title and subtitle.
const TITLE_SEPARATOR: &str = " – ";

/// Grapheme budget for the cwd segment (Mac subtitle budget, App.swift:4219).
pub const TITLE_CWD_MAX_GRAPHEMES: usize = 48;

/// Collapse a `$HOME` prefix to `~` and left-truncate with a leading `…`
/// when the result exceeds `max` grapheme clusters.
///
/// Trailing path segments are what users recognize at a glance, so the tail
/// is kept. Counts extended grapheme clusters — the same unit as Swift's
/// `Character` — so emoji, flags, and combining marks are never sliced.
///
/// `max == 0` renders zero characters rather than panicking; the function is
/// exported for testing and the truncation branch below would otherwise
/// underflow.
pub fn abbreviate_path(path: &str, home: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let collapsed = collapse_home(path, home);
    let count = collapsed.graphemes(true).count();
    if count <= max {
        return collapsed;
    }
    let tail: String = collapsed
        .graphemes(true)
        .skip(count - (max - 1))
        .collect::<String>();
    format!("…{tail}")
}

fn collapse_home(path: &str, home: &str) -> String {
    if home.is_empty() {
        return path.to_string();
    }
    if path == home {
        return "~".to_string();
    }
    if let Some(rest) = path.strip_prefix(home) {
        if rest.starts_with('/') {
            return format!("~{rest}");
        }
    }
    path.to_string()
}

/// Compose the window title from the active project's name and the cwd that
/// should be shown beside it.
///
/// Mirrors the Mac: the project name (or `Roost` when there is no active
/// project, or its name is empty) plus the home-collapsed, ≤48-grapheme cwd.
/// An empty `cwd` drops the separator segment entirely. The caller decides
/// which cwd wins — the active tab's live OSC 7 cwd, falling back to the
/// project's static cwd.
///
/// Emptiness follows the Swift check exactly (no trimming), so a
/// whitespace-only project name renders as-is rather than falling back.
pub fn window_title(project_name: &str, cwd: &str, home: &str) -> String {
    let name = if project_name.is_empty() {
        DEFAULT_WINDOW_TITLE
    } else {
        project_name
    };
    if cwd.is_empty() {
        return name.to_string();
    }
    let shown = abbreviate_path(cwd, home, TITLE_CWD_MAX_GRAPHEMES);
    if shown.is_empty() {
        return name.to_string();
    }
    format!("{name}{TITLE_SEPARATOR}{shown}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOME: &str = "/Users/me";

    #[test]
    fn collapses_home_prefix() {
        assert_eq!(
            abbreviate_path("/Users/me/projects/roost", HOME, 48),
            "~/projects/roost"
        );
    }

    #[test]
    fn collapses_bare_home() {
        assert_eq!(abbreviate_path(HOME, HOME, 48), "~");
    }

    #[test]
    fn does_not_collapse_a_sibling_of_home() {
        assert_eq!(
            abbreviate_path("/Users/melissa/src", HOME, 48),
            "/Users/melissa/src"
        );
    }

    #[test]
    fn leaves_a_path_outside_home_alone() {
        assert_eq!(abbreviate_path("/etc/nginx", HOME, 48), "/etc/nginx");
    }

    #[test]
    fn empty_home_disables_collapsing() {
        assert_eq!(abbreviate_path("/Users/me/x", "", 48), "/Users/me/x");
    }

    #[test]
    fn exactly_max_graphemes_is_not_truncated() {
        let path = format!("/{}", "a".repeat(47));
        assert_eq!(path.graphemes(true).count(), 48);
        assert_eq!(abbreviate_path(&path, HOME, 48), path);
    }

    #[test]
    fn one_over_max_truncates_to_max() {
        let path = format!("/{}", "a".repeat(48));
        let out = abbreviate_path(&path, HOME, 48);
        assert_eq!(out.graphemes(true).count(), 48);
        assert!(out.starts_with('…'));
        assert!(out.ends_with(&"a".repeat(47)));
    }

    /// The truncation unit is the extended grapheme cluster, not the scalar:
    /// a ZWJ family emoji and a flag each count as one and are never split.
    #[test]
    fn truncation_counts_grapheme_clusters() {
        let family = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
        let flag = "\u{1F1FA}\u{1F1F8}";
        let path = format!("/{}{family}{flag}", "b".repeat(50));
        let out = abbreviate_path(&path, HOME, 48);
        assert_eq!(out.graphemes(true).count(), 48);
        assert!(out.starts_with('…'));
        assert!(out.ends_with(&format!("{family}{flag}")));
        // 47 kept clusters: the family + the flag + 45 'b's.
        assert!(out.ends_with(&format!("{}{family}{flag}", "b".repeat(45))));
    }

    #[test]
    fn zero_max_renders_nothing() {
        assert_eq!(abbreviate_path("/Users/me/x", HOME, 0), "");
    }

    #[test]
    fn title_joins_project_and_cwd() {
        assert_eq!(
            window_title("Untitled 2", "/Users/me/projects", HOME),
            "Untitled 2 – ~/projects"
        );
    }

    #[test]
    fn title_without_cwd_drops_the_separator() {
        assert_eq!(window_title("roost", "", HOME), "roost");
    }

    #[test]
    fn empty_project_name_falls_back_to_roost() {
        assert_eq!(window_title("", "", HOME), "Roost");
        assert_eq!(window_title("", "/tmp", HOME), "Roost – /tmp");
    }

    #[test]
    fn title_abbreviates_a_long_cwd() {
        let deep = format!("{HOME}/{}", "segment/".repeat(12));
        let title = window_title("roost", &deep, HOME);
        let (name, shown) = title.split_once(TITLE_SEPARATOR).expect("separator");
        assert_eq!(name, "roost");
        assert_eq!(shown.graphemes(true).count(), TITLE_CWD_MAX_GRAPHEMES);
        assert!(shown.starts_with('…'));
    }
}
