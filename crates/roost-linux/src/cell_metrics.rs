//! Pango font measurement → cell width/height.
//!
//! The renderer walks libghostty's render state cell-by-cell, drawing
//! each cell at `(col * cell_width, row * cell_height)`. To pick those
//! constants we measure a representative glyph ('M') in the chosen
//! monospace font: width = advance of one 'M', height = layout's line
//! height (preferring `metrics.height()` from Pango ≥ 1.44, falling
//! back to ascent + descent).
//!
//! Mirrors the Go binary's `cmd/roost/font.go::measureMetrics` + the
//! Mac UI's `TerminalView.updateFont` 1:1.

use gtk4::pango::{self, FontDescription};
use gtk4::prelude::{FontFamilyExt, FontMapExt};

/// Default font family chain. JetBrains Mono is preferred when
/// installed; falls through to system monospace via Pango's
/// `Monospace` alias. The full font-family fallback resolution
/// (`pickFontFamily` from `cmd/roost/font.go`) lands in commit 11.
pub const DEFAULT_FONT_FAMILY: &str = "JetBrains Mono, Monospace";

/// Default font size in points. Matches the Mac UI default.
pub const DEFAULT_FONT_SIZE_PT: f64 = 13.0;

/// Pango-measured cell metrics.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)] // baseline becomes load-bearing when commit 5 enables PTY input + reflow.
pub struct CellMetrics {
    /// Horizontal advance of one cell in device pixels.
    pub cell_width: f64,
    /// Total vertical advance of one cell (ascent + descent + line gap)
    /// in device pixels.
    pub cell_height: f64,
    /// Baseline offset from the top of the cell, in device pixels.
    /// Glyphs are drawn at `(x, y + baseline)`.
    pub baseline: f64,
}

impl CellMetrics {
    /// Measure metrics from a Pango context + a font description. The
    /// context is typically created via `widget.pango_context()`.
    pub fn measure(context: &pango::Context, font: &FontDescription) -> Self {
        // Pango font metrics are reported in Pango units (1024 per
        // device pixel). The convert-to-pixels helper rounds down.
        let metrics = context.metrics(Some(font), None);
        let ascent = pango_to_px(metrics.ascent());
        let descent = pango_to_px(metrics.descent());
        // Pango 1.44+ exposes `height` (the recommended line height
        // including line gap). Earlier versions return 0 from that
        // accessor; in that case the safe fallback is ascent + descent.
        let height = pango_to_px(metrics.height());
        let cell_height = if height > 0.0 {
            height
        } else {
            ascent + descent
        };

        // Cell width = advance of an 'M' glyph (the widest reasonable
        // ASCII letter; monospace fonts assign all glyphs the same
        // advance, so 'M' is just a convenient picker).
        let layout = pango::Layout::new(context);
        layout.set_font_description(Some(font));
        layout.set_text("M");
        let (logical_width, _) = layout.size();
        let cell_width = pango_to_px(logical_width).max(1.0);

        // Floor the cell-height to integer pixels for cell-aligned
        // rendering. Sub-pixel cell heights produce visible smearing
        // across rows; the Go binary made the same choice. Cell width
        // is also floored for the same reason.
        let cell_width = cell_width.floor().max(1.0);
        let cell_height = cell_height.floor().max(1.0);

        // Baseline = ascent, also floored so glyph y-coords are integers.
        let baseline = ascent.floor().max(0.0);

        Self {
            cell_width,
            cell_height,
            baseline,
        }
    }
}

fn pango_to_px(units: i32) -> f64 {
    units as f64 / pango::SCALE as f64
}

/// Fontconfig generic aliases. These always resolve to *some* installed
/// face, so they can never be "missing" — skip them to avoid a false
/// fallback warning.
const GENERIC_FAMILIES: &[&str] = &[
    "monospace",
    "mono",
    "sans",
    "sans-serif",
    "serif",
    "system-ui",
    "cursive",
    "fantasy",
    "emoji",
    "math",
];

/// Warn once when the configured primary font family is not installed,
/// so the silent fall-through to `Monospace` is visible in the log.
/// Diagnostic only — no behavioral change: `Monospace` is a fontconfig
/// generic that always resolves to a monospace face, so cell alignment
/// is preserved. Mirrors the Go binary's `pickFontFamily`. `configured`
/// is the user's comma-separated `font_family` (the first entry is the
/// primary); `None` means the `DEFAULT_FONT_FAMILY` default applies.
pub fn warn_if_primary_family_missing(context: &pango::Context, configured: Option<&str>) {
    let list = configured.unwrap_or(DEFAULT_FONT_FAMILY);
    let Some(primary) = list
        .split(',')
        .map(str::trim)
        .find(|entry| !entry.is_empty())
    else {
        return;
    };
    if GENERIC_FAMILIES.contains(&primary.to_ascii_lowercase().as_str()) {
        return;
    }
    let Some(font_map) = context.font_map() else {
        return;
    };
    let installed = font_map
        .list_families()
        .iter()
        .any(|family| family.name().eq_ignore_ascii_case(primary));
    if !installed {
        tracing::warn!(
            requested = primary,
            "configured font family not installed; falling back to Monospace"
        );
    }
}

/// Build the default monospace font description from the family +
/// size constants above. Returns an owned `FontDescription` the
/// caller is responsible for keeping alive while measuring or
/// laying out text.
pub fn default_font_description() -> FontDescription {
    let mut font = FontDescription::from_string(DEFAULT_FONT_FAMILY);
    font.set_absolute_size(DEFAULT_FONT_SIZE_PT * pango::SCALE as f64 * 96.0 / 72.0);
    font
}
