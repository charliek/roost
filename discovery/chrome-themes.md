# Chrome themes, translucency and light/dark (discovery)

Status: **discovery**, questions for a future, dedicated planning session.
It isn't a commitment. Written 2026-10-03. File and line references are as of `main` @
`21936f7`.

**Timing:** after the Swift app is retired (v0.1.0, see
[`swift-retirement.md`](swift-retirement.md)). The work is iced only and must cover
**both macOS and Linux**. The Swift app is frozen and forces dark mode, so there is no
parity to keep.

Tracked by #584 (chrome themes and overrides) and #164 (following system light/dark).

---

## Summary

- **The idea:** users can theme the **chrome** (sidebar, tab strip, palette), not only
  the terminal. Optionally, any region can be translucent and the app can follow the
  system light/dark setting.
- **Plan 073 built the seam:** every chrome color now comes from one `ChromePalette`.
  Today only its accent can change.
- **Translucency needs no iced or winit fork.** iced 0.14 already exposes a transparent
  window and blur, and how see-through each region is depends only on the alpha of the
  color painted there. A theme can make only the sidebar translucent, only the terminal,
  or everything.
- **It all defaults to opaque `roost-dark`,** so nobody sees a change without opting in.

---

## Today

**Terminal themes.**
- **What ships:** 26 bundled themes in Ghostty's file format, under
  `crates/roost-ui-model/src/resources/themes/`. **All 26 are dark;** the brightest
  background belongs to Catppuccin Frappe.
- **How they're chosen:** `theme = <name>` in `config.conf`, or the palette's
  **Select Theme…**.
- **What a theme holds** (`Theme` in `crates/roost-ui-model/src/theme.rs`):
  - background, foreground, cursor, the selection colors, the 16-color palette and
    `bold-color`.

**Chrome.** `ChromePalette` lives in `crates/roost-iced/src/chrome.rs:129`, and the only
constructor is `ChromePalette::roost_dark(accent)`. Its fields:

