//! Dock-tile badge — parity port of `mac/Sources/Roost/App.swift`'s
//! `refreshDockBadge()`:
//!
//! ```swift
//! NSApp.dockTile.badgeLabel = count > 0 ? String(count) : nil
//! ```
//!
//! The whole path is safe API in the 0.3 bindings — no `unsafe` block in
//! this file.

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSString;

/// The badge text for a notification-inbox count. `None` at zero: AppKit
/// draws nothing for an absent label, so `None` is how the badge
/// disappears rather than showing a "0".
///
/// Pure, so the mapping is pinned by unit tests instead of by an
/// AppKit round trip.
pub(crate) fn label(count: usize) -> Option<String> {
    (count > 0).then(|| count.to_string())
}

/// Read the badge back off the live Dock tile.
///
/// Backs the `app.dock_badge` test-mode op, and deliberately reads AppKit
/// rather than recomputing from the inbox — recomputing would assert the
/// mapping (which [`label`]'s unit tests already do) while proving nothing
/// about whether the write ever reached the Dock.
pub(crate) fn read(mtm: MainThreadMarker) -> Option<String> {
    NSApplication::sharedApplication(mtm)
        .dockTile()
        .badgeLabel()
        .map(|text| text.to_string())
}

/// Acquire the main-thread marker and write [`label`] onto the Dock tile.
///
/// `MainThreadMarker::new()` cannot fail for the callers this has — the
/// iced update loop is the main thread — so `None` means a real invariant
/// break. It logs and skips instead of panicking: the badge is cosmetic,
/// and taking the whole UI down over it would be the worse failure.
pub(crate) fn sync(count: usize) {
    let Some(mtm) = MainThreadMarker::new() else {
        tracing::error!(
            count,
            "dock badge sync ran off the main thread; skipping (AppKit is main-thread-only)"
        );
        return;
    };
    let text = label(count).map(|text| NSString::from_str(&text));
    NSApplication::sharedApplication(mtm)
        .dockTile()
        .setBadgeLabel(text.as_deref());
}

#[cfg(test)]
mod tests {
    use super::label;

    #[test]
    fn zero_clears_the_badge() {
        assert_eq!(label(0), None);
    }

    #[test]
    fn a_pending_count_is_its_decimal_string() {
        assert_eq!(label(1).as_deref(), Some("1"));
        assert_eq!(label(9).as_deref(), Some("9"));
        // `notification_inbox::CAP` is 10, so double digits are reachable.
        assert_eq!(label(10).as_deref(), Some("10"));
    }
}
