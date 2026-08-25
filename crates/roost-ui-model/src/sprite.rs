//! Toolkit-neutral geometry for Unicode box-drawing (U+2500–U+257F)
//! and block-element (U+2580–U+259F) glyphs.
//!
//! Font glyphs for these ranges don't tile pixel-perfectly across
//! adjacent cells — you get visible hairline seams in TUI chrome (most
//! obvious in the opencode wordmark logo). Ghostty solves this with a
//! custom sprite renderer in
//! `ghostty/src/font/sprite/draw/{block,box}.zig`; this module is the
//! pure-data equivalent of that pixel math — every dispatch arm and
//! helper follows the Zig original. When tweaking it, cross-reference
//! the Zig source.
//!
//! Nothing here touches a toolkit: [`sprite_geometry`] answers with
//! cell-relative primitives and the UI adapter renders them its own
//! way (iced snaps [`tessellate`]'s rects to integers and emits quads;
//! the now-removed GTK UI stroked/filled them through cairo). Color
//! never crosses this boundary — sprites are monochrome foreground.

/// Axis-aligned rectangle in cell-relative pixels. The adapter adds
/// the cell origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpriteRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// A point in cell-relative pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpritePoint {
    pub x: f64,
    pub y: f64,
}

/// The *interior* corner of a rounded-corner glyph — the side the arc
/// bulges into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TL,
    TR,
    BL,
    BR,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SpritePrimitive {
    /// Axis-aligned fill in cell-relative px. `alpha` is 1.0 except
    /// for the shade glyphs ░▒▓.
    Rect { rect: SpriteRect, alpha: f64 },
    /// Rounded corner ╭╮╯╰: a straight leg from a cell edge, a cubic
    /// Bézier, then a straight leg to another edge. Carries the cell
    /// dimensions so every consumer derives the identical path from
    /// [`arc_path`].
    CornerArc {
        corner: Corner,
        w: f64,
        h: f64,
        thickness: f64,
    },
    /// Diagonal stroke ╱╲ endpoint pair (corner overshoot baked in)
    /// plus stroke thickness.
    Diagonal {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        thickness: f64,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SpriteGeometry {
    pub primitives: Vec<SpritePrimitive>,
    /// GTK-ONLY semantics: the block-element layer (U+2580–U+259F)
    /// asks for antialiasing *off* because cairo softens edges by a
    /// fraction of a pixel even on integer-aligned coordinates under
    /// some surface transforms, which shows up as a seam between
    /// adjacent full blocks. Box-drawing curves and diagonals keep AA
    /// on so they don't go jaggy. iced has no per-quad AA switch and
    /// ignores this field — its seam story is integer edge snapping.
    pub antialias: bool,
}

/// The three-segment rounded-corner path in cell-relative coordinates:
/// `start` → `leg_end` (straight), `leg_end` → `curve_end` (cubic
/// Bézier through `c1`/`c2`), `curve_end` → `end` (straight). Stroked
/// with butt caps at width `thickness`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ArcPath {
    pub start: SpritePoint,
    pub leg_end: SpritePoint,
    pub c1: SpritePoint,
    pub c2: SpritePoint,
    pub curve_end: SpritePoint,
    pub end: SpritePoint,
    pub thickness: f64,
}

/// Geometry for the codepoint in a `w` × `h` cell, or `None` when `cp`
/// is not a sprite codepoint (the caller falls back to a font glyph).
pub fn sprite_geometry(cp: u32, w: f64, h: f64) -> Option<SpriteGeometry> {
    match cp {
        0x2580..=0x259F => block_geometry(cp, w, h).map(|primitives| SpriteGeometry {
            primitives,
            antialias: false,
        }),
        0x2500..=0x257F => box_geometry(cp, w, h).map(|primitives| SpriteGeometry {
            primitives,
            antialias: true,
        }),
        _ => None,
    }
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

fn block_geometry(cp: u32, w: f64, h: f64) -> Option<Vec<SpritePrimitive>> {
    use HAlign::{Center, Left, Right};
    use VAlign::{Bottom, Middle, Top};

    let mut out = Vec::new();
    match cp {
        // ▀ upper half
        0x2580 => aligned_block(&mut out, w, h, Center, Top, 1.0, F_HALF),
        // ▁ lower 1/8
        0x2581 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_EIGHTH),
        0x2582 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_QUARTER),
        0x2583 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_3_EIGHTHS),
        // ▄ lower half
        0x2584 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_HALF),
        0x2585 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_5_EIGHTHS),
        0x2586 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_3_QUARTERS),
        0x2587 => aligned_block(&mut out, w, h, Center, Bottom, 1.0, F_7_EIGHTHS),
        // █ full
        0x2588 => fill_rect(&mut out, 0.0, 0.0, w, h),
        0x2589 => aligned_block(&mut out, w, h, Left, Middle, F_7_EIGHTHS, 1.0),
        0x258A => aligned_block(&mut out, w, h, Left, Middle, F_3_QUARTERS, 1.0),
        0x258B => aligned_block(&mut out, w, h, Left, Middle, F_5_EIGHTHS, 1.0),
        // ▌ left half
        0x258C => aligned_block(&mut out, w, h, Left, Middle, F_HALF, 1.0),
        0x258D => aligned_block(&mut out, w, h, Left, Middle, F_3_EIGHTHS, 1.0),
        0x258E => aligned_block(&mut out, w, h, Left, Middle, F_QUARTER, 1.0),
        0x258F => aligned_block(&mut out, w, h, Left, Middle, F_EIGHTH, 1.0),
        // ▐ right half
        0x2590 => aligned_block(&mut out, w, h, Right, Middle, F_HALF, 1.0),
        // ░ ▒ ▓ shades
        0x2591..=0x2593 => {
            let alpha = [0.25_f64, 0.5, 0.75][(cp - 0x2591) as usize];
            out.push(SpritePrimitive::Rect {
                rect: SpriteRect {
                    x: 0.0,
                    y: 0.0,
                    w,
                    h,
                },
                alpha,
            });
        }
        // ▔ upper 1/8
        0x2594 => aligned_block(&mut out, w, h, Center, Top, 1.0, F_EIGHTH),
        // ▕ right 1/8
        0x2595 => aligned_block(&mut out, w, h, Right, Middle, F_EIGHTH, 1.0),
        0x2596 => draw_quads(&mut out, w, h, false, false, true, false),
        0x2597 => draw_quads(&mut out, w, h, false, false, false, true),
        0x2598 => draw_quads(&mut out, w, h, true, false, false, false),
        0x2599 => draw_quads(&mut out, w, h, true, false, true, true),
        0x259A => draw_quads(&mut out, w, h, true, false, false, true),
        0x259B => draw_quads(&mut out, w, h, true, true, true, false),
        0x259C => draw_quads(&mut out, w, h, true, true, false, true),
        0x259D => draw_quads(&mut out, w, h, false, true, false, false),
        0x259E => draw_quads(&mut out, w, h, false, true, true, false),
        0x259F => draw_quads(&mut out, w, h, false, true, true, true),
        _ => return None,
    }
    Some(out)
}

