//! Geometric sprite renderer for Unicode box-drawing (U+2500–U+257F)
//! and block-element (U+2580–U+259F) glyphs.
//!
//! Pango font glyphs for these ranges don't tile pixel-perfectly
//! across adjacent cells — you get visible hairline seams in TUI
//! chrome (most obvious in the opencode wordmark logo). Ghostty
//! solves this with a custom sprite renderer in
//! `ghostty/src/font/sprite/draw/{block,box}.zig`. This module is the
//! Rust + Cairo equivalent — every dispatch arm and helper follows
//! that Zig original's pixel math. When tweaking it, cross-reference
//! the Zig source.
//!
//! Public entry point: [`draw_cell_sprite`] — returns `true` when
//! the codepoint is handled (caller skips the font glyph), `false`
//! otherwise (caller falls back to Pango).

use gtk4::cairo;
use roost_vt::ColorRgb;

/// Draw the codepoint geometrically into the cell at
/// `(x, y)..(x+w, y+h)` using the foreground color `fg`. Returns
/// `true` if `cp` is in a supported range; `false` if the caller
/// should fall back to a font glyph.
pub fn draw_cell_sprite(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    fg: ColorRgb,
    cp: u32,
) -> bool {
    match cp {
        0x2580..=0x259F => draw_block_element(cr, x, y, w, h, fg, cp),
        0x2500..=0x257F => draw_box_glyph(cr, x, y, w, h, fg, cp),
        _ => false,
    }
}

fn set_rgb(cr: &cairo::Context, c: ColorRgb) {
    let (r, g, b) = c.to_f64();
    cr.set_source_rgb(r, g, b);
}

// ---------------------------------------------------------------------------
// Layer 1: Block elements (U+2580–U+259F)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
enum HAlign {
    Left,
    Center,
    Right,
}

#[derive(Clone, Copy)]
enum VAlign {
    Top,
    Middle,
    Bottom,
}

const F_EIGHTH: f64 = 1.0 / 8.0;
const F_QUARTER: f64 = 1.0 / 4.0;
const F_3_EIGHTHS: f64 = 3.0 / 8.0;
const F_HALF: f64 = 1.0 / 2.0;
const F_5_EIGHTHS: f64 = 5.0 / 8.0;
const F_3_QUARTERS: f64 = 3.0 / 4.0;
const F_7_EIGHTHS: f64 = 7.0 / 8.0;

fn draw_block_element(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    fg: ColorRgb,
    cp: u32,
) -> bool {
    // Block elements are pure axis-aligned rect fills. Cairo's
    // default antialiasing softens edges by a fraction of a pixel
    // even on integer-aligned coordinates under some surface
    // transforms; turning it off here ensures adjacent cells (e.g.
    // the opencode wordmark) abut with no visible seam. Box-drawing
    // curves and diagonals keep the default AA so they don't go
    // jaggy.
    cr.save().ok();
    cr.set_antialias(cairo::Antialias::None);
    set_rgb(cr, fg);
    let handled = match cp {
        0x2580 => {
            // ▀ upper half
            aligned_block(cr, x, y, w, h, HAlign::Center, VAlign::Top, 1.0, F_HALF);
            true
        }
        0x2581 => {
            // ▁ lower 1/8
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_EIGHTH,
            );
            true
        }
        0x2582 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_QUARTER,
            );
            true
        }
        0x2583 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_3_EIGHTHS,
            );
            true
        }
        0x2584 => {
            // ▄ lower half
            aligned_block(cr, x, y, w, h, HAlign::Center, VAlign::Bottom, 1.0, F_HALF);
            true
        }
        0x2585 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_5_EIGHTHS,
            );
            true
        }
        0x2586 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_3_QUARTERS,
            );
            true
        }
        0x2587 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Center,
                VAlign::Bottom,
                1.0,
                F_7_EIGHTHS,
            );
            true
        }
        0x2588 => {
            // █ full
            fill_rect(cr, x, y, w, h);
            true
        }
        0x2589 => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Left,
                VAlign::Middle,
                F_7_EIGHTHS,
                1.0,
            );
            true
        }
        0x258A => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Left,
                VAlign::Middle,
                F_3_QUARTERS,
                1.0,
            );
            true
        }
        0x258B => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Left,
                VAlign::Middle,
                F_5_EIGHTHS,
                1.0,
            );
            true
        }
        0x258C => {
            // ▌ left half
            aligned_block(cr, x, y, w, h, HAlign::Left, VAlign::Middle, F_HALF, 1.0);
            true
        }
        0x258D => {
            aligned_block(
                cr,
                x,
                y,
                w,
                h,
                HAlign::Left,
                VAlign::Middle,
                F_3_EIGHTHS,
                1.0,
            );
            true
        }
        0x258E => {
            aligned_block(cr, x, y, w, h, HAlign::Left, VAlign::Middle, F_QUARTER, 1.0);
            true
        }
        0x258F => {
            aligned_block(cr, x, y, w, h, HAlign::Left, VAlign::Middle, F_EIGHTH, 1.0);
            true
        }
        0x2590 => {
            // ▐ right half
            aligned_block(cr, x, y, w, h, HAlign::Right, VAlign::Middle, F_HALF, 1.0);
            true
        }
        0x2591 | 0x2592 | 0x2593 => {
            // ░ ▒ ▓ shades
            let alpha = [0.25_f64, 0.5, 0.75][(cp - 0x2591) as usize];
            let (r, g, b) = fg.to_f64();
            cr.set_source_rgba(r, g, b, alpha);
            fill_rect(cr, x, y, w, h);
            true
        }
        0x2594 => {
            // ▔ upper 1/8
            aligned_block(cr, x, y, w, h, HAlign::Center, VAlign::Top, 1.0, F_EIGHTH);
            true
        }
        0x2595 => {
            // ▕ right 1/8
            aligned_block(cr, x, y, w, h, HAlign::Right, VAlign::Middle, F_EIGHTH, 1.0);
            true
        }
        0x2596 => {
            draw_quads(cr, x, y, w, h, false, false, true, false);
            true
        }
        0x2597 => {
            draw_quads(cr, x, y, w, h, false, false, false, true);
            true
        }
        0x2598 => {
            draw_quads(cr, x, y, w, h, true, false, false, false);
            true
        }
        0x2599 => {
            draw_quads(cr, x, y, w, h, true, false, true, true);
            true
        }
        0x259A => {
            draw_quads(cr, x, y, w, h, true, false, false, true);
            true
        }
        0x259B => {
            draw_quads(cr, x, y, w, h, true, true, true, false);
            true
        }
        0x259C => {
            draw_quads(cr, x, y, w, h, true, true, false, true);
            true
        }
        0x259D => {
            draw_quads(cr, x, y, w, h, false, true, false, false);
            true
        }
        0x259E => {
            draw_quads(cr, x, y, w, h, false, true, true, false);
            true
        }
        0x259F => {
            draw_quads(cr, x, y, w, h, false, true, true, true);
            true
        }
        _ => false,
    };
    cr.restore().ok();
    handled
}

