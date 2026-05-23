# Phase 3: Rust core MVP

**Status**: ✅ done
**Exit criteria** (all met):
* `roost-core` daemon binds on a Unix domain socket and serves the full Roost proto service.
* `tonic`-based gRPC server with handlers for: Identify, OpenTab, CloseTab, ListTabs, StreamPty, WatchEvents, CreateNotification, SetTabTitle, FocusTab, SetTabState, ClearTabNotification, SetHookActive, ReportOsc.
* SQLite persistence layer (`rusqlite` + migrations in `crates/roost-core/migrations/`) ports the Go schema byte-for-byte — `project`, `tab`, `last_command`, `user_titled`, foreign-key cascades all preserved.
* Workspace runtime state (active tab/project, hook flag, per-tab agent state) lives in-memory in a `RuntimeState` guarded by a `Mutex`, mirroring the Go side's persisted/ephemeral split.
* Event broadcast channel (`tokio::sync::broadcast`) wired into every Workspace mutator; `WatchEvents` subscribers see every state change.
* PTY supervisor (`portable-pty`) spawns one PTY per tab on `StreamPty` attach, pumps bytes both directions.
* `cargo test -p roost-core` green on macOS + Linux.

## Goal

Get the daemon to feature parity with the Go binary's `internal/ipc` + `internal/core` + `internal/store` surface, expressed through the proto contract instead of newline-delimited JSON-RPC. The daemon must be runnable in isolation and answerable via `grpcurl` / `roost-smoke`.

## Scope

In:
* Everything the proto service declares.
* SQLite migrations ported from `internal/store/migrations/`.
* Lock-order discipline (`store` mutex always taken before `runtime` mutex — see comment block in `state.rs::close_tab`).
* `block_in_place` around blocking PTY resize syscalls.

Out:
* OSC routing beyond pass-through. Phase 6b owns the OSC state machine + hook-active suppression rule.
* Multi-UI reattach semantics — Phase 5 step 5 still has the daemon re-spawn the shell on every `StreamPty` attach.

## Touches Go code?

No. The daemon is brand new; Go `internal/core` is untouched.

## Commits

* `bc1d7f3` — Phase 3: SQLite persistence + proto tightening + CodeRabbit fixes.
* `52952b4` — Fix close_tab lock-order inversion + wrap pty resize in block_in_place. (Post-Phase 3 hardening based on CodeRabbit.)
* `19bc3a6` — Address CodeRabbit pass on roost-common, runtime, Mac UI.
* `64bedbb` — Extract roost-common: single source for socket/DB paths + UDS connect.

## Risks / known gaps

* `StreamPty` currently re-spawns the shell on every attach. A second client attaching to the same tab gets a fresh shell, not the existing one. Phase 5 documents this as accepted for the single-client case; multi-client reattach is a Phase 6+ concern once we actually have two clients connecting concurrently.
* Tab `last_active` is updated on tab open but not on every input — same as the Go binary's behavior.

## Follow-ups

* OSC scanner — the daemon currently passes OSC sequences through unchanged from the UI's `ReportOsc` upcall. The full state machine + per-tab hook-active suppression is Phase 6b work.
* Multi-UI reattach is a known Phase 5+ deferral.
