// Generates Rust bindings for libghostty-vt's C API and links the static
// archive at third_party/ghostty/out/lib/libghostty-vt.a.
//
// Gated behind the `ffi` cargo feature so `cargo check` works before the
// vendored archive has been built. The bindgen build-dependency is also
// optional and pulled in through the same feature, so the default build
// doesn't pay bindgen's compile cost. CI runs the
// third_party/ghostty/build.sh step first, then `cargo build --features ffi`
// (or equivalent) for crates that actually need the FFI.

fn main() {
    // Tell cargo to rerun this script when the ffi feature toggles on/off.
    // Without this, flipping `--features ffi` between invocations wouldn't
    // re-trigger the bindgen step.
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_FFI");

    #[cfg(feature = "ffi")]
    ffi::run();
}

#[cfg(feature = "ffi")]
mod ffi {
    use std::env;
    use std::path::{Path, PathBuf};

    pub fn run() {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let workspace_root = find_workspace_root(&manifest_dir).unwrap_or_else(|| {
            panic!(
                "could not locate workspace root from {} (no third_party/ghostty/ \
                 ancestor found)",
                manifest_dir.display()
            )
        });
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

    /// Walk upward from `start` looking for the workspace root. We anchor
    /// on `third_party/ghostty/` because that's the artifact path build.rs
    /// actually needs; bailing out at the filesystem root if we don't find
    /// it makes drift visible immediately rather than producing a path
    /// to a directory that doesn't exist.
    fn find_workspace_root(start: &Path) -> Option<PathBuf> {
        for ancestor in start.ancestors() {
            if ancestor.join("third_party/ghostty").exists() {
                return Some(ancestor.to_path_buf());
            }
        }
        None
    }
}