| Group | Fields |
|---|---|
| Surfaces | `band` (sidebar header, sidebar footer **and** tab strip: one field today), `list` (the sidebar's project list), `divider`, `footer_chip` / `_hover` / `_pressed` |
| States | `active_row`, `active_tab`, `hover`, `active_agent`, `dragged_pill` (already translucent: alpha 0.65) |
| Text | `text`, `muted_text`, `project_label_active`, `project_label_inactive` |
| Accent | `accent` (notification dots, the primary button, focus rings) |
| Palette overlay | `palette_surface`, `palette_selection`, `palette_hover`, `palette_placeholder`, `palette_match` |
| Host status | `host_dot_connected` / `_offline` / `_pending`, `host_rollup_text`, `host_fidelity_text`, `host_reconnect_text`, `host_banner_bg` / `_text` / `_border` / `_button_border`, `host_frame_scrim` |
| Errors | `error_text`, `danger`, `danger_accent` |

- **The one variable today:** `chrome-accent = system | #rrggbb` (plan 073, #578).
  - On macOS, `system` follows the control accent live.
  - On Linux it stays `#007aff`.

**Rendering.**
- **Color blending:** since plan 073, iced is built with `web-colors`, so it blends in
  display (sRGB) space on both OSes.
- **The window is opaque.**

**Light/dark.**
- **Neither app follows the system.** The Swift window forces darkAqua.
- **Apps already get told:** Roost sends DEC mode 2031 color-scheme reports when the
  terminal theme changes (`tools/roosttest/test_device_queries.py`).

---

## From #584

Plan 073 filed this as its follow-up:

- **Chrome themes:** a `chrome-theme = <name>` key that loads a palette file into the
  same struct.
- **User overrides:** a map over the palette's fields, e.g.
  `chrome-color = active_row:#…`.

The seam was built so that both are data changes. Nothing more shipped in 073.

---

## Translucency (findings 2026-10-03)

**The platform layer already supports it.**
- iced 0.14 has `window::Settings { transparent, blur }`, and winit 0.30 implements them:

  | | Transparent window | Blur |
  |---|---|---|
  | macOS | yes | yes, through a private CoreGraphics call (Ghostty uses the same) |
  | Wayland | yes | only KDE's `org_kde_kwin_blur` |
  | X11 | needs a running compositor; the alpha-capable visual is chosen at window creation | no |

- `iced_wgpu` picks a premultiplied or postmultiplied alpha mode whenever the GPU offers
  one. This Pop!_OS box reports PreMultiplied.

**Every failure falls back to opaque, never to a broken window:**
- the software renderer (softbuffer is opaque XRGB on Wayland);
- a GPU or driver that offers only opaque surfaces;
- X11 with no compositor.

**Blur beyond KDE.**
- Ghostty blurs on Wayland through the newer `ext-background-effect-v1` protocol.
- Roost would need its own call on the window's Wayland surface. The next plan's Wayland
  hook for #351 builds that seam.
- COSMIC's compositor binary contains the names of both blur protocols. Whether it
  advertises them needs a live check.
- GNOME has no blur.

**Targeting is per region.** Once the window is transparent, each region shows through
in proportion to the alpha of the color painted there:

| Region | Source of its color | Can be translucent |
|---|---|---|
| Sidebar project list | `list` | yes |
| Sidebar header, footer and tab strip | `band` (split it if they should differ) | yes |
| The terminal's default background | the terminal theme, or Ghostty's `background-opacity` | yes |
| Cells with an explicit background (vim, tmux) | the app | Ghostty keeps these opaque unless `background-opacity-cells` |
| Palette, menus, cards | `palette_surface` and friends | no; keep these opaque for legibility |

- **Blur follows automatically.** The compositor blurs behind the whole window, but the
  blur only shows where we paint translucent.
- **Expressing it:** a chrome theme can use 8-digit colors (`#rrggbbaa`), with no extra
  keys. The terminal side would reuse Ghostty's names: `background-opacity`,
  `background-opacity-cells` and `background-blur`.

**Window-level constraints.**
- **Transparency is chosen at launch:**
  - On X11 it must be.
  - On macOS and Wayland it could also be set at runtime, but iced doesn't expose that.
- **macOS native full screen:** force opaque there, as Ghostty does.
- **macOS shadows:** a transparent window's shadow follows its content, so watch for
  ghost outlines.
- **Text edges:** antialiased text over a translucent background can show fringes.
- **Cost:** a translucent window costs the compositor a little more every frame, because
  it can no longer treat the window as opaque.

---

## Light/dark (#164)

**System signals.**
- **macOS:** the app's effective appearance. Plan 073 already observes a system-color
  notification for the accent.
- **Linux:** the xdg-desktop-portal setting `org.freedesktop.appearance` /
  `color-scheme`, over zbus, which Roost already uses for notifications.

**What it needs.**
- **Terminal:** theme pairs in Ghostty's syntax, `theme = light:<name>,dark:<name>`, and
  some **light** bundled themes (there are none today).
- **Chrome:** a light chrome palette, plus Ghostty-style `window-theme` behavior: follow
  the system, force light or dark, or derive from the terminal background.
- **Apps:** mode 2031 reports already tell apps when the scheme flips.
- **Window frame:** the title bar's appearance should follow too, through winit's window
  theme.

---

## Key questions for the planning session

1. **Independent or derived?** Are chrome themes their own files, or derived from the
   terminal theme's background and foreground (Ghostty's `window-theme = auto` /
   `ghostty`)? Or derived by default, with a named chrome theme as an override?
2. **How many knobs?** All 35 of `ChromePalette`'s fields, or a small semantic set
   (surface, raised surface, text, muted text, accent, selection, warning) from which the
   rest are derived? Fewer knobs make themes easier to write, and they stay valid as the
   palette grows.
3. **Format and location.** Reuse the terminal theme parser (Ghostty-style `key = value`)?
   Bundled themes plus a user directory such as `~/.config/roost/chrome-themes/`? What
   happens with unknown keys and bad values?
4. **Accent precedence.** Does a chrome theme set the accent, or does `chrome-accent`
   (including `system`) always win?
5. **Override syntax.** A repeatable `chrome-color = <field>:<color>` key? How are
   misspelled fields reported?
6. **Switching.** A palette **Select Chrome Theme…** with live preview, like Select
   Theme…? Remembered through the same config writer the other toggles use?
7. **Translucency defaults and names.** Ghostty-compatible terminal keys
   (`background-opacity`, `background-opacity-cells`, `background-blur`), and alpha in the
   chrome colors? Should `band` split into sidebar and tab-strip fields?
8. **Window transparency at launch.** Create the window transparent only when the config
   needs it, so switching into translucency takes a restart? Or always create it
   transparent and accept the compositor cost?
9. **Blur reach.** winit's flag covers macOS and KDE. Is `ext-background-effect-v1`
   through the Wayland hook worth adding, for COSMIC and others?
10. **Light/dark by default?** Which light terminal themes to bundle, and how pairs
    interact with the palette's Select Theme… (one pick per mode?).
11. **Legibility.** Do themes need a minimum-contrast check? Should text over a
    translucent sidebar get a scrim?
12. **Testing.**
    - Pixel tests per bundled chrome theme?
    - Does `roostctl screenshot`, which renders in-process, keep alpha so translucency is
      assertable?
    - Real on-screen checks: the Mac real-input harness on macOS (it needs Screen
      Recording), and headless weston or COSMIC captures on Linux.
13. **A high-contrast theme** for accessibility, given that VoiceOver is out of scope
    (DL-16)?
14. **One plan or two?** Chrome themes with light/dark, then translucency? Or translucency
    first, since it is smaller and mostly orthogonal?

---

## Sequencing and dependencies

- **After v0.1.0.** None of this is a Swift gap.
- **The next plan's Mac real-input harness comes first:** it gives the on-screen capture
  that judging translucency on macOS needs.
- **The next plan's Wayland hook (#351)** is the base for `ext-background-effect-v1` blur.
- **Related:**
  - [`text-rendering.md`](text-rendering.md): `web-colors` and text over backgrounds.
  - [`swift-retirement.md`](swift-retirement.md): it lists sidebar translucency as a Swift
    feature that's hard to match. Native vibrancy (`NSVisualEffectView`) was ruled out on
    2026-10-03 because it is macOS only, and this feature must cover both OSes.
