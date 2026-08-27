//! The `validate_runtime_dir` matrix.
//!
//! Every case roots itself at the **canonicalized** tempdir: on macOS
//! `$TMPDIR` lives under `/var`, which is a symlink to `private/var`,
//! and the validator rejects symlink components by design.

use std::fs;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use roost_ipc::{current_euid, validate_runtime_dir};
use tempfile::{tempdir, TempDir};

fn root() -> (TempDir, PathBuf) {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().canonicalize().expect("canonicalize tempdir");
    (dir, path)
}

fn mkdir(path: &Path, mode: u32) {
    fs::DirBuilder::new()
        .mode(mode)
        .create(path)
        .expect("mkdir");
    // Defeat the umask so the test pins the exact bits it means.
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path).expect("stat").mode() & 0o7777
}

#[test]
fn a_missing_leaf_is_created_0700_and_owned_by_us() {
    let (_guard, root) = root();
    let leaf = root.join("session");

    validate_runtime_dir(&leaf).expect("create-if-missing");

    let meta = fs::symlink_metadata(&leaf).expect("stat");
    assert!(meta.is_dir(), "leaf must be a directory");
    assert_eq!(meta.mode() & 0o7777, 0o700);
    assert_eq!(meta.uid(), current_euid());
}

#[test]
fn an_existing_0700_leaf_validates_and_is_idempotent() {
    let (_guard, root) = root();
    let leaf = root.join("session");
    mkdir(&leaf, 0o700);

    validate_runtime_dir(&leaf).expect("existing 0700 dir");
    validate_runtime_dir(&leaf).expect("second call must not change anything");
    assert_eq!(mode_of(&leaf), 0o700);
}

#[test]
fn a_group_or_world_readable_leaf_is_rejected() {
    let (_guard, root) = root();
    let leaf = root.join("session");
    mkdir(&leaf, 0o755);

    let err = validate_runtime_dir(&leaf).expect_err("0755 leaf must be rejected");
    assert!(
        err.to_string().contains("0755"),
        "error should name the offending mode: {err}"
    );
    // Rejected, never silently repaired.
    assert_eq!(mode_of(&leaf), 0o755);
}

#[test]
fn a_symlinked_leaf_is_rejected() {
    let (_guard, root) = root();
    let target = root.join("real");
    mkdir(&target, 0o700);
    let leaf = root.join("session");
    std::os::unix::fs::symlink(&target, &leaf).expect("symlink");

    let err = validate_runtime_dir(&leaf).expect_err("symlinked leaf must be rejected");
    assert!(
        err.to_string().contains("symlink"),
        "error should say why: {err}"
    );
}

#[test]
fn a_symlinked_intermediate_component_is_rejected() {
    let (_guard, root) = root();
    let real = root.join("real");
    mkdir(&real, 0o700);
    let via = root.join("via");
    std::os::unix::fs::symlink(&real, &via).expect("symlink");

    let err =
        validate_runtime_dir(via.join("session")).expect_err("symlinked parent must be rejected");
    assert!(
        err.to_string().contains("symlink"),
        "error should say why: {err}"
    );
    assert!(
        !real.join("session").exists(),
        "a rejected path must not have been created through the symlink"
    );
}

#[test]
fn a_regular_file_at_the_leaf_is_rejected() {
    let (_guard, root) = root();
    let leaf = root.join("session");
    fs::write(&leaf, b"not a directory").expect("write");

    let err = validate_runtime_dir(&leaf).expect_err("file collision must be rejected");
    assert!(
        err.to_string().contains("not a directory"),
        "error should say why: {err}"
    );
}

/// The rule that makes `/tmp` (and every tempdir under it) legal.
#[test]
fn a_sticky_world_writable_ancestor_is_accepted() {
    let (_guard, root) = root();
    let shared = root.join("shared");
    mkdir(&shared, 0o1777);
    assert_eq!(
        mode_of(&shared),
        0o1777,
        "test needs a real sticky 1777 dir"
    );

    let leaf = shared.join("session");
    validate_runtime_dir(&leaf).expect("sticky world-writable ancestor is acceptable");
    assert_eq!(mode_of(&leaf), 0o700);
}

#[test]
fn a_world_writable_ancestor_without_the_sticky_bit_is_rejected() {
    let (_guard, root) = root();
    let open = root.join("open");
    mkdir(&open, 0o777);
    assert_eq!(
        mode_of(&open),
        0o777,
        "test needs a real non-sticky 777 dir"
    );

    let err = validate_runtime_dir(open.join("session"))
        .expect_err("world-writable non-sticky ancestor must be rejected");
    assert!(
        err.to_string().contains("sticky"),
        "error should say why: {err}"
    );
}

#[test]
fn a_group_writable_ancestor_without_the_sticky_bit_is_rejected() {
    let (_guard, root) = root();
    let shared = root.join("shared");
    mkdir(&shared, 0o770);
    assert_eq!(
        mode_of(&shared),
        0o770,
        "test needs a real non-sticky 770 dir"
    );

    let err = validate_runtime_dir(shared.join("session"))
        .expect_err("group-writable non-sticky ancestor must be rejected");
    assert!(
        err.to_string().contains("sticky"),
        "error should say why: {err}"
    );
}

#[test]
fn a_relative_path_is_rejected() {
    let err = validate_runtime_dir("relative/session")
        .expect_err("a relative runtime dir cannot be validated");
    assert!(
        err.to_string().contains("absolute"),
        "error should say why: {err}"
    );
}

#[test]
fn a_missing_parent_is_reported_not_created() {
    let (_guard, root) = root();
    let leaf = root.join("missing").join("session");

    let err = validate_runtime_dir(&leaf).expect_err("a missing ancestor must be reported");
    assert!(
        err.to_string().contains("stat runtime-dir ancestor"),
        "error should name the ancestor step: {err}"
    );
    assert!(!leaf.exists());
}
