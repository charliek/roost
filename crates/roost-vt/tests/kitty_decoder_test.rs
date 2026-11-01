//! Kitty graphics with Roost's PNG decoder installed: what every terminal
//! Roost creates answers, and the image policy it holds. Each test goes
//! through [`installed`] before it creates a terminal. The decoder is a
//! process-global setting, so its control, `kitty_no_decoder_test.rs`,
//! is a binary of its own.
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use roost_vt::{
    KittyImagePolicy, ReadyState, SnapshotDecodeOptions, SnapshotDecoder, Terminal,
    TerminalOptions, KITTY_IMAGE_STORAGE_LIMIT,
};

/// Libghostty's own per-screen default at this pin (`Terminal.zig:311-318`).
const LIBGHOSTTY_DEFAULT_LIMIT: u64 = 10 * 1000 * 1000;

fn installed() {
    roost_vt::install_image_decoder().expect("install the decoder");
}

fn terminal(options: TerminalOptions) -> Terminal {
    installed();
    Terminal::new(options).expect("terminal")
}

fn answer(terminal: &mut Terminal, query: &str) -> String {
    let replies = Arc::new(Mutex::new(Vec::new()));
    terminal
        .set_write_pty_buffer(Arc::clone(&replies))
        .expect("reply buffer");
    terminal.vt_write(query.as_bytes());
    let bytes = std::mem::take(&mut *replies.lock().expect("lock"));
    String::from_utf8(bytes).expect("utf-8 reply")
}

fn png(
    width: u32,
    height: u32,
    color: png::ColorType,
    depth: png::BitDepth,
    palette: Option<(&[u8], &[u8])>,
    pixels: &[u8],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, width, height);
    encoder.set_color(color);
    encoder.set_depth(depth);
    if let Some((rgb, alpha)) = palette {
        encoder.set_palette(rgb.to_vec());
        encoder.set_trns(alpha.to_vec());
    }
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(pixels).expect("pixels");
    writer.finish().expect("finish");
    bytes
}

fn png_query(id: u32, png: &[u8]) -> String {
    format!("\x1b_Gi={id},a=q,t=d,f=100;{}\x1b\\", STANDARD.encode(png))
}

fn roost_policy() -> KittyImagePolicy {
    KittyImagePolicy {
        storage_limit: KITTY_IMAGE_STORAGE_LIMIT,
        medium_file: false,
        medium_temp_file: None,
        medium_shared_mem: false,
    }
}

/// A terminal written with a limit unlike Roost's, so a decoded copy
/// that reads back Roost's got it from the decode, not the snapshot.
fn snapshot_source() -> Vec<u8> {
    let mut source = terminal(TerminalOptions {
        kitty_image_storage_limit: 1_000_000,
        ..Default::default()
    });
    source.vt_write(b"a line\r\n");
    source.snapshot().expect("snapshot")
}

#[test]
fn a_png_query_is_answered_ok() {
    let mut terminal = terminal(TerminalOptions::default());
    let one_pixel = png(
        1,
        1,
        png::ColorType::Rgb,
        png::BitDepth::Eight,
        None,
        &[255, 0, 0],
    );
    assert_eq!(
        answer(&mut terminal, &png_query(32, &one_pixel)),
        "\x1b_Gi=32;OK\x1b\\"
    );
}

#[test]
fn every_png_kind_the_decoder_normalizes_is_accepted() {
    let mut terminal = terminal(TerminalOptions::default());
    let fixtures = [
        (
            "gray",
            png(
                2,
                1,
                png::ColorType::Grayscale,
                png::BitDepth::Eight,
                None,
                &[0, 255],
            ),
        ),
        (
            "1-bit gray",
            png(
                2,
                1,
                png::ColorType::Grayscale,
                png::BitDepth::One,
                None,
                &[0x40],
            ),
        ),
        (
            "gray+alpha",
            png(
                1,
                1,
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                None,
                &[9, 128],
            ),
        ),
        (
            "palette+tRNS",
            png(
                2,
                1,
                png::ColorType::Indexed,
                png::BitDepth::Eight,
                Some((&[255, 0, 0, 0, 0, 255], &[64])),
                &[0, 1],
            ),
        ),
        (
            "16-bit RGBA",
            png(
                1,
                1,
                png::ColorType::Rgba,
                png::BitDepth::Sixteen,
                None,
                &[1, 2, 3, 4, 5, 6, 7, 8],
            ),
        ),
    ];
    for (id, (kind, fixture)) in (40..).zip(fixtures) {
        assert_eq!(
            answer(&mut terminal, &png_query(id, &fixture)),
            format!("\x1b_Gi={id};OK\x1b\\"),
            "{kind}"
        );
    }
}

#[test]
fn a_new_terminal_holds_the_policy() {
    let terminal = terminal(TerminalOptions::default());
    assert_eq!(
        terminal.kitty_image_policy().expect("read back"),
        roost_policy()
    );
}

#[test]
fn a_hydrating_terminal_holds_the_policy_from_ready() {
    let bytes = snapshot_source();
    let mut decoder = SnapshotDecoder::new(SnapshotDecodeOptions::default());
    decoder.feed(&bytes).expect("feed");
    assert_eq!(decoder.try_ready().expect("try_ready"), ReadyState::Ready);
    let terminal = decoder.terminal().expect("a terminal at READY");
    assert_eq!(
        terminal.kitty_image_policy().expect("read back"),
        roost_policy()
    );
}

#[test]
fn a_decoded_terminal_holds_the_policy() {
    let decoded =
        SnapshotDecoder::decode_bytes(snapshot_source(), SnapshotDecodeOptions::default())
            .expect("decode");
    assert_eq!(
        decoded.terminal.kitty_image_policy().expect("read back"),
        roost_policy()
    );
}

/// The decode's own limit is the one images are restored under: three
/// images over libghostty's default all survive the restore.
#[test]
fn a_decode_restores_every_image_the_session_holds() {
    let (width, height) = (1100, 1000);
    let solid = vec![0x80; width * height * 4];
    let image = STANDARD.encode(png(
        u32::try_from(width).unwrap(),
        u32::try_from(height).unwrap(),
        png::ColorType::Rgba,
        png::BitDepth::Eight,
        None,
        &solid,
    ));
    let mut session = terminal(TerminalOptions::default());
    for id in 1..=3 {
        session.vt_write(format!("\x1b_Ga=t,t=d,f=100,i={id},q=2;{image}\x1b\\").as_bytes());
    }
    let held = session.memory_usage().expect("memory").primary.image_bytes;
    assert!(
        held > LIBGHOSTTY_DEFAULT_LIMIT,
        "the session holds all three: {held}"
    );

    let decoded = SnapshotDecoder::decode_bytes(
        session.snapshot().expect("snapshot"),
        SnapshotDecodeOptions::default(),
    )
    .expect("decode");
    assert_eq!(
        decoded
            .terminal
            .memory_usage()
            .expect("memory")
            .primary
            .image_bytes,
        held
    );
}
