//! `Workspace` persistence round-trip. Open against a tempfile,
//! mutate, drop, re-open — projects + next_id must survive (tabs
//! intentionally do not).

use roost_linux::daemon::Workspace;
use tempfile::tempdir;

#[test]
fn projects_and_next_id_survive_reopen() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    let (project_id, first_tab_id) = {
        let ws = Workspace::open(state_path.clone());
        let p = ws.create_project("Roost", "/tmp").unwrap();
        let t = ws.open_tab(p.id, "/tmp", "shell").unwrap();
        (p.id, t.id)
        // ws drops here; state.json should be on disk.
    };

    let ws2 = Workspace::open(state_path);
    let projects = ws2.snapshot();
    assert_eq!(projects.len(), 1);
    let p = &projects[0];
    assert_eq!(p.id, project_id);
    assert_eq!(p.name, "Roost");
    assert_eq!(p.cwd, "/tmp");
    // Tabs are NOT restored — the no-session-restore goal.
    assert!(p.tabs.is_empty(), "expected tabs to NOT be restored");

    // New tab id allocations must advance past the previous tab's
    // id so we don't collide with the legacy tab the user might
    // still see references to (e.g. in a hook config). The check
    // against project_id alone wasn't strong enough — open_tab
    // already returns ids greater than any project id in practice,
    // so the meaningful invariant is "ids monotonically advance."
    let next_tab = ws2.open_tab(project_id, "/", "").unwrap();
    assert!(
        next_tab.id > first_tab_id,
        "ids must advance past the previous tab ({}), got {}",
        first_tab_id,
        next_tab.id,
    );
}

#[test]
fn corrupted_state_starts_empty() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    std::fs::write(&state_path, b"not valid json").unwrap();
    let ws = Workspace::open(state_path);
    assert!(ws.snapshot().is_empty(), "corrupt state must start empty");
}

#[test]
fn delete_project_removes_persisted_row() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    let pid = {
        let ws = Workspace::open(state_path.clone());
        let pid = ws.create_project("Roost", "/").unwrap().id;
        ws.delete_project(pid).unwrap();
        pid
    };

    let ws2 = Workspace::open(state_path);
    assert!(
        ws2.snapshot().is_empty(),
        "deleted project must not resurrect from state.json"
    );
    let _ = pid;
}