/// Fill a sub-rect of the cell whose size is `(w*fw, h*fh)`, rounded
/// to integer pixels, then placed by the given alignment. Mirrors
/// `block.zig:121-152`'s blockShade.
fn aligned_block(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    ha: HAlign,
    va: VAlign,
    fw: f64,
    fh: f64,
) {
    let rw = (w * fw).round();
    let rh = (h * fh).round();
    let ox = match ha {
        HAlign::Left => 0.0,
        HAlign::Center => ((w - rw) / 2.0).floor(),
        HAlign::Right => w - rw,
    };
    let oy = match va {
        VAlign::Top => 0.0,
        VAlign::Middle => ((h - rh) / 2.0).floor(),
        VAlign::Bottom => h - rh,
    };
    fill_rect(cr, x + ox, y + oy, rw, rh);
}

/// Paint any combination of the four quadrants. The bottom and right
/// rects use `(h-half_h)`/`(w-half_w)` so the quadrants tile the cell
/// exactly even when `h` or `w` is odd.
fn draw_quads(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    tl: bool,
    tr: bool,
    bl: bool,
    br: bool,
) {
    let half_w = (w / 2.0).round();
    let half_h = (h / 2.0).round();
    if tl {
        fill_rect(cr, x, y, half_w, half_h);
    }
    if tr {
        fill_rect(cr, x + half_w, y, w - half_w, half_h);
    }
    if bl {
        fill_rect(cr, x, y + half_h, half_w, h - half_h);
    }
    if br {
        fill_rect(cr, x + half_w, y + half_h, w - half_w, h - half_h);
    }
}

fn fill_rect(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64) {
    cr.rectangle(x, y, w, h);
    cr.fill().ok();
}

// ---------------------------------------------------------------------------
// Layer 2: Box drawing (U+2500–U+257F)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum LineStyle {
    None,
    Light,
    Heavy,
    Double,
}

#[derive(Clone, Copy, Default)]
struct Lines4 {
    up: LineStyle,
    right: LineStyle,
    down: LineStyle,
    left: LineStyle,
}

impl Default for LineStyle {
    fn default() -> Self {
        LineStyle::None
    }
}

#[derive(Clone, Copy)]
enum Corner {
    TL,
    TR,
    BL,
    BR,
}

