//! Turn the saved layout in `state.json` back into live tabs.
//!
//! The restore itself is the engine's, shared with the UI's own launch
//! ([`roost_engine::hydrate`]). Every open passes an explicit directory,
//! so the daemon's own cwd (`/`, after `daemonize` moves it there) never
//! reaches a PTY.
//!
//! What is the session's own is how it asks:
//!
//! * At the grid a session opens every tab of its own at
//!   ([`DEFAULT_TAB_COLS`] × [`DEFAULT_TAB_ROWS`]); no window has told it
//!   one yet.
//! * A failed title lock or selection only warns. `take_restore_layout`
//!   is a one-shot and the opens so far are already written through, so
//!   bailing would drop every *later* saved tab permanently, and a session
//!   whose shells are all open but whose selection is off is a working
//!   session with a cosmetic flaw.
//! * [`FirstProject::Withheld`] leaves the workspace empty, and that is a
//!   legitimate state: nothing in a session exits on empty, and the next
//!   *ordinary* connect seeds it anyway (plan 063 §D6), so a caller that
//!   withheld the seed and then failed leaves a session that heals itself.

use anyhow::Result;
use roost_engine::{Hydration, LocalClient, OnRestoreError};

use crate::consts::{FirstProject, DEFAULT_TAB_COLS, DEFAULT_TAB_ROWS};

/// Re-open the saved layout, or seed a first project at `$HOME`.
pub async fn hydrate(client: &LocalClient, first_project: FirstProject) -> Result<()> {
    roost_engine::hydrate(
        client,
        Hydration {
            first_project,
            grid: (DEFAULT_TAB_COLS, DEFAULT_TAB_ROWS),
            on_error: OnRestoreError::Warn,
        },
    )
    .await
}
