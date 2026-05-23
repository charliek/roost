# Phase 8: Bundling

**Status**: ⏳ pending — gated on the M0–M9 inline-core refactor closing.
**Exit criteria**:
* Tagged-release CI produces downloadable artifacts:
  * `Roost-<version>.dmg` — signed + notarized Mac `.app` bundle with the Swift UI + the bundled `roostctl` CLI.
  * `Roost-<version>-x86_64.AppImage` and `Roost-<version>-aarch64.AppImage` — Linux AppImages with the Rust gtk4-rs UI + the bundled `roostctl` CLI.
* The user can double-click the DMG / AppImage and have a working Roost without installing anything else (no Homebrew, no GTK pre-reqs on the Mac side because the .app uses AppKit only).
* Existing `cargo build -p roost-linux` + `swift build` + `mac/scripts/bundle.sh` workflow still works for developers.
* Auto-update path documented (Sparkle on Mac? Self-hosted updater feed? Punt to a Phase 8.x slice if needed.)

**Mergeability to main**: yes. Bundling artifacts live in CI and a `release/` directory; the source code is unchanged except for the `release.yml` workflow + a few packaging-only files.

## Goal

Make Roost installable for real users without `cargo run` / `swift run`. This is the gate between "works for developers on the refactor branch" and "the user double-clicks an icon and it just works." Past this phase the Rust/Swift surface is shippable.

## Architecture context (read first)

Phase 8 follows the M0–M9 inline-core refactor. The shape changed materially from the pre-rewrite plan:

* **No daemon.** Each UI process owns the workspace + PTY supervisor + IPC server in-process. There is no `roost-core` binary to bundle, supervise, or spawn at launch.
* **CLI is `roostctl`**, embedded in the bundle for `claude install` path correctness.
* **Wire format is JSON over UDS**, not gRPC. No `tonic`, no `prost`, no `proto/`, no `grpc-swift`.
* **Persistence is `state.json`**, not SQLite. Single small file under the bundle profile's state dir.
* **bundle.sh already produces a working .app** (M8 added the embedded `roostctl` + ad-hoc codesign with hardened-runtime + the `Roost.entitlements` file). Phase 8 layers Developer ID signing, notarization, and DMG packaging on top.

The current bundle layout (post-M8):

```
Roost.app/
└── Contents/
    ├── Info.plist
    ├── PkgInfo
    ├── MacOS/
    │   └── Roost                          # SwiftPM-built executable
    └── Resources/
        ├── bin/
        │   └── roostctl                   # Cargo-built (release config)
        ├── Roost_Roost.bundle/themes/     # Bundled theme files
        └── AppIcon.icns                   # optional
```

`Roost.entitlements` is currently an empty plist — Roost doesn't need hardened-runtime exceptions for the in-process workspace + the forkpty PTY supervisor + the embedded `roostctl` exec. Notarization will scrutinize each entitlement we add, so the default is "add only when proven necessary."

## Scope

In:
* Developer ID Application certificate (Mac) + Developer ID Installer certificate (if we ship a `.pkg` for the CLI as well, deferrable).
* `xcrun notarytool submit` + `xcrun stapler staple` for the Mac DMG.
* `create-dmg` (or `hdiutil` directly) produces a styled DMG with the `.app`, an Applications symlink, and a background image.
* Linux AppImage via `linuxdeploy --plugin gtk` + `appimagetool`. The bundled binary is `roost-linux` (the gtk4-rs UI), with `roostctl` co-located.
* CI: a `release-bundle` job triggered on tag push (e.g. `v0.x.0`).
* `mac/scripts/bundle.sh release` continues to be the developer-facing entry point; the release CI job calls it with `ROOST_DEVELOPER_ID_IDENTITY=…` set instead of ad-hoc signing.

Out:
* Anything related to the App Store or Mac App Store sandboxing. The architecture (UDS, PTY-spawning) is incompatible with the sandbox.
* Homebrew tap / Linux package-manager repos. Distribute via direct DMG / AppImage initially; package-manager support is its own slice (Phase 8.5 or later).

## Touches Go code?

No. Phase 8 deals exclusively with packaging the Rust + Swift artifacts.

## Step plan

