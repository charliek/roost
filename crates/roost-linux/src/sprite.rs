//! Cairo adapter for the geometric sprite renderer covering Unicode
//! box-drawing (U+2500–U+257F) and block-element (U+2580–U+259F)
//! glyphs.
//!
//! Pango font glyphs for these ranges don't tile pixel-perfectly
//! across adjacent cells — you get visible hairline seams in TUI
//! chrome (most obvious in the opencode wordmark logo). The pixel math
//! lives in [`roost_ui_model::sprite`] so both UIs draw the identical
//! geometry; this module only turns its primitives into cairo calls.
//!
//! Public entry point: [`draw_cell_sprite`] — returns `true` when
//! the codepoint is handled (caller skips the font glyph), `false`
//! otherwise (caller falls back to Pango).

use gtk4::cairo;
use roost_ui_model::sprite::{arc_path, sprite_geometry, SpriteGeometry, SpritePrimitive};
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
    let Some(geometry) = sprite_geometry(cp, w, h) else {
        return false;
    };
    if geometry.antialias {
        draw_primitives(cr, x, y, fg, &geometry);
    } else {
        // Cairo's default antialiasing softens edges by a fraction of
        // a pixel even on integer-aligned coordinates under some
        // surface transforms, which reopens the seam between adjacent
        // block cells. Curves and diagonals keep the default AA so
        // they don't go jaggy.
        cr.save().ok();
        cr.set_antialias(cairo::Antialias::None);
        draw_primitives(cr, x, y, fg, &geometry);
        cr.restore().ok();
    }
    true
}

fn draw_primitives(cr: &cairo::Context, x: f64, y: f64, fg: ColorRgb, geometry: &SpriteGeometry) {
    let (r, g, b) = fg.to_f64();
    for prim in &geometry.primitives {
        match *prim {
            SpritePrimitive::Rect { rect, alpha } => {
                if alpha < 1.0 {
                    cr.set_source_rgba(r, g, b, alpha);
                } else {
                    cr.set_source_rgb(r, g, b);
                }
                cr.rectangle(x + rect.x, y + rect.y, rect.w, rect.h);
                cr.fill().ok();
            }
            SpritePrimitive::CornerArc {
                corner,
                w,
                h,
                thickness,
            } => {
                let path = arc_path(corner, w, h);
                cr.set_source_rgb(r, g, b);
                cr.new_path();
                cr.move_to(x + path.start.x, y + path.start.y);
                cr.line_to(x + path.leg_end.x, y + path.leg_end.y);
                cr.curve_to(
                    x + path.c1.x,
                    y + path.c1.y,
                    x + path.c2.x,
                    y + path.c2.y,
                    x + path.curve_end.x,
                    y + path.curve_end.y,
                );
                cr.line_to(x + path.end.x, y + path.end.y);
                cr.set_line_cap(cairo::LineCap::Butt);
                cr.set_line_width(thickness);
                cr.stroke().ok();
            }
            SpritePrimitive::Diagonal {
                x0,
                y0,
                x1,
                y1,
                thickness,
            } => {
                cr.set_source_rgb(r, g, b);
                cr.set_line_cap(cairo::LineCap::Butt);
                cr.set_line_width(thickness);
                cr.new_path();
                cr.move_to(x + x0, y + y0);
                cr.line_to(x + x1, y + y1);
                cr.stroke().ok();
            }
        }
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

    pub(super) fn render(cp: u32, w: i32, h: i32) -> (Vec<u8>, i32, bool) {
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

#[cfg(test)]
mod golden_tests {
    //! Full-surface fingerprint of the rasterizer-stable sprite
    //! codepoints at three cell sizes — the fast-fail bar for refactors
    //! of this module: any pixel that moves fails here, named by
    //! codepoint and size. The fixture is generated by
    //! [`regenerate_fixture`] (`#[ignore]`d) from the renderer as it
    //! stands.
    //!
    //! The rounded corners (U+256D–U+2570) and diagonals
    //! (U+2571–U+2573) are deliberately excluded: they are the only
    //! *stroked* glyphs, and cairo's antialiased stroke rasterization
    //! differs across cairo versions, so hashing them would fail CI on
    //! Ubuntu's cairo against a fixture generated on a dev machine's —
    //! a spurious failure with nothing to do with this module. Every
    //! other codepoint is either an AA-off block fill or an
    //! integer-aligned AA-on rect fill (full pixel coverage, so the
    //! rasterizer has no partial-coverage decisions to differ on).
    //! The seven excluded glyphs stay covered by the property-style
    //! pixel tests above (`rounded_corner_tl`, `diagonal_ur_to_ll`,
    //! `diagonal_cross`) and by the end-to-end shed screenshot oracle.
    use super::tests::render;

    const SIZES: [(i32, i32); 3] = [(8, 16), (9, 19), (12, 24)];
    /// Stroked glyphs — see the module doc for why they're not hashed.
    const STROKED: std::ops::RangeInclusive<u32> = 0x256D..=0x2573;
    const FIXTURE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/sprite_golden.txt"
    );

    fn fnv1a64(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for b in bytes {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        hash
    }

    fn surface_hash(cp: u32, w: i32, h: i32) -> u64 {
        let (data, stride, handled) = render(cp, w, h);
        assert!(handled, "U+{cp:04X} should be handled");
        // Hash only the w*4 pixel bytes of each row: the stride tail is
        // cairo's alignment padding (`cairo_format_stride_for_width`),
        // an implementation detail that could drift across cairo
        // versions with nothing wrong in the geometry — the same class
        // of false failure the stroked-glyph exclusion avoids.
        let row_bytes = (w * 4) as usize;
        let mut pixels = Vec::with_capacity(row_bytes * h as usize);
        for y in 0..h {
            let start = (y * stride) as usize;
            pixels.extend_from_slice(&data[start..start + row_bytes]);
        }
        fnv1a64(&pixels)
    }

    fn current() -> Vec<String> {
        let mut out = Vec::new();
        for cp in 0x2500u32..=0x259F {
            if STROKED.contains(&cp) {
                continue;
            }
            for (w, h) in SIZES {
                out.push(format!("{cp:04X} {w}x{h} {:016x}", surface_hash(cp, w, h)));
            }
        }
        out
    }

    #[test]
    fn renders_match_the_golden_fixture() {
        let fixture = std::fs::read_to_string(FIXTURE).expect("read sprite golden fixture");
        let expected: Vec<&str> = fixture
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        let actual = current();
        assert_eq!(
            expected.len(),
            actual.len(),
            "fixture entry count drifted; regenerate with --ignored regenerate_fixture"
        );
        let mut drifted = Vec::new();
        for (want, got) in expected.iter().zip(actual.iter()) {
            if want != got {
                drifted.push(format!("expected `{want}`, got `{got}`"));
            }
        }
        assert!(
            drifted.is_empty(),
            "{} sprite renders drifted from the golden fixture:\n{}",
            drifted.len(),
            drifted.join("\n")
        );
    }

    #[test]
    #[ignore = "writes the golden fixture; run deliberately"]
    fn regenerate_fixture() {
        let mut body = String::from(
            "# Generated by `cargo test -p roost-linux --bin roost golden_tests::regenerate_fixture -- --ignored`.\n\
             # One line per (codepoint, cell size): FNV-1a 64 of the ARGB32 surface bytes.\n\
             # U+256D-U+2573 (stroked arcs + diagonals) are excluded — see the module doc.\n",
        );
        for line in current() {
            body.push_str(&line);
            body.push('\n');
        }
        let path = std::path::Path::new(FIXTURE);
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        std::fs::write(path, body).expect("write sprite golden fixture");
    }
}