fn draw_box_glyph(
    cr: &cairo::Context,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
    fg: ColorRgb,
    cp: u32,
) -> bool {
    set_rgb(cr, fg);
    match cp {
        // --- simple horizontal/vertical lines ---
        0x2500 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2501 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2502 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2503 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- dashed (3-count) ---
        0x2504 => draw_h_dash(cr, x, y, w, h, 3, LineStyle::Light),
        0x2505 => draw_h_dash(cr, x, y, w, h, 3, LineStyle::Heavy),
        0x2506 => draw_v_dash(cr, x, y, w, h, 3, LineStyle::Light),
        0x2507 => draw_v_dash(cr, x, y, w, h, 3, LineStyle::Heavy),
        // (4-count)
        0x2508 => draw_h_dash(cr, x, y, w, h, 4, LineStyle::Light),
        0x2509 => draw_h_dash(cr, x, y, w, h, 4, LineStyle::Heavy),
        0x250A => draw_v_dash(cr, x, y, w, h, 4, LineStyle::Light),
        0x250B => draw_v_dash(cr, x, y, w, h, 4, LineStyle::Heavy),

        // --- single-line corners (light/heavy mixes) ---
        0x250C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x250D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x250E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x250F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2510 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2511 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2512 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2513 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2514 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2515 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2516 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2517 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2518 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2519 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x251A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x251B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- T-junctions, right side (├ family) ---
        0x251C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x251D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x251E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                right: LineStyle::Light,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x251F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                right: LineStyle::Light,
                up: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2520 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2521 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                right: LineStyle::Heavy,
                up: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2522 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                right: LineStyle::Heavy,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2523 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- T-junctions, left side (┤ family) ---
        0x2524 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2525 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2526 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                left: LineStyle::Light,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2527 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                up: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2528 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2529 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Heavy,
                up: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x252A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Heavy,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x252B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- T-junctions, down (┬ family) ---
        0x252C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x252D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                right: LineStyle::Light,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x252E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                left: LineStyle::Light,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x252F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2530 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2531 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Light,
                left: LineStyle::Heavy,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2532 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                right: LineStyle::Heavy,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2533 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- T-junctions, up (┴ family) ---
        0x2534 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2535 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                right: LineStyle::Light,
                up: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2536 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                left: LineStyle::Light,
                up: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2537 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2538 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2539 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Light,
                left: LineStyle::Heavy,
                up: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x253A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                right: LineStyle::Heavy,
                up: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x253B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- crosses (┼ family) ---
        0x253C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Light,
                right: LineStyle::Light,
            },
        ),
        0x253D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                right: LineStyle::Light,
                up: LineStyle::Light,
                down: LineStyle::Light,
            },
        ),
        0x253E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                left: LineStyle::Light,
                up: LineStyle::Light,
                down: LineStyle::Light,
            },
        ),
        0x253F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
            },
        ),
        0x2540 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Light,
                left: LineStyle::Light,
                right: LineStyle::Light,
            },
        ),
        0x2541 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                up: LineStyle::Light,
                left: LineStyle::Light,
                right: LineStyle::Light,
            },
        ),
        0x2542 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                right: LineStyle::Light,
            },
        ),
        0x2543 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                up: LineStyle::Heavy,
                right: LineStyle::Light,
                down: LineStyle::Light,
            },
        ),
        0x2544 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                up: LineStyle::Heavy,
                left: LineStyle::Light,
                down: LineStyle::Light,
            },
        ),
        0x2545 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                down: LineStyle::Heavy,
                right: LineStyle::Light,
                up: LineStyle::Light,
            },
        ),
        0x2546 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                down: LineStyle::Heavy,
                left: LineStyle::Light,
                up: LineStyle::Light,
            },
        ),
        0x2547 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                up: LineStyle::Heavy,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
            },
        ),
        0x2548 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Heavy,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
            },
        ),
        0x2549 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Light,
                left: LineStyle::Heavy,
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
            },
        ),
        0x254A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                right: LineStyle::Heavy,
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
            },
        ),
        0x254B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Heavy,
                left: LineStyle::Heavy,
                right: LineStyle::Heavy,
            },
        ),

        // --- 2-count dashed ---
        0x254C => draw_h_dash(cr, x, y, w, h, 2, LineStyle::Light),
        0x254D => draw_h_dash(cr, x, y, w, h, 2, LineStyle::Heavy),
        0x254E => draw_v_dash(cr, x, y, w, h, 2, LineStyle::Light),
        0x254F => draw_v_dash(cr, x, y, w, h, 2, LineStyle::Heavy),

        // --- double-line variants ---
        0x2550 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2551 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2552 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2553 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2554 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2555 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2556 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2557 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2558 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2559 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x255A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x255B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x255C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x255D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x255E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x255F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2560 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2561 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2562 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2563 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                left: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2564 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                left: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2565 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2566 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Double,
                left: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2567 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                left: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x2568 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                left: LineStyle::Light,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2569 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                left: LineStyle::Double,
                right: LineStyle::Double,
                ..Default::default()
            },
        ),
        0x256A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Light,
                left: LineStyle::Double,
                right: LineStyle::Double,
            },
        ),
        0x256B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                left: LineStyle::Light,
                right: LineStyle::Light,
            },
        ),
        0x256C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Double,
                down: LineStyle::Double,
                left: LineStyle::Double,
                right: LineStyle::Double,
            },
        ),

        // --- rounded corners ---
        0x256D => draw_arc(cr, x, y, w, h, Corner::BR),
        0x256E => draw_arc(cr, x, y, w, h, Corner::BL),
        0x256F => draw_arc(cr, x, y, w, h, Corner::TL),
        0x2570 => draw_arc(cr, x, y, w, h, Corner::TR),

        // --- diagonals ---
        0x2571 => draw_diag(cr, x, y, w, h, true, false),
        0x2572 => draw_diag(cr, x, y, w, h, false, true),
        0x2573 => draw_diag(cr, x, y, w, h, true, true),

        // --- half-edges (light) ---
        0x2574 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2575 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2576 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x2577 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Light,
                ..Default::default()
            },
        ),

        // --- half-edges (heavy) ---
        0x2578 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x2579 => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x257A => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x257B => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),

        // --- mixed-weight half-edges ---
        0x257C => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Light,
                right: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x257D => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Light,
                down: LineStyle::Heavy,
                ..Default::default()
            },
        ),
        0x257E => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: LineStyle::Heavy,
                right: LineStyle::Light,
                ..Default::default()
            },
        ),
        0x257F => draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: LineStyle::Heavy,
                down: LineStyle::Light,
                ..Default::default()
            },
        ),

        _ => return false,
    }
    true
}