/// Emit a sub-rect of the cell whose size is `(w*fw, h*fh)`, rounded
/// to integer pixels, then placed by the given alignment. Mirrors
/// `block.zig:121-152`'s blockShade.
fn aligned_block(
    out: &mut Vec<SpritePrimitive>,
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
    fill_rect(out, ox, oy, rw, rh);
}

/// Any combination of the four quadrants. The bottom and right rects
/// use `(h-half_h)`/`(w-half_w)` so the quadrants tile the cell exactly
/// even when `h` or `w` is odd.
fn draw_quads(
    out: &mut Vec<SpritePrimitive>,
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
        fill_rect(out, 0.0, 0.0, half_w, half_h);
    }
    if tr {
        fill_rect(out, half_w, 0.0, w - half_w, half_h);
    }
    if bl {
        fill_rect(out, 0.0, half_h, half_w, h - half_h);
    }
    if br {
        fill_rect(out, half_w, half_h, w - half_w, h - half_h);
    }
}

fn fill_rect(out: &mut Vec<SpritePrimitive>, x: f64, y: f64, w: f64, h: f64) {
    out.push(SpritePrimitive::Rect {
        rect: SpriteRect { x, y, w, h },
        alpha: 1.0,
    });
}

// ---------------------------------------------------------------------------
// Layer 2: Box drawing (U+2500–U+257F)
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum LineStyle {
    #[default]
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

const fn l4(up: LineStyle, right: LineStyle, down: LineStyle, left: LineStyle) -> Lines4 {
    Lines4 {
        up,
        right,
        down,
        left,
    }
}

fn box_geometry(cp: u32, w: f64, h: f64) -> Option<Vec<SpritePrimitive>> {
    use LineStyle::{Double as D, Heavy as H, Light as L, None as N};

    let out = match cp {
        // --- simple horizontal/vertical lines ---
        0x2500 => box_lines(w, h, l4(N, L, N, L)),
        0x2501 => box_lines(w, h, l4(N, H, N, H)),
        0x2502 => box_lines(w, h, l4(L, N, L, N)),
        0x2503 => box_lines(w, h, l4(H, N, H, N)),

        // --- dashed (3-count, 4-count) ---
        0x2504 => h_dash(w, h, 3, L),
        0x2505 => h_dash(w, h, 3, H),
        0x2506 => v_dash(w, h, 3, L),
        0x2507 => v_dash(w, h, 3, H),
        0x2508 => h_dash(w, h, 4, L),
        0x2509 => h_dash(w, h, 4, H),
        0x250A => v_dash(w, h, 4, L),
        0x250B => v_dash(w, h, 4, H),

        // --- single-line corners (light/heavy mixes) ---
        0x250C => box_lines(w, h, l4(N, L, L, N)),
        0x250D => box_lines(w, h, l4(N, H, L, N)),
        0x250E => box_lines(w, h, l4(N, L, H, N)),
        0x250F => box_lines(w, h, l4(N, H, H, N)),
        0x2510 => box_lines(w, h, l4(N, N, L, L)),
        0x2511 => box_lines(w, h, l4(N, N, L, H)),
        0x2512 => box_lines(w, h, l4(N, N, H, L)),
        0x2513 => box_lines(w, h, l4(N, N, H, H)),
        0x2514 => box_lines(w, h, l4(L, L, N, N)),
        0x2515 => box_lines(w, h, l4(L, H, N, N)),
        0x2516 => box_lines(w, h, l4(H, L, N, N)),
        0x2517 => box_lines(w, h, l4(H, H, N, N)),
        0x2518 => box_lines(w, h, l4(L, N, N, L)),
        0x2519 => box_lines(w, h, l4(L, N, N, H)),
        0x251A => box_lines(w, h, l4(H, N, N, L)),
        0x251B => box_lines(w, h, l4(H, N, N, H)),

        // --- T-junctions, right side (├ family) ---
        0x251C => box_lines(w, h, l4(L, L, L, N)),
        0x251D => box_lines(w, h, l4(L, H, L, N)),
        0x251E => box_lines(w, h, l4(H, L, L, N)),
        0x251F => box_lines(w, h, l4(L, L, H, N)),
        0x2520 => box_lines(w, h, l4(H, L, H, N)),
        0x2521 => box_lines(w, h, l4(H, H, L, N)),
        0x2522 => box_lines(w, h, l4(L, H, H, N)),
        0x2523 => box_lines(w, h, l4(H, H, H, N)),

        // --- T-junctions, left side (┤ family) ---
        0x2524 => box_lines(w, h, l4(L, N, L, L)),
        0x2525 => box_lines(w, h, l4(L, N, L, H)),
        0x2526 => box_lines(w, h, l4(H, N, L, L)),
        0x2527 => box_lines(w, h, l4(L, N, H, L)),
        0x2528 => box_lines(w, h, l4(H, N, H, L)),
        0x2529 => box_lines(w, h, l4(H, N, L, H)),
        0x252A => box_lines(w, h, l4(L, N, H, H)),
        0x252B => box_lines(w, h, l4(H, N, H, H)),

        // --- T-junctions, down (┬ family) ---
        0x252C => box_lines(w, h, l4(N, L, L, L)),
        0x252D => box_lines(w, h, l4(N, L, L, H)),
        0x252E => box_lines(w, h, l4(N, H, L, L)),
        0x252F => box_lines(w, h, l4(N, H, L, H)),
        0x2530 => box_lines(w, h, l4(N, L, H, L)),
        0x2531 => box_lines(w, h, l4(N, L, H, H)),
        0x2532 => box_lines(w, h, l4(N, H, H, L)),
        0x2533 => box_lines(w, h, l4(N, H, H, H)),

        // --- T-junctions, up (┴ family) ---
        0x2534 => box_lines(w, h, l4(L, L, N, L)),
        0x2535 => box_lines(w, h, l4(L, L, N, H)),
        0x2536 => box_lines(w, h, l4(L, H, N, L)),
        0x2537 => box_lines(w, h, l4(L, H, N, H)),
        0x2538 => box_lines(w, h, l4(H, L, N, L)),
        0x2539 => box_lines(w, h, l4(H, L, N, H)),
        0x253A => box_lines(w, h, l4(H, H, N, L)),
        0x253B => box_lines(w, h, l4(H, H, N, H)),

        // --- crosses (┼ family) ---
        0x253C => box_lines(w, h, l4(L, L, L, L)),
        0x253D => box_lines(w, h, l4(L, L, L, H)),
        0x253E => box_lines(w, h, l4(L, H, L, L)),
        0x253F => box_lines(w, h, l4(L, H, L, H)),
        0x2540 => box_lines(w, h, l4(H, L, L, L)),
        0x2541 => box_lines(w, h, l4(L, L, H, L)),
        0x2542 => box_lines(w, h, l4(H, L, H, L)),
        0x2543 => box_lines(w, h, l4(H, L, L, H)),
        0x2544 => box_lines(w, h, l4(H, H, L, L)),
        0x2545 => box_lines(w, h, l4(L, L, H, H)),
        0x2546 => box_lines(w, h, l4(L, H, H, L)),
        0x2547 => box_lines(w, h, l4(H, H, L, H)),
        0x2548 => box_lines(w, h, l4(L, H, H, H)),
        0x2549 => box_lines(w, h, l4(H, L, H, H)),
        0x254A => box_lines(w, h, l4(H, H, H, L)),
        0x254B => box_lines(w, h, l4(H, H, H, H)),

        // --- 2-count dashed ---
        0x254C => h_dash(w, h, 2, L),
        0x254D => h_dash(w, h, 2, H),
        0x254E => v_dash(w, h, 2, L),
        0x254F => v_dash(w, h, 2, H),

        // --- double-line variants ---
        0x2550 => box_lines(w, h, l4(N, D, N, D)),
        0x2551 => box_lines(w, h, l4(D, N, D, N)),
        0x2552 => box_lines(w, h, l4(N, D, L, N)),
        0x2553 => box_lines(w, h, l4(N, L, D, N)),
        0x2554 => box_lines(w, h, l4(N, D, D, N)),
        0x2555 => box_lines(w, h, l4(N, N, L, D)),
        0x2556 => box_lines(w, h, l4(N, N, D, L)),
        0x2557 => box_lines(w, h, l4(N, N, D, D)),
        0x2558 => box_lines(w, h, l4(L, D, N, N)),
        0x2559 => box_lines(w, h, l4(D, L, N, N)),
        0x255A => box_lines(w, h, l4(D, D, N, N)),
        0x255B => box_lines(w, h, l4(L, N, N, D)),
        0x255C => box_lines(w, h, l4(D, N, N, L)),
        0x255D => box_lines(w, h, l4(D, N, N, D)),
        0x255E => box_lines(w, h, l4(L, D, L, N)),
        0x255F => box_lines(w, h, l4(D, L, D, N)),
        0x2560 => box_lines(w, h, l4(D, D, D, N)),
        0x2561 => box_lines(w, h, l4(L, N, L, D)),
        0x2562 => box_lines(w, h, l4(D, N, D, L)),
        0x2563 => box_lines(w, h, l4(D, N, D, D)),
        0x2564 => box_lines(w, h, l4(N, D, L, D)),
        0x2565 => box_lines(w, h, l4(N, L, D, L)),
        0x2566 => box_lines(w, h, l4(N, D, D, D)),
        0x2567 => box_lines(w, h, l4(L, D, N, D)),
        0x2568 => box_lines(w, h, l4(D, L, N, L)),
        0x2569 => box_lines(w, h, l4(D, D, N, D)),
        0x256A => box_lines(w, h, l4(L, D, L, D)),
        0x256B => box_lines(w, h, l4(D, L, D, L)),
        0x256C => box_lines(w, h, l4(D, D, D, D)),

        // --- rounded corners ---
        0x256D => arc(w, h, Corner::BR),
        0x256E => arc(w, h, Corner::BL),
        0x256F => arc(w, h, Corner::TL),
        0x2570 => arc(w, h, Corner::TR),

        // --- diagonals ---
        0x2571 => diag(w, h, true, false),
        0x2572 => diag(w, h, false, true),
        0x2573 => diag(w, h, true, true),

        // --- half-edges (light) ---
        0x2574 => box_lines(w, h, l4(N, N, N, L)),
        0x2575 => box_lines(w, h, l4(L, N, N, N)),
        0x2576 => box_lines(w, h, l4(N, L, N, N)),
        0x2577 => box_lines(w, h, l4(N, N, L, N)),

        // --- half-edges (heavy) ---
        0x2578 => box_lines(w, h, l4(N, N, N, H)),
        0x2579 => box_lines(w, h, l4(H, N, N, N)),
        0x257A => box_lines(w, h, l4(N, H, N, N)),
        0x257B => box_lines(w, h, l4(N, N, H, N)),

        // --- mixed-weight half-edges ---
        0x257C => box_lines(w, h, l4(N, H, N, L)),
        0x257D => box_lines(w, h, l4(L, N, H, N)),
        0x257E => box_lines(w, h, l4(N, L, N, H)),
        0x257F => box_lines(w, h, l4(H, N, L, N)),

        _ => return None,
    };
    Some(out)
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

/// Up to four cardinal-direction strokes that meet at the cell center
/// with correct heavy/double junction precedence. Direct port of
/// `box.zig::linesChar` (lines 399-637).
fn box_lines(w: f64, h: f64, ln: Lines4) -> Vec<SpritePrimitive> {
    let mut out = Vec::new();
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
        LineStyle::Light => box_rect(&mut out, v_light_left, 0.0, v_light_right, up_bottom),
        LineStyle::Heavy => box_rect(&mut out, v_heavy_left, 0.0, v_heavy_right, up_bottom),
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
            box_rect(&mut out, v_double_left, 0.0, v_light_left, left_bot);
            box_rect(&mut out, v_light_right, 0.0, v_double_right, right_bot);
        }
    }

    // RIGHT stroke
    match ln.right {
        LineStyle::None => {}
        LineStyle::Light => box_rect(&mut out, right_left, h_light_top, w, h_light_bot),
        LineStyle::Heavy => box_rect(&mut out, right_left, h_heavy_top, w, h_heavy_bot),
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
            box_rect(&mut out, top_left, h_double_top, w, h_light_top);
            box_rect(&mut out, bot_left, h_light_bot, w, h_double_bot);
        }
    }

    // DOWN stroke
    match ln.down {
        LineStyle::None => {}
        LineStyle::Light => box_rect(&mut out, v_light_left, down_top, v_light_right, h),
        LineStyle::Heavy => box_rect(&mut out, v_heavy_left, down_top, v_heavy_right, h),
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
            box_rect(&mut out, v_double_left, left_top, v_light_left, h);
            box_rect(&mut out, v_light_right, right_top, v_double_right, h);
        }
    }

    // LEFT stroke
    match ln.left {
        LineStyle::None => {}
        LineStyle::Light => box_rect(&mut out, 0.0, h_light_top, left_right, h_light_bot),
        LineStyle::Heavy => box_rect(&mut out, 0.0, h_heavy_top, left_right, h_heavy_bot),
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
            box_rect(&mut out, 0.0, h_double_top, top_right, h_light_top);
            box_rect(&mut out, 0.0, h_light_bot, bot_right, h_double_bot);
        }
    }

    out
}

