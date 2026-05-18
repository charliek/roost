# Discovery: Focus Roost on Notification Click (macOS) — Option A Plan

**Status:** Discovery complete, not implemented. Pick up if you want
the click-to-foreground UX before Phase 4 distribution work happens.

**Date:** 2026-05-01. macOS Tahoe (Darwin 25.3) was the test bed.

**Approach in this doc:** "Option A" — a small AppKit cgo wrapper that
sets a Regular activation policy at startup and calls the deprecated
`[NSApp activateIgnoringOtherApps:]` from inside the IPC focus handler.
Borrowed-time fix; durable replacements are noted at the end.

---

## Problem statement

Clicking a Roost desktop notification on macOS fires the entire IPC
chain successfully, selects the correct tab inside Roost's main
window, but does **not** bring Roost to the foreground. The user sees
nothing visibly change unless they Cmd-Tab to Roost manually. Same
symptom when invoking `roost-cli tab focus N` from any external
shell.

This was confirmed empirically during the May 2026 discovery:

- `terminal-notifier -execute` fires reliably on fresh-banner clicks
  (verified with `-execute "/usr/bin/touch /tmp/marker"`).
- `roost-cli tab focus N` from a clean shell with no
  `ROOST_*` env vars connects to the IPC socket and `App.FocusTab`
  selects the right tab internally.
- Roost's window does not come forward in either case.

The reference implementation that *does* foreground reliably is cmux
(see `../cmux/Sources/AppDelegate.swift:11441`), which works because
it is a signed `.app` posting via `UNUserNotificationCenter` and
handling the click in an in-process delegate. None of those properties
hold for Roost today.

---

## Code in play

### Notification posting (macOS path)

`cmd/roost/notify.go`

- `sendDesktopNotification` — dispatches by GOOS.
- `sendMacNotification` — forks `terminal-notifier` with
  `-title ... -message ... -group roost.tab.<id>
  -execute "<roost-cli-path> tab focus --tab <id>"`. Process is
  `Start()`ed and reaped on a goroutine.
- `lookupRoostCLI` — resolves roost-cli next to the running roost
  binary, then PATH.
- `quoteForExecute` — single-quotes the cli path for terminal-notifier's
  shell-parsed `-execute`.

### Click → IPC chain

`cmd/roost-cli/tab.go:36-48` — `cmdTabFocus`: shells `tab.focus`
JSON-RPC against the unix socket. Path resolved by `socketPath()` →
`lookupSocketPath()` (env `ROOST_SOCKET` first, then platform default
`~/Library/Application Support/Roost/roost.sock` on macOS).

`internal/ipc/protocol.go` — `MethodTabFocus = "tab.focus"`,
`TabFocusParams{TabID int64}`, `TabFocusResult{PreviousProjectID,
PreviousTabID}`.

`internal/ipc/server.go:193` — server dispatch to `Handler.FocusTab`.

`cmd/roost/app.go:457-501` — `App.FocusTab`: marshals onto the GTK
main thread via `coreglib.IdleAdd`, selects the owning project, sets
the AdwTabView selected page, clears the needs-attention badge, calls
`a.win.Present()`. Returns previous (project, tab) for "go back".

### Linux click path (works; included for contrast)