/// "Light" stroke width derived from cell height. Roughly 7% of cell
/// height, min 1px.
fn box_thickness(h: f64) -> f64 {
    let t = (h / 14.0).round();
    if t < 1.0 {
        1.0
    } else {
        t
    }
}

/// Paint up to four cardinal-direction strokes that meet at the cell
/// center with correct heavy/double junction precedence. Direct port
/// of `box.zig::linesChar` (lines 399-637).
fn draw_box_lines(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, ln: Lines4) {
    let light = box_thickness(h);
    let heavy = 2.0 * light;

    let h_light_top = ((h - light) / 2.0).floor();
    let h_light_bot = h_light_top + light;
    let h_heavy_top = ((h - heavy) / 2.0).floor();
    let h_heavy_bot = h_heavy_top + heavy;
    let h_double_top = h_light_top - light;
    let h_double_bot = h_light_bot + light;

    let v_light_left = ((w - light) / 2.0).floor();
    let v_light_right = v_light_left + light;
    let v_heavy_left = ((w - heavy) / 2.0).floor();
    let v_heavy_right = v_heavy_left + heavy;
    let v_double_left = v_light_left - light;
    let v_double_right = v_light_right + light;

    let up_bottom = pick_junction(
        ln.left,
        ln.right,
        ln.down,
        ln.up,
        h_heavy_bot,
        h_double_bot,
        h_light_bot,
        h_light_top,
    );
    let down_top = pick_junction(
        ln.left,
        ln.right,
        ln.up,
        ln.down,
        h_heavy_top,
        h_double_top,
        h_light_top,
        h_light_bot,
    );
    let left_right = pick_junction(
        ln.up,
        ln.down,
        ln.right,
        ln.left,
        v_heavy_right,
        v_double_right,
        v_light_right,
        v_light_left,
    );
    let right_left = pick_junction(
        ln.up,
        ln.down,
        ln.left,
        ln.right,
        v_heavy_left,
        v_double_left,
        v_light_left,
        v_light_right,
    );

    // UP stroke
    match ln.up {
        LineStyle::None => {}
        LineStyle::Light => box_rect(cr, x, y, v_light_left, 0.0, v_light_right, up_bottom),
        LineStyle::Heavy => box_rect(cr, x, y, v_heavy_left, 0.0, v_heavy_right, up_bottom),
        LineStyle::Double => {
            let left_bot = if ln.left == LineStyle::Double {
                h_light_top
            } else {
                up_bottom
            };
            let right_bot = if ln.right == LineStyle::Double {
                h_light_top
            } else {
                up_bottom
            };
            box_rect(cr, x, y, v_double_left, 0.0, v_light_left, left_bot);
            box_rect(cr, x, y, v_light_right, 0.0, v_double_right, right_bot);
        }
    }

    // RIGHT stroke
    match ln.right {
        LineStyle::None => {}
        LineStyle::Light => box_rect(cr, x, y, right_left, h_light_top, w, h_light_bot),
        LineStyle::Heavy => box_rect(cr, x, y, right_left, h_heavy_top, w, h_heavy_bot),
        LineStyle::Double => {
            let top_left = if ln.up == LineStyle::Double {
                v_light_right
            } else {
                right_left
            };
            let bot_left = if ln.down == LineStyle::Double {
                v_light_right
            } else {
                right_left
            };
            box_rect(cr, x, y, top_left, h_double_top, w, h_light_top);
            box_rect(cr, x, y, bot_left, h_light_bot, w, h_double_bot);
        }
    }

    // DOWN stroke
    match ln.down {
        LineStyle::None => {}
        LineStyle::Light => box_rect(cr, x, y, v_light_left, down_top, v_light_right, h),
        LineStyle::Heavy => box_rect(cr, x, y, v_heavy_left, down_top, v_heavy_right, h),
        LineStyle::Double => {
            let left_top = if ln.left == LineStyle::Double {
                h_light_bot
            } else {
                down_top
            };
            let right_top = if ln.right == LineStyle::Double {
                h_light_bot
            } else {
                down_top
            };
            box_rect(cr, x, y, v_double_left, left_top, v_light_left, h);
            box_rect(cr, x, y, v_light_right, right_top, v_double_right, h);
        }
    }

    // LEFT stroke
    match ln.left {
        LineStyle::None => {}
        LineStyle::Light => box_rect(cr, x, y, 0.0, h_light_top, left_right, h_light_bot),
        LineStyle::Heavy => box_rect(cr, x, y, 0.0, h_heavy_top, left_right, h_heavy_bot),
        LineStyle::Double => {
            let top_right = if ln.up == LineStyle::Double {
                v_light_left
            } else {
                left_right
            };
            let bot_right = if ln.down == LineStyle::Double {
                v_light_left
            } else {
                left_right
            };
            box_rect(cr, x, y, 0.0, h_double_top, top_right, h_light_top);
            box_rect(cr, x, y, 0.0, h_light_bot, bot_right, h_double_bot);
        }
    }
}

