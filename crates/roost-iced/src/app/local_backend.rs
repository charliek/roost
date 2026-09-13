//! Where this UI's own tabs run, and what it publishes about that
//! (plan 063 §D1).
//!
//! The [`LocalRoute`] snapshot is the only thing the engine's IPC
//! handler knows about the backend — it answers `identify` from it, and
//! nothing else in the UI may write it. [`App::publish_local_route`]
//! is the single writer; this module owns what it writes.
//!
//! [`App::publish_local_route`]: super::App::publish_local_route

use roost_ipc::{LocalBackendMode, LocalRoute};

/// The snapshot for `mode`.
///
/// Both derived fields hang off the mode: in-process has no slot, so it
/// has neither a slot socket nor a slot selection. The socket comes
/// from the session bundle profile because that is what determines it —
/// the daemon binds that path whether or not anyone is connected to it.
pub(crate) fn route_snapshot(
    mode: LocalBackendMode,
    slot_active: Option<(i64, i64)>,
) -> LocalRoute {
    match mode {
        LocalBackendMode::InProcess => LocalRoute {
            mode,
            slot_socket: None,
            slot_active: None,
        },
        LocalBackendMode::Session => LocalRoute {
            mode,
            slot_socket: roost_ipc::session_socket_path(),
            slot_active,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use roost_ipc::paths::BundleProfile;

    #[test]
    fn the_session_snapshot_carries_the_profile_socket_and_the_slot_selection() {
        let route = route_snapshot(LocalBackendMode::Session, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::Session);
        assert_eq!(route.slot_active, Some((4, 9)));
        assert_eq!(
            route.slot_socket,
            Some(
                BundleProfile::session()
                    .unwrap()
                    .socket_path
                    .to_string_lossy()
                    .into_owned()
            )
        );
    }

    /// In-process has no slot at all, so a selection handed in from a
    /// previous session mode cannot survive the switch back.
    #[test]
    fn the_in_process_snapshot_has_no_slot() {
        let route = route_snapshot(LocalBackendMode::InProcess, Some((4, 9)));
        assert_eq!(route.mode, LocalBackendMode::InProcess);
        assert_eq!(route.slot_socket, None);
        assert_eq!(route.slot_active, None);
    }
}
