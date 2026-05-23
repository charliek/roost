# Phase 1: De-risk spikes

**Status**: ✅ done
**Exit criteria** (all met):
* `tonic` + `tokio` over UDS proved: a hello-world tonic server binds on a `tokio::net::UnixListener`, a tonic client connects via a `service_fn` + `UnixStream` connector, one RPC round-trips clean.
* `grpc-swift v2` over UDS proved: `HTTP2ClientTransport.Posix(target: .unixDomainSocket(path:authority:))` round-trips against a tonic server.
* libghostty-vt FFI from Rust proved: `ghostty_terminal_new` + `ghostty_terminal_vt_write` + `ghostty_terminal_free` round-trip via bindgen, gated on a `ffi` cargo feature.
* libghostty-vt FFI from Swift proved: a `CGhosttyVT` SwiftPM system-library target wraps the C header; the same three functions round-trip from a Swift Testing target.
* Spike code archived (folded into the real Phase 2+ crates) before Phase 2 lands.

## Goal

Prove the three "unknown unknowns" in the target architecture in isolation before committing to the proto + workspace layout in Phase 2. A failure in any one of them would change which language/library decisions in the vision doc are tenable.

## Scope

In:
* Smallest possible binary per spike that round-trips one call.
* CI jobs gated `continue-on-error: true` while spike velocity is high.

Out:
* Anything resembling a real service or UI.
* Anything in `proto/` (Phase 2 owns the schema).

## Touches Go code?

No. Spikes lived in `crates/spike-*/` and `mac/Spikes/` and were folded into Phase 2+ targets when complete.

## Commits

This phase landed across several commits:

* `bc1d7f3` — Phase 3 commit (it absorbed the Phase 1 Rust spike content into the real `roost-core` after the proto landed; see Phase 3 doc).
* `b2e6540` — Phase 5 step 3 (Mac side): wire libghostty-vt into the SwiftPM build (this absorbed the Swift libghostty-vt spike).
* `6c8009e` — Phase 5 step 3 (Rust side): libghostty-vt FFI smoke + CI build of vendored Ghostty (the Rust libghostty-vt spike's permanent home is `crates/roost-vt`).

## Risks / known gaps

The grpc-swift v2 UDS spike proved compile + connect, but did NOT exercise the `:authority` pseudo-header. The latent bug there surfaced only in Phase 5 when the real Mac UI tried to call Identify against the real daemon. See [Phase 5 doc](phase-5-mac-ui-mvp.md) for the post-mortem; the fix landed in `4a7cf4c` and was hardened by adding a live-daemon test step in CI (`82f1237`).

Lesson learned: a spike that only proves "compiles and connects" doesn't prove "the protocol round-trip is correct." Future spikes should call one real RPC end-to-end, not just the transport handshake.

## Follow-ups

None — the spikes' artifacts now live in the real crates and the lesson on spike thoroughness is captured here and in the Phase 6a CI step.
