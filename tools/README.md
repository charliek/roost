# `tools/` — Roost test & automation harnesses

Three layers, by *what they can verify* and *how they drive the app*.
Reach for the highest layer that can answer your question — it's faster,
more deterministic, and more portable.

```
tools/
  roosttest/      Layer 1 — functional (IPC op-set), pytest. PRIMARY, runs in CI.
  screenshot/     Layer 2 — visual (pixels): roostctl capture + pngtool inspect.
  input/          Layer 3 — real OS input injection, platform-specific.
    linux/          uinput key/pointer + clipboard + single-monitor (COSMIC/Wayland).
    (mac/)          CGEvent equivalent — planned.
```

| Layer | Dir | Drives via | Verifies | Platforms | CI |
|---|---|---|---|---|---|
| **1 — functional** | [`roosttest/`](roosttest/README.md) | JSON IPC (Python client) | the op set: `tab.dump`/`tab.list`/`palette.*`/`identify` — behavior + content (text) | mac + gtk | ✅ headless |
| **2 — visual** | [`screenshot/`](screenshot/README.md) | `roostctl` + `roostctl screenshot` | pixels: colors, badges, cursor, reflow, which tab/sidebar is shown | mac + gtk | local |
| **3 — real input** | [`input/`](input/linux/README.md) | OS key/pointer injection | the *real* key-encoder + mouse-gesture + clipboard path: selection, copy/paste, scroll | per-OS (linux now) | local |

## Which layer for what

- **Behavior / content** (a command runs, state changes, a tab's text) →
  **Layer 1** (`roosttest`). Always prefer this: no sleeps, runs in CI on
  both UIs. Most new coverage lands here.
- **It renders correctly** (theme colors, a notification badge, the
  reflow after a window resize, sidebar shown/hidden) → **Layer 2**
  (`screenshot`). `tab.dump` is text-only, so anything color/layout needs
  a screenshot + `pngtool` inspection.
- **Real input actually works** (drag-select text, Cmd/Ctrl-C → the OS
  clipboard → paste, the key encoder, scroll) → **Layer 3** (`input`).
  This is the only layer that goes through the OS input stack; it's
  platform-specific and local-only (needs `/dev/uinput` on Linux, etc.).

## Why this layout

It mirrors the architecture: one core (the workspace op set), reached
three ways. Layer boundaries are about *capability*, not which PR added a
file — e.g. `pngtool.py` (PNG inspection) is cross-platform and serves
every screenshot check, so it lives in `screenshot/`, not under `linux/`.
`input/` is a parent (not a flat `linux/`) because the real-input layer is
inherently per-OS and a Mac CGEvent sibling is planned.

See [`docs/development/test-automation.md`](../docs/development/test-automation.md)
for the tiered CI plan and the relationship between these harnesses.
