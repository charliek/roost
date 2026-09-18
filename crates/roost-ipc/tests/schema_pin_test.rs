//! Pins `docs/reference/api/roost-ipc.schema.json` to
//! [`roost_ipc::schema::bundle_json`], so a wire-shape change is visible
//! in a diff instead of only in a reviewer's memory, and so `roostctl
//! schema` (which `include_str!`s this same file) cannot drift from the
//! generator.
//!
//! Regenerate after an intentional wire change:
//! `ROOST_UPDATE_SCHEMA=1 cargo test -p roost-ipc --test schema_pin_test`
//!
//! The doc lives outside this crate's `CARGO_MANIFEST_DIR` and is
//! absent from a packaged build (a source tree with no `docs/`
//! directory) — that case is a loud skip, not a failure, the same as
//! `docs_name_every_op_test`.

use std::path::PathBuf;

const REGENERATE: &str =
    "regenerate it with `ROOST_UPDATE_SCHEMA=1 cargo test -p roost-ipc --test schema_pin_test`";

fn docs_dir() -> PathBuf {
    let mut p = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(p.pop()); // pop "roost-ipc"
    assert!(p.pop()); // pop "crates"
    p.push("docs");
    p
}

fn schema_path() -> PathBuf {
    docs_dir()
        .join("reference")
        .join("api")
        .join("roost-ipc.schema.json")
}

/// The pinned file's text, or `None` with a loud `eprintln!` when
/// `docs/` is absent (a packaged build).
fn read_pinned(docs: &std::path::Path, path: &std::path::Path) -> Option<String> {
    if !docs.is_dir() {
        eprintln!(
            "SKIP: {} not found - this looks like a packaged build with no \
             `docs/` directory, so the schema pin is skipped.",
            docs.display()
        );
        return None;
    }
    Some(std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!("read {}: {e} - {REGENERATE}", path.display());
    }))
}

#[test]
fn the_pinned_schema_matches_the_bundle() {
    let docs = docs_dir();
    let path = schema_path();
    let generated = roost_ipc::schema::bundle_json();

    if std::env::var_os("ROOST_UPDATE_SCHEMA").is_some() {
        if !docs.is_dir() {
            eprintln!(
                "SKIP: {} not found - nothing to regenerate in a packaged build.",
                docs.display()
            );
            return;
        }
        let parent = path.parent().expect("schema_path has a parent");
        std::fs::create_dir_all(parent)
            .unwrap_or_else(|e| panic!("create {}: {e}", parent.display()));
        std::fs::write(&path, &generated)
            .unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    }

    let Some(pinned) = read_pinned(&docs, &path) else {
        return;
    };

    assert_eq!(
        pinned,
        generated,
        "{} is stale - {REGENERATE}",
        path.display()
    );
}

#[test]
fn read_pinned_skips_loudly_over_a_missing_docs_dir() {
    let bogus = PathBuf::from("/nonexistent/does-not-exist");
    assert!(read_pinned(&bogus, &bogus.join("roost-ipc.schema.json")).is_none());
}
