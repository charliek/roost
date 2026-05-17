// Generates Rust bindings from proto/roost.proto via tonic-build.
//
// The generated code is written under target/.../out/roost.v1.rs and
// pulled in by lib.rs through `tonic::include_proto!`. We don't check
// the generated Rust code into the repo — it's regenerated on every
// build, so drift is impossible by construction. (Swift bindings, by
// contrast, are checked in: see proto/gen-swift.sh.)

use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let proto_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .ok_or("could not locate workspace root from CARGO_MANIFEST_DIR")?
        .join("proto");

    let proto_file = proto_root.join("roost.proto");

    println!("cargo:rerun-if-changed={}", proto_file.display());
    println!("cargo:rerun-if-changed={}", proto_root.display());

    tonic_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(&[proto_file], &[proto_root])?;

    Ok(())
}
