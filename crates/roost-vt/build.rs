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
        let ghostty_build_script = workspace_root.join("third_party/ghostty/build.sh");
        let sha = parse_ghostty_sha(&ghostty_build_script);
        println!("cargo:rustc-env=ROOST_GHOSTTY_SHA={sha}");
        println!("cargo:rerun-if-changed={}", ghostty_build_script.display());

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

        // Two link directives because they serve different
        // consumers:
        //
        // 1. `rustc-link-arg=<full path to libghostty-vt.a>` is
        //    consumed ONLY by `roost-vt`'s own artifacts (its
        //    tests + any roost-vt-direct binaries). Forces the
        //    test binary to link the static archive even though
        //    macOS `ld` would otherwise prefer the sibling
        //    `libghostty-vt.dylib` in `<out>/lib/` when both
        //    are in the search path. Without this, `cargo test
        //    -p roost-vt --features ffi` produced a binary
        //    referencing `@rpath/libghostty-vt.dylib` that
        //    failed at runtime with `Library not loaded` (GitHub
        //    issue #81 tracks the prior breakage).
        //
        // 2. `rustc-link-lib=static=ghostty-vt` (paired with the
        //    link-search line below) is the standard form that
        //    propagates downstream — `roost-iced`'s binary
        //    inherits this directive and links libghostty-vt
        //    into its own final image. macOS `ld` treats
        //    `static=` as a no-op flag (`-Bstatic/-Bdynamic`
        //    are GNU extensions), but the `-lghostty-vt` it
        //    emits + the search path is enough for downstream
        //    binaries to pick up the archive. The Mac Swift
        //    UI uses a parallel positional-archive trick on
        //    its `linkerSettings`; documented in
        //    `mac/Package.swift`.
        println!("cargo:rustc-link-arg={}", lib_archive.display());
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

    /// Extract the pinned Ghostty commit from
    /// `third_party/ghostty/build.sh`'s `GHOSTTY_SHA="<40 hex>"` line.
    /// Requires exactly one line matching `^GHOSTTY_SHA="[0-9a-f]{40}"` —
    /// zero matches means the script's pin format moved out from under
    /// us, and more than one means the pin is ambiguous. Either way a
    /// silently wrong SHA baked into `libghostty_build()`'s output is
    /// worse than a build failure that names the file to fix.
    fn parse_ghostty_sha(build_script: &Path) -> String {
        let contents = std::fs::read_to_string(build_script).unwrap_or_else(|err| {
            panic!(
                "could not read {} to extract GHOSTTY_SHA: {err}",
                build_script.display()
            )
        });

        const PREFIX: &str = "GHOSTTY_SHA=\"";
        let matches: Vec<&str> = contents
            .lines()
            .filter_map(|line| {
                let rest = line.strip_prefix(PREFIX)?;
                let candidate = rest.get(..40)?;
                (rest[40..].starts_with('"')
                    && candidate
                        .bytes()
                        .all(|b| b.is_ascii_digit() || matches!(b, b'a'..=b'f')))
                .then_some(candidate)
            })
            .collect();

        match matches.as_slice() {
            [sha] => (*sha).to_string(),
            [] => panic!(
                "no line in {} matches ^GHOSTTY_SHA=\"[0-9a-f]{{40}}\" — \
                 the pin format moved; update build.rs's parser to match.",
                build_script.display()
            ),
            multiple => panic!(
                "expected exactly one GHOSTTY_SHA=\"<40 hex>\" line in {}, found {}: {:?}",
                build_script.display(),
                multiple.len(),
                multiple
            ),
        }
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