/// Perpendicular-stroke termination logic from `linesChar`. Given the
/// perpendicular pair `(perp1, perp2)` and the parallel pair
/// `(parallel, this)`, return the coordinate where `this`'s stroke
/// ends.
#[allow(clippy::too_many_arguments)]
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

/// Emit a rect given cell-relative edges (left, top, right, bottom).
fn box_rect(out: &mut Vec<SpritePrimitive>, l: f64, t: f64, r: f64, b: f64) {
    if r <= l || b <= t {
        return;
    }
    out.push(SpritePrimitive::Rect {
        rect: SpriteRect {
            x: l,
            y: t,
            w: r - l,
            h: b - t,
        },
        alpha: 1.0,
    });
}

fn arc(w: f64, h: f64, corner: Corner) -> Vec<SpritePrimitive> {
    vec![SpritePrimitive::CornerArc {
        corner,
        w,
        h,
        thickness: box_thickness(h),
    }]
}

/// One or both light diagonals across the cell. Strokes overshoot the
/// corners slightly so the slope stays correct across adjacent cells
/// (see `box.zig:638-692`).
fn diag(w: f64, h: f64, ur_to_ll: bool, ul_to_lr: bool) -> Vec<SpritePrimitive> {
    let mut out = Vec::new();
    let t = box_thickness(h);
    let slope_x = (w / h).min(1.0);
    let slope_y = (h / w).min(1.0);

    if ur_to_ll {
        out.push(SpritePrimitive::Diagonal {
            x0: w + 0.5 * slope_x,
            y0: -0.5 * slope_y,
            x1: -0.5 * slope_x,
            y1: h + 0.5 * slope_y,
            thickness: t,
        });
    }
    if ul_to_lr {
        out.push(SpritePrimitive::Diagonal {
            x0: -0.5 * slope_x,
            y0: -0.5 * slope_y,
            x1: w + 0.5 * slope_x,
            y1: h + 0.5 * slope_y,
            thickness: t,
        });
    }
    out
}

