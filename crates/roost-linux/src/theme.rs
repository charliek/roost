//! Minimal hard-coded theme for Phase 7 commit 4.
//!
//! The full theme system (bundled ghostty-format files, config-file
//! override, palette push into libghostty) lands in commit 11. For now
//! we just need a roost-dark-flavoured palette so the static `vt_write`
//! "hello" render looks reasonable side-by-side with the Go binary.

use roost_vt::ColorRgb;

#[derive(Debug, Clone)]
#[allow(dead_code)] // selection_background lands in commit 7 (selection overlay).
pub struct Theme {
    /// Canvas background — drawn before the per-cell walk.
    pub background: ColorRgb,
    /// Default cell foreground (text color).
    pub foreground: ColorRgb,
    /// Cursor color when the libghostty cursor doesn't carry an
    /// OSC-12 override.
    pub cursor: ColorRgb,
    /// Selection overlay fill (drawn with 35% alpha over the cell).
    pub selection_background: ColorRgb,
}

impl Theme {
    /// Roost-dark defaults. Matches the `roost-dark` theme bundled
    /// with the Mac UI under `mac/Sources/Roost/Resources/themes/`.
    pub fn roost_dark() -> Self {
        Self {
            background: ColorRgb::new(0x1c, 0x1c, 0x1c),
            foreground: ColorRgb::new(0xe5, 0xe5, 0xe5),
            cursor: ColorRgb::new(0xbb, 0xbb, 0xbb),
            selection_background: ColorRgb::new(0x44, 0x4f, 0x69),
        }
    }
}

impl Default for Theme {
    fn default() -> Self {
        Self::roost_dark()
    }
}
