//! Where the UI's own "local" tabs live, as a snapshot its IPC handler
//! can read (plan 063 §D1).
//!
//! This lives here rather than beside the `local-backend` config key it
//! mirrors because `roost-engine` — which answers `identify` from it —
//! does not depend on `roost-ui-model`. Both crates depend on this one,
//! so this is the only place a type can be shared by them.

use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

use crate::paths::BundleProfile;

/// Which local backend the UI is running its own tabs on.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LocalBackendMode {
    /// PTYs in the UI process — the original arrangement (DL-4).
    #[default]
    InProcess,
    /// PTYs in a `roost-session` daemon on this machine, reached
    /// through *the slot*: the first saved host with a localhost
    /// transport.
    Session,
}

impl LocalBackendMode {
    /// The one spelling: the `local-backend` config value, the
    /// `identify.local_backend` wire string, and what `roostctl doctor`
    /// prints are all this.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InProcess => "in-process",
            Self::Session => "session",
        }
    }
}

impl std::fmt::Display for LocalBackendMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where the local host session listens.
///
/// Resolved from the session bundle profile rather than from any live
/// connection: the path is a property of the profile, so it is knowable
/// even when the slot is down or the UI has yet to publish anything.
/// Both the UI (publishing a route) and the engine (answering
/// `identify`) need the same answer, which is why it is here and not
/// beside either of them.
pub fn session_socket_path() -> Option<String> {
    match BundleProfile::session() {
        Ok(profile) => Some(profile.socket_path.to_string_lossy().into_owned()),
        Err(error) => {
            tracing::warn!(%error, "cannot resolve the local session socket path");
            None
        }
    }
}

/// What the UI currently knows about the local backend, as one
/// immutable snapshot.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LocalRoute {
    pub mode: LocalBackendMode,
    /// The slot's socket path. `None` under
    /// [`LocalBackendMode::InProcess`], where there is no slot.
    pub slot_socket: Option<String>,
    /// The slot's UI-selected `(project_id, tab_id)`, which is what a
    /// bare id means on the UI socket under
    /// [`LocalBackendMode::Session`]. `None` when nothing is selected.
    pub slot_active: Option<(i64, i64)>,
    /// Which phase of a local-backend switch the UI is in, or `None`
    /// when it is idle (plan 063 §D8a).
    ///
    /// A string rather than an enum because the phases are the UI's:
    /// the state machine lives in `roost-iced`, and duplicating its
    /// variants here would be a second spelling that could drift. This
    /// crate only carries the name so `identify` can report it — the
    /// one thing outside the UI process that can see a switch is in
    /// flight, and therefore why a mutation was refused.
    pub switch: Option<&'static str>,
}

/// The shared cell the UI writes and the IPC handler reads.
///
/// A `RwLock<Arc<..>>` rather than an `ArcSwap`: the write rate is one
/// per mode/selection change and the read is a cheap `Arc` clone, which
/// does not justify a new dependency.
#[derive(Debug, Default)]
pub struct LocalBackendCell(RwLock<Arc<LocalRoute>>);

impl LocalBackendCell {
    pub fn new(route: LocalRoute) -> Self {
        Self(RwLock::new(Arc::new(route)))
    }

    /// The current snapshot.
    pub fn load(&self) -> Arc<LocalRoute> {
        Arc::clone(&self.read())
    }

    /// Replace the snapshot. Readers already holding one keep it; they
    /// see the new route on their next [`Self::load`].
    pub fn store(&self, route: LocalRoute) {
        *self.write() = Arc::new(route);
    }

    // A poisoned lock is recovered rather than propagated: the cell
    // holds a whole snapshot that is replaced in one assignment, so a
    // panic elsewhere cannot have left it half-written, and refusing to
    // answer `identify` over it would be the worse failure.
    fn read(&self) -> std::sync::RwLockReadGuard<'_, Arc<LocalRoute>> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, Arc<LocalRoute>> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The spelling is the contract: the config value, the `identify`
    /// field, and the doctor line are the same three strings.
    #[test]
    fn a_mode_spells_itself_the_same_way_everywhere() {
        assert_eq!(LocalBackendMode::InProcess.as_str(), "in-process");
        assert_eq!(LocalBackendMode::Session.as_str(), "session");
        assert_eq!(LocalBackendMode::Session.to_string(), "session");
        assert_eq!(
            serde_json::to_string(&LocalBackendMode::InProcess).unwrap(),
            "\"in-process\""
        );
        assert_eq!(
            serde_json::from_str::<LocalBackendMode>("\"session\"").unwrap(),
            LocalBackendMode::Session
        );
    }

    #[test]
    fn every_handle_on_the_cell_sees_the_last_route_stored() {
        let cell = Arc::new(LocalBackendCell::new(LocalRoute::default()));
        let reader = Arc::clone(&cell);
        assert_eq!(reader.load().mode, LocalBackendMode::InProcess);
        assert_eq!(reader.load().slot_socket, None);

        cell.store(LocalRoute {
            mode: LocalBackendMode::Session,
            slot_socket: Some("/run/roost/session.sock".into()),
            slot_active: Some((3, 7)),
            switch: Some("replaying"),
        });

        let seen = reader.load();
        assert_eq!(seen.mode, LocalBackendMode::Session);
        assert_eq!(seen.slot_socket.as_deref(), Some("/run/roost/session.sock"));
        assert_eq!(seen.slot_active, Some((3, 7)));
        assert_eq!(seen.switch, Some("replaying"));
    }
}
