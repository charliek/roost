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
    let notes_cwd = layout.subdir("notes");

    // ---- run 1 -------------------------------------------------------
    let first = layout.spawn();
    let mut client = support::connect(&layout.socket_path()).await;
    // Whatever directory the seed landed in — $HOME today, but this test
    // is about restore fidelity, not about where a first start seeds.
    let seeded_cwd = support::tab_list(&mut client).await.projects[0].cwd.clone();
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
    let second = layout.spawn();
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
        vec![seeded_cwd],
        "the restored project keeps its own directory, from the saved layout \
         rather than being re-seeded"
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

/// Plan 063 §D8 phase 1: the starter can withhold the first project,
/// and that withholds **only** the seed.
///
/// Two runs, because the sharp edge is the second one. Skipping the
/// seed is one branch of `hydrate`; skipping *hydration* would look
/// identical on an empty workspace and lose the whole saved layout on a
/// populated one — so run 2 comes up `Withheld` over a layout run 1
/// left and must restore every tab of it.
///
/// Run 1 also pins the part the switch depends on: a session that comes
/// up with nothing is a **working, empty** session, not a wedged one.
/// It answers, and it accepts a creation — which is what makes a
/// rolled-back switch's leftover daemon a legitimate state rather than
/// a casualty (the next ordinary connect seeds it, §D6).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_withheld_first_project_skips_the_seed_and_nothing_else() {
    use roost_ipc::session_launch::FirstProject;

    let layout = support::Layout::new();
    let unseeded = || roost_session::SessionConfig {
        first_project: FirstProject::Withheld,
        ..layout.config()
    };

    // ---- run 1: nothing on disk, and nothing seeded -----------------
    let first = layout.spawn_config(unseeded());
    let mut client = support::connect(&layout.socket_path()).await;
    assert!(
        support::tab_list(&mut client).await.projects.is_empty(),
        "the starter said it would fill this workspace itself"
    );

    // Serving, not wedged: it takes work the way any session does.
    let made = support::create_project(&mut client, "migrated", &layout.subdir("migrated")).await;
    let tab = support::open_tab(
        &mut client,
        made,
        &layout.subdir("migrated"),
        "",
        &["/bin/sh", "-c", "exec cat"],
    )
    .await;
    support::set_tab_title(&mut client, tab.id, "Pinned").await;
    support::session_stop(&mut client).await;
    first.await.expect("join").expect("run 1");

    // ---- run 2: still withheld, but there IS a layout now -----------
    let second = layout.spawn_config(unseeded());
    let mut client = support::connect(&layout.socket_path()).await;
    let restored = support::wait_for_tabs(&mut client, "the saved tab to reopen", |tabs| {
        tabs.len() == 1
    })
    .await;
    let projects = support::tab_list(&mut client).await.projects;
    assert_eq!(
        projects.len(),
        1,
        "withholding the seed must not withhold the restore"
    );
    assert_eq!(projects[0].name, "migrated");
    assert_eq!(restored[0].title, "Pinned");
    assert!(restored[0].user_titled, "the title lock survives too");

    support::session_stop(&mut client).await;
    second.await.expect("join").expect("run 2");
}

/// A first-ever start has no layout, so it seeds one — at its own
/// `$HOME`, the same place every UI seeds from (plan 063 §D4), never at
/// the `/` the daemon `chdir`'d to and never at the directory the start
/// command happened to run from.
///
/// This test drives `serve` in-process (no fork — see the module doc),
/// so it shares the real test process's `$HOME` with the code under
/// test rather than mutating the process-global var itself, which would
/// race any other test in this binary that also seeds an empty
/// workspace.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_first_start_seeds_its_project_at_home() {
    let layout = support::Layout::new();
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    let served = layout.spawn();
    let mut client = support::connect(&layout.socket_path()).await;

    let projects = support::tab_list(&mut client).await.projects;
    assert_eq!(projects.len(), 1);
    assert_eq!(projects[0].name, "Untitled 1");
    assert_eq!(
        support::canonical(&projects[0].cwd),
        support::canonical(&home)
    );
    assert_eq!(
        projects[0].tabs.len(),
        1,
        "an empty state opens exactly one tab"
    );
    assert_eq!(
        support::canonical(&projects[0].tabs[0].cwd),
        support::canonical(&home),
        "the seeded tab must inherit the project's directory, never the daemon's"
    );

    // The row saying the right thing is not the same as the shell
    // landing there. A relative redirect only resolves inside the tab's
    // own directory; if `/` had leaked into the spawn it would fail
    // outright.
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