/// Perpendicular-stroke termination logic from `linesChar`. Given
/// the perpendicular pair `(perp1, perp2)` and the parallel pair
/// `(parallel, this)`, return the coordinate where `this`'s stroke
/// ends.
fn pick_junction(
    perp1: LineStyle,
    perp2: LineStyle,
    parallel: LineStyle,
    this: LineStyle,
    heavy_edge: f64,
    double_edge: f64,
    light_edge_far: f64,
    light_edge_near: f64,
) -> f64 {
    if perp1 == LineStyle::Heavy || perp2 == LineStyle::Heavy {
        return heavy_edge;
    }
    if perp1 != perp2 || parallel == this {
        if perp1 == LineStyle::Double || perp2 == LineStyle::Double {
            return double_edge;
        }
        return light_edge_far;
    }
    if perp1 == LineStyle::None && perp2 == LineStyle::None {
        return light_edge_far;
    }
    light_edge_near
}

/// Paint a rect in cell-relative pixel coordinates (left, top,
/// right, bottom).
fn box_rect(cr: &cairo::Context, x: f64, y: f64, l: f64, t: f64, r: f64, b: f64) {
    if r <= l || b <= t {
        return;
    }
    cr.rectangle(x + l, y + t, r - l, b - t);
    cr.fill().ok();
}

/// Quadrant Bezier for the rounded-corner glyphs ╭ ╮ ╯ ╰. `corner`
/// is the *interior* corner (the side the arc bulges into).
fn draw_arc(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, c: Corner) {
    let t = box_thickness(h);
    let cx = ((w - t) / 2.0).floor() + t / 2.0;
    let cy = ((h - t) / 2.0).floor() + t / 2.0;
    let r = (w.min(h)) / 2.0;
    const S: f64 = 0.25;

    cr.new_path();
    match c {
        Corner::TL => {
            // ╯ — strokes go up + left
            cr.move_to(x + cx, y);
            cr.line_to(x + cx, y + cy - r);
            cr.curve_to(
                x + cx,
                y + cy - S * r,
                x + cx - S * r,
                y + cy,
                x + cx - r,
                y + cy,
            );
            cr.line_to(x, y + cy);
        }
        Corner::TR => {
            // ╰ — up + right
            cr.move_to(x + cx, y);
            cr.line_to(x + cx, y + cy - r);
            cr.curve_to(
                x + cx,
                y + cy - S * r,
                x + cx + S * r,
                y + cy,
                x + cx + r,
                y + cy,
            );
            cr.line_to(x + w, y + cy);
        }
        Corner::BL => {
            // ╮ — down + left
            cr.move_to(x + cx, y + h);
            cr.line_to(x + cx, y + cy + r);
            cr.curve_to(
                x + cx,
                y + cy + S * r,
                x + cx - S * r,
                y + cy,
                x + cx - r,
                y + cy,
            );
            cr.line_to(x, y + cy);
        }
        Corner::BR => {
            // ╭ — down + right
            cr.move_to(x + cx, y + h);
            cr.line_to(x + cx, y + cy + r);
            cr.curve_to(
                x + cx,
                y + cy + S * r,
                x + cx + S * r,
                y + cy,
                x + cx + r,
                y + cy,
            );
            cr.line_to(x + w, y + cy);
        }
    }
    cr.set_line_cap(cairo::LineCap::Butt);
    cr.set_line_width(t);
    cr.stroke().ok();
}

/// One or both light diagonals across the cell. Strokes overshoot
/// the corners slightly so the slope stays correct (see
/// `box.zig:638-692`).
fn draw_diag(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, ur_to_ll: bool, ul_to_lr: bool) {
    let t = box_thickness(h);
    let slope_x = (w / h).min(1.0);
    let slope_y = (h / w).min(1.0);

    cr.set_line_cap(cairo::LineCap::Butt);
    cr.set_line_width(t);
    if ur_to_ll {
        cr.new_path();
        cr.move_to(x + w + 0.5 * slope_x, y - 0.5 * slope_y);
        cr.line_to(x - 0.5 * slope_x, y + h + 0.5 * slope_y);
        cr.stroke().ok();
    }
    if ul_to_lr {
        cr.new_path();
        cr.move_to(x - 0.5 * slope_x, y - 0.5 * slope_y);
        cr.line_to(x + w + 0.5 * slope_x, y + h + 0.5 * slope_y);
        cr.stroke().ok();
    }
}

