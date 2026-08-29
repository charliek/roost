//! The fast-exit race: a command that is already dead by the time its
//! drain looks for it.
//!
//! The supervisor's reap task removes a tab's session *before* it
//! announces the exit, so a drain that polls in that window sees the
//! same "nothing here" a not-yet-spawned tab shows. Without the exit
//! ledger the drain waits out its whole attach deadline and then walks
//! away, leaving a workspace row with no process behind it — a phantom
//! that `tab.list` reports, `session.stop` persists, and the next start
//! resurrects as a fresh shell the user never asked for.
//!
//! `sh -c 'exit 0'` is enough to hit that window, and opening a batch of
//! them without waiting in between makes hitting it reliable rather than
//! occasional.

mod support;

use std::path::PathBuf;
use std::time::Instant;

/// How many throwaway tabs to open. Large enough that at least one is
/// virtually certain to land inside the remove-then-announce window,
/// small enough to stay a fast test.
const BURST: usize = 16;

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
    // is what proves the exit ledger did it rather than the attach
    // deadline, which is a backstop that would also eventually clear a
    // phantom. Half the deadline separates the two cleanly: the ledger
    // path is milliseconds, the backstop is the full 10s. Deliberately
    // unscaled, because the deadline it is distinguishing itself from is
    // unscaled too — scaling this would let a 3x runner slide past the
    // backstop and call it a pass.
    let prompt = roost_session::consts::ATTACH_TIMEOUT / 2;
    assert!(
        elapsed < prompt,
        "rows took {elapsed:?} to close (budget {prompt:?}); that is the attach \
         deadline, not the exit ledger — the supervisor's TabExited never reached the drain"
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
