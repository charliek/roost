//! What a renderer reads to draw Kitty images: `RenderState::image_placements`
//! (where, in what order) and `Terminal::kitty_image` (the pixels), with
//! Roost's PNG decoder installed.
//!
//! Gated on `ffi`; run with: `cargo test -p roost-vt --features ffi`.
#![cfg(feature = "ffi")]

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use roost_vt::{
    HistoryStep, ImageFormat, ImageLayer, ImagePlacement, KittyImage, PixelRect, PlacementKind,
    ReadyState, RenderState, SnapshotDecodeOptions, SnapshotDecoder, Terminal, TerminalOptions,
};

const COLS: u16 = 20;
const ROWS: u16 = 10;
const CELL_W: u32 = 8;
const CELL_H: u32 = 16;

fn bare_terminal() -> Terminal {
    roost_vt::install_image_decoder().expect("install the decoder");
    Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: 100,
        ..Default::default()
    })
    .expect("terminal")
}

fn terminal() -> Terminal {
    let mut terminal = bare_terminal();
    terminal
        .resize(COLS, ROWS, CELL_W, CELL_H)
        .expect("cell px");
    terminal
}

fn listed(terminal: &Terminal, render: &mut RenderState) -> Vec<ImagePlacement> {
    render.update(terminal).expect("update");
    render.image_placements().expect("image placements")
}

/// Transmit and place image `id` (`a=T`) from raw pixels in Kitty format
/// `f` (24 = RGB, 32 = RGBA, 100 = PNG), with `keys` added to the command.
fn send(
    terminal: &mut Terminal,
    id: u32,
    f: u32,
    size: Option<(u32, u32)>,
    keys: &str,
    data: &[u8],
) {
    let size = size.map_or(String::new(), |(w, h)| format!(",s={w},v={h}"));
    let command = format!(
        "\x1b_Ga=T,t=d,f={f}{size},i={id},q=2{keys};{}\x1b\\",
        STANDARD.encode(data)
    );
    terminal.vt_write(command.as_bytes());
}

fn place_2x2(terminal: &mut Terminal, id: u32, keys: &str) {
    send(terminal, id, 32, Some((2, 2)), keys, &rgba_2x2());
}

fn send_png(terminal: &mut Terminal, id: u32, png: &[u8]) {
    send(terminal, id, 100, None, "", png);
}

fn stored(terminal: &Terminal, id: u32) -> KittyImage<'_> {
    terminal.kitty_image(id).expect("read").expect("stored")
}

fn rgba_2x2() -> Vec<u8> {
    vec![
        255, 0, 0, 255, 0, 255, 0, 128, //
        0, 0, 255, 0, 10, 20, 30, 40,
    ]
}

fn png(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(color);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(pixels).expect("pixels");
    writer.finish().expect("finish");
    bytes
}

fn ids(placements: &[ImagePlacement]) -> Vec<u32> {
    placements.iter().map(|p| p.image_id).collect()
}

#[test]
fn an_rgba_placement_is_listed_where_and_how_big_it_was_placed() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    terminal.vt_write(b"\x1b[3;5H");
    place_2x2(&mut terminal, 7, ",c=2,r=1");

    let placements = listed(&terminal, &mut render);
    assert_eq!(placements.len(), 1, "{placements:?}");
    let p = placements[0];
    let image = stored(&terminal, 7);
    assert_eq!(
        p,
        ImagePlacement {
            image_id: 7,
            generation: image.generation(),
            layer: ImageLayer::AboveText,
            z: 0,
            col: 4,
            row: 2,
            grid_cols: 2,
            grid_rows: 1,
            offset_x: 0,
            offset_y: 0,
            pixel_width: 2 * CELL_W,
            pixel_height: CELL_H,
            source: PixelRect {
                x: 0,
                y: 0,
                width: 2,
                height: 2
            },
            placement_id: 0,
            kind: PlacementKind::Fixed,
        }
    );
    assert_ne!(image.generation(), 0);
    assert_eq!((image.id(), image.width(), image.height()), (7, 2, 2));
    assert_eq!(image.format(), ImageFormat::Rgba);
    assert_eq!(image.data(), rgba_2x2().as_slice());
    assert_eq!(image.to_rgba(), rgba_2x2());
}

