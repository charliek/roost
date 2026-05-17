# Phase 6b: Mac OSC + notifications

**Status**: ⏳ pending
**Exit criteria**:
* The Mac UI parses OSC sequences during VT processing (it owns the libghostty-vt instance, so the OSC stream is right there in render-state walks).
* OSC 0/1/2 (title) — UI updates the tab's button title locally; if the tab is `user_titled`, the OSC is dropped daemon-side via `SetTabTitle` with `user=false`.
* OSC 7 (cwd) — `file://host/<percent-encoded-path>` → UI reports via `ReportOsc`; daemon updates `tab.cwd` and emits `TabCwdChangedEvent`.
* OSC 9 (notification, title-only) and OSC 777 (`notify;title;body`) — UI reports via `ReportOsc`; daemon routes through the OSC scanner state machine (port from `internal/osc/scanner.go`) and either fires a `NotificationEvent` or drops it under hook-active suppression.
* Per-tab notification badge — UI receives `TabNotificationEvent` via `WatchEvents` (subscription wired in Phase 6a step 2c) and renders a small dot / count on the tab's button + the project's sidebar button.
* `NSUserNotification` (or `UNUserNotificationCenter` — pick the modern API) fires for `NotificationEvent`.
* Claude Code hook flow ported: `roost-cli-rs notify --tab $ROOST_TAB_ID --title X --body Y` from inside a shell session sets `set_hook_active(true)` while owning the notification surface; the daemon suppresses raw OSC 9/777 from the shell in that mode.
* Feature parity with the current Go binary's notification surface confirmed by running both binaries side-by-side against the same test agents.

**Mergeability to main**: yes. Phase 6b is purely additive on the Mac UI + the daemon's OSC scanner port; the Go OSC code in `internal/osc/` is untouched.

## Goal

Land the *differentiator*. OSC routing + `set_hook_active` suppression is what makes Roost useful for parallel Claude Code / Codex sessions — without it, the Mac UI is just a less-polished terminal multiplexer than what already exists on `main`. The Go binary's OSC scanner is subtle (DL-8 in vision.md flags it as worth its own slice); the Rust port should match it semantically and the Mac UI should consume the same `NotificationEvent` surface.

## Scope

In:
* Mac-side OSC detection during VT processing. libghostty-vt surfaces detected OSC sequences; the renderer walk routes them to `ReportOsc` for OSC 0/1/2/7/9/99/777.
* Daemon-side OSC routing — port `internal/osc/scanner.go` to a new `crates/roost-core/src/osc.rs`. Includes:
  * The `set_hook_active` per-tab suppression rule (when a hook session owns a tab's notification surface, raw OSC 9/777 from the shell is suppressed).
  * OSC 7 cwd parsing with percent-decoding (some scaffolding already lives in `service.rs::parse_cwd_from_osc7`).
* Notification UI on the Mac side:
  * `NSUserNotification` (or modern `UNUserNotificationCenter`) for desktop notifications.
  * Tab-bar badge + sidebar badge driven by `TabNotificationEvent`.
* The Claude Code hook flow — `roost-cli-rs notify` and `roost-cli-rs tab set-state` against a Rust daemon. The shell-side hook script ships with the Go binary today (`internal/claudehook/`); confirm it works unchanged against `roost-core`.

Out:
* Anything outside the OSC + notification feature set.
* Linux UI work (Phase 7).

## Touches Go code?

No. The Go `internal/osc/scanner.go` stays in place; the Rust port is a parallel implementation. Phase 9 deletes the Go side.

## Step plan

* **Step 1 — Port the OSC scanner.** Translate `internal/osc/scanner.go` to Rust. The Go side is a stateful byte-by-byte scanner that emits `OSCEvent { command, payload }` once a sequence completes. Bring along its tests verbatim — they're the spec.
* **Step 2 — Wire the scanner into the daemon's `ReportOsc` handler.** Today `ReportOsc` is a thin stub. After this step it dispatches to the OSC router:
  * OSC 7 → `Workspace::set_tab_cwd` → `TabCwdChangedEvent`.
  * OSC 9, 777 → either `Workspace::fire_notification` (which emits `NotificationEvent` + sets `tab.has_notification`) OR suppress under `hook_active`.
  * OSC 0/1/2 → `Workspace::set_tab_title` with `user=false` (locked tabs ignore).
  * OSC 99 → with id, opportunity to also dismiss / replace. Match Go behavior.
* **Step 3 — Mac UI OSC detection.** During the render-state walk, harvest OSC events from libghostty-vt and call `ReportOsc` for each. There's a `ghostty_terminal_*` API for this — confirm signature, may need a bindgen update on `roost-vt` first.
* **Step 4 — Per-tab notification badge.** UI consumes `TabNotificationEvent` from the WatchEvents stream (depends on Phase 6a step 2c). Renders a small dot or count in the tab button's title (e.g. `"● Tab 1 ●"` for active+notif, or use a real custom NSView once visual polish lands).
* **Step 5 — Sidebar project badge.** Same idea, one level up: a project's sidebar button gets a badge when any of its tabs has a pending notification.
* **Step 6 — Desktop notifications.** Subscribe to `NotificationEvent` on the WatchEvents stream → `UNUserNotificationCenter` request. Request authorization on first run via the standard Mac prompt.
* **Step 7 — Claude Code hook end-to-end.** Confirm the Go-side hook script in `internal/claudehook/` (or its on-disk install path) calls `roost-cli-rs` correctly. The hook script may be language-agnostic shell already; verify the daemon's `SetHookActive` semantics match.
* **Step 8 — Side-by-side parity test.** Run Roost-Go and Roost-Mac against the same hook-emitting test (a tiny Claude Code session, or a hand-rolled OSC emitter). Both binaries should show the same notification arrival, the same badge state, the same suppression behavior.

## Risks / known gaps

* libghostty-vt's OSC surface from the Rust/Swift side may not be a clean stream of events — it might require polling the render state or hooking a callback. Need to confirm against `../ghostty/include/ghostty.h` and `../ghostty/src/lib_vt.zig`. If the surface is missing, the Mac UI might need to scan the byte stream itself before feeding libghostty-vt, mirroring what the Go binary does today (it intercepts the OSC bytes before passing them on).
* The OSC scanner's state machine is the most subtle piece of the Go codebase (per DL-8); reading it end-to-end + porting tests first is non-negotiable.
* The Claude Code hook contract is "what the script writes today" — if the script paths or env vars differ on Mac vs. Linux, we need to mirror them.

## Follow-ups

* Phase 7 (Linux UI) will need the same OSC + notification surface in gtk4-rs — the daemon-side scanner ported here serves both UIs, so only the Linux-side desktop-notification dispatch is new work in Phase 7.