/// `count` horizontal dash segments centered vertically. Follows
/// `box.zig::dashHorizontal` (lines 779-851).
fn h_dash(w: f64, h: f64, count: i32, style: LineStyle) -> Vec<SpritePrimitive> {
    let mut out = Vec::new();
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
        return box_lines(
            w,
            h,
            Lines4 {
                left: style,
                right: style,
                ..Default::default()
            },
        );
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
        box_rect(&mut out, xi, yi, xi + dw as f64, yi + thick);
        xi += (dw + gap) as f64;
    }
    out
}

/// Vertical analogue of [`h_dash`].
fn v_dash(w: f64, h: f64, count: i32, style: LineStyle) -> Vec<SpritePrimitive> {
    let mut out = Vec::new();
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
        return box_lines(
            w,
            h,
            Lines4 {
                up: style,
                down: style,
                ..Default::default()
            },
        );
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
        box_rect(&mut out, xi, yi, xi + thick, yi + dh as f64);
        yi += (dh + gap) as f64;
    }
    out
}

// ---------------------------------------------------------------------------
// Arc path + tessellation
// ---------------------------------------------------------------------------

/// Bézier control-point pull for the quadrant arc. Matches the Zig
/// original's rounded-corner shape.
const ARC_S: f64 = 0.25;

