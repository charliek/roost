# Phase 8: Bundling

**Status**: ⏳ pending — gated on the M0–M9 inline-core refactor closing.
**Exit criteria**:
* `git tag v0.x.0 && git push --tags` produces, via CI, signed downloadable artifacts on the tag's GitHub Release:
  * `Roost-<version>.dmg` — signed + notarized Mac `.app` bundle with the Swift UI + the bundled `roostctl` CLI.
  * `roost_<version>_amd64.deb` and `roost_<version>_arm64.deb` — Linux Debian packages with the gtk4-rs UI (`roost-linux`) + `roostctl` + the `.desktop` entry + the icon.
* The release workflow fires `repository_dispatch` at [`charliek/apt-charliek`](https://github.com/charliek/apt-charliek) so the new `.deb` lands in the apt repo within ~5 minutes of the tag push (matches the [shed pattern](https://github.com/charliek/shed/blob/main/.github/workflows/release.yaml)).
* The user can either:
  1. Double-click the DMG (Mac) — Gatekeeper passes the notarized signature, the .app installs without a "downloaded from internet" dialog.
  2. Add `apt.stridelabs.ai` once, then `sudo apt install roost` (Ubuntu Noble / Pop!_OS 24.04+).
  3. For one-off Linux installs, `curl -fLO https://github.com/charliek/roost/releases/download/v<v>/roost_<v>_<arch>.deb && sudo apt install ./roost_<v>_<arch>.deb`.
* `mac/scripts/bundle.sh` + `linux/scripts/build-deb.sh` still work as developer-facing entry points (no goreleaser dependency for local builds).

**Out of scope (explicitly deferred)**:
* **Sparkle auto-update.** Users re-download from the GitHub Release until a separate Sparkle slice lands.
* **Homebrew tap.** Mac users install via the DMG. The `charliek/homebrew-tap` repo (used by other charliek projects' CLIs) is not extended for Roost in this phase.
* **AppImage.** The pre-rewrite Phase 8 plan called for AppImage. The apt-charliek pattern is the user's preferred Linux distribution channel; AppImage doesn't fit it.
* **Mac App Store / Linux package-manager repos beyond apt-charliek.** Flatpak, RPM, etc. land in a separate phase.

**Mergeability to main**: yes. New code is packaging-only (workflows + scripts + a `.desktop` file + an icon). Source code is unchanged except for one Cargo.toml workspace version pin.

## Architecture context (read first)

Phase 8 follows the M0–M9 inline-core refactor. The shape changed materially from the pre-rewrite plan:

* **No daemon.** Each UI process owns the workspace + PTY supervisor + IPC server in-process. There is no `roost-core` binary to bundle, supervise, or spawn at launch.
* **Two binaries per platform.**
  * Mac: `Roost.app` (Swift UI) + embedded `roostctl` under `Contents/Resources/bin/`. The Mac ships ONE artifact (the DMG).
  * Linux: `roost-linux` (gtk4-rs UI) + `roostctl` (CLI). The Linux ships ONE deb that installs both under `/usr/bin/`.
* **Wire format is JSON over UDS**, not gRPC. No `tonic`, no `prost`, no `proto/`, no `grpc-swift`.
* **Persistence is `state.json`**, not SQLite. Single small file under the bundle profile's state dir.
* **bundle.sh already produces a working .app** (M8 added the embedded `roostctl` + ad-hoc codesign with hardened-runtime + the `Roost.entitlements` file). Phase 8 layers Developer ID signing + notarization + DMG packaging on top.

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
        │   └── roostctl                   # cargo-built (release config)
        ├── Roost_Roost.bundle/themes/     # bundled theme files
        └── AppIcon.icns                   # optional
```

`Roost.entitlements` is currently an empty plist — Roost doesn't need hardened-runtime exceptions for the in-process workspace + the forkpty PTY supervisor + the embedded `roostctl` exec. Notarization will scrutinize each entitlement we add, so the default is "add only when proven necessary."

## Deb layout (target)

```
roost_0.2.0_amd64.deb installs:
  /usr/bin/roost-linux                                     # the gtk4-rs UI
  /usr/bin/roostctl                                        # the CLI
  /usr/share/applications/ai.stridelabs.Roost.gtk.desktop  # XDG desktop entry
  /usr/share/icons/hicolor/256x256/apps/roost.png          # app icon (one size; scale via hicolor)
  /usr/share/icons/hicolor/512x512/apps/roost.png
  /usr/share/doc/roost/copyright                           # license text
  /usr/share/doc/roost/README.md                           # short pointer to https://...

Depends:
  libgtk-4-1 (>= 4.10),
  libadwaita-1-0 (>= 1.5),
  libc6 (>= 2.31)

Recommends:
  fonts-noto-color-emoji  # for the prompt's emoji titles

Conflicts: (none)
```

The `.desktop` file mirrors what the GTK UI already announces via `application_id`:
* `Name=Roost`, `GenericName=Terminal Multiplexer`, `Categories=System;TerminalEmulator;`
* `Exec=roost-linux %F`, `Icon=roost`
* `StartupWMClass=ai.stridelabs.Roost.gtk` (matches the GTK Application id so the dock/alt-tab grouping works).

## Scope

In:
* **Mac side**: Developer ID Application certificate, `xcrun notarytool submit` + `xcrun stapler staple`, DMG packaging via `create-dmg`, GH release upload.
* **Linux side**: `nfpm pkg` (or `goreleaser`'s `nfpms` block) building the `.deb`, GH release upload, `repository_dispatch` to `charliek/apt-charliek`.
* CI: a `release.yml` workflow triggered on tag push (`v*`).
* `mac/scripts/bundle.sh release` continues to be the developer-facing local entry point; the release CI job calls it with `ROOST_DEVELOPER_ID_IDENTITY=…` set instead of ad-hoc signing.
* `linux/scripts/build-deb.sh` is the developer-facing local entry point for the deb (so a contributor can `./linux/scripts/build-deb.sh 0.2.0-dev` without pushing a tag).
* `packaging/` directory holds the `.desktop` file, the icon at two sizes, and an `nfpm.yaml` template.

Out:
* Sparkle, Homebrew tap, AppImage, App Store, Flatpak, RPM (see "Out of scope" at the top).
* Code-signing the embedded `roostctl` with a separate certificate. Roost.app's signature covers it.

## Touches Go code?

No. Phase 8 deals exclusively with packaging the Rust + Swift artifacts.

## Step plan

* **Step 0 — bundle.sh portability fixes** (already in flight on `refactor/inline-core`):
  * Discover `cargo` via `command -v cargo` instead of hardcoding `~/.cargo/bin/cargo`. ✅ landed.
  * Discover the SwiftPM bin path via `swift build --show-bin-path -c "${CONFIG}"` instead of hardcoding `arm64-apple-macosx`. ✅ landed.
  * Respect `CARGO_TARGET_DIR` for `roostctl` discovery. ✅ landed.
  * Failure-mode hardening for codesign (`ROOST_ALLOW_UNSIGNED=1` bypass; default exit 1). ✅ landed.

* **Step 1 — Mac Developer ID signing.** Layer onto `bundle.sh`'s codesign step. When `ROOST_DEVELOPER_ID_IDENTITY` is set, use it instead of the ad-hoc `-`. The two-step inner-then-outer signing order (sign the embedded `roostctl` first, then the .app) is already correct.

* **Step 2 — Mac notarization wrapper.** `mac/scripts/notarize.sh <path-to-dmg>` runs `xcrun notarytool submit --keychain-profile … --wait`, then `xcrun stapler staple`. The CI job calls this after `bundle.sh release && make-dmg.sh`.

* **Step 3 — Mac DMG packaging.** `mac/scripts/make-dmg.sh` invokes `create-dmg` (or `hdiutil create`) with the signed `.app` + an Applications symlink. Output: `mac/build/Roost-<version>.dmg`. Optionally use a background image from `mac/Resources/dmg-background.png`.

* **Step 4 — Linux assets.** New `packaging/` directory at the repo root:
  * `packaging/roost.desktop` — XDG desktop entry pointing at `/usr/bin/roost-linux`.
  * `packaging/icons/256/roost.png`, `packaging/icons/512/roost.png` — app icon.
  * `packaging/copyright` — Debian-format copyright file.
  * `packaging/nfpm.yaml` — nfpm config template with `${ROOST_VERSION}` substitution. Builds for `amd64` + `arm64`.

* **Step 5 — Linux build script.** `linux/scripts/build-deb.sh <version>`:
  1. `cargo build --release -p roost-linux -p roost-cli` (Rust toolchain pinned via mise; zig 0.15.x for libghostty-vt; GTK4 + libadwaita system deps).
  2. Stage `roost-linux` + `roostctl` + the `packaging/` assets into a temp dir.
  3. Run `nfpm pkg --packager deb --config packaging/nfpm.yaml --target out/`.
  4. Emit `out/roost_<version>_<arch>.deb`.
  Build runs on both `amd64` and `arm64` runners.

* **Step 6 — Versioning.** A single version string in `Cargo.toml` `[workspace.package]`. `mac/Resources/Info.plist.template` and `packaging/nfpm.yaml` both read it via env var (`ROOST_VERSION`) at packaging time. Tag pushes (`v0.2.0` etc.) set `ROOST_VERSION` to the tag minus the `v`. A CI assertion fails the release if the tag and the `Cargo.toml` version disagree (catches "forgot to bump").

* **Step 7 — Release CI workflow** (`.github/workflows/release.yml`):
  * Triggered on tag push matching `v*`.
  * `mac` job (macos-latest, matrix: arm64): build, sign, notarize, DMG, upload to GH release.
  * `linux` job (ubuntu-latest, matrix: amd64 + arm64): install GTK4 + libadwaita + nfpm + mise (rust + zig); run `linux/scripts/build-deb.sh`; upload deb to GH release.
  * `dispatch-apt-charliek` job (depends on linux): fires `repository_dispatch` at `charliek/apt-charliek` with `client_payload.package=roost` + `client_payload.tag=${{ github.ref_name }}`. Uses an `APT_DISPATCH_TOKEN` secret scoped to `Contents: write` on apt-charliek.
  * Apple Developer ID certificate + notarytool API key live in repo secrets; the workflow only reads them, never logs them.

* **Step 8 — apt-charliek registration.** A one-line addition to `packages.yaml` in the apt-charliek repo:
  ```yaml
  - name: roost
    repo: charliek/roost
    glob: "roost_*.deb"
    include_prerelease: false
  ```
  This lands as a small PR on apt-charliek (not part of this repo's Phase 8 PR).

* **Step 9 — First-launch UX validation.**
  * Mac: download the DMG from a tag → open on a clean macOS account → no Gatekeeper dialog (notarization stapled).
  * Linux: spin up a fresh Ubuntu Noble VM, add the apt repo, `sudo apt install roost`, launch from the app menu, verify a tab opens at `$HOME` with the `👻 /Users/…` prompt rendering correctly + emoji + CJK in OSC titles surviving.

## Risks / known gaps

* **Notarization is finicky.** The first attempt usually surfaces something Apple objects to (unexpected entitlements, hardened runtime exceptions, unsigned nested binaries, libraries linked the wrong way). Budget time for one or two iterations. The current empty `Roost.entitlements` minimizes the surface, but expect Apple to scrutinize the embedded `roostctl` exec path.

* **Cross-arch Linux builds.** `cargo build --release` for `aarch64-unknown-linux-gnu` from an `amd64` runner requires either a runner per arch (preferred) or cross-compilation toolchains. GitHub Actions now offers `ubuntu-24.04-arm` runners; using both runner types matches what shed already does.

* **libghostty-vt static archive on Linux**: `third_party/ghostty/build.sh` needs zig 0.15.x on the runner. The same script is already wired into the gtk-build CI job; reuse the pattern.

* **Architecture-specific Mac builds**. Apple Silicon (`arm64`) is primary; Intel (`x86_64`) is a "nice to have" through Phase 8 — most users on supported macOS are on Apple Silicon by now. A `universal2` binary is an option but doubles the binary size and the signing surface; skip for v1.

* **`.desktop` file's `StartupWMClass` must match the running app's wm_class.** The GTK `Application` is created with `application_id = "ai.stridelabs.Roost.gtk"` per `crates/roost-linux/src/main.rs:41`; mirror that exactly in the `.desktop` file or the dock won't group the launcher icon with the running window.

* **apt-charliek dispatch token rotation.** The `APT_DISPATCH_TOKEN` is a fine-grained PAT scoped to one repo (`charliek/apt-charliek`, `Contents: write`). When it expires (PAT default 90 days), the dispatch silently 401s and the deb stays at the previous version even though the GH Release has it. Calendar reminder; consider a longer-lived setting.

## Follow-ups (deferred from Phase 8)

* **Sparkle auto-update** on Mac. EdDSA appcast feed, version comparison, framework embed.
* **Homebrew tap** for the Mac side (the user has `charliek/homebrew-tap` set up; adding a `roost-cask` formula installing the DMG is a natural extension).
* **Flatpak / RPM** for Linux distros outside the Debian/Ubuntu world.
* **Windows.** Not in scope (vision.md non-goal).
* **Universal2 Mac binary.** If user demand from Intel Macs surfaces.

---

## Execution prompt

Paste the block below into a fresh assistant session to execute Phase 8. The plan above is the spec; this prompt is a self-contained brief.

````
Execute Phase 8 (bundling) of the Roost daemon-removal refactor. The
shaped plan is at /Users/charliek/projects/roost/plans/phase-8-bundling.md
— read that first; it captures the post-M0–M9 architecture context
(no daemon, two binaries per platform, in-process workspace + JSON IPC,
embedded roostctl).

Goal: a tag push of `v0.x.0` produces signed downloadable artifacts on
the GH Release, plus the .deb auto-published via charliek/apt-charliek.

Reference repos (sibling directories, read-only):
* ../shed — Go project that already does the apt-charliek dispatch
  flow. Read .goreleaser.yaml (nfpms: block at line 302) and
  .github/workflows/release.yaml end-to-end before designing anything.
* ../apt-charliek — the apt repo itself. packages.yaml is the
  registration surface; README.md documents the contract.

Constraints (user-stated, hard):
* LINUX: ship a .deb (NOT AppImage) to the GH Release + dispatch to
  ../apt-charliek. Pattern matches shed-server exactly.
* MAC: ship a notarized DMG to the GH Release.
* OUT OF SCOPE for now: Sparkle auto-update; Homebrew tap; AppImage;
  Flatpak; RPM; App Store; universal2 Mac binary.

Order of execution (per the plan's Step plan section):
1. Step 0 portability fixes — already landed on refactor/inline-core
   (commit 1a27410). Confirm bundle.sh release runs locally before
   moving on.
2. Step 4 + 5 Linux assets + build-deb.sh first (cheapest to iterate
   on; Mac signing/notarization needs Apple credentials and is the
   risky step).
3. Step 1–3 Mac Developer ID signing + notarytool + create-dmg. The
   first notarization attempt will almost certainly fail; budget two
   iterations.
4. Step 7 release.yml workflow. Mac job + Linux job + dispatch job.
5. Step 6 versioning enforcement (tag → Cargo.toml workspace version
   match assertion).
6. Step 8 apt-charliek registration — one-line PR on ../apt-charliek
   adding `roost` to packages.yaml.
7. Step 9 first-launch UX validation on a clean macOS account + a
   fresh Ubuntu Noble VM.

Working directory: /Users/charliek/projects/roost
Branch: cut a new `feature/phase-8-bundling` branch from `main` AFTER
PR #78 (the inline-core refactor) has merged. If #78 hasn't merged
yet, ask before starting — Phase 8 depends on the post-M9 surface.

Things to check / get from the user before writing release.yml:
* What's the Developer ID Application certificate's identity name?
  (e.g. "Developer ID Application: Charles Knudsen (TEAMID)"). Will
  live in the ROOST_DEVELOPER_ID_IDENTITY repo secret.
* Has the notarytool keychain profile been set up? (`xcrun notarytool
  store-credentials` once on the runner / locally). The CI uses an
  App-Specific Password + Apple ID + Team ID stored in repo secrets.
* APT_DISPATCH_TOKEN — fine-grained PAT scoped to charliek/apt-charliek
  with Contents: write. Should be added as a repo secret on
  charliek/roost. Without it, the dispatch step silently no-ops and
  the apt repo never picks up the new deb (a missed dispatch will
  self-heal if apt-charliek's publish workflow re-scans on demand —
  shed's pattern documents this).

Validation gate (don't declare done until all four pass):
1. `git tag v0.0.99-test && git push --tags` (a throwaway version)
   produces the DMG + both .deb artifacts on the GH Release.
2. The Mac DMG opens without Gatekeeper interaction on a clean
   macOS account (notarization stapled).
3. The Ubuntu Noble apt repo serves the new .deb within ~5 min of
   the dispatch (poll apt-charliek's Actions page; URL is in the
   dispatch step's `::notice::` output).
4. `sudo apt install roost` on a fresh Ubuntu Noble VM installs the
   package, the launcher icon shows up in the app menu, and clicking
   it opens Roost with a tab at $HOME showing the shell's custom OSC
   prompt (👻 /home/user) rendering correctly.

Skip + file an issue on: any step that would require Sparkle,
Homebrew, AppImage, Flatpak, or RPM tooling — those are explicit
non-goals.
````