/// `count` horizontal dash segments centered vertically. Follows
/// `box.zig::dashHorizontal` (lines 779-851).
fn draw_h_dash(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, count: i32, style: LineStyle) {
    let mut thick = box_thickness(h);
    if matches!(style, LineStyle::Heavy) {
        thick *= 2.0;
    }
    let mut desired_gap = thick;
    if matches!(style, LineStyle::Light) && desired_gap < 4.0 {
        desired_gap = 4.0;
    }

    let wi = w as i32;
    if wi < count * 2 {
        draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                left: style,
                right: style,
                ..Default::default()
            },
        );
        return;
    }

    let mut gap = desired_gap as i32;
    let max_gap = wi / (2 * count);
    if gap > max_gap {
        gap = max_gap;
    }
    let total_gap = gap * count;
    let total_dash = wi - total_gap;
    let dash = total_dash / count;
    let mut extra = total_dash % count;

    let yi = ((h - thick) / 2.0).floor();
    let mut xi = (gap / 2) as f64;
    for _ in 0..count {
        let mut dw = dash;
        if extra > 0 {
            dw += 1;
            extra -= 1;
        }
        box_rect(cr, x, y, xi, yi, xi + dw as f64, yi + thick);
        xi += (dw + gap) as f64;
    }
}

/// Vertical analogue of [`draw_h_dash`].
fn draw_v_dash(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, count: i32, style: LineStyle) {
    let mut thick = box_thickness(h);
    if matches!(style, LineStyle::Heavy) {
        thick *= 2.0;
    }
    let mut desired_gap = thick;
    if matches!(style, LineStyle::Light) && desired_gap < 4.0 {
        desired_gap = 4.0;
    }

    let hi = h as i32;
    if hi < count * 2 {
        draw_box_lines(
            cr,
            x,
            y,
            w,
            h,
            Lines4 {
                up: style,
                down: style,
                ..Default::default()
            },
        );
        return;
    }

    let mut gap = desired_gap as i32;
    let max_gap = hi / (2 * count);
    if gap > max_gap {
        gap = max_gap;
    }
    let total_gap = gap * count;
    let total_dash = hi - total_gap;
    let dash = total_dash / count;
    let mut extra = total_dash % count;

    let xi = ((w - thick) / 2.0).floor();
    let mut yi = (gap / 2) as f64;
    for _ in 0..count {
        let mut dh = dash;
        if extra > 0 {
            dh += 1;
            extra -= 1;
        }
        box_rect(cr, x, y, xi, yi, xi + thick, yi + dh as f64);
        yi += (dh + gap) as f64;
    }
}

#[cfg(test)]
mod tests {
    //! Pixel-assertion suite for the sprite renderer.
    //! Renders each glyph into a Cairo ARGB32 image surface and
    //! pokes at raw bytes to verify fills land in the right places.
    //! The OpenCode-logo regression is `block_tiling_no_gap` — two
    //! adjacent █ cells must abut with no seam.
    use super::*;
    use gtk4::cairo;

    fn render(cp: u32, w: i32, h: i32) -> (Vec<u8>, i32, bool) {
        let mut surf =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("create image surface");
        let cr = cairo::Context::new(&surf).expect("create cairo context");
        let handled = draw_cell_sprite(
            &cr,
            0.0,
            0.0,
            w as f64,
            h as f64,
            ColorRgb::new(255, 255, 255),
            cp,
        );
        drop(cr);
        surf.flush();
        let stride = surf.stride();
        let data = {
            let bytes = surf.data().expect("surface data").to_vec();
            bytes
        };
        (data, stride, handled)
    }

    fn pixel_on(data: &[u8], stride: i32, x: i32, y: i32) -> bool {
        // ARGB32 in memory on little-endian hosts: B G R A bytes/pixel.
        let off = (y * stride + x * 4) as usize;
        data[off] != 0 || data[off + 1] != 0 || data[off + 2] != 0
    }

    fn pixels_on_rect(data: &[u8], stride: i32, x0: i32, y0: i32, x1: i32, y1: i32) -> i32 {
        let mut n = 0;
        for y in y0..y1 {
            for x in x0..x1 {
                if pixel_on(data, stride, x, y) {
                    n += 1;
                }
            }
        }
        n
    }

    fn rect_filled(data: &[u8], stride: i32, x0: i32, y0: i32, x1: i32, y1: i32, msg: &str) {
        for y in y0..y1 {
            for x in x0..x1 {
                assert!(
                    pixel_on(data, stride, x, y),
                    "{msg}: expected on at ({x},{y}), got off"
                );
            }
        }
    }

    fn rect_empty(data: &[u8], stride: i32, x0: i32, y0: i32, x1: i32, y1: i32, msg: &str) {
        for y in y0..y1 {
            for x in x0..x1 {
                assert!(
                    !pixel_on(data, stride, x, y),
                    "{msg}: expected off at ({x},{y}), got on"
                );
            }
        }
    }

