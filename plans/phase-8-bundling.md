# Phase 8: Bundling

**Status**: ⏳ pending
**Exit criteria**:
* Tagged-release CI produces downloadable artifacts:
  * `Roost-<version>.dmg` — signed + notarized Mac `.app` bundle with the Swift UI + the bundled Rust daemon binary.
  * `Roost-<version>.AppImage` — Linux AppImage with the Rust gtk4-rs UI + the daemon.
* The user can double-click the DMG / AppImage and have a working Roost without installing anything else.
* Existing `cargo build -p roost-core` + `swift run Roost` workflow still works for developers.
* Auto-update path documented (Sparkle on Mac? Self-hosted updater feed? Punt to Phase 9 if needed.)

**Mergeability to main**: yes. Bundling artifacts live in CI and a `release/` directory; the source code is unchanged.

## Goal

Make Roost installable for real users without `cargo run` / `swift run`. This is the gate between "works for developers on the refactor branch" and "the user double-clicks an icon and it just works." Past this phase the Rust/Swift surface is shippable.

## Scope

In:
* Mac `.app` build via `xcodebuild` (or SwiftPM + manual `.app` packaging) with all assets, Info.plist, icon, code-signing identity.
* Mac DMG packaging via `create-dmg` or `xcrun` tooling.
* Notarization via `xcrun notarytool` against an Apple Developer credential stored as a GH Actions secret.
* Linux AppImage via `linuxdeploy` + the gtk4 plugin, or `appimagetool` directly.
* The daemon (`roost-core`) bundled inside each artifact. The Mac `.app` ships it as a helper binary under `Contents/MacOS/`; the AppImage carries it alongside the UI.
* CI: a `release-bundle` job triggered on tag push (e.g. `v0.x.0`).

Out:
* Anything related to the App Store or Mac App Store sandboxing. The architecture (UDS, PTY-spawning daemon) is incompatible with the sandbox.
* Homebrew tap / Linux package-manager repos. Distribute via direct DMG / AppImage initially; package-manager support is its own slice.

## Touches Go code?

No. Phase 8 deals exclusively with packaging the Rust + Swift artifacts.

## Step plan

* **Step 1 — Mac `.app` layout.** Decide the bundle structure:
  ```
  Roost.app/
  └── Contents/
      ├── Info.plist
      ├── MacOS/
      │   ├── Roost         # Swift UI, the main executable
      │   └── roost-core    # Daemon, spawned by Roost on launch
      └── Resources/
          ├── Assets.car
          └── ...
  ```
  Roost-the-UI launches `Contents/MacOS/roost-core` if the socket isn't already present. The launch path needs `NSPrivacyAccessedAPI*` keys and other Apple boilerplate.
* **Step 2 — Code-signing + notarization.** Set up the Developer ID Application certificate as a GitHub Actions secret. `codesign --options runtime --entitlements ...` over the `.app`. `notarytool submit` + `stapler staple`.
* **Step 3 — DMG packaging.** `create-dmg` (or `hdiutil` directly) produces a styled DMG with the `.app`, an Applications symlink, and a background image.
* **Step 4 — Linux AppImage.** `cargo build --release -p roost-linux -p roost-core` → `linuxdeploy --plugin gtk` → `appimagetool` → `Roost-<version>-x86_64.AppImage`. The AppImage spawns its own daemon on launch (or expects one already running, same as the Mac path).
* **Step 5 — Versioning.** A single `version` string in `Cargo.toml` workspace metadata + Swift `Package.swift` reading it at build time. Tag pushes (`v0.2.0` etc.) drive the release CI; tag → version sync enforced.
* **Step 6 — Release CI job.** New job in `refactor.yml` (or a separate `release.yml`):
  * Triggered on tag push matching `v*`.
  * On macOS runner: build, sign, notarize, DMG.
  * On Ubuntu runner: build, AppImage.
  * Upload both artifacts to the tag's GitHub Release.
* **Step 7 — Daemon discovery.** Today the UI dials whatever socket `~/Library/Caches/roost/roost.sock` exists. In a bundled scenario where the `.app` spawns its own daemon, decide whether:
  * The `.app` spawns the daemon explicitly on every launch (simple, works offline).
  * The `.app` uses `launchd` / `systemd --user` to manage the daemon lifecycle (idiomatic, requires plist/unit shipping).
  Recommend the explicit-spawn approach for v1; revisit launchd in a follow-up.

## Risks / known gaps

* Notarization is finicky — the first attempt usually surfaces something Apple objects to (entitlements, hardened runtime exceptions, etc.). Budget time for one or two iterations.
* AppImage + GTK4 + libadwaita has known traction issues — libadwaita is relatively young, AppImage's bundled-library approach sometimes conflicts with Adwaita's expectations. Test on a clean Ubuntu 22.04 + Fedora + Arch box before declaring ready.
* The daemon's SQLite file is created at first use under `~/Library/Application Support/roost/roost.db` (Mac) or `~/.local/share/roost/roost.db` (Linux per XDG). Same as today; bundling doesn't change this. Users with existing `roost.db` files from the Go binary should see their state preserved (DL-7).
* Auto-update path is undefined. Sparkle on Mac is the de-facto standard but adds complexity; punting to a follow-up phase is acceptable for v1.

## Follow-ups

* Homebrew tap: post-Phase 8, a `homebrew-roost` repo with a formula installing the DMG.
* Linux package repos: Flatpak is the modern target. Could land as Phase 8.5 or be deferred.
* Windows. Not in scope (vision.md non-goal).
