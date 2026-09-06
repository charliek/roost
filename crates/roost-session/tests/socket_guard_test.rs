//! The guard that keeps a stopping session from unlinking a successor's
//! socket.
//!
//! The failure this prevents is quiet and bad: session A is stopping,
//! session B has already bound the same path, and A's tidy-up removes
//! B's socket. B stays alive and answering on a socket nobody can dial,
//! and every client sees "no such file". Comparing `(dev, ino)` against
//! what A actually bound is the whole fix.

use roost_session::socket_guard::{unlink_if_ours, SocketIdentity, Unlinked};

#[test]
fn our_own_socket_is_unlinked() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roost.sock");
    std::fs::write(&path, b"").unwrap();

    let recorded = SocketIdentity::of(&path).unwrap();
    assert_eq!(unlink_if_ours(&path, recorded).unwrap(), Unlinked::Removed);
    assert!(!path.exists());
}

/// The successor case. Same path, different inode — which is exactly
/// what a rebind produces.
#[test]
fn a_successor_at_the_same_path_is_left_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roost.sock");
    std::fs::write(&path, b"ours").unwrap();
    let recorded = SocketIdentity::of(&path).unwrap();

    // Replace it via create-then-rename: the replacement exists beside
    // the original before taking its name, so the two can never share
    // an inode. (A plain remove+create let ext4 hand the freed inode
    // straight back, tripping the precondition below on CI.)
    let staged = dir.path().join("roost.sock.next");
    std::fs::write(&staged, b"theirs").unwrap();
    std::fs::rename(&staged, &path).unwrap();
    assert_ne!(
        SocketIdentity::of(&path).unwrap(),
        recorded,
        "the replacement must have a different inode for this test to mean anything"
    );

    assert_eq!(unlink_if_ours(&path, recorded).unwrap(), Unlinked::Foreign);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"theirs",
        "the successor's socket must survive our shutdown"
    );
}

/// Somebody already cleaned up. Not an error — the postcondition (our
/// socket is not at this path) holds.
#[test]
fn an_absent_path_is_not_a_failure() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roost.sock");
    std::fs::write(&path, b"").unwrap();
    let recorded = SocketIdentity::of(&path).unwrap();
    std::fs::remove_file(&path).unwrap();

    assert_eq!(unlink_if_ours(&path, recorded).unwrap(), Unlinked::Absent);
}

/// A symlink dropped over the path reads as the *link's* identity, not
/// its target's, so it can never match — and following it would delete
/// whatever it pointed at.
#[test]
fn a_symlink_over_the_path_never_matches() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("roost.sock");
    let target = dir.path().join("elsewhere");
    std::fs::write(&path, b"precious").unwrap();
    let recorded = SocketIdentity::of(&path).unwrap();

    // Our own file moves aside and becomes the link's target, so the
    // target carries exactly the recorded identity: following the link
    // is the one mistake this test can catch. The link is staged beside
    // the path and renamed over it for the reason the successor test
    // gives — remove+create let ext4 hand the inode straight back.
    std::fs::rename(&path, &target).unwrap();
    assert_eq!(SocketIdentity::of(&target).unwrap(), recorded);
    let staged = dir.path().join("roost.sock.next");
    std::os::unix::fs::symlink(&target, &staged).unwrap();
    std::fs::rename(&staged, &path).unwrap();
    assert_ne!(
        SocketIdentity::of(&path).unwrap(),
        recorded,
        "the symlink must have a different inode for this test to mean anything"
    );

    assert_eq!(unlink_if_ours(&path, recorded).unwrap(), Unlinked::Foreign);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        b"precious",
        "the symlink's target must be untouched"
    );
}
