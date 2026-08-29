//! Turn the saved layout in `state.json` back into live tabs.
//!
//! This is the same bootstrap contract every Roost front end honours,
//! stated once more without a toolkit: the persisted layout is
//! `{title, cwd, position, user_titled}` per tab and nothing else, so a
//! restore re-opens each saved tab **as a fresh shell in its directory**.
//! No process survives a restart, no scrollback does, and the session
//! never pretends otherwise.
//!
//! Three things it must get right, all of which a UI also gets right:
//!
//! * The layout is a one-shot. `take_restore_layout` drains it, and the
//!   live tab map is built from the shells this function opens — the
//!   descriptors are never inserted as rows themselves.
//! * A manual rename outlives the restart. `user_titled` re-asserts the
//!   title lock, so a post-relaunch `cd` does not silently re-derive the
//!   name from the new cwd.
//! * The active selection is restored by *position*, not by id: the ids
//!   in the file belong to the previous run's tabs, which no longer
//!   exist.
//!
//! The one thing it does differently from a UI: a first-ever start seeds
//! its project from the directory the user launched from, not from
//! `$HOME`. A session is started from somewhere on purpose.

use std::path::Path;

use anyhow::Result;
use roost_engine::{LocalClient, RestoreLayout, RestoreTab};
use tracing::warn;

use crate::consts::{DEFAULT_TAB_COLS, DEFAULT_TAB_ROWS};

/// Name given to the project a first-ever start creates. Matches what
/// the UIs seed, so a state file written by one is unsurprising to the
/// other.
const FIRST_PROJECT_NAME: &str = "Roost";

/// Re-open the saved layout, or seed a first project at `launch_cwd`.
///
/// `launch_cwd` is the directory the user ran the start command from,
/// captured before the daemon `chdir`'d away from it. The daemon's own
/// cwd (`/`) must never reach a PTY, which is why every open below
/// passes an explicit directory.
pub async fn hydrate(client: &LocalClient, launch_cwd: &Path) -> Result<()> {
    let mut projects = client.list_projects().await?;
    if projects.is_empty() {
        let cwd = launch_cwd.to_string_lossy();
        projects.push(client.create_project(FIRST_PROJECT_NAME, &cwd).await?);
    }

    let restore = client.workspace.take_restore_layout();
    for project in &projects {
        let saved = restore
            .as_ref()
            .and_then(|layout| {
                layout
                    .projects
                    .iter()
                    .find(|item| item.project_id == project.id)
            })
            .map(|item| item.tabs.as_slice())
            .unwrap_or_default();

        // A project with no saved tabs still opens one, so a session is
        // never restored into a state with nothing to attach to.
        let fallback = [RestoreTab {
            cwd: project.cwd.clone(),
            title: String::new(),
            user_titled: false,
        }];
        let specs = if saved.is_empty() {
            fallback.as_slice()
        } else {
            saved
        };

        for spec in specs {
            let opened = client
                .open_tab(
                    project.id,
                    &spec.cwd,
                    &spec.title,
                    &[],
                    u32::from(DEFAULT_TAB_COLS),
                    u32::from(DEFAULT_TAB_ROWS),
                )
                .await;
            match opened {
                Ok(tab) if spec.user_titled && !spec.title.is_empty() => {
                    // Warn rather than `?` for the same reason the open
                    // does: `take_restore_layout` is a one-shot and the
                    // opens so far have already been written through, so
                    // bailing here would drop every *later* saved tab
                    // permanently. Losing a title lock is a small,
                    // recoverable loss; losing the rest of the layout is
                    // not.
                    if let Err(error) = client.workspace.set_tab_title(tab.id, &spec.title) {
                        warn!(tab_id = tab.id, %error, "restoring the tab's title lock failed");
                    }
                }
                Ok(_) => {}
                // One tab whose directory no longer exists must not cost
                // the user the rest of the session.
                Err(error) => {
                    warn!(project_id = project.id, cwd = %spec.cwd, ?error, "restore tab failed");
                }
            }
        }
    }

    restore_selection(client, restore.as_ref());
    Ok(())
}

/// Re-select the project and tab that were active, matching the tab by
/// its saved *position* — the ids in the file are the previous run's.
///
/// Infallible by choice: a session whose tabs are all open and drained
/// but whose *selection* could not be restored is a working session with
/// a cosmetic flaw, and failing the start over it would throw away the
/// layout that was just rebuilt.
fn restore_selection(client: &LocalClient, restore: Option<&RestoreLayout>) {
    let snapshot = client.workspace.snapshot();
    let Some(project_id) = restore
        .map(|layout| layout.active_project_id)
        .filter(|id| snapshot.iter().any(|project| project.id == *id))
        .or_else(|| snapshot.first().map(|project| project.id))
    else {
        return;
    };
    let position = restore.map_or(0, |layout| layout.active_tab_position.max(0) as usize);
    if let Some(tab_id) = snapshot
        .iter()
        .find(|project| project.id == project_id)
        .and_then(|project| project.tabs.get(position).or_else(|| project.tabs.first()))
        .map(|tab| tab.id)
    {
        if let Err(error) = client.workspace.focus_tab(tab_id) {
            warn!(tab_id, %error, "restoring the active selection failed");
        }
    }
}
