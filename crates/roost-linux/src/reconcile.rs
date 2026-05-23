//! Pure (GTK-free) reconcile planner for full-state resync.
//!
//! When the workspace event bridge reports `Lagged`, the UI can no
//! longer trust its incremental state — it may have missed
//! `TabOpened` / `ProjectDeleted` / reorder events. [`plan`] diffs
//! the live UI membership against a ground-truth snapshot and returns
//! the adds / removes / reorders / active-selection to apply. Keeping
//! it pure makes it unit-testable without a glib main loop or GTK
//! display, and keeps the GTK application code in `app.rs` thin.
//!
//! Only *membership* is read from the current UI — the target
//! ordering and active selection come entirely from the snapshot and
//! are applied idempotently, so a stale local order can't survive a
//! reconcile.

use std::collections::BTreeSet;

use roost_ipc::messages::Project;

/// Lightweight view of what the UI currently shows. Membership only;
/// the planner does not need the current order.
#[derive(Debug, Default, Clone)]
pub struct CurrentView {
    pub project_ids: Vec<i64>,
    /// `(tab_id, project_id)` for every tab currently attached.
    pub tabs: Vec<(i64, i64)>,
}

/// The delta to apply to converge the UI onto the snapshot. Apply in
/// this order: remove projects, then per surviving/added project
/// remove+add tabs, then reorder, then set active.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcilePlan {
    pub projects_to_add: Vec<i64>,
    pub projects_to_remove: Vec<i64>,
    pub tabs_to_add: Vec<i64>,
    pub tabs_to_remove: Vec<i64>,
    /// Post-reconcile sidebar order (every snapshot project, ordered).
    pub project_order: Vec<i64>,
    /// Post-reconcile per-project tab order: `(project_id, tab_ids)`.
    pub tab_order: Vec<(i64, Vec<i64>)>,
    /// `0` when the snapshot has no active selection.
    pub active_project: i64,
    pub active_tab: i64,
}

/// Diff `current` against `snapshot` (ground truth). `snapshot` is
/// expected to already be position-sorted (it comes from
/// `Workspace::snapshot`), and its order is preserved verbatim into
/// `project_order` / `tab_order`.
pub fn plan(current: &CurrentView, snapshot: &[Project]) -> ReconcilePlan {
    let current_projects: BTreeSet<i64> = current.project_ids.iter().copied().collect();
    let snapshot_projects: BTreeSet<i64> = snapshot.iter().map(|p| p.id).collect();

    let projects_to_add: Vec<i64> = snapshot
        .iter()
        .map(|p| p.id)
        .filter(|id| !current_projects.contains(id))
        .collect();
    let projects_to_remove: Vec<i64> = current
        .project_ids
        .iter()
        .copied()
        .filter(|id| !snapshot_projects.contains(id))
        .collect();

    // Tabs in a removed project are torn down with the project
    // (cascade), so they are not listed individually. Only consider
    // current tabs whose project survives.
    let current_tabs_surviving: BTreeSet<i64> = current
        .tabs
        .iter()
        .filter(|(_, pid)| snapshot_projects.contains(pid))
        .map(|(tid, _)| *tid)
        .collect();
    let snapshot_tabs: BTreeSet<i64> = snapshot
        .iter()
        .flat_map(|p| p.tabs.iter().map(|t| t.id))
        .collect();

    let tabs_to_add: Vec<i64> = snapshot
        .iter()
        .flat_map(|p| p.tabs.iter().map(|t| t.id))
        .filter(|id| !current_tabs_surviving.contains(id))
        .collect();
    let tabs_to_remove: Vec<i64> = current
        .tabs
        .iter()
        .filter(|(tid, pid)| snapshot_projects.contains(pid) && !snapshot_tabs.contains(tid))
        .map(|(tid, _)| *tid)
        .collect();

    let project_order: Vec<i64> = snapshot.iter().map(|p| p.id).collect();
    let tab_order: Vec<(i64, Vec<i64>)> = snapshot
        .iter()
        .map(|p| (p.id, p.tabs.iter().map(|t| t.id).collect()))
        .collect();

    let active_tab = snapshot
        .iter()
        .flat_map(|p| p.tabs.iter())
        .find(|t| t.is_active)
        .map(|t| t.id)
        .unwrap_or(0);
    // The active project is the one holding the active tab. If no tab
    // is active (e.g. a project with zero tabs), fall back to the
    // first project so the UI lands somewhere sensible.
    let active_project = snapshot
        .iter()
        .find(|p| p.tabs.iter().any(|t| t.is_active))
        .map(|p| p.id)
        .or_else(|| snapshot.first().map(|p| p.id))
        .unwrap_or(0);

    ReconcilePlan {
        projects_to_add,
        projects_to_remove,
        tabs_to_add,
        tabs_to_remove,
        project_order,
        tab_order,
        active_project,
        active_tab,
    }
}
