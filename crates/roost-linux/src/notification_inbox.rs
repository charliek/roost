//! In-memory inbox of pending agent notifications, surfaced through the
//! command palette ("View Notifications") and the HeaderBar button
//! badge.
//!
//! The PURE, GTK-free store — like `palette::PaletteState` /
//! `rollup::project_rollup`, it's unit-tested in isolation. The wiring
//! in `app.rs` composes records (from the live project/tab model) and
//! drives membership off the `has_notification` edges it already
//! receives; this file just keeps the ordered, deduped, capped list.
//!
//! Port of `mac/Sources/Roost/NotificationInbox.swift`. Design: a LIVE
//! inbox, not a history log. One entry per tab, newest-first, capped at
//! [`CAP`]. Invariant: a tab has a row here iff it has a pending
//! notification (modulo cap eviction).

use std::time::Instant;

/// Cap chosen to fit the palette card without scrolling; matches
/// Roost's "tabs don't survive UI quits" ephemerality (no persistence,
/// no history). With >cap pending tabs a tab can lack an inbox row
/// (evicted) — acceptable.
pub const CAP: usize = 10;

/// One pending notification, keyed by `tab_id` (the dedup key + jump
/// target). `title` is composed for display ("<project> · <tab>");
/// `body` is the latest message; `at` drives relative-time + ordering.
#[derive(Debug, Clone)]
pub struct NotificationRecord {
    pub tab_id: i64,
    pub project_id: i64,
    pub title: String,
    pub body: String,
    pub at: Instant,
}

impl NotificationRecord {
    pub fn new(
        tab_id: i64,
        project_id: i64,
        title: impl Into<String>,
        body: impl Into<String>,
    ) -> Self {
        Self {
            tab_id,
            project_id,
            title: title.into(),
            body: body.into(),
            at: Instant::now(),
        }
    }
}

/// Ordered, newest-first ring of pending notifications, one entry per
/// tab, capped at [`CAP`]. Pure value type — no GTK dependencies.
#[derive(Debug, Default)]
pub struct NotificationInbox {
    records: Vec<NotificationRecord>,
}

impl NotificationInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Insert or update the entry for `record.tab_id`. An existing tab's
    /// row is replaced (fresh body/title/time) and moved to the front;
    /// a new tab is prepended, evicting the oldest (tail) past [`CAP`].
    pub fn upsert(&mut self, record: NotificationRecord) {
        self.records.retain(|r| r.tab_id != record.tab_id);
        self.records.insert(0, record);
        if self.records.len() > CAP {
            self.records.truncate(CAP);
        }
    }

    /// Drop the entry for `tab_id` (focus / clear / session-end /
    /// close). No-op if absent.
    pub fn remove(&mut self, tab_id: i64) {
        self.records.retain(|r| r.tab_id != tab_id);
    }

    /// Front-to-back (newest first) for rendering.
    pub fn snapshot(&self) -> &[NotificationRecord] {
        &self.records
    }

    pub fn count(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Tab ids of every pending entry — used by "Clear All" to clear
    /// each tab's notification through the normal false-edge.
    pub fn tab_ids(&self) -> Vec<i64> {
        self.records.iter().map(|r| r.tab_id).collect()
    }
}

/// Compose the project-forward row title: "<project> · <tab>".
pub fn compose_title(project: &str, tab: &str) -> String {
    format!("{project} · {tab}")
}

/// Compact relative-time label ("just now", "2m", "1h", "3d") for the
/// inbox row's trailing text. Mirrors the Mac `relativeTimeLabel`.
pub fn relative_time(elapsed_secs: u64) -> String {
    if elapsed_secs < 60 {
        "just now".to_string()
    } else if elapsed_secs < 3600 {
        format!("{}m", elapsed_secs / 60)
    } else if elapsed_secs < 86400 {
        format!("{}h", elapsed_secs / 3600)
    } else {
        format!("{}d", elapsed_secs / 86400)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(tab_id: i64, body: &str) -> NotificationRecord {
        NotificationRecord::new(tab_id, 1, format!("proj · tab{tab_id}"), body)
    }

    #[test]
    fn upsert_new_prepends_newest_first() {
        let mut inbox = NotificationInbox::new();
        inbox.upsert(rec(1, "a"));
        inbox.upsert(rec(2, "b"));
        inbox.upsert(rec(3, "c"));
        assert_eq!(inbox.count(), 3);
        let ids: Vec<i64> = inbox.snapshot().iter().map(|r| r.tab_id).collect();
        assert_eq!(ids, vec![3, 2, 1]);
    }

    #[test]
    fn upsert_existing_updates_in_place_and_moves_to_front() {
        let mut inbox = NotificationInbox::new();
        inbox.upsert(rec(1, "first"));
        inbox.upsert(rec(2, "second"));
        inbox.upsert(rec(1, "updated"));
        assert_eq!(inbox.count(), 2);
        let ids: Vec<i64> = inbox.snapshot().iter().map(|r| r.tab_id).collect();
        assert_eq!(ids, vec![1, 2]);
        assert_eq!(inbox.snapshot()[0].body, "updated");
    }

    #[test]
    fn caps_at_ten_evicting_oldest() {
        let mut inbox = NotificationInbox::new();
        for i in 1..=10 {
            inbox.upsert(rec(i, "x"));
        }
        assert_eq!(inbox.count(), 10);
        inbox.upsert(rec(11, "x"));
        assert_eq!(inbox.count(), 10);
        let ids: Vec<i64> = inbox.snapshot().iter().map(|r| r.tab_id).collect();
        assert_eq!(ids.first(), Some(&11));
        assert!(!ids.contains(&1), "oldest (tab 1) evicted");
        assert!(ids.contains(&2));
    }

    #[test]
    fn remove_drops_only_that_tab() {
        let mut inbox = NotificationInbox::new();
        inbox.upsert(rec(1, "a"));
        inbox.upsert(rec(2, "b"));
        inbox.remove(1);
        let ids: Vec<i64> = inbox.snapshot().iter().map(|r| r.tab_id).collect();
        assert_eq!(ids, vec![2]);
        inbox.remove(99); // absent → no-op
        assert_eq!(inbox.count(), 1);
    }

    #[test]
    fn remove_all_leaves_empty() {
        let mut inbox = NotificationInbox::new();
        inbox.upsert(rec(1, "a"));
        inbox.upsert(rec(2, "b"));
        inbox.remove(1);
        inbox.remove(2);
        assert!(inbox.is_empty());
        assert_eq!(inbox.count(), 0);
        assert!(inbox.tab_ids().is_empty());
    }

    #[test]
    fn tab_ids_tracks_pending_entries() {
        let mut inbox = NotificationInbox::new();
        inbox.upsert(rec(5, "a"));
        inbox.upsert(rec(6, "b"));
        assert_eq!(inbox.tab_ids(), vec![6, 5]);
    }

    #[test]
    fn compose_title_is_project_forward() {
        assert_eq!(compose_title("roost", "claude"), "roost · claude");
    }

    #[test]
    fn relative_time_buckets() {
        assert_eq!(relative_time(0), "just now");
        assert_eq!(relative_time(30), "just now");
        assert_eq!(relative_time(120), "2m");
        assert_eq!(relative_time(3600), "1h");
        assert_eq!(relative_time(172800), "2d");
    }
}
