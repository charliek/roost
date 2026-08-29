//! The fast-exit race: a command that is already dead by the time
//! anything looks for it.
//!
//! A workspace row is opened *before* its PTY is spawned, and the
//! supervisor's reap task removes a tab's session *before* it announces
//! the exit — so anything that discovers tabs by polling sees the same
//! "nothing here" for a not-yet-spawned tab and an already-reaped one,
//! and leaves a workspace row with no process behind it: a phantom that
//! `tab.list` reports, `session.stop` persists, and the next start
//! resurrects as a fresh shell the user never asked for.
//!
//! What closes the window now is that the row-close lives on the tab
//! task, which `spawn` creates synchronously — there is no discovery
//! step left to lose the race in. `sh -c 'exit 0'` is enough to hit the
//! window, and opening a batch of them without waiting in between makes
//! hitting it reliable rather than occasional.

mod support;

use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How many throwaway tabs to open. Large enough that at least one is
/// virtually certain to land inside the remove-then-announce window,
/// small enough to stay a fast test.
const BURST: usize = 16;

/// What "promptly" means for a row whose close is published by the tab
/// task itself: the exit and the close are the same step, so this is
/// spawn-and-reap latency for `BURST` shells and nothing else. Any
/// mechanism that had to *discover* the exit would need seconds.
const PROMPT: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_fast_exiting_tab_closes_its_own_row() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;

    let seeded = support::tabs(&mut client).await;
    assert_eq!(seeded.len(), 1);
    let project_id = seeded[0].project_id;
    let scratch = layout.subdir("fast-exit");

    // No waiting between opens: each drain task is still being scheduled
    // while the next child is already spawning and dying.
    let mut opened = Vec::with_capacity(BURST);
    for _ in 0..BURST {
        let tab = support::open_tab(
            &mut client,
            project_id,
            &scratch,
            "",
            &["/bin/sh", "-c", "exit 0"],
        )
        .await;
        opened.push(tab.id);
    }

    let started = Instant::now();
    let remaining = support::wait_for_tabs(&mut client, "every exited tab to close its row", {
        let opened = opened.clone();
        move |tabs| !tabs.iter().any(|tab| opened.contains(&tab.id))
    })
    .await;
    let elapsed = started.elapsed();

    assert_eq!(
        remaining.len(),
        1,
        "only the seeded tab should be left: {remaining:?}"
    );

    // The rows closing at all is the leak guard; closing them *promptly*
    // is what proves the tab task published the exit rather than
    // something backstopping a missed one. Deliberately unscaled: this
    // budget is generous enough that a slow runner cannot reach it
    // without something actually having gone wrong.
    assert!(
        elapsed < PROMPT,
        "rows took {elapsed:?} to close (budget {PROMPT:?}); a row-close that slow \
         did not come from the tab task's own exit path"
    );

    // The phantom's real cost is that it outlives the session, so check
    // the persisted layout too, not just the live one.
    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");

    let state = support::read_state(&layout.state_path());
    let persisted: Vec<PathBuf> = state.projects[0]
        .tabs
        .iter()
        .map(|tab| support::canonical(&tab.cwd))
        .collect();
    assert_eq!(
        persisted,
        vec![support::canonical(&launch_cwd)],
        "an exited tab must not be persisted for the next start to resurrect"
    );
}