    #[test]
    fn dispatch_skips_non_geometric() {
        for cp in [0x41u32, 0x20, 0x30, 0x24FF, 0x25A0, 0x2700] {
            let (_, _, handled) = render(cp, 8, 16);
            assert!(
                !handled,
                "U+{cp:04X} should not be handled by sprite renderer"
            );
        }
    }

    #[test]
    fn dispatch_handles_ranges() {
        for cp in [0x2500u32, 0x2580, 0x2588, 0x256D, 0x2571, 0x257F] {
            let (_, _, handled) = render(cp, 12, 24);
            assert!(handled, "U+{cp:04X} should be handled");
        }
    }

    #[test]
    fn full_block_fills_cell() {
        let (data, stride, _) = render(0x2588, 8, 16);
        rect_filled(&data, stride, 0, 0, 8, 16, "█");
    }

    #[test]
    fn upper_half_block() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x2580, w, h);
        rect_filled(&data, stride, 0, 0, w, h / 2, "▀ top half");
        rect_empty(&data, stride, 0, h / 2, w, h, "▀ bottom half");
    }

    #[test]
    fn lower_half_block() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x2584, w, h);
        rect_empty(&data, stride, 0, 0, w, h / 2, "▄ top half");
        rect_filled(&data, stride, 0, h / 2, w, h, "▄ bottom half");
    }

    #[test]
    fn left_half_block() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x258C, w, h);
        rect_filled(&data, stride, 0, 0, w / 2, h, "▌ left half");
        rect_empty(&data, stride, w / 2, 0, w, h, "▌ right half");
    }

    #[test]
    fn right_half_block() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x2590, w, h);
        rect_empty(&data, stride, 0, 0, w / 2, h, "▐ left half");
        rect_filled(&data, stride, w / 2, 0, w, h, "▐ right half");
    }

    #[test]
    fn quadrant_tl() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x2598, w, h);
        rect_filled(&data, stride, 0, 0, w / 2, h / 2, "▘ TL");
        rect_empty(&data, stride, w / 2, 0, w, h / 2, "▘ TR");
        rect_empty(&data, stride, 0, h / 2, w / 2, h, "▘ BL");
        rect_empty(&data, stride, w / 2, h / 2, w, h, "▘ BR");
    }

    #[test]
    fn quadrant_tr_plus_bl() {
        let (w, h) = (10, 20);
        let (data, stride, _) = render(0x259E, w, h);
        rect_empty(&data, stride, 0, 0, w / 2, h / 2, "▞ TL");
        rect_filled(&data, stride, w / 2, 0, w, h / 2, "▞ TR");
        rect_filled(&data, stride, 0, h / 2, w / 2, h, "▞ BL");
        rect_empty(&data, stride, w / 2, h / 2, w, h, "▞ BR");
    }

    #[test]
    fn horizontal_line_reaches_edges() {
        let (w, h) = (12, 24);
        let (data, stride, _) = render(0x2500, w, h);
        assert!(pixel_on(&data, stride, 0, h / 2), "─ left edge");
        assert!(pixel_on(&data, stride, w - 1, h / 2), "─ right edge");
        rect_empty(&data, stride, 0, 0, w, 1, "─ top row");
        rect_empty(&data, stride, 0, h - 1, w, h, "─ bottom row");
    }

    #[test]
    fn vertical_line_reaches_edges() {
        let (w, h) = (12, 24);
        let (data, stride, _) = render(0x2502, w, h);
        assert!(pixel_on(&data, stride, w / 2, 0), "│ top edge");
        assert!(pixel_on(&data, stride, w / 2, h - 1), "│ bottom edge");
        rect_empty(&data, stride, 0, 0, 1, h, "│ left col");
        rect_empty(&data, stride, w - 1, 0, w, h, "│ right col");
    }

    #[test]
    fn light_cross_reaches_all_edges() {
        let (w, h) = (14, 28);
        let (data, stride, _) = render(0x253C, w, h);
        assert!(pixel_on(&data, stride, 0, h / 2), "┼ left");
        assert!(pixel_on(&data, stride, w - 1, h / 2), "┼ right");
        assert!(pixel_on(&data, stride, w / 2, 0), "┼ top");
        assert!(pixel_on(&data, stride, w / 2, h - 1), "┼ bottom");
    }

    #[test]
    fn heavy_cross_has_more_pixels_than_light() {
        let (w, h) = (14, 28);
        let (data_light, stride, _) = render(0x253C, w, h);
        let (data_heavy, _, _) = render(0x254B, w, h);
        let count = |d: &[u8]| pixels_on_rect(d, stride, 0, 0, w, h);
        assert!(
            count(&data_heavy) > count(&data_light),
            "expected ╋ to have more on-pixels than ┼ (heavy={}, light={})",
            count(&data_heavy),
            count(&data_light)
        );
    }

    #[test]
    fn double_horizontal_has_two_runs() {
        let (w, h) = (16, 32);
        let (data, stride, _) = render(0x2550, w, h);
        let col = w / 2;
        let mut runs = 0;
        let mut prev = false;
        for y in 0..h {
            let cur = pixel_on(&data, stride, col, y);
            if cur && !prev {
                runs += 1;
            }
            prev = cur;
        }
        assert_eq!(
            runs, 2,
            "═ expected 2 horizontal stroke runs in middle column"
        );
    }

    #[test]
    fn square_corner_tl() {
        let (w, h) = (14, 28);
        let (data, stride, _) = render(0x250C, w, h);
        assert!(pixel_on(&data, stride, w - 1, h / 2), "┌ right edge");
        assert!(pixel_on(&data, stride, w / 2, h - 1), "┌ bottom edge");
        rect_empty(&data, stride, 0, 0, w, h / 2 - 2, "┌ no up stroke");
        rect_empty(&data, stride, 0, 0, w / 2 - 2, h, "┌ no left stroke");
    }

    #[test]
    fn rounded_corner_tl() {
        let (w, h) = (16, 32);
        let (data, stride, _) = render(0x256D, w, h);
        assert!(pixel_on(&data, stride, w - 1, h / 2), "╭ right edge");
        assert!(pixel_on(&data, stride, w / 2, h - 1), "╭ bottom edge");
        rect_empty(&data, stride, 0, 0, w / 4, h / 4, "╭ corner interior empty");
    }

    #[test]
    fn diagonal_ur_to_ll() {
        let (w, h) = (16, 32);
        let (data, stride, _) = render(0x2571, w, h);
        assert!(
            pixels_on_rect(&data, stride, w - 3, 0, w, 3) > 0,
            "╱ expected on-pixels near top-right"
        );
        assert_eq!(
            pixels_on_rect(&data, stride, w - 3, h - 3, w, h),
            0,
            "╱ expected no pixels near bottom-right"
        );
        assert!(
            pixels_on_rect(&data, stride, 0, h - 3, 3, h) > 0,
            "╱ expected on-pixels near bottom-left"
        );
    }

    #[test]
    fn diagonal_cross() {
        let (w, h) = (16, 32);
        let (data, stride, _) = render(0x2573, w, h);
        for c in [
            (0, 0, 3, 3),
            (w - 3, 0, w, 3),
            (0, h - 3, 3, h),
            (w - 3, h - 3, w, h),
        ] {
            assert!(
                pixels_on_rect(&data, stride, c.0, c.1, c.2, c.3) > 0,
                "╳ expected on-pixels in corner {c:?}"
            );
        }
    }

    #[test]
    fn dashed_horizontal_three_segments() {
        let (w, h) = (30, 16);
        let (data, stride, _) = render(0x2504, w, h);
        let col_on = |x| {
            for y in (h / 2 - 2)..=(h / 2 + 2) {
                if pixel_on(&data, stride, x, y) {
                    return true;
                }
            }
            false
        };
        let mut runs = 0;
        let mut prev = false;
        for x in 0..w {
            let cur = col_on(x);
            if cur && !prev {
                runs += 1;
            }
            prev = cur;
        }
        assert_eq!(runs, 3, "┄ expected 3 dash segments");
    }

    /// THE regression test — opencode-logo seams. Two █ cells stacked
    /// (or side-by-side) must abut without a gap row/column.
    #[test]
    fn block_tiling_no_gap() {
        let w = 8;
        let cell_h = 20;
        let mut surf =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w * 2, cell_h * 2).expect("surf");
        let cr = cairo::Context::new(&surf).expect("ctx");
        for row in 0..2 {
            for col in 0..2 {
                let ok = draw_cell_sprite(
                    &cr,
                    (col * w) as f64,
                    (row * cell_h) as f64,
                    w as f64,
                    cell_h as f64,
                    ColorRgb::new(255, 255, 255),
                    0x2588,
                );
                assert!(ok, "█ not handled");
            }
        }
        drop(cr);
        surf.flush();
        let stride = surf.stride();
        let data = surf.data().unwrap().to_vec();
        rect_filled(&data, stride, 0, 0, w * 2, cell_h * 2, "█x4 grid");

        // Half-block adjacency: ▄ above ▀ in the same column should tile.
        let mut surf2 =
            cairo::ImageSurface::create(cairo::Format::ARgb32, w, cell_h * 2).expect("surf2");
        let cr2 = cairo::Context::new(&surf2).expect("ctx2");
        assert!(draw_cell_sprite(
            &cr2,
            0.0,
            0.0,
            w as f64,
            cell_h as f64,
            ColorRgb::new(255, 255, 255),
            0x2584
        ));
        assert!(draw_cell_sprite(
            &cr2,
            0.0,
            cell_h as f64,
            w as f64,
            cell_h as f64,
            ColorRgb::new(255, 255, 255),
            0x2580
        ));
        drop(cr2);
        surf2.flush();
        let stride2 = surf2.stride();
        let data2 = surf2.data().unwrap().to_vec();
        let col = w / 2;
        assert!(
            pixel_on(&data2, stride2, col, cell_h - 1),
            "▄: last row of cell 0 should be on (boundary)"
        );
        assert!(
            pixel_on(&data2, stride2, col, cell_h),
            "▀: first row of cell 1 should be on (boundary)"
        );
    }
}
