//! `Workspace` persistence round-trip. Open against a tempfile,
//! mutate, drop, re-open — projects + next_id must survive. Tabs
//! survive as restore *descriptors* (the layout the UI re-opens as
//! fresh shells), not as live tabs in the workspace.

use roost_engine::persistence::read_state;
use roost_engine::Workspace;
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
    // Tabs are not LIVE after reopen — they come back as restore
    // descriptors the UI re-opens as fresh shells, kept out of the
    // live snapshot.
    assert!(
        p.tabs.is_empty(),
        "live snapshot must carry no tabs at boot"
    );
    let restore = ws2.take_restore_layout().expect("layout present");
    let rp = restore
        .projects
        .iter()
        .find(|rp| rp.project_id == project_id)
        .expect("project in restore layout");
    assert_eq!(rp.tabs.len(), 1, "the saved tab survives as a descriptor");
    assert_eq!(rp.tabs[0].cwd, "/tmp");

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

/// Saved hosts are client-side state the workspace itself never
/// touches, so the risk is an ordinary write-through silently erasing
/// them. Load a file that has them, mutate, and check the rewrite.
#[test]
fn saved_hosts_survive_an_ordinary_rewrite() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    std::fs::write(
        &state_path,
        br#"{
            "next_id": 3,
            "projects": [{
                "id": 1, "name": "Old", "cwd": "/tmp",
                "position": 0, "created_at": 1, "tabs": []
            }],
            "hosts": [
                { "id": "h1", "label": "shed", "target": "test1@localhost",
                  "last_connected": "2026-08-27T00:00:00Z" },
                { "id": "h2", "label": "laptop", "target": "localhost" }
            ]
        }"#,
    )
    .unwrap();

    {
        let ws = Workspace::open(state_path.clone());
        let p = ws.create_project("Roost", "/tmp").unwrap();
        ws.open_tab(p.id, "/tmp", "shell").unwrap();
    }

    let back = read_state(&state_path).unwrap().expect("present");
    assert_eq!(back.hosts.len(), 2, "hosts must survive the rewrite");
    assert_eq!(back.hosts[0].id, "h1");
    assert_eq!(back.hosts[0].label, "shed");
    assert_eq!(back.hosts[0].target, "test1@localhost");
    assert_eq!(
        back.hosts[0].last_connected.as_deref(),
        Some("2026-08-27T00:00:00Z")
    );
    assert_eq!(back.hosts[1].id, "h2");
    assert_eq!(back.hosts[1].target, "localhost");
    assert_eq!(back.hosts[1].last_connected, None);
    assert_eq!(back.projects.len(), 2, "the mutation still landed");

    let ws2 = Workspace::open(state_path);
    ws2.create_project("Third", "/tmp").unwrap();
}

#[test]
fn legacy_state_without_hosts_loads_and_rewrites_empty() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    std::fs::write(
        &state_path,
        br#"{"next_id":5,"projects":[{"id":1,"name":"Old","cwd":"/tmp","position":0,"created_at":1}]}"#,
    )
    .unwrap();

    {
        let ws = Workspace::open(state_path.clone());
        assert_eq!(ws.snapshot().len(), 1);
        ws.create_project("New", "/tmp").unwrap();
    }

    let back = read_state(&state_path).unwrap().expect("present");
    assert!(back.hosts.is_empty(), "absent hosts key loads as none");
}

#[test]
fn fresh_workspace_persists_empty_hosts() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");
    {
        let ws = Workspace::open(state_path.clone());
        ws.create_project("Roost", "/tmp").unwrap();
    }
    let raw = std::fs::read_to_string(&state_path).unwrap();
    assert!(raw.contains("\"hosts\": []"), "hosts key written: {raw}");
    let back = read_state(&state_path).unwrap().expect("present");
    assert!(back.hosts.is_empty());
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
