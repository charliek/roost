# Phase 4: Smoke client

**Status**: ✅ done
**Exit criteria** (all met):
* `crates/roost-smoke/` binary opens a `StreamPty` against `roost-core`, attaches stdin → PTY input, PTY output → stdout, runs bash interactively until the user exits.
* `cargo run -p roost-core` in one terminal + `cargo run -p roost-smoke` in another lets the user type `ls`, `pwd`, `exit` end-to-end through the gRPC daemon.
* `crates/roost-cli-rs/` (the Phase 9 successor to `cmd/roost-cli`) exposes `identify`, `notify`, `set-title`, `tab focus`, `tab list`, `tab set-state`, `tab clear-notification` subcommands. Phase 6a step 2a added `project list/create/rename/delete`.

## Goal

Prove the end-to-end Rust path before any UI work lands. `roost-smoke` is the smallest plausible client of the proto contract; a regression in the daemon's `StreamPty` surface fails the smoke even without a graphical UI.

## Scope

In:
* `roost-smoke` interactive bash session.
* `roost-cli-rs` subcommands mirroring the Go `cmd/roost-cli` surface so the shell-integration hooks the Go binary writes (Claude Code's hook, the agent state setters) keep working unchanged when the daemon is the Rust one.

Out:
* Any visual rendering. Smoke is raw bytes in/out; the renderer is the UI's job.

## Touches Go code?

No. `roost-cli-rs` is transitional under a non-conflicting name (DL-9). The Go `cmd/roost-cli` keeps working alongside it until Phase 9 cutover.

## Commits

* Multiple commits in the Phase 3 series — `roost-smoke` and `roost-cli-rs` landed together with the daemon.

## Risks / known gaps

* `roost-smoke` is line-buffered enough that high-throughput output (e.g. `cat /usr/share/dict/words`) may visibly stutter. It's a smoke binary; the real renderer is in-process VT parsing on the UI side. Not a concern.
* `roost-cli-rs` shells out to the daemon over the same UDS the Go `cmd/roost-cli` uses, so on a machine running the Go binary, `roost-cli-rs notify` against `~/Library/Caches/roost/roost.sock` would talk to the wrong daemon. Documented; users running both binaries concurrently is not the intended deployment.

## Follow-ups

* Phase 6a step 2a added the `project` subcommand. Future phases may add `project switch`, `tab open`, etc. as the Mac UI's needs surface them — keep the CLI's surface in lockstep with what the Mac UI exercises so the smoke continues to cover both clients.
