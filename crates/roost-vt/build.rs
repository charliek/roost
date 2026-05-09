// Generates Rust bindings for libghostty-vt's C API and links the static
// archive at third_party/ghostty/out/lib/libghostty-vt.a.
//
// Gated behind the `ffi` feature so `cargo check` works before the vendored
// archive has been built. CI runs the third_party/ghostty/build.sh step
// first, then `cargo build --features ffi` (or equivalent) for crates that
// actually need the FFI.

use std::env;
use std::path::PathBuf;

fn main() {
    if env::var_os("CARGO_FEATURE_FFI").is_none() {
        // Stub mode: no FFI requested, nothing to do.
        return;
    }

    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let workspace_root = manifest_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("could not locate workspace root from CARGO_MANIFEST_DIR");
    let ghostty_out = workspace_root.join("third_party/ghostty/out");

    let header = ghostty_out.join("include/ghostty/vt.h");
    let lib_dir = ghostty_out.join("lib");
    let lib_archive = lib_dir.join("libghostty-vt.a");

    if !header.exists() || !lib_archive.exists() {
        panic!(
            "libghostty-vt artifacts not found.\n\
             expected:\n  {}\n  {}\n\
             run third_party/ghostty/build.sh first.",
            header.display(),
            lib_archive.display()
        );
    }

    println!("cargo:rerun-if-changed={}", header.display());
    println!("cargo:rerun-if-changed={}", lib_archive.display());
    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ghostty-vt");

    let bindings = bindgen::Builder::default()
        .header(header.to_string_lossy())
        .clang_arg(format!("-I{}", ghostty_out.join("include").display()))
        .allowlist_function("ghostty_.*")
        .allowlist_type("Ghostty.*")
        .allowlist_type("ghostty_.*")
        .allowlist_var("GHOSTTY_.*")
        .derive_default(true)
        .generate_comments(false)
        .layout_tests(false)
        .generate()
        .expect("bindgen failed");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("ghostty_vt.rs");
    bindings
        .write_to_file(&out_path)
        .expect("could not write bindings");
}
