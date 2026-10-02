# Retiring the Swift app (discovery)

Status: **decided direction, not an implementation plan.** Written
2026-09-27. Companion to [`text-rendering.md`](text-rendering.md), which is
about tuning iced's terminal text; that work does not block this one. File
and line references are as of `main` @ `474e6b5`.

Re-sequenced 2026-10-02: the gap work ships as releases **before** v0.1.0,
with Swift frozen, so each gap is judged against the live Swift app. Plan 073
is the first of those releases; the [Mac feature gaps](#mac-feature-gaps-to-close-before-v010)
and [Phases](#phases) below record what it took and what is left. A
verified removal inventory for the v0.1.0 cutover exists, kept with the
plan-073 working notes outside the repo, for that plan's planning session
to start from.

Implementation plans, and the `docs/development/vision.md` decision-log
entry that supersedes DL-1, DL-5, DL-16 and DL-28, land in the PRs that do
the work, per this folder's [README](README.md).

---

## Decision

Roost stops shipping two Mac apps. The Swift + AppKit `Roost.app` is
retired, and the Rust + iced app becomes the only UI on macOS and Linux.
This is branch (a) of vision.md's *Direction (under evaluation)*.

Pinned by Charlie, 2026-09-27:

| Topic | Decision |
|---|---|
| Identity | The iced Mac build takes over the production identity: bundle id `ai.stridelabs.Roost`, the name `Roost.app`, the Swift update feed (`docs/appcast.xml`) and its Sparkle signing key. |
| Roost-Iced.app | Sunset, along with its feed (`docs/appcast-iced.xml`). It has no real users. How to tell anyone who installed it is an open discussion; release notes may be enough. |
| Releases | Gap-closing releases ship **before** v0.1.0, with the Swift app frozen in them (plan 073 is the first). The first Swift-less release is **v0.1.0**. The legacy snapshot pin is the last release before v0.1.0. |
| Code | The Swift app moves to a private `roost-legacy-swift` repo, modeled on `roost-legacy-go`. |
| Issues | Swift-related issues are closed or rewritten only **after** the work that retires them merges, never before. |
| Deferred | The `mac/` layout, the Mac-only e2e modules and the Swift-facing facade are decided at removal time (see [Deferred](#deferred-to-removal-time)). |

---

## Why

### Duplication and the parity tax

- **Size.** The Swift app is 29,058 lines of source plus 16,743 lines of
  tests. About 16.6k of the source duplicates Rust logic: the workspace
  state machine, IPC handling and wire types, the PTY supervisor, config,
  keybinds, agent state, OSC parsing, git metrics, sprites, and more.
  Only four modules are pinned to Rust by shared fixtures (agent state,
  agent display names, URL detection, word selection). The rest can drift
  silently.
- **Missing operations.** Swift answers 44 of the 74 operations Rust
  dispatches. The 29 missing ones include the whole agent-control surface
  (`project.ensure`, a real event stream, `tab prompt`), host sessions and
  the local session backend, and IME input.
- **Where the work goes.** Of the last 300 non-merge commits (2026-09-06 to
  09-26), 12% touched `mac/` and 60% touched only `crates/`. That 60% is
  features the Swift app never got.
- **Cost when parity is kept.** Measured on recent features, the Swift
  side needed 26–67% as many lines as the Rust side:

  | Feature | Rust lines | Swift lines (source / tests) | Swift as % of Rust |
  |---|---|---|---|
  | `tab.open` activate + `cwd_from_tab` | 1,111 | 91 / 203 | 26% |
  | `tab.dump` scrollback | 790 | 274 / 256 | 67% |
  | Agent-hooks consent (plan 064) | 5,785 | ~1,611 / ~1,067 | 47% |
  | Remember the last-viewed tab | 1,321 | ~370 / ~284 | ~50% |
  | Agents palette shows the agent's name | 383 | 234 | 61% |

- **Structural friction.** Swift can't be built on Linux, where most
  development happens. Every Swift commit needs a `swift build` and
  `swift test` round trip on the mac-mini (plan 072 ran four).

### Ways of keeping Swift that were considered

Sized in full-time engineer-weeks; a part-time pace is about double in
calendar time.

| Option | Swift lines deleted | Cost to reach today's iced features | Extra cost per feature afterwards |
|---|---|---|---|
| Keep today's shape and keep parity | 0 | n/a | +45–60% |
| Swift as a client of `roost-session` over the socket | ~5.5k (the Swift app grows) | 10–14 weeks without remote hosts, 20–30 with | +35–50% |
| Link only the existing shared crates into Swift | ~8–9k | 8–12 weeks, still no host sessions | +30–40% |
| Move iced's app logic into a shared crate, generate Swift bindings with UniFFI, keep Swift views only | ~18–20k | 22–34 weeks | +12–20% |
| Swift chrome hosting a Rust-rendered terminal | ~5.7k | +4–6 weeks on top of another option | a few points off whichever it joins |
| **iced everywhere (chosen)** | all 29k | — | 0% |

None of the Swift options avoids writing every user-visible feature twice.
The best case is one copy of the logic with two sets of views. The session
client code (host connection, local backend, host tabs; about 14k lines in
`crates/roost-iced` with no `iced::` references) is where 58% of iced's
recent lines went, and any Swift client would have to mirror it.

### CI and release cost

- `swift-mac` averages 6.2 minutes and `e2e-mac` 8.3 minutes, about 20% of
  macOS runner time per CI run. Removing them doesn't shorten a run: the
  slowest job is the Linux `iced-build-e2e` at about 24 minutes.
- Neither Swift job failed in the last 100 CI runs. The flaky job is
  `rust-build (macos)`, which stays. The repo is public, so macOS minutes
  cost nothing.
- Releases lose one notarization, one DMG and one appcast. One of the
  three failed v0.0.20 attempts was Swift-only (tests overwrote the signed
  bundle before notarizing).

### Footprint

Measured on the mac-mini with the installed v0.0.19 apps, one tab each,
idle:

| | Roost.app (Swift) | Roost-Iced.app |
|---|---|---|
| Memory footprint | 26 MB | 35 MB |
| Resident memory (includes shared pages) | 87 MiB | 122 MiB |
| Threads | 4 | 28 (tokio runtime) |
| Idle CPU | 0.0% | 0.0% |
| DMG (v0.0.20) | 6 MiB | 16 MiB |

### What the Swift app actually gives

- **Felt daily, fixable in iced:** right-click menus, a working primary
  selection, dropping links or text onto the terminal, pasting files copied
  in Finder, the system accent color, and the palette's native text field.
- **Hard to match:** Core Text glyph rendering (see
  [`text-rendering.md`](text-rendering.md)), VoiceOver (accepted in
  DL-16), and sidebar translucency.
- **Neither app has:** a Services menu, a menu-bar icon, a Dock menu,
  reopen from the Dock, following light/dark mode, Secure Keyboard Entry,
  window restoration.
- **Swift is worse:** no IME, no agent-control surface, no host sessions.

Downloads are small for every artifact: 1 to 8 per release, so a simple
transition is enough.

---

## Mac feature gaps to close before v0.1.0

Already in iced on macOS: a native menu bar (`crates/roost-iced/src/macos/menu.rs`,
routed through the same action path as hotkeys), the Dock badge,
notification banners with click-to-focus, Sparkle, and IME input.

**Disposition after plan 073** (the first gap release; Swift frozen,
2026-10-02). Each item below ends with what became of it. Plan 073's new
issues are #575–#581; its PR also closes #338 and #341, and files #582.

### Must-have

1. **Primary selection on macOS (bug).** `window_clipboard` 0.5.1's macOS
   backend doesn't implement the primary selection, and the default
   `copy-on-select = true` writes only there. Select-to-copy and
   middle-click paste silently do nothing. The Swift app fakes a primary
   selection with a private named pasteboard. Small. New issue.
   **Shipped in plan 073 (#575):** the same private pasteboard (Swift's
   name, so the two apps share it), and middle-click paste enabled on macOS.
2. **Right-click menus** on tab and project rows (#338) and in the terminal
   (#165). Medium.
   **Rows shipped in plan 073 (#338 closed):** tab, project and host-band
   menus, designed rather than ported, from one shared model: native
   `NSMenu` on macOS (ctrl-click too), an iced overlay on Linux. **The
   terminal menu (#165) is deferred**; Swift never had one either, so it is
   new work, not a gap.
3. **Drops and paste.** Dropping links or text onto the terminal (iced only
   accepts dropped files), and pasting files copied in Finder. Small to
   medium. New issue.
   **Pasting files shipped in plan 073 (#576), on both OSes:** a paste of
   copied files does what dropping them does, any file type. **Link and text
   drops are deferred (#302).**
4. **Menu completeness.** Add Agent Hooks…, Settings (⌘,), Help, Enter Full
   Screen and Services; make Cut and Select All work; rebuild key
   equivalents when keybinds change (today they are read once at startup).
   Small. New issue.
   **Shipped in plan 073 (#577):** Agent Hooks…, Settings… (⌘,; opens
   `config.conf` in the text editor), Help → Roost Help, and a full-screen
   item of our own. **Deferred:** Services, and Cut and Select All (iced's
   text inputs handle them). **Dropped:** rebuilding key equivalents on a
   keybind change, because neither app reloads config while running.
5. **The identity cutover** ([below](#identity-cutover)). Medium.
   **Deferred to the v0.1.0 plan.**
6. **Test coverage the Swift jobs provide today.**
   - The Swift-only `EntitlementsTests.swift` guards entitlements files the
     iced bundle keeps (`roostctl.entitlements`, `roost-session.entitlements`)
     and the usage-description strings. Port those checks into the iced
     bundle step before deleting the Swift tests.
   - The Mac-only e2e modules and the four modules the macOS iced cell
     skips (see [Deferred](#deferred-to-removal-time)).

   **Deferred to the v0.1.0 plan**, with the removal.

### Should-have

- **The system accent color.** iced hard-codes `#007aff`. Small.
  **Shipped in plan 073 (#578):** a chrome palette seam, and a
  `chrome-accent = system | #rrggbb` key. On macOS `system` follows the
  control accent live; on Linux it stays `#007aff`.
- **Block cursor drawn over the glyph**, inverting the character under it.
  Today the cursor and selection are drawn under the text. See
  [`text-rendering.md`](text-rendering.md). Small to medium.
  **Shipped in plan 073 (#579):** an opaque cursor that inverts its glyph,
  Ghostty-style selection colors, and the theme's bold color. (The
  cursor sat *under* the glyph because iced paints all quads before text
  within a layer, not because of the order the widget submits them in.)
- **Option-as-Meta (#341).** Most keys can be done inside Roost: iced's
  key event carries the key without modifiers (`key`) plus `modifiers.alt()`.
  The dead-key chords (⌥e, ⌥u, ⌥n, ⌥i, ⌥\`) still go through the macOS
  input method and need winit's option-as-alt setting, which iced 0.14
  doesn't expose. Small, plus a small iced patch for dead keys.
  **Shipped in plan 073 (#341 closed):** `macos-option-as-alt = false | true |
  left | right`, Ghostty's key and values, through libghostty's encoder with
  no winit patch. **The dead-key chords are deferred (#582)** because they
  need the patch. Swift has no Option-as-Meta at all, so this is new for
  both apps, not a gap.
- **Follow system light/dark mode (#164).** Small to medium.
  **Deferred.** Not a Swift gap either: the Swift window forces darkAqua.

Plan 073 also took on terminal text from
[`text-rendering.md`](text-rendering.md), which this list never carried:
macOS text sized like a Mac (#581: 1 pt = 1 px, Swift's cell rule, default
`font-size` 14, hinting off), and JetBrains Mono bundled as the default
terminal font (#580).

### Later

- **A menu-bar icon** (NSStatusItem). About 300–450 lines following the
  `menu.rs` pattern; the data exists (notification inbox count, per-project
  agent rollup). If it should outlive the window, the app moves from
  `iced::application` to `iced::daemon` (available in 0.14) and gains
  window-reopen logic.
- **Dock menu and reopen from the Dock.** winit owns the app delegate, so
  these need Apple Event handlers or runtime methods on winit's delegate
  class. The Swift app never had them.
- Better editing in the palette text field.
- Sidebar translucency (an `NSVisualEffectView` behind the wgpu surface).
- Secure Keyboard Entry.
- An Intel universal build (#117).
- `roostctl` on PATH from the app (#261).
- A CGEvent real-input test harness for Mac (#189 and #285, merged into
  one issue).

### Accepted

- VoiceOver. iced exposes nothing to accessibility (DL-16).

### Corrections found during plan 073 discovery

Checked against the code, 2026-10-02. They amend the lists above.

- **Swift has no terminal right-click menu.** #165 is new work, not a gap.
- **Option-as-Meta and light/dark are not Swift gaps.** Neither app has
  them; the Swift window forces darkAqua.
- **Only Agent Hooks… is a real menu-bar parity gap.** "Rebuild key
  equivalents when keybinds change" is moot: neither app reloads config
  while running.
- **The cursor sits under the glyph.** It is drawn after the text, but iced
  paints all quads before all text within a layer. The "under the text"
  reading above is right; an "over" reading is wrong.
- **Removal-time corrections** (`SnapshotFile` defaults, the CI checks, the
  release steps that read Swift files) live in the verified removal
  inventory, kept with the plan-073 working notes outside the repo. It is
  the starting point for the v0.1.0 planning session.

---

## Identity cutover

**Precedent.** Linux did this in iced migration step M4: iced adopted the
GTK package's production profile through the `linux-package` cargo
feature (`default_profile_kind`, `crates/roost-iced/src/main.rs:265`),
with the same socket and `state.json` and no migration step.

**The Rust seam is reserved.** On macOS the profile comes from a runtime
bundle-id probe, `mac_bundle_default_kind` (`main.rs:289`). Today it maps
every id to the `Iced` profile, and a comment reserves
`ai.stridelabs.Roost` → `BundleProfileKind::Mac` for the cutover. The
change is one match arm plus flipping its pinning test (around `main.rs:1260`).
`app.rs` already titles the `Mac` profile "Roost".

**Bundling needs parameters.** `mac/scripts/bundle-iced.sh` hard-codes
`APP_NAME="Roost-Iced"` and the bundle id, and
`mac/Resources/Info-iced.plist.template` hard-codes the identifier, name,
display name and executable. Dev builds could keep the Roost-Iced identity
so they run beside the production app.

**What carries over for an existing Swift user:**

- `state.json`: Rust's `SnapshotFile` (`crates/roost-engine/src/persistence.rs`)
  defaults every field, so it reads the Swift file.
- `~/.config/roost/config.conf`: already shared.
- Notification permission: macOS keys it to the bundle id plus the signing
  team.
- Lock file names, and any `roostctl` symlink into
  `Roost.app/Contents/Resources/bin/roostctl`.
- Not carried over: sidebar width and visibility, which Swift keeps in
  UserDefaults. iced falls back to its defaults.
- Because a `state.json` and `config.conf` exist, iced won't switch a
  migrating user to the session backend automatically; they stay on
  in-process tabs.

**The target picker becomes correct.** `--target mac` now means the
production Mac app. The hook path, which dials without probing, prefers the
Mac socket on macOS (`crates/roost-ipc/src/target.rs`), so today a hook run
outside a tab dials a dead Swift socket when only Roost-Iced is running.

**Sparkle handoff for existing Swift installs:**

- Installed copies poll `https://charliek.github.io/roost/appcast.xml`,
  on demand only (`SUEnableAutomaticChecks=false`), and verify updates
  against the Swift public key in `mac/Resources/Info.plist.template`.
- The v0.1.0 item on `appcast.xml` points at the iced-built `Roost.app`
  DMG, signed with `SPARKLE_ED_PRIVATE_KEY`. The new app keeps that feed
  and key from then on. The release job can inject them through the
  existing `ROOST_ICED_SPARKLE_FEED_URL` and
  `ROOST_ICED_SPARKLE_ED_PUBLIC_KEY` variables.
- **Test it live on the mini before relying on it:** install the last
  Swift release, point it at a test feed carrying the iced-built Roost.app,
  update, and confirm state, notification permission and the `roostctl`
  path carry over.
- **Never delete `docs/appcast.xml`.** Shipped apps poll it.

**Sunsetting Roost-Iced.app (open discussion).** It has its own bundle id,
so Sparkle can't turn it into Roost.app. Options:

1. Release notes only. Probably enough given the download counts.
2. A final `appcast-iced.xml` item whose release notes say the app has
   moved. That means building and shipping one more Roost-Iced build.
3. Optionally, a one-time import of `~/Library/Application Support/Roost-iced/state.json`
   when the production state directory is empty. Small, but probably not
   worth it.

After the sunset, the iced-only Sparkle key (`mac/keys/roost-iced-sparkle-ed-public-key.txt`)
and `appcast-iced.xml` can be frozen or removed.

**CI checks that flip.** `ci.yml` asserts the iced bundle id is
`ai.stridelabs.Roost.iced` and that the DMG contains no `Roost.app`
("would drag-install over the Swift app"). Both invert.

**Release artifacts.** `Roost-X.dmg` becomes the iced build. That touches
`make-dmg.sh` defaults, `update-appcast.py` defaults, and the asset list in
`publish-release`.

---

## roost-legacy-swift

**Model: `roost-legacy-go`.** It is private, holds one snapshot commit
("Import legacy Go prototype snapshot from charliek/roost @ e7d98661e2e1"),
and its README says it is frozen, names the source commit and date,
describes the layout and explains how to build it. The main repo has no
back-reference.

**Snapshot, not filtered history.** The main repo keeps the full history
of `mac/` (`git log <tag> -- mac/` works forever), so filtering adds
nothing.

**Pin:** the tag of the last release that ships the Swift app.

**What goes in:**

- `mac/Sources/` (including `CGhosttyVT`), `mac/Tests/`, `mac/Package.swift`,
  `mac/Package.resolved`, `mac/README.md`
- Swift-only scripts and resources: `mac/scripts/bundle.sh`,
  `mac/scripts/smoke-launch.sh`, `mac/Resources/Info.plist.template`,
  `mac/Resources/Roost.entitlements`, the Swift copies of the themes and
  shell integration under `mac/Sources/Roost/Resources/`
- `tests/config-fixtures/` (only Swift loads it) if it is retired rather
  than given a Rust loader
- `scripts/smoke/mac_single_instance.sh`, `scripts/smoke/stale_socket_recovery.sh`
- Reference copies (not live workflows) of the `swift-mac` and `e2e-mac`
  CI jobs and the release `mac` job
- A copy of `third_party/ghostty/build.sh` at the pin

**README:** frozen and not maintained; `swift build` works on its own once
libghostty-vt is built at the pinned Ghostty commit; building the full
bundle needs the main repo checked out at the tag, because the bundle
embeds `roostctl`.

---

## Removal inventory

Everything in the main repo that references the Swift app. Items marked
*deferred* are decided at removal time.

### `mac/` tree

- **Moves to the legacy repo:** everything listed in
  [roost-legacy-swift](#roost-legacy-swift).
- **Used by the iced bundle and releases (keep, location deferred):**
  - `mac/scripts/bundle-iced.sh`, `bundle-lib.sh`, `make-dmg.sh`,
    `notarize.sh`, `notarize_test.sh`, `update-appcast.py`
  - `mac/Resources/Info-iced.plist.template`, `Roost-Iced.entitlements`,
    `roostctl.entitlements`, `roost-session.entitlements`, `AppIcon.icns`
  - `mac/AppIcon.icon/`
  - `mac/keys/` (the Swift public key is inline in `Info.plist.template`,
    not a file here)
  - The output folder `mac/build/`, which about 50 references depend on
    (CI, release, `tools/roosttest/ui.py`, `tools/session/dev-session.sh`,
    the Makefile, docs)
- **Swift wording inside the kept scripts:** `bundle-iced.sh`,
  `bundle-lib.sh`, `make-dmg.sh` (default `APP_DIR` is `mac/build/Roost.app`),
  `notarize.sh` (an `/Applications/Roost.app` hint), `update-appcast.py`
  (defaults to `docs/appcast.xml` and `Roost-{v}.dmg`).

### CI (`.github/workflows/ci.yml`)

- Delete the `swift-mac` (`:469`), `e2e-mac` (`:2164`) and `themes-parity`
  (`:287`) jobs.
- **`ci-success`:** remove the three from `needs`, and lower
  `required_jobs=11` (`:2477`) to 8 **in the same commit**.
- Path filters in the `changes` job: the `mac` output exists only for the
  deleted jobs; the `tests`, `macbundle` and `scripts` filters list `mac/`
  paths. **Filters fail silently**: a wrong path just stops CI triggering.
- The `facade` test step (`:399-402`), if the facade goes.
- The two iced macOS checks that assert it is *not* the production app
  (see [Identity cutover](#identity-cutover)).
- Header and comment wording ("Rust + Swift").

### Release (`.github/workflows/release.yml`, `RELEASING.md`)

- Delete the `mac` job. Repurpose or remove the `appcast` job, which
  writes `docs/appcast.xml`.
- **`publish-release`** has `needs: [linux, mac, mac-iced]` (`:1091`) and an
  expected-asset list containing `Roost-${ver}.dmg` (`:1202`). Left as is,
  every release refuses to publish.
- **`appcast-iced`** has `needs: appcast` (`:1417`). It must depend on
  `publish-release` instead, or go away with the iced feed.
- The `SPARKLE_ED_PRIVATE_KEY` secret moves to whichever job publishes the
  production feed. The Apple and certificate secrets are shared with
  `mac-iced` and stay.
- Release e2e: the Swift job ran the whole directory, while `mac-iced`
  runs only `test_smoke.py` and `test_sparkle.py`. Revisit with the test
  evaluation.
- `RELEASING.md` describes both Mac jobs and both feeds.

### Makefile

- Delete `build-mac`, `bundle`, `run-mac`, `test-mac`, `e2e-mac`,
  `e2e-mac-ci`, `smoke-mac`, `smoke-mac-launch`, `themes-check`, and the
  `APP` variable.
- **`test`** (`:221`) depends on `test-mac`, and **`check`** (`:523`) on
  `themes-check`, so both run Swift today.
- `build-all`, the `e2e` dispatch's `mac)` case, and comments naming
  `e2e-mac`.
- `visual-parity` (Swift-vs-iced comparison): delete or keep for iced only.

### `tools/`

- `tools/roosttest/ui.py`: the `mac` target spec, bundle building, launch,
  teardown and log-path helpers. Keep the `open --env` forwarding, which
  the iced bundle launch also uses.
- `tools/roosttest/conftest.py` (`--roost-target` choices), `util.py`,
  `client.py`, `README.md`.
- Per-test `target == "mac"` branches (`test_smoke.py`, `test_palette.py`,
  `test_project_lifecycle.py`, `test_sidebar_pixels.py`, `parity_capture.py`)
  and Swift-only comments in about 20 test files.
- `tools/roosttest_unit/`: delete `test_mac_bundle_staleness.py`; edit
  `test_ui_targets.py` (asserts `TARGETS == ("mac", "iced")`),
  `test_launch_teardown.py`, and `test_osc7_emitters.py` (checks all four
  shell-integration copies). `test_update_appcast.py` and
  `test_sparkle_plist.py` hard-code `mac/scripts` and `mac/Resources`, so
  they follow wherever those move.
- `tools/screenshot/`: the `mac` branch in `lib.sh`, `launch.sh`,
  `quit.sh`, `smoke.sh`, and `parity.py`, which exists to compare Swift
  with iced.
- `tools/perf/render-stats.sh`, `tools/perf/echo-latency.py`: Mac-target
  wording.

### Rust crates

- **Profiles** (`crates/roost-ipc/src/paths.rs`): `BundleProfileKind::Mac`
  is documented as "the Swift Roost.app" and "KEEP IN SYNC with
  BundleProfile.swift". It becomes the production Mac app.
- **Target picker** (`crates/roost-ipc/src/target.rs`) and its callers in
  `roost-cli` (`main.rs`, `error.rs`, `doctor.rs`): wording, and whether an
  `iced` target is still needed on macOS after the sunset.
- **`roostctl` fallbacks that exist because Swift lacks operations:** the
  `wait` polling fallback, the `unsupported` refusal when `project.ensure`
  is missing, the refusal text for `events` and `tab prompt`, the
  `agent ensure --json` contract the Swift app spawns (and its test
  `the_argv_the_mac_app_spawns_still_parses`), and absent-tolerant
  `identify` fields. They also serve older servers, so reword first and
  drop later.
- **Facade** (`crates/roost-engine/src/facade.rs`, 814 lines; the `facade`
  feature in `crates/roost-engine/Cargo.toml`): deferred.
- About 60 Rust files carry Swift-parity comments (the most in
  `macos/menu.rs`, `roost-ipc/src/messages.rs`, `macos/notifications.rs`).
  Optional cleanup. Also `crates/roost-vt/build.rs` and
  `third_party/ghostty/build.sh`, which mention `Package.swift`.

### Fixtures and duplicated resources

- Keep, because Rust loads them: `tests/ipc-vectors/`,
  `tests/agent-state-fixtures/`, `tests/agent-display-name-fixtures/`,
  `tests/word-fixtures/`, `tests/url-fixtures/`. Their READMEs describe the
  Swift side and need editing.
- `tests/config-fixtures/`: only Swift loads it (deferred).
- Themes: `crates/roost-ui-model/src/resources/themes` is canonical, and
  the Swift copy is byte-identical. The copy and its guard go.
- Shell integration: `crates/roost-engine/resources/shell-integration/` is
  canonical. The Swift copies already differ.

### Docs (`docs/`)

- `reference/ipc.md`: 55 mentions, including the Swift-limitations
  paragraph near the top and the per-operation "Swift answers `unknown-op`"
  notes.
- `reference/cli.md`: the `Roost.app` roostctl path and symlink advice,
  Swift limitations on `open`, `events`, `tab prompt` and `wait`.
- `getting-started/installation.md` (which today steers agent users to
  Roost-Iced and calls it experimental), `getting-started/first-run.md`.
- `reference/architecture.md`, `reference/paths.md` (profile table),
  `reference/config.md`, `reference/ipc-compatibility.md`,
  `reference/terminal-queries.md`, `reference/themes.md` (the SwiftPM copy
  and `themes-check`), `reference/fonts.md`, `index.md`.
- Guides: `automation.md`, `agents.md`, `notifications.md`,
  `host-sessions.md`, `extending.md`, `cwd-tracking.md`, `keybindings.md`.
- `development/vision.md`: a new decision entry superseding DL-1, DL-5,
  DL-16 and DL-28, and a rewrite of *Direction (under evaluation)*.
  **Mark the old entries superseded; don't rename their headings.** Anchor
  links depend on them, and roostctl's doc-anchor test checks them.
- `development/iced-migration.md`, `setup.md`, `test-automation.md`,
  `shared-rust-engine.md`, `host-sessions.md`, `claude-testing.md`.

### Meta files

- `CLAUDE.md`: most sections mention Swift; delete the Swift threading
  subsection.
- `README.md`.
- `skills/roost/SKILL.md`: disambiguates Roost.app from Roost-Iced.
  roostctl embeds this file and tests its recipes.
- `.gitignore` (SwiftPM and Xcode entries), `.coderabbit.yaml` (an example
  path into `App.swift`).
- `packaging/icon/generate_icons.py` and `regenerate.sh` write into `mac/`.
- `CHANGELOG.md`: the Unreleased section still describes Swift fixes. They
  ship in the last Swift release; v0.1.0's entry describes the switch.

---

## Deferred to removal time

- **`mac/` layout.** Keep what iced and releases need going forward and
  remove what is only legacy. Keeping the directory name avoids editing
  about 50 `mac/build/` references and the CI path filters. Renaming is
  possible but must update every filter in the same commit.
- **The Mac-only e2e modules.** Nine modules appear in no Makefile list and
  no CI lane; today they run only because `e2e-mac` sweeps the whole
  `tools/roosttest` directory: `test_device_queries`, `test_test_ops`,
  `test_word_selection`, `test_ordering`, `test_sidebar_agents`,
  `test_terminal`, `test_launcher`, `test_sidebar_layout`,
  `test_sidebar_collapse_persistence`. Separately, the macOS iced cell
  skips four modules the Linux lanes run: `test_shell_integration`,
  `test_newtab_cwd`, `test_boot_failure`, `test_quit_with_foreground_job`
  (so the macOS login-shell path has no Mac CI coverage). Evaluate each
  module on its own value. The goal is shorter test time with solid
  coverage, not 100% duplication across lanes.
- **The facade (#286).** vision.md calls it the seam for keeping Swift, to
  be held at "don't invest, don't delete" until the direction resolved. It
  has no consumer once Swift is gone. Decide with the codebase at the time.
- **`tests/config-fixtures/`.** Add a Rust loader or retire it with Swift.
- **When to drop `roostctl`'s older-server fallbacks** entirely, rather than
  only rewording them.

---

## Issue triage

Nothing here happens until the matching work merges. State as of
2026-09-27: 77 open issues.

**Close when the Swift removal merges:**

| Issue | Why |
|---|---|
| #569, #552, #573 | Swift app bugs (main-actor reap, no SIGTERM handler, IPC socket not close-on-exec). |
| #510 | Swift parity for plan 066's operations. |
| #497 | No Swift surface for durability failures. |
| #289 | swift-testing runner crash. |
| #250 | Swift sidebar divider vs terminal drags. |
| #226 | Swift `app.active_terminal_focused`; iced has it. |
| #123 | SwiftPM resource bundle. |
| #185 | Sharing mouse-motion helpers between Swift and Rust; the reason goes away. |
| #286 | The facade, if it is deleted. |
| #124 | A `--selftest` clean-install check for the Swift app; iced's bundle smoke covers it. Confirm first. |
| #342 | Selection auto-scroll at window edges, reported against Swift. Check iced first; if iced lacks it too, rewrite instead. |

**Rewrite to drop the Swift half, keep open:**

- Features and fixes that name both apps: #209, #188, #187, #186, #364,
  #134 (Sparkle UX; applies to the production feed), #117 (Intel build),
  #261, #341, #164, #390.
- #338 and #339 use the Swift app as a reference. Point them at the legacy
  repo.
- Checklists with Swift items to remove: #273, #268, #275.
- Old feature requests that still point at deleted code
  (`crates/roost-linux` and Swift files): #162, #163, #165, #166, #167,
  #168, #169, #170. Rewrite them for iced.
- Merge #189 and #285 (the Mac real-input harness) into one.

**Unaffected, though they mention macOS:** #572, #568, #553, #351.

**Already closed** by plan 072 and related work: #556, #557, #562, #565,
#559, #554, #193.

**New issues to file when planning:** primary selection on macOS; link and
text drops plus Finder paste; menu completeness; the accent color; porting
the entitlements checks; the e2e module evaluation.

**Closing mechanics** (lessons from earlier plans):

- Write `Closes #1, closes #2` as bare text, one keyword per issue.
  A backticked keyword or a shared keyword closes nothing past the first.
- Check after the merge that every issue actually closed.
- Leave a short comment on each: "Swift app retired in #N; the code is
  archived in roost-legacy-swift @ <sha>."

---

## Phases

0. **Swift frozen.** Plan 072 has merged; no edits to `mac/` and no Swift
   twins of new work. The Swift app still ships and still runs in CI.
1. **Gap releases.** The must-haves and should-haves, as releases that ship
   both apps side by side, so each gap is judged against the live Swift app.
   Plan 073 is the first; more may follow. The Swift fixes already in
   Unreleased ship in these. The tag of the last gap release, the last one
   before v0.1.0, is the legacy snapshot pin.
2. **Cutover and removal (v0.1.0).** One or two PRs, merged before the
   release:
   - the identity change and a live-tested Sparkle handoff;
   - extracting `roost-legacy-swift` at the pin;
   - deleting the Swift code, CI jobs, Makefile targets, tool branches and
     docs;
   - the test evaluation and the deferred decisions;
   - the vision.md decision entry.

   v0.1.0 is the first Swift-less release; its notes say Roost-Iced.app is
   sunset. The verified removal inventory for this plan is kept with the
   plan-073 working notes, outside the repo.
3. **Issue triage.**

Tier 0 of [`text-rendering.md`](text-rendering.md) shipped in plan 073, the
first gap release, ahead of the cutover. The macOS size change is a
CHANGELOG note, since migrating Swift users already expect 13pt to render at
13 px.

---

## Easy to miss

1. Nine e2e modules stop running anywhere when `e2e-mac` goes.
2. `ci-success`'s `required_jobs=11` must drop to 8 in the same commit as
   the job deletions.
3. `publish-release` expects `Roost-${ver}.dmg`, and `appcast-iced` waits
   on `appcast`. Either one blocks the next release.
4. Shipped Swift apps poll `docs/appcast.xml`. Don't let it 404.
5. Moving `mac/scripts` or `mac/Resources` breaks harness path constants and
   CI path filters, and the filters fail silently.
6. `mac/build/` is load-bearing in about 50 places.
7. `make test` and `make check` run `swift test` and `themes-check`.
8. Renaming the superseded decision-log headings breaks anchor links that
   roostctl's doc test checks.
9. `test_osc7_emitters.py` checks all four shell-integration copies.
10. The Swift entitlements tests guard files the iced bundle keeps.
11. `tests/config-fixtures/` has no Rust consumer.
12. The in-process Swift app leaks its IPC listener into tab shells
    (#573). A killed test run can leave orphaned shells holding the
    production socket path, which wedges any later Roost.app launch until
    they are killed. This stops mattering after removal but can bite
    during the transition.