* **Step 0 — `bundle.sh` portability fixes** (Phase 8 prep, may land before Phase 8 proper):
  * Discover `cargo` via `command -v cargo` instead of hardcoding `~/.cargo/bin/cargo`.
  * Discover the SwiftPM bin path via `swift build --show-bin-path -c "${CONFIG}"` instead of hardcoding `arm64-apple-macosx` (x86_64 macOS runners exist; Apple Silicon vs Intel matters).
  * Respect `CARGO_TARGET_DIR` for `roostctl` discovery (CI shared-cache scenarios).
  * Already in: failure-mode hardening for codesign (`ROOST_ALLOW_UNSIGNED=1` bypass; default is exit 1).

* **Step 1 — Developer ID signing layered on bundle.sh.** Replace the M8-era ad-hoc signer (`codesign --sign -`) with a real identity when the env var `ROOST_DEVELOPER_ID_IDENTITY` is set. The two-step inner-then-outer signing order (sign the embedded `roostctl` first, then the .app) is already correct.

* **Step 2 — Notarization wrapper.** A separate script `mac/scripts/notarize.sh` that takes a path to the signed DMG and runs `notarytool submit --keychain-profile … --wait`, then `stapler staple`. The CI job calls this after `bundle.sh release`.

* **Step 3 — DMG packaging.** `mac/scripts/make-dmg.sh` invokes `create-dmg` (or `hdiutil create`) with the signed `.app` + an Applications symlink. Output: `mac/build/Roost-<version>.dmg`.

* **Step 4 — Linux AppImage.** `linux/scripts/appimage.sh` (new) runs:
  1. `cargo build --release -p roost-linux -p roost-cli`.
  2. Assembles an AppDir with `roost-linux` as the main binary + `roostctl` under `usr/bin/`.
  3. `linuxdeploy --appdir … --plugin gtk` to pull in GTK4 + libadwaita.
  4. `appimagetool` to produce the AppImage.
  Builds on both x86_64 + aarch64 runners.

* **Step 5 — Versioning.** A single version string in `Cargo.toml` workspace metadata. `mac/Resources/Info.plist.template` reads it via env var (`ROOST_VERSION`) at bundle time. Tag pushes (`v0.2.0` etc.) set `ROOST_VERSION` to the tag minus the `v`. Tag → version-string sync enforced by a CI assertion.

* **Step 6 — Release CI job.** New `.github/workflows/release.yml`:
  * Triggered on tag push matching `v*`.
  * On macOS runner (matrix: arm64 + x86_64): build, sign, notarize, DMG.
  * On Ubuntu runner (matrix: x86_64 + aarch64): build, AppImage.
  * Upload artifacts to the tag's GitHub Release.
  * Apple Developer ID certificate + notarytool API key live in repo secrets; the workflow only reads them, never logs them.

* **Step 7 — First-launch UX.** The unsigned `.app` from M8 trips Gatekeeper's "downloaded from internet" dialog on first launch. After notarization Step 2 + stapler, that dialog goes away. Verify on a clean macOS account before tagging.

(The pre-rewrite Phase 8 plan had a Step 7 about "daemon discovery." That's now N/A — there is no daemon to discover or spawn.)

## Risks / known gaps

* **Notarization is finicky** — the first attempt usually surfaces something Apple objects to (unexpected entitlements, hardened runtime exceptions, unsigned nested binaries). Budget time for one or two iterations. The current empty `Roost.entitlements` minimizes the surface, but expect Apple to scrutinize the embedded `roostctl` exec path.

* **AppImage + GTK4 + libadwaita** has known traction issues — libadwaita is relatively young, AppImage's bundled-library approach sometimes conflicts with Adwaita's expectations. Test on a clean Ubuntu 22.04 + Fedora + Arch box before declaring ready. The libghostty-vt static archive simplifies one dimension (no dyld @rpath hunt for it), but the GTK side still has the full library zoo.

* **Auto-update path is undefined.** Sparkle on Mac is the de-facto standard but adds complexity (an EdDSA appcast feed, version comparison, framework embed). Punting to a Phase 8.x slice is acceptable for v1; users can re-download from the GitHub Release until then.

* **Architecture-specific Mac builds.** Apple Silicon (`arm64`) is primary; Intel (`x86_64`) is a "nice to have" through Phase 8 — most users on supported macOS are on Apple Silicon by now. A `universal2` binary is an option but doubles the binary size and the signing surface.

## Follow-ups (deferred from Phase 8)

* **Sparkle auto-update.** Mac.
* **Homebrew tap.** A `homebrew-roost` formula installing the DMG.
* **Linux package repos.** Flatpak is the modern target. Flatpak builds need their own manifest and a runtime pinning story.
* **Windows.** Not in scope (vision.md non-goal).
