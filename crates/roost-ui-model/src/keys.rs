//! Host-qualified identity for the UI's tabs and projects.
//!
//! Engine ids are bare `i64`s drawn from one id-space per host, so the
//! moment a second id-space exists — a connected host beside the local
//! workspace, two hosts, or a restarted server reusing ids — a bare id
//! stops identifying anything. These keys are what the UI's maps, feed
//! events and lookups are keyed on instead.

use std::fmt;

/// The one wire encoder both keys share: a local key renders as the bare
/// engine id, so every id on the wire today is byte-identical to what it
/// was before keys existed; another instance's key is prefixed, so two
/// instances' rows can never collide in a client's hands.
fn write_wire(f: &mut fmt::Formatter<'_>, host: HostId, id: i64) -> fmt::Result {
    if host.is_local() {
        write!(f, "{id}")
    } else {
        write!(f, "h{}.{id}", host.raw())
    }
}

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

/// The id-space-qualified form a wire-facing row id carries (palette rows,
/// native menu wire names, the macOS notification identifier).
impl fmt::Display for TabKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_wire(f, self.host, self.tab)
    }
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

    /// The bare id to hand the in-process engine, or `None` when this key
    /// belongs to another instance.
    ///
    /// The engine and the IPC wire are host-unaware by design, so every
    /// call into them is a narrowing — and a key from a dead (or another)
    /// connection epoch names a tab the local workspace has never heard
    /// of. Answering `None` is what stops that numeric id being applied
    /// to whichever local tab happens to share it.
    pub const fn local_tab(self) -> Option<i64> {
        if self.is_local() {
            Some(self.tab)
        } else {
            None
        }
    }

    /// The id-space-qualified form a wire-facing row id carries (palette
    /// rows, native menu wire names), as an owned string.
    ///
    /// [`Display`](std::fmt::Display) is the encoder; prefer interpolating
    /// the key directly when the result is going into a larger string.
    pub fn to_wire(self) -> String {
        self.to_string()
    }

    /// Inverse of [`Self::to_wire`]. `None` for anything malformed — the
    /// empty-sentinel row ids included — and for any non-canonical
    /// spelling: the round-trip `from_wire(s)?.to_wire() == s` is the
    /// contract, so `h0.7` (the local host is always bare), `+7`, or a
    /// leading zero are rejected rather than normalized. A parser that
    /// aliased several spellings onto one key would let a crafted row id
    /// reach a tab its literal text never named.
    pub fn from_wire(text: &str) -> Option<Self> {
        let key = match text.strip_prefix('h') {
            Some(qualified) => {
                let (host, tab) = qualified.split_once('.')?;
                Self::new(HostId::new(host.parse().ok()?), tab.parse().ok()?)
            }
            None => Self::local(text.parse().ok()?),
        };
        (key.to_string() == text).then_some(key)
    }
}

/// One project, qualified the same way [`TabKey`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProjectKey {
    pub host: HostId,
    pub project: i64,
}

/// [`TabKey`]'s wire form, on the same encoder — so a project row id
/// never has to borrow `TabKey`'s to render.
impl fmt::Display for ProjectKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write_wire(f, self.host, self.project)
    }
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

    /// [`TabKey::local_tab`]'s twin, for the ops that take a project.
    pub const fn local_project(self) -> Option<i64> {
        if self.is_local() {
            Some(self.project)
        } else {
            None
        }
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

    /// The narrowing every engine and IPC call goes through: a key from
    /// another instance names nothing the local workspace owns.
    #[test]
    fn only_a_local_key_yields_an_engine_id() {
        assert_eq!(TabKey::local(7).local_tab(), Some(7));
        assert_eq!(TabKey::new(HostId::new(1), 7).local_tab(), None);
        assert_eq!(ProjectKey::local(3).local_project(), Some(3));
        assert_eq!(ProjectKey::new(HostId::new(1), 3).local_project(), None);
    }

    /// The wire form stays byte-identical for local keys — every palette
    /// row id and menu wire name a client sees today is a bare number.
    #[test]
    fn the_wire_form_is_bare_for_local_and_qualified_otherwise() {
        assert_eq!(TabKey::local(42).to_wire(), "42");
        assert_eq!(TabKey::new(HostId::new(3), 42).to_wire(), "h3.42");

        for key in [
            TabKey::local(0),
            TabKey::local(42),
            TabKey::new(HostId::new(1), 42),
            TabKey::new(HostId::new(9), -1),
        ] {
            assert_eq!(TabKey::from_wire(&key.to_wire()), Some(key));
        }
        assert_eq!(TabKey::from_wire("none"), None);
        assert_eq!(TabKey::from_wire(""), None);
        assert_eq!(TabKey::from_wire("h1"), None);
        assert_eq!(TabKey::from_wire("h.1"), None);
        assert_eq!(TabKey::from_wire("hx.1"), None);
        // Non-canonical spellings are rejected, not normalized: the
        // local host is always bare, and integers have one spelling.
        assert_eq!(TabKey::from_wire("h0.7"), None);
        assert_eq!(TabKey::from_wire("+7"), None);
        assert_eq!(TabKey::from_wire("07"), None);
        assert_eq!(TabKey::from_wire("h01.7"), None);
        assert_ne!(
            TabKey::from_wire("h1.42"),
            TabKey::from_wire("42"),
            "the same number on two instances parses to two keys"
        );

        // Projects render on the same encoder, so a menu wire name never
        // has to borrow `TabKey`'s.
        assert_eq!(ProjectKey::local(1).to_string(), "1");
        assert_eq!(ProjectKey::new(HostId::new(4), 1).to_string(), "h4.1");
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
