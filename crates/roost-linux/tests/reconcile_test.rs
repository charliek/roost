//! Unit tests for the pure resync reconcile planner
//! (`roost_linux::reconcile`). No GTK / glib main loop required.

use roost_ipc::messages::{Project, Tab, TabState};
use roost_linux::reconcile::{plan, CurrentView};

fn tab(id: i64, project_id: i64, position: i32, is_active: bool) -> Tab {
    Tab {
        id,
        project_id,
        title: String::new(),
        cwd: "/tmp".into(),
        state: TabState::None,
        has_notification: false,
        is_active,
        user_titled: false,
        position,
        created_at: 0,
        last_active: 0,
        hook_active: false,
    }
}

fn project(id: i64, position: i32, tabs: Vec<Tab>) -> Project {
    Project {
        id,
        name: format!("p{id}"),
        cwd: "/tmp".into(),
        position,
        created_at: 0,
        tabs,
    }
}

fn current(project_ids: &[i64], tabs: &[(i64, i64)]) -> CurrentView {
    CurrentView {
        project_ids: project_ids.to_vec(),
        tabs: tabs.to_vec(),
    }
}

#[test]
fn identical_state_yields_empty_membership_delta() {
    let snapshot = vec![project(1, 0, vec![tab(10, 1, 0, true), tab(11, 1, 1, false)])];
    let cur = current(&[1], &[(10, 1), (11, 1)]);

    let p = plan(&cur, &snapshot);

    assert!(p.projects_to_add.is_empty());
    assert!(p.projects_to_remove.is_empty());
    assert!(p.tabs_to_add.is_empty());
    assert!(p.tabs_to_remove.is_empty());
    assert_eq!(p.project_order, vec![1]);
    assert_eq!(p.tab_order, vec![(1, vec![10, 11])]);
    assert_eq!(p.active_project, 1);
    assert_eq!(p.active_tab, 10);
}

#[test]
fn missed_open_events_become_adds() {
    // UI only knows project 1 with one tab; snapshot has a second
    // project and a second tab the UI never saw (dropped events).
    let snapshot = vec![
        project(1, 0, vec![tab(10, 1, 0, true), tab(11, 1, 1, false)]),
        project(2, 1, vec![tab(20, 2, 0, false)]),
    ];
    let cur = current(&[1], &[(10, 1)]);

    let p = plan(&cur, &snapshot);

    assert_eq!(p.projects_to_add, vec![2]);
    assert!(p.projects_to_remove.is_empty());
    assert_eq!(p.tabs_to_add, vec![11, 20]);
    assert!(p.tabs_to_remove.is_empty());
}

#[test]
fn missed_delete_events_become_removes() {
    // UI still shows project 2 and tab 11; snapshot dropped both.
    let snapshot = vec![project(1, 0, vec![tab(10, 1, 0, true)])];
    let cur = current(&[1, 2], &[(10, 1), (11, 1), (20, 2)]);

    let p = plan(&cur, &snapshot);

    assert!(p.projects_to_add.is_empty());
    assert_eq!(p.projects_to_remove, vec![2]);
    assert!(p.tabs_to_add.is_empty());
    // tab 11 is removed individually; tab 20 cascades with project 2.
    assert_eq!(p.tabs_to_remove, vec![11]);
}

#[test]
fn tabs_in_removed_project_are_not_listed_individually() {
    let snapshot = vec![project(1, 0, vec![tab(10, 1, 0, true)])];
    let cur = current(&[1, 2], &[(10, 1), (20, 2), (21, 2)]);

    let p = plan(&cur, &snapshot);

    assert_eq!(p.projects_to_remove, vec![2]);
    assert!(
        p.tabs_to_remove.is_empty(),
        "tabs of a removed project cascade; got {:?}",
        p.tabs_to_remove
    );
}

#[test]
fn reorder_only_produces_snapshot_order() {
    // Same membership, different order in the snapshot.
    let snapshot = vec![
        project(2, 0, vec![tab(21, 2, 0, false), tab(20, 2, 1, true)]),
        project(1, 1, vec![tab(10, 1, 0, false)]),
    ];
    let cur = current(&[1, 2], &[(10, 1), (20, 2), (21, 2)]);

    let p = plan(&cur, &snapshot);

    assert!(p.projects_to_add.is_empty());
    assert!(p.projects_to_remove.is_empty());
    assert!(p.tabs_to_add.is_empty());
    assert!(p.tabs_to_remove.is_empty());
    assert_eq!(p.project_order, vec![2, 1]);
    assert_eq!(p.tab_order, vec![(2, vec![21, 20]), (1, vec![10])]);
    assert_eq!(p.active_project, 2);
    assert_eq!(p.active_tab, 20);
}

#[test]
fn no_active_tab_falls_back_to_first_project() {
    let snapshot = vec![
        project(5, 0, vec![]),
        project(6, 1, vec![tab(60, 6, 0, false)]),
    ];
    let cur = current(&[], &[]);

    let p = plan(&cur, &snapshot);

    assert_eq!(p.active_tab, 0);
    assert_eq!(p.active_project, 5);
}

#[test]
fn empty_snapshot_removes_everything() {
    let snapshot: Vec<Project> = vec![];
    let cur = current(&[1, 2], &[(10, 1), (20, 2)]);

    let p = plan(&cur, &snapshot);

    assert_eq!(p.projects_to_remove, vec![1, 2]);
    assert!(p.projects_to_add.is_empty());
    // Both tabs cascade with their removed projects.
    assert!(p.tabs_to_remove.is_empty());
    assert_eq!(p.active_project, 0);
    assert_eq!(p.active_tab, 0);
}
