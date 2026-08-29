//! The headless drain, end to end from a real child's bytes.
//!
//! This is the one thing a session does that a UI does *for* it: with no
//! terminal to write into, somebody still has to read the PTY and lift
//! the OSC sequences out of it, or a tab's title and directory would
//! freeze at whatever they were when it opened. A scripted shell emits
//! the sequences and `tab.list` is asked what the workspace made of
//! them — no internal state is inspected, because the whole point is
//! that a remote client sees the result.

mod support;

use std::path::Path;

/// OSC 7 (cwd) then OSC 0 (title), in that order and never the reverse:
/// a cwd change re-derives an un-renamed tab's title, so a title emitted
/// first would be overwritten by the directory that followed it. A real
/// shell's prompt hook emits them in this order for the same reason.
///
/// The bytes are written from Rust and `cat`'d rather than built with
/// `printf` escapes: the shells that answer to `/bin/sh` disagree about
/// `\033` versus `\0033`, and a test of the *drain* should not be able to
/// fail because of a quoting dialect. `exec cat` then parks the child on
/// the PTY so the tab is still live when its state is read back.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn osc_from_a_child_moves_the_tab_through_the_headless_drain() {
    let layout = support::Layout::new();
    let launch_cwd = layout.launch_cwd.clone();
    let served = layout.spawn(&launch_cwd);
    let mut client = support::connect(&layout.socket_path()).await;

    let project_id = support::tabs(&mut client).await[0].project_id;
    let reported_cwd = layout.subdir("reported");
    let osc_file = layout.root().join("osc.bin");
    let mut osc = Vec::new();
    osc.extend_from_slice(b"\x1b]7;file://");
    osc.extend_from_slice(reported_cwd.to_string_lossy().as_bytes());
    osc.extend_from_slice(b"\x07");
    osc.extend_from_slice(b"\x1b]0;Scripted Title\x07");
    std::fs::write(&osc_file, &osc).expect("write the OSC fixture");

    // The tab starts somewhere else entirely, so a passing assertion
    // cannot be the opening value.
    let start_cwd = layout.subdir("start");
    let tab = support::open_tab(
        &mut client,
        project_id,
        &start_cwd,
        "",
        &[
            "/bin/sh",
            "-c",
            "cat \"$0\"; exec cat",
            &osc_file.to_string_lossy(),
        ],
    )
    .await;
    assert_eq!(tab.cwd, start_cwd.to_string_lossy());
    assert_ne!(tab.title, "Scripted Title");

    let tabs = support::wait_for_tabs(&mut client, "the OSC title and cwd to land", |tabs| {
        tabs.iter().any(|candidate| {
            candidate.id == tab.id
                && candidate.title == "Scripted Title"
                && same_dir(Path::new(&candidate.cwd), &reported_cwd)
        })
    })
    .await;

    let scanned = tabs
        .iter()
        .find(|candidate| candidate.id == tab.id)
        .expect("the tab is still open");
    assert!(
        !scanned.user_titled,
        "a title from the shell must not claim the manual-rename lock"
    );

    support::session_stop(&mut client).await;
    served.await.expect("join").expect("serve");
}

fn same_dir(a: &Path, b: &Path) -> bool {
    support::canonical(a) == support::canonical(b)
}
