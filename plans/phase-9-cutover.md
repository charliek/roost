# Phase 9: Cutover

**Status**: ⏳ pending
**Mergeability to main**: **destructive — separate PR after Phase 8 ships**.

**Exit criteria**:
* `cmd/roost`, `cmd/roost-cli`, and all of `internal/` deleted.
* `go.mod`, `go.sum`, Go-specific `Makefile` targets, the legacy `build/build.sh`, and `.github/workflows/ci.yml` (the Go CI) all deleted.
* `roost-cli-rs` renamed to `roost-cli` (DL-9 closes here).
* `third_party/ghostty/build.sh` is now the only Ghostty SHA pin. The legacy `build/build.sh` (which cross-linked to it during the transition) goes away with the Go code.
* `docs/development/spec.md` and `docs/reference/architecture.md` move to `docs/historical/` with a one-line "describes the pre-Phase-9 Go binary" banner; a new `docs/reference/architecture.md` describing the Rust + Swift architecture takes their place.
* `CLAUDE.md` rewritten so the "current implementation" sections describe Rust + Swift instead of Go + GTK4; the "Direction" section that points at `vision.md` either folds in or remains as historical context.
* CI: only `refactor.yml` (or a renamed `ci.yml`) remains; no Go-specific jobs.
* `main` after this commit builds Rust + Swift only and produces the same shipping artifacts the refactor branch produced in Phase 8.

## Goal

End the dual-binary world. After this commit there is one Roost: Rust core + Swift UI on Mac, Rust core + gtk4-rs UI on Linux. No Go path remains. The vision document moves from "where we're heading" to "what this is."

## Scope

In:
* Deleting code, deleting tooling, deleting docs that describe the Go implementation.
* Renaming `roost-cli-rs` → `roost-cli`.
* Promoting the post-cutover architecture docs into authority.

Out:
* New features. Cutover is a deletion commit; no behavior change should land in the same commit (or PR).
* Any backward-compatibility shim for users still running the Go binary — by the time this lands, Phase 8 bundling has produced installable artifacts and users have moved over.

## Touches Go code?

Yes — destructively. This is the only refactor phase where the Go program is affected.

## Pre-cutover checklist

Before opening the cutover PR, every box below must be checked:

* [ ] Phase 5–7 features at parity with the Go binary's behavior, validated by manual side-by-side testing AND by a deliberate "feature gap audit" walking through the Go binary's user-visible surface.
* [ ] Phase 8 release artifacts (DMG + AppImage) downloadable from at least one tagged release on the refactor branch.
* [ ] A version of the Roost CLI has been published that warns Go-binary users "the Rust build is now the only supported build; please switch to the bundled artifact."
* [ ] User has confirmed their personal install switched to the Rust/Swift build.
* [ ] The proto schema's "Pre-1.0 schema tightening" CHANGELOG section is closed with a 1.0 entry — Phase 9 is when the schema stops being pre-release.
* [ ] All "Phase 9 cleanup" markers in source comments (`// XXX: collapse with build.sh on cutover`, etc.) are reviewed and resolved.

## Step plan

* **Step 1 — Delete the Go binary code.** `git rm -r cmd/ internal/ build/ go.mod go.sum` plus any Go-specific Makefile targets. Verify nothing in the workspace references them (search for `cmd/roost`, `internal/`, `pangoextra`, `gotk4`).
* **Step 2 — Delete the legacy CI workflow.** `.github/workflows/ci.yml` goes away. `.github/workflows/refactor.yml` may rename to `ci.yml` (or stay as-is — it's been the de facto CI since the branch started).
* **Step 3 — Rename `roost-cli-rs` → `roost-cli`.** Update the Cargo.toml, the binary target, the README references, the docs. Make sure shell scripts (`scripts/`, install paths) follow.
* **Step 4 — Collapse the two Ghostty pins.** `build/build.sh` was always documented as "deletes in Phase 9 cutover." Confirm `third_party/ghostty/build.sh` covers every consumer (the Rust `roost-vt` bindgen path, the Mac Swift link path, and any prior Go consumer).
* **Step 5 — Promote / archive docs.**
  * `docs/development/spec.md` → `docs/historical/spec-pre-rust.md` with a one-line header.
  * `docs/reference/architecture.md` → `docs/historical/architecture-pre-rust.md` similarly.
  * A new `docs/reference/architecture.md` is written from scratch describing the Rust + Swift implementation. (Could draft this during Phase 8 to keep the cutover PR small.)
  * `vision.md` stays — it's still the durable design doc, just describing "what we built" instead of "what we're heading toward."
* **Step 6 — Rewrite `CLAUDE.md`.** The current sections describing the Go implementation's threading model, gotk4 gotchas, etc. are no longer relevant. Replace with the Rust + Swift counterparts. The "Direction" section at the top folds into the rewritten body.
* **Step 7 — Final pre-merge validation.** Build artifacts from the cutover branch, install them, run side-by-side smoke against an older Roost-Go install pointed at the same `~/Library/Application Support/roost/roost.db`. Confirm the DB round-trips and no data is lost.

## Risks / known gaps

* Persistent state migration (DL-7) — the SQLite schema ports byte-for-byte, but the file path is HOME-derived on both sides. A user who somehow runs both binaries during Phase 8 → Phase 9 transition could see inconsistent state. The pre-cutover checklist's "user has switched" step is the mitigation; for safety the cutover commit could include a migration note in the release notes.
* The Claude Code hook script's on-disk install path needs to be unchanged across the cutover so existing user hooks keep working. Verify before deletion.
* Documentation breakage — many file paths in old PRs / commit messages reference `cmd/roost/*.go`. We can't update history; we just accept those links become broken-by-design after the cutover.

## Follow-ups

* After cutover, the `vision.md` decision log entries that explicitly call out the dual-binary world (DL-9 binary name, DL-10 Ghostty pin) can fold into the "historical decisions" section since they're no longer active concerns.
* `proto/CHANGELOG.md` graduates to its 1.0 entry the same commit that completes the cutover. After this, breaking schema changes are real coordinated client+daemon releases.
