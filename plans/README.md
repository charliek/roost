# Plans

Working notes for in-flight Roost changes. One file per planned PR;
delete (or move to the archive) once landed.

The multi-phase **Go → Rust/Swift migration is complete**. Its history —
the `phase-*` / `goal-*` phase docs, the `GODELETE` plan, the migration
index, and the pre-migration UX assessment — now lives with the archived
Go prototype in the `roost-legacy-go` repository (`plans/` there),
extracted when the legacy Go code was removed from this repo. That archive
currently exists only as a local sibling checkout (`../roost-legacy-go`),
not yet hosted on a remote; see its `README.md`.

## Active plans

| Plan | Covers |
|---|---|
| [title-follows-cwd-2026-05-31.md](title-follows-cwd-2026-05-31.md) | #196 — model re-derives tab title from cwd |
| [ci-shell-provisioning-2026-05-31.md](ci-shell-provisioning-2026-05-31.md) | #197 — zsh + brew bash on CI runners |
| [local-env-e2e-flakes-2026-05-31.md](local-env-e2e-flakes-2026-05-31.md) | local-env E2E flakes (test_osc52 + test_sidebar_layout) |
