//! Host-qualified identity for the UI's tabs and projects.
//!
//! Engine ids are bare `i64`s drawn from one id-space per host, so the
//! moment a second id-space exists — a connected host beside the local
//! workspace, two hosts, or a restarted server reusing ids — a bare id
//! stops identifying anything. These keys are what the UI's maps, feed
//! events and lookups are keyed on instead.

/// A UI-local id for one connection *instance*, not for a host name.
///
/// Connecting a host mints a fresh `HostId`; reconnecting the same saved
/// host mints another one. That is the whole staleness mechanism: a feed
/// event or a delayed callback minted against a dead connection epoch
/// carries the old instance, so it fails to match anything live and is
/// dropped by the ordinary lookup — no separate generation field, and no
/// chance of an event from the previous epoch landing on a tab of the new
/// one that happens to have the same numeric id.
///
/// The stable saved-host identity (`HostSnapshot.id` in `state.json`) is
/// mapped to whichever `HostId` is current by the connection layer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostId(u32);

impl HostId {
    /// The in-process backend. Reserved: a minted instance id never
    /// takes this value, so a local tab can never be confused with a
    /// host's tab of the same numeric id.
    pub const LOCAL: Self = Self(0);

    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub const fn raw(self) -> u32 {
        self.0
    }

    pub const fn is_local(self) -> bool {
        self.0 == Self::LOCAL.0
    }
}

/// One tab, qualified by the instance whose id-space its `tab` belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TabKey {
    pub host: HostId,
    pub tab: i64,
}

impl TabKey {
    pub const fn new(host: HostId, tab: i64) -> Self {
        Self { host, tab }
    }

    /// The in-process backend's tab — what every id crossing the engine
    /// or IPC boundary is wrapped as, since both stay host-unaware.
    pub const fn local(tab: i64) -> Self {
        Self::new(HostId::LOCAL, tab)
    }

    pub const fn is_local(self) -> bool {
        self.host.is_local()
    }
}

/// One project, qualified the same way [`TabKey`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectKey {
    pub host: HostId,
    pub project: i64,
}

impl ProjectKey {
    pub const fn new(host: HostId, project: i64) -> Self {
        Self { host, project }
    }

    pub const fn local(project: i64) -> Self {
        Self::new(HostId::LOCAL, project)
    }

    pub const fn is_local(self) -> bool {
        self.host.is_local()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::collections::HashMap;
    use std::hash::{Hash, Hasher};

    use super::*;

    fn hash_of<T: Hash>(value: &T) -> u64 {
        let mut hasher = DefaultHasher::new();
        value.hash(&mut hasher);
        hasher.finish()
    }

    #[test]
    fn local_is_the_reserved_zero_instance() {
        assert_eq!(HostId::LOCAL.raw(), 0);
        assert!(HostId::LOCAL.is_local());
        assert!(!HostId::new(1).is_local());
        assert!(TabKey::local(7).is_local());
        assert!(!TabKey::new(HostId::new(1), 7).is_local());
        assert!(ProjectKey::local(7).is_local());
        assert!(!ProjectKey::new(HostId::new(1), 7).is_local());
    }

    #[test]
    fn the_same_numeric_id_on_two_instances_is_two_keys() {
        let local = TabKey::local(7);
        let host = TabKey::new(HostId::new(1), 7);
        let other_host = TabKey::new(HostId::new(2), 7);

        assert_ne!(local, host);
        assert_ne!(host, other_host);
        assert_ne!(hash_of(&local), hash_of(&host));

        let mut map = HashMap::new();
        map.insert(local, "local");
        map.insert(host, "host");
        map.insert(other_host, "other");
        assert_eq!(map.len(), 3, "three id-spaces, three entries");
        assert_eq!(map.get(&TabKey::local(7)), Some(&"local"));
        assert_eq!(map.remove(&host), Some("host"));
        assert_eq!(
            map.get(&TabKey::local(7)),
            Some(&"local"),
            "dropping a host's tab leaves the local tab of the same id"
        );
    }

    #[test]
    fn projects_qualify_the_same_way() {
        assert_ne!(ProjectKey::local(3), ProjectKey::new(HostId::new(1), 3));
        assert_eq!(ProjectKey::local(3), ProjectKey::new(HostId::LOCAL, 3));
    }

    /// `PendingAttachments` keys a `BTreeMap` on `TabKey`, so the order
    /// has to be total and stable: instance first, then numeric id.
    #[test]
    fn keys_order_by_instance_then_id() {
        let mut keys = vec![
            TabKey::new(HostId::new(1), 1),
            TabKey::local(2),
            TabKey::local(1),
        ];
        keys.sort();
        assert_eq!(
            keys,
            vec![
                TabKey::local(1),
                TabKey::local(2),
                TabKey::new(HostId::new(1), 1),
            ]
        );
    }
}
