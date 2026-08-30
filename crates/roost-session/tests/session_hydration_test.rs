//! Hydration: what a session does with the layout the last one left,
//! and what it does when there is no layout at all.
//!
//! The contract is the same one every Roost front end honours — saved
//! tabs come back as *fresh shells in their directories*, manual renames
//! survive, and the selection is restored by position — so these tests
//! are the session's proof that it did not quietly invent its own.

mod support;

use roost_ipc::messages::{ops, TabFocusParams, TabFocusResult, WireTabRef};

/// Run 1 builds a layout; run 2 must come back to it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restart_reopens_the_saved_layout() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let notes_cwd = layout.subdir("notes");

    // ---- run 1 -------------------------------------------------------
    let first = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;
    let seeded = support::tabs(&mut client).await;
    assert_eq!(seeded.len(), 1);
    let project_id = seeded[0].project_id;

    // A second tab with a manual rename. `exec cat` parks on the PTY so
    // the tab is still live at flush time, and dies on the hangup.
    let notes = support::open_tab(
        &mut client,
        project_id,
        &notes_cwd,
        "",
        &["/bin/sh", "-c", "exec cat"],
    )
    .await;
    support::set_tab_title(&mut client, notes.id, "Notes").await;

    // Select it, so the restored selection has something to be wrong
    // about: position 1, not the default 0.
    let _: TabFocusResult = client
        .call(
            ops::TAB_FOCUS,
            TabFocusParams {
                tab_id: WireTabRef::Local(notes.id),
            },
        )
        .await
        .expect("tab.focus");

    support::session_stop(&mut client).await;
    first.await.expect("join").expect("run 1");

    // The layout is on disk with the rename recorded as the user's.
    let state = support::read_state(&layout.state_path());
    let saved = &state.projects[0].tabs;
    assert_eq!(saved.len(), 2, "{saved:?}");
    let saved_notes = saved
        .iter()
        .find(|tab| tab.cwd == notes_cwd.to_string_lossy())
        .expect("the renamed tab must be persisted");
    assert_eq!(saved_notes.title, "Notes");
    assert!(
        saved_notes.user_titled,
        "a manual rename must persist its lock"
    );
    assert_eq!(state.active_tab_position, saved_notes.position);

    // ---- run 2 -------------------------------------------------------
    // A different launch directory, to prove the restore comes from the
    // saved layout and not from wherever the second start happened.
    let elsewhere = layout.subdir("elsewhere");
    let second = layout.spawn(&elsewhere);
    let mut client = support::connect(&layout.socket_path()).await;

    let restored = support::wait_for_tabs(&mut client, "both saved tabs to reopen", |tabs| {
        tabs.len() == 2
    })
    .await;
    let project_cwds: Vec<String> = support::tab_list(&mut client)
        .await
        .projects
        .into_iter()
        .map(|project| project.cwd)
        .collect();
    assert_eq!(
        project_cwds,
        vec![launch_cwd.to_string_lossy().into_owned()],
        "the restored project keeps its own directory"
    );

    let notes_again = restored
        .iter()
        .find(|tab| tab.cwd == notes_cwd.to_string_lossy())
        .expect("the renamed tab must reopen in its directory");
    assert_eq!(notes_again.title, "Notes");
    assert!(
        notes_again.user_titled,
        "the title lock must be re-asserted so a later cd cannot overwrite it"
    );
    // A restored tab is a *fresh shell*, not a re-inserted record. The
    // ids differing proves nothing (the workspace mints them from a
    // monotonic counter, so they always differ), but a resize does:
    // `tab.resize` reaches the supervisor and answers `not-found` unless
    // this row has a live PTY behind it.
    support::resize_tab(&mut client, notes_again.id, 100, 30)
        .await
        .expect("a restored tab must have a live PTY behind it");

    let id = support::identify(&mut client).await;
    assert_eq!(
        id.active_tab_id, notes_again.id,
        "the selection is restored by position"
    );

    support::session_stop(&mut client).await;
    second.await.expect("join").expect("run 2");
}

/// A first-ever start has no layout, so it seeds one — from the
/// directory the user launched in, not from the `/` the daemon
/// `chdir`'d to and not from `$HOME`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_start_seeds_its_project_at_the_launch_directory() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;

    let projects = support::tab_list(&mut client).await.projects;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].cwd, launch_cwd.to_string_lossy());
    assert_eq!(
        projects[0].tabs.len(),
        1,
        "an empty state opens exactly one tab"
    );
    assert_eq!(
        support::canonical(&projects[0].tabs[0].cwd),
        support::canonical(&launch_cwd),
        "the seeded tab must inherit the project's directory, never the daemon's"
    );

    // The row saying the right thing is not the same as the shell
    // landing there. A relative redirect only resolves inside the launch
    // directory; if `/` had leaked into the spawn it would fail outright.
    let probe = layout.subdir("probe");
    support::open_tab(
        &mut client,
        projects[0].id,
        &probe,
        "",
        &["/bin/sh", "-c", "pwd > where.txt"],
    )
    .await;
    let reported = support::wait_for_file(&probe.join("where.txt")).await;
    assert_eq!(
        support::canonical(&reported),
        support::canonical(&probe),
        "the shell ran somewhere other than the directory it was given"
    );

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
}
