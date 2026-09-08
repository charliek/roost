//! `tab.dump`'s scrollback half, over the session socket (plan 053 R4).
//!
//! A session has no UI, so these dumps come straight out of the tab
//! task's own server terminal — the path a phone or a `roostctl tab dump
//! --scrollback` against a host takes. Content is laid down with
//! `tab.feed_pty_bytes` on a tab parked on a child that emits nothing,
//! so every row on the screen is one the test wrote, and every wait is a
//! poll against a scaled deadline.

mod support;

use std::time::Instant;

use roost_ipc::messages::{ops, TabDumpResult, MAX_DUMP_SCROLLBACK};
use roost_ipc::IpcClient;

/// The geometry every case seeds and dumps at. Set before the content
/// lands so no reflow moves a row out from under an assertion.
const COLS: u32 = 80;
const ROWS: u32 = 24;

/// More lines than the viewport holds by a wide margin, so history is
/// deep enough that a 50-row ask is not accidentally "all of it".
const SEEDED_LINES: usize = 200;

struct Session {
    layout: support::Layout,
    served: tokio::task::JoinHandle<anyhow::Result<()>>,
    client: IpcClient,
}

impl Session {
    /// A running session with one tab, sized, parked on a quiet child,
    /// and seeded with [`SEEDED_LINES`] numbered lines that have all
    /// reached the screen.
    async fn with_seeded_tab() -> (Self, i64) {
        let layout = support::Layout::new();
        let launch_cwd = layout.launch_cwd.clone();
        let served = layout.spawn(&launch_cwd);
        let mut client = support::connect(&layout.socket_path()).await;
        let project_id = support::tabs(&mut client).await[0].project_id;

        let cwd = layout.subdir("tab");
        let tab = support::open_tab(
            &mut client,
            project_id,
            &cwd,
            "",
            &["/bin/sh", "-c", "exec sleep 300"],
        )
        .await;
        support::resize_tab(&mut client, tab.id, COLS, ROWS)
            .await
            .expect("tab.resize");

        let mut session = Self {
            layout,
            served,
            client,
        };
        session
            .wait_for(tab.id, "the tab to reach 80x24", |dump| {
                (dump.cols, dump.rows) == (COLS, ROWS)
            })
            .await;

        let seed: String = (0..SEEDED_LINES)
            .map(|i| format!("line-{i:03}\r\n"))
            .collect();
        support::feed_pty_bytes(&mut session.client, tab.id, seed.as_bytes()).await;
        session
            .wait_for(tab.id, "the last seeded line to reach the screen", |dump| {
                dump.rows_text
                    .iter()
                    .any(|row| row.contains(&format!("line-{:03}", SEEDED_LINES - 1)))
            })
            .await;
        (session, tab.id)
    }

    async fn wait_for(
        &mut self,
        tab_id: i64,
        what: &str,
        mut predicate: impl FnMut(&TabDumpResult) -> bool,
    ) -> TabDumpResult {
        let deadline = support::deadline();
        loop {
            let dump = support::tab_dump(&mut self.client, tab_id).await;
            if predicate(&dump) {
                return dump;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the screen was {:#?}",
                dump.rows_text
            );
            support::tick().await;
        }
    }

    async fn dump(&mut self, tab_id: i64, scrollback: u32) -> TabDumpResult {
        support::tab_dump_scrollback(&mut self.client, tab_id, scrollback).await
    }

    async fn stop(mut self) {
        support::session_stop(&mut self.client).await;
        self.served.await.expect("join").expect("serve");
        // The layout owns the tempdir holding the socket and the state
        // file, so it is released only once the served task has run its
        // tail.
        drop(self.layout);
    }
}

/// The `NNN` of a `line-NNN` row, so adjacency is checked by number
/// rather than by re-deriving where the viewport happens to sit.
fn line_number(row: &str) -> usize {
    row.trim()
        .strip_prefix("line-")
        .unwrap_or_else(|| panic!("expected a seeded row, got {row:?}"))
        .parse()
        .unwrap_or_else(|_| panic!("expected a numbered row, got {row:?}"))
}

/// A request that omits `scrollback` must come back exactly as it always
/// did: the viewport and no history array at all. Asserted on the raw
/// JSON because "no key" is the wire fact an old client depends on —
/// `scrollback_text` is `skip_serializing_if` empty, `scrollback_rows`
/// is not.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_without_scrollback_returns_the_viewport_and_no_history() {
    let (mut session, tab_id) = Session::with_seeded_tab().await;

    let raw: serde_json::Value = session
        .client
        .call(
            ops::TAB_DUMP,
            serde_json::json!({ "tab_id": tab_id.to_string() }),
        )
        .await
        .expect("tab.dump");

    assert!(
        raw.get("scrollback_text").is_none(),
        "an unasked-for history must not be serialized at all: {raw}"
    );
    let rows = raw["scrollback_rows"]
        .as_u64()
        .expect("scrollback_rows is always serialized");
    assert!(
        rows > 0,
        "the seeded tab has history whether or not it was asked for"
    );
    let rows_text = raw["rows_text"].as_array().expect("rows_text");
    assert_eq!(rows_text.len(), ROWS as usize);

    session.stop().await;
}

/// The contract's core: the history array's last entry is the row
/// immediately above `rows_text[0]`, and the array itself is contiguous.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_with_scrollback_returns_rows_contiguous_with_the_viewport() {
    let (mut session, tab_id) = Session::with_seeded_tab().await;

    let dump = session.dump(tab_id, 50).await;
    assert_eq!(dump.scrollback_text.len(), 50);

    let first_visible = line_number(&dump.rows_text[0]);
    let last_history = line_number(dump.scrollback_text.last().expect("history"));
    assert_eq!(
        last_history + 1,
        first_visible,
        "history's last row must be the one directly above the viewport's first"
    );
    for (index, row) in dump.scrollback_text.iter().enumerate() {
        assert_eq!(
            line_number(row),
            first_visible - 50 + index,
            "history row {index} is out of order: {:#?}",
            dump.scrollback_text
        );
    }

    session.stop().await;
}

/// Asking for more history than the tab retains is answered with what
/// exists — not an error, and not a padded array.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_past_the_end_of_history_returns_what_exists() {
    let (mut session, tab_id) = Session::with_seeded_tab().await;

    let dump = session.dump(tab_id, MAX_DUMP_SCROLLBACK - 1).await;
    assert!(
        dump.scrollback_rows > 0 && dump.scrollback_rows < MAX_DUMP_SCROLLBACK - 1,
        "the fixture must ask for more than it seeded (rows were {})",
        dump.scrollback_rows
    );
    assert_eq!(dump.scrollback_text.len(), dump.scrollback_rows as usize);
    assert_eq!(
        line_number(dump.scrollback_text.last().expect("history")) + 1,
        line_number(&dump.rows_text[0])
    );

    session.stop().await;
}

/// Above the maximum is clamped server-side, never refused — a client
/// asking for "everything" does not know the tab's retention.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dump_above_the_maximum_is_clamped_rather_than_refused() {
    let (mut session, tab_id) = Session::with_seeded_tab().await;

    let dump = support::try_tab_dump(&mut session.client, tab_id, MAX_DUMP_SCROLLBACK * 100)
        .await
        .expect("an oversized scrollback ask is clamped, not refused");
    assert_eq!(dump.scrollback_text.len(), dump.scrollback_rows as usize);
    assert_eq!(
        line_number(dump.scrollback_text.last().expect("history")) + 1,
        line_number(&dump.rows_text[0])
    );

    session.stop().await;
}