/// The exact three-segment path for a rounded-corner glyph ╭ ╮ ╯ ╰ in
/// cell-relative coordinates. [`tessellate`] derives from this one
/// description (as the now-removed GTK adapter's cairo calls once did).
pub fn arc_path(corner: Corner, w: f64, h: f64) -> ArcPath {
    let t = box_thickness(h);
    let cx = ((w - t) / 2.0).floor() + t / 2.0;
    let cy = ((h - t) / 2.0).floor() + t / 2.0;
    let r = (w.min(h)) / 2.0;
    let s = ARC_S;

    let p = |x: f64, y: f64| SpritePoint { x, y };
    match corner {
        // ╯ — strokes go up + left
        Corner::TL => ArcPath {
            start: p(cx, 0.0),
            leg_end: p(cx, cy - r),
            c1: p(cx, cy - s * r),
            c2: p(cx - s * r, cy),
            curve_end: p(cx - r, cy),
            end: p(0.0, cy),
            thickness: t,
        },
        // ╰ — up + right
        Corner::TR => ArcPath {
            start: p(cx, 0.0),
            leg_end: p(cx, cy - r),
            c1: p(cx, cy - s * r),
            c2: p(cx + s * r, cy),
            curve_end: p(cx + r, cy),
            end: p(w, cy),
            thickness: t,
        },
        // ╮ — down + left
        Corner::BL => ArcPath {
            start: p(cx, h),
            leg_end: p(cx, cy + r),
            c1: p(cx, cy + s * r),
            c2: p(cx - s * r, cy),
            curve_end: p(cx - r, cy),
            end: p(0.0, cy),
            thickness: t,
        },
        // ╭ — down + right
        Corner::BR => ArcPath {
            start: p(cx, h),
            leg_end: p(cx, cy + r),
            c1: p(cx, cy + s * r),
            c2: p(cx + s * r, cy),
            curve_end: p(cx + r, cy),
            end: p(w, cy),
            thickness: t,
        },
    }
}

/// Flatten a primitive into axis-aligned rects. `Rect` passes through
/// unchanged (alpha stays on the primitive — it never rides on the
/// tessellation output). Curves and diagonals become overlapping
/// `thickness`×`thickness` stamps centered on samples spaced no more
/// than `thickness/2` apart.
pub fn tessellate(prim: &SpritePrimitive) -> Vec<SpriteRect> {
    match *prim {
        SpritePrimitive::Rect { rect, .. } => vec![rect],
        SpritePrimitive::CornerArc {
            corner,
            w,
            h,
            thickness,
        } => {
            let path = arc_path(corner, w, h);
            let mut samples = Vec::new();
            sample_line(&mut samples, path.start, path.leg_end, thickness, true);
            sample_cubic(
                &mut samples,
                path.leg_end,
                path.c1,
                path.c2,
                path.curve_end,
                thickness,
            );
            sample_line(&mut samples, path.curve_end, path.end, thickness, false);
            // Cairo strokes this path with butt caps, so the ink stops
            // at the endpoints; centered stamps would spill half a
            // thickness past the cell edge into the neighbor. Clipping
            // to the cell keeps a sprite's ink inside its own cell.
            stamps(&samples, thickness, Some((w, h)))
        }
        SpritePrimitive::Diagonal {
            x0,
            y0,
            x1,
            y1,
            thickness,
        } => {
            let mut samples = Vec::new();
            sample_line(
                &mut samples,
                SpritePoint { x: x0, y: y0 },
                SpritePoint { x: x1, y: y1 },
                thickness,
                true,
            );
            // No clip: the endpoints deliberately overshoot the cell so
            // the slope stays continuous across adjacent cells.
            stamps(&samples, thickness, None)
        }
    }
}

fn step_count(len: f64, thickness: f64) -> usize {
    let spacing = thickness / 2.0;
    let n = (len / spacing).ceil();
    if n.is_finite() && n >= 1.0 {
        n as usize
    } else {
        1
    }
}

fn sample_line(
    out: &mut Vec<SpritePoint>,
    a: SpritePoint,
    b: SpritePoint,
    thickness: f64,
    include_start: bool,
) {
    let n = step_count((b.x - a.x).hypot(b.y - a.y), thickness);
    let first = if include_start { 0 } else { 1 };
    out.reserve(n + 1 - first);
    for i in first..=n {
        let u = i as f64 / n as f64;
        out.push(SpritePoint {
            x: a.x + (b.x - a.x) * u,
            y: a.y + (b.y - a.y) * u,
        });
    }
}

fn sample_cubic(
    out: &mut Vec<SpritePoint>,
    p0: SpritePoint,
    c1: SpritePoint,
    c2: SpritePoint,
    p3: SpritePoint,
    thickness: f64,
) {
    // |B'(u)| <= 3 * max control-polygon leg, so stepping on that bound
    // keeps sample spacing at or under thickness/2 without measuring
    // arc length.
    let leg = |a: SpritePoint, b: SpritePoint| (b.x - a.x).hypot(b.y - a.y);
    let max_leg = leg(p0, c1).max(leg(c1, c2)).max(leg(c2, p3));
    let n = step_count(3.0 * max_leg, thickness);
    out.reserve(n);
    for i in 1..=n {
        let u = i as f64 / n as f64;
        let v = 1.0 - u;
        let (b0, b1, b2, b3) = (v * v * v, 3.0 * v * v * u, 3.0 * v * u * u, u * u * u);
        out.push(SpritePoint {
            x: b0 * p0.x + b1 * c1.x + b2 * c2.x + b3 * p3.x,
            y: b0 * p0.y + b1 * c1.y + b2 * c2.y + b3 * p3.y,
        });
    }
}

