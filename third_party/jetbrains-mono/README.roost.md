# Vendored JetBrains Mono (roost)

Static instances from the official [JetBrains Mono](https://www.jetbrains.com/lp/mono/)
v2.304 release, SIL Open Font License 1.1 (`OFL.txt`, unmodified):
<https://github.com/JetBrains/JetBrainsMono/releases/tag/v2.304>, asset
`JetBrainsMono-2.304.zip` (SHA-256
`6f6376c6ed2960ea8a963cd7387ec9d76e3f629125bc33d1fdcd7eb7012f7bbf`), files
taken from its `fonts/ttf/` directory and its root.

| File | SHA-256 |
|---|---|
| `JetBrainsMono-Regular.ttf` | `a0bf60ef0f83c5ed4d7a75d45838548b1f6873372dfac88f71804491898d138f` |
| `JetBrainsMono-Bold.ttf` | `5590990c82e097397517f275f430af4546e1c45cff408bde4255dad142479dcb` |
| `JetBrainsMono-Italic.ttf` | `9d0a1f7a708e6af183f1193b7e81d40da294f5c67682c085d8401c60aac8ded4` |
| `JetBrainsMono-BoldItalic.ttf` | `4039d5ce0ed225bf9c8b2c8c6436290ae2f356b7e90d70fa666227238324aa3b` |
| `OFL.txt` | `30f0c136e3c88e422d0791acd97238870f9054a9729bc34cf2ff0d4ed8cac4ad` |

Consumed via `include_bytes!` in `crates/roost-iced/src/fonts.rs`:
`install_bundled_terminal_fonts()` loads the four faces straight into
iced's font database and maps the generic `Monospace` family to them. It
is the first statement of `App::bootstrap`, ahead of the one-shot
`font_registry.rs` scan that resolves the terminal font and measures the
cell, which is why these faces are not handed to iced's `.font()` builder
the way `third_party/inter/` is: iced loads those bytes only when it
builds its compositor, after bootstrap. The result is that the default
`font-family` (`JetBrains Mono, Monospace`) and any family that does not
resolve render JetBrains Mono on every machine, and the picker always
lists it. A user's own installed font still resolves as before.

The packaged licenses ride along with the binary: `packaging/copyright`
carries the OFL stanza for the `.deb`, and `mac/scripts/bundle-iced.sh`
copies `OFL.txt` into `Roost-Iced.app/Contents/Resources/`.

**Removal condition.** Delete `third_party/jetbrains-mono/`, `fonts.rs`
and its call in `App::bootstrap` (and the license copies above) if the
terminal font becomes download-on-demand, or if the iced UI is retired.

Authoritative rationale: `CLAUDE.md` § Library preferences.
