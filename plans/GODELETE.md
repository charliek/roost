# GODELETE: remove the legacy Go implementation

**Status**: ⏳ pending — deferred until Rust/Swift parity is confirmed.
**Mergeability to main**: **destructive — its own PR**, after the checklist below.

Not numbered on purpose: the numbered phases (0–9) were the build-up to making
Rust/Swift the direction on `main` (done — see [`phase-9-cutover.md`](phase-9-cutover.md)).
This is a discretionary cleanup done whenever there's confidence the Go binary is
no longer needed.

## Goal

End the dual-binary world. Delete the Go program so there is one Roost: Swift UI
on macOS, gtk4-rs UI on Linux, both over in-process JSON IPC with `roostctl`.

## Pre-deletion checklist

Every box must be checked before opening the deletion PR:

* [ ] Rust/Swift at parity with the Go binary's user-visible behavior — validated
  by manual side-by-side testing + a deliberate feature-gap audit.
* [ ] The shipping artifacts (DMG + `.deb` via apt) have been in real use long
  enough to trust (v0.0.1 shipped 2026-05-23).
* [ ] User has confirmed their personal installs (incl. mini2/mini3) run the
  Rust/Swift build, not the Go binary.
* [ ] No remaining tooling/scripts/docs depend on the Go binary.

## Step plan

* **Delete the Go code:** `git rm -r cmd/ internal/ build/ go.mod go.sum` + any
  Go-specific `Makefile` targets. Grep for stragglers (`cmd/roost`, `internal/`,
  `pangoextra`, `gotk4`, `build/build.sh`).
* **Delete the legacy CI:** remove [`.github/workflows/go-legacy.yml`](../.github/workflows/go-legacy.yml).
  (The primary `ci.yml` is already Go-free.)
* **Collapse the Ghostty pins:** `build/build.sh` (the Go cgo libghostty-vt build)
  goes away with the Go code; `third_party/ghostty/build.sh` is already the only
  pin every live consumer uses (Rust `roost-vt` bindgen + Swift link).
* **Archive legacy docs:** move the "Legacy (Go prototype)" docs (incl.
  `docs/development/spec.md`) to `docs/historical/` with a one-line banner; drop
  the Legacy nav section from `mkdocs.yml`.
* **Tidy `CLAUDE.md`:** remove any lingering Go threading/gotk4 notes (most are
  already gone); keep it Rust/Swift-only.

## Notes / non-issues (already resolved by the inline-core refactor)

* No `roost-cli-rs` → `roost-cli` rename needed — the CLI is already `roostctl`.
* No SQLite/`roost.db` migration — persistence is `state.json`; tabs aren't
  persisted by design.
* No `proto/` schema graduation — gRPC/proto was removed; IPC is JSON.
* Binary-name note: the Linux UI binary is already `roost` (crate stays `roost-linux`).
