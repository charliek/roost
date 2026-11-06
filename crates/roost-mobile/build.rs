//! flutter_rust_bridge's `#[frb]` attributes expand to
//! `cfg_attr(frb_expand, …)`; declare the cfg so `unexpected_cfgs` stays
//! quiet without leaving the workspace lint table.
fn main() {
    println!("cargo::rustc-check-cfg=cfg(frb_expand)");
}