#[test]
fn rgb_and_png_images_read_back_as_exact_rgba() {
    let mut terminal = terminal();
    send(
        &mut terminal,
        1,
        24,
        Some((2, 1)),
        "",
        &[1, 2, 3, 250, 251, 252],
    );
    let rgb = stored(&terminal, 1);
    assert_eq!(rgb.format(), ImageFormat::Rgb);
    assert_eq!(rgb.data(), [1, 2, 3, 250, 251, 252]);
    assert_eq!(rgb.to_rgba(), [1, 2, 3, 255, 250, 251, 252, 255]);

    send_png(
        &mut terminal,
        2,
        &png(2, 1, png::ColorType::Rgba, &[9, 8, 7, 6, 5, 4, 3, 2]),
    );
    let decoded = stored(&terminal, 2);
    assert_eq!((decoded.width(), decoded.height()), (2, 1));
    assert_eq!(decoded.format(), ImageFormat::Rgba);
    assert_eq!(decoded.to_rgba(), [9, 8, 7, 6, 5, 4, 3, 2]);

    send_png(
        &mut terminal,
        3,
        &png(2, 1, png::ColorType::Grayscale, &[0x10, 0xf0]),
    );
    let gray = stored(&terminal, 3);
    assert_eq!(
        gray.to_rgba(),
        [0x10, 0x10, 0x10, 0xff, 0xf0, 0xf0, 0xf0, 0xff]
    );

    send_png(
        &mut terminal,
        4,
        &png(1, 1, png::ColorType::GrayscaleAlpha, &[0x40, 0x80]),
    );
    let gray_alpha = stored(&terminal, 4);
    assert_eq!(gray_alpha.to_rgba(), [0x40, 0x40, 0x40, 0x80]);
}

#[test]
fn an_absent_image_is_none() {
    let terminal = terminal();
    assert!(terminal.kitty_image(99).expect("read").is_none());
}

#[test]
fn z_picks_the_layer_and_the_list_is_in_draw_order() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    // Sent in reverse draw order, with ids reversed too, so neither
    // iteration nor id order alone sorts them.
    for (id, z) in [(1, 5), (2, -1), (3, -1_073_741_824), (4, -1_073_741_825)] {
        terminal.vt_write(format!("\x1b[{};1H", id).as_bytes());
        place_2x2(&mut terminal, id, &format!(",z={z},C=1"));
    }

    let placements = listed(&terminal, &mut render);
    let order: Vec<(u32, ImageLayer, i32)> = placements
        .iter()
        .map(|p| (p.image_id, p.layer, p.z))
        .collect();
    assert_eq!(
        order,
        [
            (4, ImageLayer::BelowBackground, -1_073_741_825),
            (3, ImageLayer::BelowText, -1_073_741_824),
            (2, ImageLayer::BelowText, -1),
            (1, ImageLayer::AboveText, 5),
        ]
    );
}

#[test]
fn a_source_crop_is_listed_as_the_source_rect() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    let pixels: Vec<u8> = (0..4 * 3 * 4).map(|i| i as u8).collect();
    send(
        &mut terminal,
        5,
        32,
        Some((4, 3)),
        ",x=1,y=2,w=2,h=1,c=1,r=1",
        &pixels,
    );

    let placements = listed(&terminal, &mut render);
    assert_eq!(placements.len(), 1, "{placements:?}");
    assert_eq!(
        placements[0].source,
        PixelRect {
            x: 1,
            y: 2,
            width: 2,
            height: 1
        }
    );
    assert_eq!(
        (placements[0].pixel_width, placements[0].pixel_height),
        (CELL_W, CELL_H)
    );
}

#[test]
fn a_placement_scrolled_partly_off_has_a_negative_row_and_fully_off_is_gone() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    place_2x2(&mut terminal, 6, ",c=1,r=3,C=1");
    assert_eq!(listed(&terminal, &mut render)[0].row, 0);

    terminal.vt_write(format!("\x1b[{ROWS};1H\n").as_bytes());
    let placements = listed(&terminal, &mut render);
    assert_eq!(placements.len(), 1, "{placements:?}");
    assert_eq!((placements[0].row, placements[0].grid_rows), (-1, 3));

    terminal.vt_write(b"\n\n");
    assert_eq!(listed(&terminal, &mut render), []);
}

#[test]
fn nothing_is_listed_until_the_cell_size_is_known() {
    let mut terminal = bare_terminal();
    let mut render = RenderState::new().expect("render state");
    place_2x2(&mut terminal, 8, ",c=2,r=1");
    assert_eq!(listed(&terminal, &mut render), []);

    terminal
        .resize(COLS, ROWS, CELL_W, CELL_H)
        .expect("cell px");
    assert_eq!(ids(&listed(&terminal, &mut render)), [8]);
}

