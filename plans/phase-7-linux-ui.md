# Phase 7: Linux UI (gtk4-rs)

**Status**: 🚧 in progress — Step 1 (crate skeleton + Identify spike + `gtk-build` CI job) landed via M8 of [`goal-rust-port-polish-2026-05-16.md`](goal-rust-port-polish-2026-05-16.md). Steps 2–7 (cell renderer, PTY round-trip, sidebar + tab bar, keybind config, OSC + notifications, visual polish) still pending. The user is driving the remaining work on their Linux laptop in parallel.
**Exit criteria**:
* `linux/` Rust crate added to the Cargo workspace, building a runnable `roost-linux` binary.
* gtk4-rs + libadwaita window with a project sidebar + tab bar + terminal area, structurally equivalent to the Mac UI.
* tonic client over UDS to `roost-core` (the Rust path already works — `roost-cli-rs identify` is the canary).
* libghostty-vt linked via the existing `crates/roost-vt` bindgen + the pre-built `third_party/ghostty/out/lib/libghostty-vt.a`. No cgo; raw bindgen FFI only.
* Cell renderer using Cairo + Pango, walking libghostty-vt's render state — same shape as the current Go renderer but without the `gotk4` `pangocairo.ContextSetFontOptions` mismatch (gtk4-rs uses raw FFI for that call — see DL-6).
* PTY round-trip works: type bash in the Linux window backed by the Rust daemon.
* OSC scanner (ported in Phase 6b) consumed by the Linux UI to fire `libnotify` desktop notifications.
* Default keybinds match the Linux defaults from `cmd/roost/app.go::defaultBindings`: `ctrl` as primary modifier, `alt` for project/clipboard.
* The keybind override config (Phase 6a step 2d) ports unchanged to Linux — same parser, same action namespace.
* Feature parity with the Mac UI at this point: multi-tab, projects, OSC, notifications, hook flow.
* CI: new `linux-ui-build` job in `refactor.yml` runs `cargo build -p roost-linux` on ubuntu-latest with GTK4 + libadwaita dev packages.

**Mergeability to main**: yes. Phase 7 is additive in `linux/` and `crates/`; the Go `cmd/roost` GTK4 binary is untouched.

## Goal

Bring up the second native UI. After Phase 7 we have Mac (Swift + AppKit) and Linux (Rust + gtk4-rs) both talking to the same `roost-core` over the same proto contract, with feature parity.

## Scope

In:
* Everything the Mac UI does, but in gtk4-rs.
* Cairo + Pango cell renderer.
* gtk4-rs ↔ libghostty-vt via the existing `roost-vt` crate's bindings.
* libnotify (or `org.freedesktop.Notifications` directly) for desktop notifications.

Out:
* Anything in `cmd/roost/` — that's the Go binary; it keeps shipping on `main`.
* Windows. Per vision.md non-goals.
* Multi-window. Per vision.md non-goals.

## Touches Go code?

No. New crate; the Go `cmd/roost` GTK4 binary remains the user-facing Linux build until Phase 9.

## Step plan

* **Step 1 — Crate skeleton.** ✅ landed as `crates/roost-linux/` via M8 of `goal-rust-port-polish-2026-05-16.md` (PR #31, squash `c4d0d38`). gtk4-rs + libadwaita-rs + tokio deps; single-window `adw::ApplicationWindow` that does Identify against `roost-core`. CI job `gtk-build` runs on both `ubuntu-latest` and `macos-latest` (Homebrew GTK4). Resolved the "or `linux/`" vision-doc ambiguity in favor of the workspace-consistent `crates/` location.
* **Step 2 — Terminal renderer.** Port the Mac `TerminalView` to a `gtk::DrawingArea` subclass. Same render-state walk via `crates/roost-vt`. Same Cairo per-cell background fill + Pango per-cell glyph layout. Mac Phase 5 step 4 equivalent. The Go binary's renderer in `cmd/roost/render.go` is a useful reference for cell-metric math + glyph caching strategy.
* **Step 3 — PTY round-trip.** Bidirectional `StreamPty` against the daemon, same pattern as Mac Phase 5 step 5. The Rust tonic client side already works (`roost-cli-rs` proves it); just need the keystroke + output plumbing into the renderer.
* **Step 4 — Sidebar + tab bar.** GTK4 has native `adw::TabView` / `adw::TabBar`. Use them or hand-roll like Mac — `adw::TabBar` is probably the right choice (it's what the Go binary uses). Sidebar is an `adw::NavigationSplitView` or hand-rolled `gtk::Box`.
* **Step 5 — Keybind config.** Port the Phase 6a step 2d config + action map to a `gtk::ShortcutController`. The Go binary's `cmd/roost/shortcuts.go::installShortcuts` is the direct reference; modifier rules already documented there.
* **Step 6 — OSC + notifications.** OSC scanner is daemon-side after Phase 6b — just consume `NotificationEvent` + `TabNotificationEvent` from `WatchEvents`. Dispatch to `gio::Notification` via the application's notify path, or call `org.freedesktop.Notifications` directly.
* **Step 7 — Visual polish + AppImage prep.** Pass over icons (currently the Go binary has SVGs at `cmd/roost/icon_*.svg`), about dialog, app metadata. Stops short of bundling — that's Phase 8.

## Risks / known gaps

* The Go binary uses `gotk4` which has a known `pangocairo.ContextSetFontOptions` mismatch (DL-6) requiring the `internal/pangoextra` cgo workaround. gtk4-rs uses raw FFI for that call and doesn't have the problem — but a future bug in some other gtk4-rs binding could push us into a similar workaround. Watch for it.
* `cargo build -p roost-linux` on ubuntu-latest needs `libgtk-4-dev` and `libadwaita-1-dev` apt packages plus pkg-config wiring. The existing rust-build job's Linux step doesn't install these — `linux-ui-build` will be its own job.
* `cargo build` on macOS-latest of a workspace that includes `roost-linux` will fail unless `roost-linux` is gated by `cfg(target_os = "linux")` or the workspace excludes it on Mac. The cleaner pattern: a workspace member with default-features that gate gtk4-rs to Linux only, or — simpler — make `roost-linux` a non-default workspace member and only build it via `cargo build -p roost-linux` in the Linux CI job.
* Phase 5.5c-full key encoder considerations apply equally — the Linux UI should use libghostty-vt's full key encoder (`ghostty_key_encoder_*`) from the start; no lite stopgap.

## Step ordering — sequential with Mac or in parallel?

Phase 7 has no dependencies on Phase 6a steps 2c–2i (it can subscribe to `WatchEvents` independently, define its own keybind config layer using the same shared parser). It DOES depend on Phase 6b's daemon-side OSC scanner port.

Recommendation: hold Phase 7 until Phase 6a is complete, so the keybind config and WatchEvents subscription get one canonical implementation in `crates/roost-common` (or a new shared crate) that both UIs consume. Otherwise we end up with two implementations of the trigger parser and a likely drift.

## Follow-ups

* Wayland-specific features (e.g. `wlr_layer_shell` for floating notifications) — not in scope. We ride gtk4-rs's defaults.
* HiDPI / fractional-scaling — gtk4 handles this for us, but verify glyph rendering doesn't fuzz on a 200% scale display before declaring parity.