fn stamps(samples: &[SpritePoint], thickness: f64, clip: Option<(f64, f64)>) -> Vec<SpriteRect> {
    let half = thickness / 2.0;
    let mut out = Vec::with_capacity(samples.len());
    for s in samples {
        let (mut x0, mut y0) = (s.x - half, s.y - half);
        let (mut x1, mut y1) = (s.x + half, s.y + half);
        if let Some((w, h)) = clip {
            x0 = x0.max(0.0);
            y0 = y0.max(0.0);
            x1 = x1.min(w);
            y1 = y1.min(h);
            if x1 <= x0 || y1 <= y0 {
                continue;
            }
        }
        out.push(SpriteRect {
            x: x0,
            y: y0,
            w: x1 - x0,
            h: y1 - y0,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIZES: [(f64, f64); 3] = [(8.0, 16.0), (9.0, 19.0), (12.0, 24.0)];

    fn rects(cp: u32, w: f64, h: f64) -> Vec<(SpriteRect, f64)> {
        sprite_geometry(cp, w, h)
            .expect("geometry")
            .primitives
            .iter()
            .filter_map(|p| match *p {
                SpritePrimitive::Rect { rect, alpha } => Some((rect, alpha)),
                _ => None,
            })
            .collect()
    }

    fn only_rect(cp: u32, w: f64, h: f64) -> SpriteRect {
        let r = rects(cp, w, h);
        assert_eq!(r.len(), 1, "U+{cp:04X} expected exactly one rect");
        r[0].0
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> SpriteRect {
        SpriteRect { x, y, w, h }
    }

    fn touches(a: SpriteRect, b: SpriteRect) -> bool {
        a.x <= b.x + b.w && b.x <= a.x + a.w && a.y <= b.y + b.h && b.y <= a.y + a.h
    }

    /// A stroke tessellates to a plausible number of stamps that form
    /// one unbroken chain — the seam property every stroked glyph owes
    /// its consumers.
    fn assert_stamp_chain(stamps: &[SpriteRect], what: &str) {
        assert!(
            (4..=256).contains(&stamps.len()),
            "{what}: {} stamps out of bounds",
            stamps.len()
        );
        for pair in stamps.windows(2) {
            assert!(
                touches(pair[0], pair[1]),
                "{what}: stamps {:?} and {:?} leave a gap",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn dispatch_rejects_non_sprite_codepoints() {
        for cp in [0x41u32, 0x20, 0x30, 0x24FF, 0x25A0, 0x2700] {
            assert!(
                sprite_geometry(cp, 8.0, 16.0).is_none(),
                "U+{cp:04X} should not be handled by the sprite geometry"
            );
        }
    }

    #[test]
    fn dispatch_covers_both_ranges() {
        for cp in [0x2500u32, 0x2580, 0x2588, 0x256D, 0x2571, 0x257F, 0x259F] {
            assert!(
                sprite_geometry(cp, 12.0, 24.0).is_some(),
                "U+{cp:04X} should be handled"
            );
        }
    }

    #[test]
    fn full_range_emits_primitives() {
        for (w, h) in SIZES {
            for cp in 0x2500u32..=0x259F {
                let g = sprite_geometry(cp, w, h)
                    .unwrap_or_else(|| panic!("U+{cp:04X} at {w}x{h} not handled"));
                assert!(
                    !g.primitives.is_empty(),
                    "U+{cp:04X} at {w}x{h} emitted no primitives"
                );
                assert_eq!(
                    g.antialias,
                    cp < 0x2580,
                    "U+{cp:04X} antialias flag follows the layer"
                );
            }
        }
    }

    #[test]
    fn block_halves_and_quadrants() {
        // ▀ upper half, ▄ lower half at 10x20.
        assert_eq!(only_rect(0x2580, 10.0, 20.0), rect(0.0, 0.0, 10.0, 10.0));
        assert_eq!(only_rect(0x2584, 10.0, 20.0), rect(0.0, 10.0, 10.0, 10.0));
        // ▌ left half, ▐ right half.
        assert_eq!(only_rect(0x258C, 10.0, 20.0), rect(0.0, 0.0, 5.0, 20.0));
        assert_eq!(only_rect(0x2590, 10.0, 20.0), rect(5.0, 0.0, 5.0, 20.0));
        // █ full cell.
        assert_eq!(only_rect(0x2588, 8.0, 16.0), rect(0.0, 0.0, 8.0, 16.0));
        // ▘ top-left quadrant.
        assert_eq!(only_rect(0x2598, 10.0, 20.0), rect(0.0, 0.0, 5.0, 10.0));
        // ▞ top-right + bottom-left quadrants.
        let quads = rects(0x259E, 10.0, 20.0);
        assert_eq!(quads.len(), 2);
        assert_eq!(quads[0].0, rect(5.0, 0.0, 5.0, 10.0));
        assert_eq!(quads[1].0, rect(0.0, 10.0, 5.0, 10.0));
    }

    #[test]
    fn odd_cell_quadrants_tile_exactly() {
        // ▟ (bottom-left + bottom-right + top-right) on an odd cell:
        // the right/bottom rects take the remainder so the halves abut.
        let q = rects(0x259F, 9.0, 19.0);
        assert_eq!(q.len(), 3);
        assert_eq!(q[0].0, rect(5.0, 0.0, 4.0, 10.0));
        assert_eq!(q[1].0, rect(0.0, 10.0, 5.0, 9.0));
        assert_eq!(q[2].0, rect(5.0, 10.0, 4.0, 9.0));
    }

    #[test]
    fn eighth_blocks_round_to_pixels() {
        // ▁ lower 1/8 of a 20px cell = 3px (round(2.5)), bottom-aligned.
        assert_eq!(only_rect(0x2581, 10.0, 20.0), rect(0.0, 17.0, 10.0, 3.0));
        // ▔ upper 1/8, top-aligned.
        assert_eq!(only_rect(0x2594, 10.0, 20.0), rect(0.0, 0.0, 10.0, 3.0));
        // ▏ left 1/8 of a 10px-wide cell = 1px.
        assert_eq!(only_rect(0x258F, 10.0, 20.0), rect(0.0, 0.0, 1.0, 20.0));
        // ▕ right 1/8.
        assert_eq!(only_rect(0x2595, 10.0, 20.0), rect(9.0, 0.0, 1.0, 20.0));
    }

    #[test]
    fn shades_carry_alpha_over_the_full_cell() {
        for (cp, alpha) in [(0x2591u32, 0.25), (0x2592, 0.5), (0x2593, 0.75)] {
            let r = rects(cp, 8.0, 16.0);
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].0, rect(0.0, 0.0, 8.0, 16.0));
            assert_eq!(r[0].1, alpha, "U+{cp:04X} alpha");
        }
    }

    #[test]
    fn opaque_glyphs_have_alpha_one() {
        for (w, h) in SIZES {
            for cp in 0x2500u32..=0x259F {
                if (0x2591..=0x2593).contains(&cp) {
                    continue;
                }
                for (_, alpha) in rects(cp, w, h) {
                    assert_eq!(alpha, 1.0, "U+{cp:04X} should be opaque");
                }
            }
        }
    }

    #[test]
    fn light_lines_land_on_the_stroke_band() {
        // 12x24 cell: box_thickness = round(24/14) = 2, so the light
        // band is y 11..13 / x 5..7.
        let horizontal = rects(0x2500, 12.0, 24.0);
        assert_eq!(horizontal.len(), 2);
        assert_eq!(horizontal[0].0, rect(5.0, 11.0, 7.0, 2.0)); // right leg
        assert_eq!(horizontal[1].0, rect(0.0, 11.0, 7.0, 2.0)); // left leg

        let vertical = rects(0x2502, 12.0, 24.0);
        assert_eq!(vertical.len(), 2);
        assert_eq!(vertical[0].0, rect(5.0, 0.0, 2.0, 13.0)); // up leg
        assert_eq!(vertical[1].0, rect(5.0, 11.0, 2.0, 13.0)); // down leg
    }

    #[test]
    fn heavy_line_is_twice_the_light_thickness() {
        let light = only_rect(0x2576, 12.0, 24.0); // ╶
        let heavy = only_rect(0x257A, 12.0, 24.0); // ╺
        assert_eq!(light.h, 2.0);
        assert_eq!(heavy.h, 4.0);
        assert_eq!(light.y, 11.0);
        assert_eq!(heavy.y, 10.0);
    }

    #[test]
    fn double_horizontal_has_two_runs() {
        // ═ at 16x32: thickness 2, light band y 15..17, double bands
        // y 13..15 and y 17..19.
        let r = rects(0x2550, 16.0, 32.0);
        assert_eq!(r.len(), 4);
        let bands: Vec<(f64, f64)> = r.iter().map(|(rc, _)| (rc.y, rc.h)).collect();
        assert_eq!(
            bands,
            vec![(13.0, 2.0), (17.0, 2.0), (13.0, 2.0), (17.0, 2.0)]
        );
        assert_eq!(r[0].0, rect(7.0, 13.0, 9.0, 2.0)); // right leg, upper run
        assert_eq!(r[2].0, rect(0.0, 13.0, 9.0, 2.0)); // left leg, upper run
    }

    #[test]
    fn dashes_split_into_segments() {
        // ┄ three light dashes across a 30px-wide cell.
        let r = rects(0x2504, 30.0, 16.0);
        assert_eq!(r.len(), 3);
        for (rc, _) in &r {
            assert!(rc.w > 0.0 && rc.h > 0.0);
        }
        assert!(r[0].0.x + r[0].0.w < r[1].0.x, "dashes must not touch");
        assert!(r[1].0.x + r[1].0.w < r[2].0.x, "dashes must not touch");
        // ┆ three vertical dashes, ┊ four.
        assert_eq!(rects(0x2506, 12.0, 24.0).len(), 3);
        assert_eq!(rects(0x250A, 12.0, 24.0).len(), 4);
    }

    #[test]
    fn narrow_dash_falls_back_to_solid_lines() {
        // Cell too narrow for `count` dashes: the renderer draws the
        // plain line instead.
        let r = rects(0x2508, 5.0, 16.0);
        assert_eq!(r.len(), 2, "expected the two-leg solid-line fallback");
        assert_eq!(r[0].0.x + r[0].0.w, 5.0, "right leg reaches the cell edge");
        assert_eq!(r[1].0.x, 0.0, "left leg starts at the cell edge");
    }

    #[test]
    fn rounded_corners_map_to_arcs() {
        for (cp, corner) in [
            (0x256Du32, Corner::BR),
            (0x256E, Corner::BL),
            (0x256F, Corner::TL),
            (0x2570, Corner::TR),
        ] {
            let g = sprite_geometry(cp, 16.0, 32.0).expect("geometry");
            assert_eq!(g.primitives.len(), 1);
            match g.primitives[0] {
                SpritePrimitive::CornerArc {
                    corner: c,
                    w,
                    h,
                    thickness,
                } => {
                    assert_eq!(c, corner, "U+{cp:04X} corner");
                    assert_eq!((w, h), (16.0, 32.0));
                    assert_eq!(thickness, 2.0);
                }
                other => panic!("U+{cp:04X} expected CornerArc, got {other:?}"),
            }
        }
    }

    #[test]
    fn arc_path_starts_and_ends_on_cell_edges() {
        let p = arc_path(Corner::BR, 16.0, 32.0);
        // thickness 2 => cx = floor(14/2)+1 = 8, cy = floor(30/2)+1 = 16,
        // r = min(16,32)/2 = 8.
        assert_eq!(p.thickness, 2.0);
        assert_eq!(p.start, SpritePoint { x: 8.0, y: 32.0 });
        assert_eq!(p.leg_end, SpritePoint { x: 8.0, y: 24.0 });
        assert_eq!(p.c1, SpritePoint { x: 8.0, y: 18.0 });
        assert_eq!(p.c2, SpritePoint { x: 10.0, y: 16.0 });
        assert_eq!(p.curve_end, SpritePoint { x: 16.0, y: 16.0 });
        assert_eq!(p.end, SpritePoint { x: 16.0, y: 16.0 });

        let tl = arc_path(Corner::TL, 16.0, 32.0);
        assert_eq!(tl.start, SpritePoint { x: 8.0, y: 0.0 });
        assert_eq!(tl.end, SpritePoint { x: 0.0, y: 16.0 });
    }

    #[test]
    fn diagonals_overshoot_the_cell() {
        let g = sprite_geometry(0x2573, 16.0, 32.0).expect("geometry"); // ╳
        assert_eq!(g.primitives.len(), 2);
        let slope_x = 16.0f64 / 32.0;
        let slope_y = 1.0f64; // (32/16).min(1.0)
        match g.primitives[0] {
            SpritePrimitive::Diagonal {
                x0,
                y0,
                x1,
                y1,
                thickness,
            } => {
                assert_eq!(x0, 16.0 + 0.5 * slope_x);
                assert_eq!(y0, -0.5 * slope_y);
                assert_eq!(x1, -0.5 * slope_x);
                assert_eq!(y1, 32.0 + 0.5 * slope_y);
                assert_eq!(thickness, 2.0);
            }
            other => panic!("expected Diagonal, got {other:?}"),
        }
        match g.primitives[1] {
            SpritePrimitive::Diagonal { x0, y0, x1, y1, .. } => {
                assert_eq!((x0, y0), (-0.5 * slope_x, -0.5 * slope_y));
                assert_eq!((x1, y1), (16.0 + 0.5 * slope_x, 32.0 + 0.5 * slope_y));
            }
            other => panic!("expected Diagonal, got {other:?}"),
        }
    }

    #[test]
    fn tessellate_passes_rects_through() {
        let prim = SpritePrimitive::Rect {
            rect: rect(1.0, 2.0, 3.0, 4.0),
            alpha: 0.5,
        };
        assert_eq!(tessellate(&prim), vec![rect(1.0, 2.0, 3.0, 4.0)]);
    }

    #[test]
    fn arc_stamps_are_contiguous_and_inside_the_cell() {
        for (w, h) in SIZES {
            for cp in 0x256Du32..=0x2570 {
                let g = sprite_geometry(cp, w, h).expect("geometry");
                let s = tessellate(&g.primitives[0]);
                assert_stamp_chain(&s, &format!("U+{cp:04X} at {w}x{h}"));
                for r in &s {
                    assert!(
                        r.x >= 0.0 && r.y >= 0.0 && r.x + r.w <= w && r.y + r.h <= h,
                        "U+{cp:04X} at {w}x{h}: stamp {r:?} escapes the cell"
                    );
                    assert!(r.w > 0.0 && r.h > 0.0, "empty stamp {r:?}");
                }
            }
        }
    }

    #[test]
    fn diagonal_stamps_stay_within_the_overshoot_envelope() {
        for (w, h) in SIZES {
            let g = sprite_geometry(0x2573, w, h).expect("geometry");
            let t = (h / 14.0).round().max(1.0);
            let slope_x = (w / h).min(1.0);
            let slope_y = (h / w).min(1.0);
            for prim in &g.primitives {
                let s = tessellate(prim);
                assert_stamp_chain(&s, &format!("diagonal at {w}x{h}"));
                for r in &s {
                    assert!(
                        r.x >= -(0.5 * slope_x + t / 2.0) - f64::EPSILON
                            && r.y >= -(0.5 * slope_y + t / 2.0) - f64::EPSILON
                            && r.x + r.w <= w + 0.5 * slope_x + t / 2.0 + f64::EPSILON
                            && r.y + r.h <= h + 0.5 * slope_y + t / 2.0 + f64::EPSILON,
                        "diagonal at {w}x{h}: stamp {r:?} exceeds the overshoot envelope"
                    );
                    assert!(
                        (r.w - t).abs() < 1e-9 && (r.h - t).abs() < 1e-9,
                        "diagonal stamps are thickness-square, got {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn tessellated_stamps_step_by_at_most_half_thickness() {
        for (w, h) in SIZES {
            for cp in 0x256Du32..=0x2573 {
                let g = sprite_geometry(cp, w, h).expect("geometry");
                for prim in &g.primitives {
                    // Samples are spaced at most thickness/2 apart. Arc
                    // stamps are additionally clipped to the cell, which
                    // shifts a clipped stamp's center by up to another
                    // half thickness.
                    let (t, budget) = match *prim {
                        SpritePrimitive::CornerArc { thickness, .. } => (thickness, thickness),
                        SpritePrimitive::Diagonal { thickness, .. } => (thickness, thickness / 2.0),
                        SpritePrimitive::Rect { .. } => unreachable!(),
                    };
                    let s = tessellate(prim);
                    for pair in s.windows(2) {
                        let dx = (pair[1].x + pair[1].w / 2.0) - (pair[0].x + pair[0].w / 2.0);
                        let dy = (pair[1].y + pair[1].h / 2.0) - (pair[0].y + pair[0].h / 2.0);
                        assert!(
                            dx.hypot(dy) <= budget + 1e-9,
                            "U+{cp:04X} at {w}x{h}: stamp step {} exceeds {budget} (t={t})",
                            dx.hypot(dy)
                        );
                    }
                }
            }
        }
    }
}
