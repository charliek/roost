# linux/

Packaging for the Linux `.deb`. There is no daemon and no separate
Linux-only crate here — the UI code lives in `crates/roost-linux/`
(gtk4-rs) and `crates/roost-iced/` (iced); this directory only builds
and stages the package.

`scripts/build-deb.sh <version>` builds `roost-iced` in release with
`--features roost-iced/linux-package` plus the `roostctl` CLI, stages
both under `dist/`, and runs [`nfpm`](https://nfpm.goreleaser.com)
against [`../packaging/nfpm.yaml`](../packaging/nfpm.yaml) to emit
`out/roost_<version>_<arch>.deb`. Run it on the target architecture —
no cross-compile.

The `linux-package` feature makes the packaged binary resolve the
production `roost` bundle profile (same socket, `state.json`, and log
dir the GTK package used to own), so the package is a clean upgrade
in place for existing users. See
[Paths & Environment](../docs/reference/paths.md) for the profile
details and [Installation](../docs/getting-started/installation.md)
for build prerequisites and building `roost-linux` (GTK) from source.