#[test]
fn deleting_placements_keeps_the_image_and_freeing_it_drops_it() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    listed(&terminal, &mut render);
    assert_eq!(render.image_placements_generation().expect("read"), None);

    place_2x2(&mut terminal, 9, ",c=1,r=1");
    assert_eq!(ids(&listed(&terminal, &mut render)), [9]);
    let placed = render.image_placements_generation().expect("read");
    assert!(placed.is_some());

    terminal.vt_write(b"\x1b_Ga=d,d=i,i=9,q=2\x1b\\");
    assert_eq!(listed(&terminal, &mut render), []);
    let deleted = render.image_placements_generation().expect("read");
    assert_ne!(deleted, placed);
    assert!(terminal.kitty_image(9).expect("read").is_some());

    terminal.vt_write(b"\x1b_Ga=p,i=9,c=1,r=1,q=2\x1b\\");
    assert_eq!(ids(&listed(&terminal, &mut render)), [9]);
    let replaced = render.image_placements_generation().expect("read");
    assert_ne!(replaced, deleted);

    terminal.vt_write(b"\x1b_Ga=d,d=I,i=9,q=2\x1b\\");
    assert_eq!(listed(&terminal, &mut render), []);
    assert_ne!(
        render.image_placements_generation().expect("read"),
        replaced
    );
    assert!(terminal.kitty_image(9).expect("read").is_none());
}

#[test]
fn retransmitting_an_id_gives_a_new_generation() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    place_2x2(&mut terminal, 10, ",c=1,r=1");
    let first = listed(&terminal, &mut render)[0].generation;
    assert_eq!(stored(&terminal, 10).generation(), first);

    let replacement = [7u8; 16];
    send(
        &mut terminal,
        10,
        32,
        Some((2, 2)),
        ",c=1,r=1",
        &replacement,
    );
    let second = listed(&terminal, &mut render)[0].generation;
    let image = stored(&terminal, 10);
    assert_ne!(second, first);
    assert_eq!(image.generation(), second);
    assert_eq!(image.data(), replacement);
}

#[test]
fn a_virtual_placement_lists_one_entry_per_placeholder_row() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    place_2x2(&mut terminal, 3, ",U=1,c=2,r=2");
    // U+10EEEE with row and column diacritics (U+0305 = 0, U+030D = 1),
    // the image id in the foreground color.
    let placeholder = |row: &str, col: &str| format!("\u{10EEEE}{row}{col}");
    let (zero, one) = ("\u{0305}", "\u{030D}");
    let cells = format!(
        "\x1b[2;3H\x1b[38;5;3m{}{}\x1b[3;3H{}{}\x1b[39m",
        placeholder(zero, zero),
        placeholder(zero, one),
        placeholder(one, zero),
        placeholder(one, one),
    );
    terminal.vt_write(cells.as_bytes());

    let placements = listed(&terminal, &mut render);
    let rows: Vec<(u32, PlacementKind, ImageLayer, i32, i32, i32, u32)> = placements
        .iter()
        .map(|p| (p.image_id, p.kind, p.layer, p.z, p.row, p.col, p.grid_rows))
        .collect();
    assert_eq!(
        rows,
        [
            (
                3,
                PlacementKind::Virtual,
                ImageLayer::BelowText,
                -1,
                1,
                2,
                1
            ),
            (
                3,
                PlacementKind::Virtual,
                ImageLayer::BelowText,
                -1,
                2,
                2,
                1
            ),
        ]
    );
}

#[test]
fn a_view_held_across_a_render_update_keeps_its_bytes() {
    let mut terminal = terminal();
    let mut render = RenderState::new().expect("render state");
    place_2x2(&mut terminal, 11, ",c=1,r=1");

    let image = stored(&terminal, 11);
    let before = image.data().to_vec();
    render.update(&terminal).expect("update");
    assert_eq!(render.image_placements().expect("list").len(), 1);
    render.update(&terminal).expect("update");
    assert_eq!(image.data(), before.as_slice());
    assert_eq!(image.data(), rgba_2x2().as_slice());
}

/// A hydrating terminal holds a snapshot's images before their pixels
/// arrive: such an image is pending, so it has nothing to lend.
#[test]
fn an_image_whose_pixels_are_still_on_the_way_is_none() {
    let mut source = terminal();
    place_2x2(&mut source, 12, ",c=1,r=1");
    let bytes = source.snapshot().expect("snapshot");

    let mut decoder = SnapshotDecoder::new(SnapshotDecodeOptions::default());
    decoder.feed(&bytes).expect("feed");
    assert_eq!(decoder.try_ready().expect("try_ready"), ReadyState::Ready);
    let hydrating = decoder.terminal().expect("a terminal at READY");
    assert!(hydrating.kitty_image(12).expect("read").is_none());

    loop {
        match decoder.try_next().expect("try_next") {
            HistoryStep::Finished => break,
            HistoryStep::NeedMoreBytes => panic!("the snapshot is all buffered"),
            HistoryStep::Page { .. } | HistoryStep::Image { .. } => {}
        }
    }
    let decoded = decoder.finish().expect("finish").terminal;
    let image = stored(&decoded, 12);
    assert_eq!(image.data(), rgba_2x2().as_slice());
}
