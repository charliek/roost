# Vendored Inter (roost)

Static instances from the official [Inter](https://rsms.me/inter/) v4.1
release, SIL Open Font License 1.1 (`LICENSE.txt`, unmodified):

* `Inter-Regular.ttf`
* `Inter-Medium.ttf`
* `Inter-SemiBold.ttf`

This is the repo's **first bundled binary asset**. `third_party/swash`'s
README/license pattern transfers (provenance + removal condition, license
file alongside), but its layout does not — swash is a patched source crate
wired through `[patch.crates-io]`; Inter is three font files loaded
directly by the application.

Consumed via `include_bytes!` in `crates/roost-iced/src/main.rs`'s
`iced::application(...)` builder (`.font(...)` per weight,
`.default_font(...)` naming the family) — the iced UI's chrome font only.
Terminal cells keep the user-configured monospace font
(`font_registry.rs`'s system scan and picker are untouched; Inter is not
monospace and is excluded from the terminal font picker). The family name
and the `chrome_font(weight)` helper live in `crates/roost-iced/src/
chrome.rs` next to the other chrome constants — one seam, so a future
config-driven chrome font would touch only that helper.

**Removal condition.** Delete `third_party/inter/` and the builder wiring
in `main.rs` if the chrome font becomes user-configurable (a config key
would likely replace the bundled bytes with a system-font lookup through
`font_registry.rs`) or if the iced UI is retired.

Authoritative rationale: `CLAUDE.md` § Library preferences.
