//! Kitty graphics queries with no PNG decoder installed: the libghostty
//! facts Roost starts from (plan 087 §2.2), and the control for
//! `kitty_decoder_test.rs`. The decoder is a process-global setting, so
//! this binary must never install it; it is a binary of its own for that.
#![cfg(feature = "ffi")]

use std::sync::{Arc, Mutex};

use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use roost_vt::{Terminal, TerminalOptions};

fn answer(query: &str) -> String {
    let mut terminal = Terminal::new(TerminalOptions::default()).expect("terminal");
    let replies = Arc::new(Mutex::new(Vec::new()));
    terminal
        .set_write_pty_buffer(Arc::clone(&replies))
        .expect("reply buffer");
    terminal.vt_write(query.as_bytes());
    let bytes = std::mem::take(&mut *replies.lock().expect("lock"));
    String::from_utf8(bytes).expect("utf-8 reply")
}

fn one_pixel_png() -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut encoder = png::Encoder::new(&mut bytes, 1, 1);
    encoder.set_color(png::ColorType::Rgb);
    let mut writer = encoder.write_header().expect("header");
    writer.write_image_data(&[255, 0, 0]).expect("pixels");
    writer.finish().expect("finish");
    bytes
}

#[test]
fn a_png_query_is_refused_without_a_decoder() {
    let query = format!(
        "\x1b_Gi=32,a=q,t=d,f=100;{}\x1b\\",
        STANDARD.encode(one_pixel_png())
    );
    assert_eq!(
        answer(&query),
        "\x1b_Gi=32;EINVAL: unsupported format\x1b\\"
    );
}

#[test]
fn an_rgb_query_is_answered_ok() {
    assert_eq!(
        answer("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\"),
        "\x1b_Gi=31;OK\x1b\\"
    );
}

#[test]
fn a_file_medium_query_is_refused() {
    let query = format!(
        "\x1b_Gi=33,s=1,v=1,a=q,t=f,f=24;{}\x1b\\",
        STANDARD.encode("/tmp/no-such-image.rgb")
    );
    assert_eq!(
        answer(&query),
        "\x1b_Gi=33;EINVAL: unsupported medium\x1b\\"
    );
}
