//! `Workspace` persistence round-trip. Open against a tempfile,
//! mutate, drop, re-open — projects + next_id must survive. Tabs
//! survive as restore *descriptors* (the layout the UI re-opens as
//! fresh shells), not as live tabs in the workspace.

use roost_engine::persistence::read_state;
use roost_engine::{Workspace, WorkspaceError};
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

/// `add_host` mints a fresh id, persists through a reopen, and
/// `remove_host` forgets it the same way — the accessor round-trip the
/// opaque-carry tests above don't cover (those load hosts from a
/// hand-written fixture; these exercise the mutation API itself).
#[test]
fn add_host_persists_and_remove_host_forgets_it_across_reopen() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    let host_id = {
        let ws = Workspace::open(state_path.clone());
        let host = ws.add_host("pop-os", "test1@localhost").unwrap();
        assert_eq!(host.label, "pop-os");
        assert_eq!(host.target, "test1@localhost");
        assert_eq!(host.last_connected, None);
        assert!(!host.id.is_empty());
        assert_eq!(ws.hosts(), vec![host.clone()]);
        host.id
    };

    let ws2 = Workspace::open(state_path.clone());
    let hosts = ws2.hosts();
    assert_eq!(hosts.len(), 1);
    assert_eq!(hosts[0].id, host_id);
    assert_eq!(hosts[0].label, "pop-os");
    drop(ws2);

    let ws3 = Workspace::open(state_path.clone());
    ws3.remove_host(&host_id).unwrap();
    assert!(ws3.hosts().is_empty());
    drop(ws3);

    let ws4 = Workspace::open(state_path);
    assert!(
        ws4.hosts().is_empty(),
        "the removal must have persisted, not just applied in memory"
    );
}

/// Removing an id that isn't there is a `HostNotFound`, not a silent
/// no-op — a client (or `roostctl host remove`) needs to know its id
/// was already gone.
#[test]
fn remove_host_reports_not_found() {
    let dir = tempdir().unwrap();
    let ws = Workspace::open(dir.path().join("state.json"));
    let err = ws.remove_host("does-not-exist").unwrap_err();
    assert!(matches!(err, WorkspaceError::HostNotFound(id) if id == "does-not-exist"));
}

/// `touch_host_connected` stamps `last_connected` and the stamp
/// survives a reopen — the field a fresh `add_host` deliberately leaves
/// `None` until a real connect happens.
#[test]
fn touch_host_connected_stamps_last_connected_and_it_persists() {
    let dir = tempdir().unwrap();
    let state_path = dir.path().join("state.json");

    let host_id = {
        let ws = Workspace::open(state_path.clone());
        let host = ws.add_host("shed", "localhost").unwrap();
        assert_eq!(host.last_connected, None);
        ws.touch_host_connected(&host.id).unwrap();
        let hosts = ws.hosts();
        assert_eq!(hosts.len(), 1);
        assert!(
            hosts[0].last_connected.is_some(),
            "touch must set last_connected"
        );
        host.id
    };

    let ws2 = Workspace::open(state_path);
    let hosts = ws2.hosts();
    assert_eq!(hosts[0].id, host_id);
    assert!(
        hosts[0].last_connected.is_some(),
        "the stamp must survive a reopen"
    );
}

#[test]
fn touch_host_connected_reports_not_found() {
    let dir = tempdir().unwrap();
    let ws = Workspace::open(dir.path().join("state.json"));
    let err = ws.touch_host_connected("ghost").unwrap_err();
    assert!(matches!(err, WorkspaceError::HostNotFound(id) if id == "ghost"));
}

/// Label validation (host-sessions plan §3.1): non-empty, unique
/// case-insensitively among existing saved hosts, and not `local` (any
/// case) — the sidebar's reserved header for the in-process workspace.
#[test]
fn add_host_rejects_an_empty_label() {
    let ws = Workspace::new();
    let err = ws.add_host("", "localhost").unwrap_err();
    assert!(matches!(err, WorkspaceError::HostLabelEmpty));
}

#[test]
fn add_host_rejects_the_reserved_local_label_any_case() {
    let ws = Workspace::new();
    for label in ["local", "Local", "LOCAL", "LoCaL"] {
        let err = ws.add_host(label, "localhost").unwrap_err();
        assert!(
            matches!(err, WorkspaceError::HostLabelReserved),
            "label {label:?} must be rejected as reserved"
        );
    }
}

#[test]
fn add_host_rejects_a_duplicate_label_case_insensitively() {
    let ws = Workspace::new();
    ws.add_host("pop-os", "test1@localhost").unwrap();
    let err = ws.add_host("Pop-OS", "somewhere-else").unwrap_err();
    assert!(matches!(err, WorkspaceError::HostLabelTaken(label) if label == "Pop-OS"));
    // The first host survives untouched — a rejected add must not have
    // mutated the registry.
    assert_eq!(ws.hosts().len(), 1);
}

/// Trimming happens before every check AND before storage, so
/// whitespace can neither smuggle an empty-looking label past the
/// non-empty check nor dodge the reserved / uniqueness comparisons.
#[test]
fn add_host_trims_labels_before_validating_and_storing() {
    let ws = Workspace::new();
    assert!(matches!(
        ws.add_host("   ", "localhost").unwrap_err(),
        WorkspaceError::HostLabelEmpty
    ));
    assert!(matches!(
        ws.add_host(" local ", "localhost").unwrap_err(),
        WorkspaceError::HostLabelReserved
    ));
    ws.add_host("  pop-os  ", "test1@localhost").unwrap();
    assert_eq!(ws.hosts()[0].label, "pop-os", "stored trimmed");
    assert!(matches!(
        ws.add_host("pop-os", "elsewhere").unwrap_err(),
        WorkspaceError::HostLabelTaken(_)
    ));
}

/// Case-insensitive means Unicode folding, not ASCII: "Éclair" and
/// "éclair" render identically in a sidebar header and must collide.
#[test]
fn add_host_rejects_a_duplicate_label_across_unicode_case() {
    let ws = Workspace::new();
    ws.add_host("Éclair", "localhost").unwrap();
    let err = ws.add_host("éclair", "elsewhere").unwrap_err();
    assert!(matches!(err, WorkspaceError::HostLabelTaken(label) if label == "éclair"));
}

/// The pair `to_lowercase` alone cannot separate: "straße" lowercases to
/// itself, so a lowercase-only comparison saves it alongside "STRASSE"
/// — and the sidebar then draws the identical header "STRASSE" twice,
/// which is exactly what the uniqueness rule exists to prevent. The
/// uppercase form IS that header, so it is compared too.
#[test]
fn add_host_rejects_a_duplicate_the_uppercase_header_would_collapse() {
    for (first, second) in [("straße", "STRASSE"), ("STRASSE", "straße")] {
        let ws = Workspace::new();
        ws.add_host(first, "localhost").unwrap();
        let err = ws.add_host(second, "elsewhere").unwrap_err();
        assert!(
            matches!(err, WorkspaceError::HostLabelTaken(label) if label == second),
            "{first:?} then {second:?} must collide"
        );
        assert_eq!(ws.hosts().len(), 1);
    }
}

/// Labels that only *look* alike under one folding still both save —
/// the rule is about the two rendered headers being the same string, not
/// about visual similarity.
#[test]
fn add_host_still_accepts_labels_that_fold_apart() {
    let ws = Workspace::new();
    ws.add_host("pop-os", "localhost").unwrap();
    ws.add_host("pop-os-2", "elsewhere").unwrap();
    assert_eq!(ws.hosts().len(), 2);
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