`cmd/roost/app.go:147-166` — registers an app-level `tab-focus` GIO
action with an int64 variant parameter. `gio.Notification.SetDefault
ActionAndTarget("app.tab-focus", ...)` invokes it in-process. This
path works on Linux because GTK's wayland/X11 backends do call the
platform activation primitives via `gtk_window_present`. The macOS
GTK backend does not (see Root Cause #2 below).

### Reference implementation (cmux, for comparison)

`../cmux/Sources/AppDelegate.swift:11441` — after handling the
click, cmux calls:

```swift
NSRunningApplication.current.activate(
    options: [.activateAllWindows, .activateIgnoringOtherApps]
)
```

`../cmux/Sources/AppDelegate.swift:11445-11505` — `userNotification
Center(_:didReceive:withCompletionHandler:)` reads `userInfo["tabId"]`
from the notification and calls `openNotification(tabId:surfaceId:
notificationId:)`. This callback runs in user-action context inside
the cmux process; that's why the activate above is honored.

`../cmux/Sources/TerminalNotificationStore.swift:1130-1152` — posting
side: `UNMutableNotificationContent` with `userInfo: ["tabId": ...,
"surfaceId": ..., "notificationId": ...]`, dispatched via
`UNUserNotificationCenter.current().add(request)`. No external helper.

---

## Root cause (research-backed)

Three independent gates each prevent activation in Roost's current
setup. Option A is engineered to bypass #1 and #2; #3 is structurally
unfixable without further work and is the reason this is a
"borrowed-time" approach.

### 1. Unbundled binaries default to "Prohibited" activation policy

Per the AppKit headers, `NSApplicationActivationPolicyProhibited`
is the default for an unbundled executable launched without an
`Info.plist`. While in this policy, every activation API returns
silently without doing anything. Roost is launched as `./roost`
from a shell — it sits in this default policy.

Fix: call `[NSApp setActivationPolicy:NSApplicationActivationPolicyRegular]`
once during initialization, on the main thread, after the AppKit run
loop is up. (Calling it too early is a known foot-gun; see GLFW
fix #1802 / issue #1648.)

### 2. Sonoma+ cooperative activation rejects "spontaneous" requests

Starting in macOS 14, `[NSApp activate]` and
`NSRunningApplication.activate(options:)` were re-gated. Empirically
(per tzahola's testing on the Electron panel-window PR thread):

- Activation from a global hotkey: **honored**.
- Activation from a Dock-menu / system-menubar interaction: **honored**.
- Activation from timers, network events, IPC handlers: **rejected**.
- Activation while `Terminal.app` is the front app: **blocked entirely**
  for both old and new API.

Apple's intended replacement is `NSApp.yieldActivation(to:)`, where
the app currently holding activation rights explicitly hands them to
a named recipient by bundle identifier.

A unix-socket IPC request handler is, by Sonoma's classification, a
network-class event. There is no signal carried over `AF_UNIX` that
the system recognizes as a continuation of the original notification
click.

The escape hatch: the older `[NSApp activateIgnoringOtherApps:YES]`
(note: the *NSApp* method, distinct from `NSRunningApplication
.activate`) still works in the spontaneous case on 14.x and 15.x,
and (as of writing, May 2026) still works on Tahoe (26 / Darwin 25).
Apple has marked it deprecated and the broader signal is that it
will be removed in a future major. Treat its useful life in years,
not many.

### 3. User-action context does not survive the process chain

`terminal-notifier` is itself a small bundled `.app` (it has to be
to use the notification APIs). When the user clicks the banner,
terminal-notifier briefly receives the click and then does
`/bin/sh -c '/path/to/roost-cli ...'`. The shell is a child process
with no inherited AppKit state; `roost-cli` is a fresh Go binary that
opens an `AF_UNIX` socket and writes JSON-RPC bytes; `roost`'s IPC
handler reads those bytes and has zero metadata about who clicked
what.

There is no public macOS API that flows activation rights through
that chain. Cooperative activation (`yieldActivation(to:)`) is
process-pair scoped and requires the yielding app to know the
receiving app's bundle identifier. Roost has no bundle ID, and
even if it did, terminal-notifier doesn't know about it.

This is structurally why cmux works and Roost does not. Option A
side-steps the cooperative-activation gate via the deprecation
escape hatch in #2; it does **not** restore real user-action context.

---

## Proposed fix (Option A)

### Scope summary

- New cgo location: `internal/macactivate/` (darwin-only build tag).
  Becomes the third cgo carve-out in the project, alongside
  `internal/ghostty` and `internal/pangoextra`. Justification per
  CLAUDE.md *Architecture* rule (no pure-Go alternative; wrapper is
  small and self-contained): both true here.
- Two call sites in `cmd/roost/app.go`:
  - **Startup** — set Regular activation policy after AppKit is up.
  - **`App.FocusTab`** — call `[NSApp activateIgnoringOtherApps:YES]`
    after `a.win.Present()`.
- No changes required to `roost-cli`, `internal/ipc`, or notification
  posting code.

### Files to add

**`internal/macactivate/macactivate_darwin.go`** (sketch)

```go
// Package macactivate wraps two AppKit primitives needed to bring
// Roost to the foreground from the IPC focus handler. Mac-only. All
// calls must run on the GTK/AppKit main thread.
//
// This package exists because GTK4's macOS backend does not call
// NSApp activation APIs from gtk_window_present(), and because an
// unbundled Go binary launched from a shell starts in the Prohibited
// activation policy where every other activation API is a no-op.
//
// The activate path uses the deprecated -[NSApp
// activateIgnoringOtherApps:] which, as of macOS 26, is still the
// only AppKit call that bypasses Sonoma+ cooperative activation
// gating in the network/IPC-event case. See docs/discovery/focus.md.
package macactivate

// #cgo darwin LDFLAGS: -framework AppKit
// #include "macactivate_darwin.h"
import "C"

func SetRegularPolicy() { C.macactivate_set_regular_policy() }
func ActivateApp() bool { return C.macactivate_activate_app() != 0 }
```

**`internal/macactivate/macactivate_darwin.h`**

```c
void macactivate_set_regular_policy(void);
int macactivate_activate_app(void);
```

**`internal/macactivate/macactivate_darwin.m`**

```objc
#import <AppKit/AppKit.h>
#import "macactivate_darwin.h"

void macactivate_set_regular_policy(void) {
    [NSApp setActivationPolicy:NSApplicationActivationPolicyRegular];
}

int macactivate_activate_app(void) {
    BOOL ok = [NSApp activateIgnoringOtherApps:YES];
    return ok ? 1 : 0;
}
```

**`internal/macactivate/macactivate_other.go`** (`//go:build !darwin`)

```go
package macactivate

func SetRegularPolicy() {}
func ActivateApp() bool { return true }
```

### Files to modify

**`cmd/roost/app.go`**

- In `App.activate(...)` (the GTK application activate signal handler;
  search for `lookupTerminalNotifier()` for context — the policy call
  belongs near the existing macOS-aware initialization), add:
  ```go
  macactivate.SetRegularPolicy()
  ```
  Already on the main thread.
- In `App.FocusTab`, immediately after `a.win.Present()` (currently
  line 495), add:
  ```go
  macactivate.ActivateApp()
  ```
  Already on the main thread (inside `coreglib.IdleAdd`).

**`docs/guides/notifications.md`**

- Update the click-through caveats around line 100 to reflect what
  works after Option A: same-Terminal launch may still fail to
  foreground (gate #2's Terminal.app block); Notification Center
  retroactive clicks remain unreliable upstream of any Roost code.

**`build/build.sh`**

- Likely no change needed; cgo's `LDFLAGS: -framework AppKit` is
  honored by the standard Go toolchain. Verify after first build.

### Threading

All three AppKit calls (set policy, activate, present) must be on the
GTK/AppKit main thread. Both call sites already are:

- `App.activate` is the application's `activate` signal handler.
- `App.FocusTab` does its work inside `coreglib.IdleAdd`.

Document this in `internal/macactivate`'s package doc to prevent
future callers from invoking these from a goroutine.

### Optional Carbon fallback

If verification reveals that activation fails when `Terminal.app` is
foreground (likely in dev), a Carbon-era fallback exists:

```objc
ProcessSerialNumber psn = { 0, kCurrentProcess };
SetFrontProcessWithOptions(&psn, kSetFrontProcessFrontWindowOnly);
```

Carbon is more deprecated than `activateIgnoringOtherApps:`. Treat as
belt-and-suspenders only if `macactivate_activate_app` returns 0.
Adds `-framework ApplicationServices` (or `Carbon`) to LDFLAGS.

---

## Verification plan

1. **Live banner click, Roost not foremost.** Trigger a Claude Code
   `notification` hook from a Roost-hosted shell while a different
   app (e.g., a browser) is foreground. Click the banner immediately.
   Roost should come to the foreground with the right tab selected.

2. **Live banner click, Terminal.app foremost.** Same test but with
   the Terminal Roost was launched from as the foreground app. If
   this fails, gate #2's Terminal-block hit; consider the Carbon
   fallback or accept the limitation (document it).

3. **`roost-cli tab focus N` from a separate Terminal.** Should
   also raise Roost (the activate call lives in `App.FocusTab`, which
   this path also runs through). Becomes a quick regression check.

4. **Linux smoke test.** Confirm `internal/macactivate` no-op stubs
   compile and the existing GIO `app.tab-focus` action path still
   works end-to-end.

5. **Notification Center retroactive click.** Out of scope for
   Option A; still expected to be unreliable because of gate #3 and
   terminal-notifier's unsigned-helper status. Document as a known
   limitation.

---

## Known limitations and durability

- **Deprecation timer.** `[NSApp activateIgnoringOtherApps:]` is
  deprecated in Sonoma. Useful life is uncertain — likely years, not
  months, but Apple has been telegraphing the cooperative model for
  two releases. When it stops working, replace with Option B/C from
  the discovery (sidecar bundle + `yieldActivation(to:)`, or a
  `roost://` URL scheme via LaunchServices). Both require Roost to
  acquire an `Info.plist` and a bundle identifier — basically the
  beginning of Phase 4 distribution work.
- **Terminal.app foremost.** May not foreground even after the policy
  fix. Carbon fallback is the only known mitigation, and it's even
  more deprecated.
- **Notification Center retroactive click.** Unreliable upstream of
  any Roost change. Live-banner clicks are the supported path.
- **Cosmetic.** Roost still has no Dock icon, no proper app name in
  Cmd-Tab (shows the binary path), and can't be addressed by
  `osascript -e 'tell app "Roost" to activate'`. Option A doesn't
  improve any of this.

---

## When to revisit

Option A is sized for "you, dogfooding Roost as a single user." When
Phase 4 distribution work happens (FEATURES.md item #22), most of
this should be replaced rather than extended:

- `internal/macactivate` becomes redundant once Roost has a bundle
  identifier and posts via `UNUserNotificationCenter` from a fourth
  cgo location (`internal/macnotify` wrapping `UserNotifications
  .framework`).
- The terminal-notifier dependency goes away.
- `App.FocusTab` continues to exist but is called from an in-process
  click delegate that has real user-action context, so
  `NSRunningApplication.current.activate(options:)` (the non-deprecated
  path) works without any of the gating workarounds.

At that point, delete `internal/macactivate` and the corresponding
calls.

---

## References

- Apple Developer Forums — "NSRunningApplication activateWithOptions
  on Sonoma" (cooperative activation behavior, deprecation context).
- Electron PR thread — tzahola's empirical mapping of which event
  classes can and cannot trigger activation on macOS 14.1.
- GLFW issue #1648 / fix #1802 — deferring `setActivationPolicy(.regular)`
  for non-bundled CLI launches.
- terminal-notifier README — `-execute`, `-sender`, `-activate` flag
  semantics; `-sender` cannot combine with `-execute`/`-activate`.
- cmux: `Sources/AppDelegate.swift:11441` — `NSRunningApplication
  .current.activate` reference call site.
- cmux: `Sources/AppDelegate.swift:11445-11505` — `UNUserNotification
  CenterDelegate.didReceive` reference handler.
- cmux: `Sources/TerminalNotificationStore.swift:1130-1152` —
  `UNUserNotificationCenter` reference posting.
- Roost source of truth at time of discovery:
  - `cmd/roost/notify.go`
  - `cmd/roost/app.go:147-166` (Linux GIO action wiring)
  - `cmd/roost/app.go:457-501` (`App.FocusTab`)
  - `cmd/roost-cli/tab.go:36-48` (`cmdTabFocus`)
- CLAUDE.md — cgo carve-out policy (justification bar for a third
  location).
- FEATURES.md item #22 — Distribution backlog (where the durable
  fix eventually lands).
