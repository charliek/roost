//! Clipboard image materialization for the paste path.
//!
//! A paste reads text first; when the system clipboard carries no text
//! the UI probes here. The clipboard's raw RGBA is encoded to a temp PNG
//! (mode `0o600`) and the *path* is what gets pasted, as ordinary
//! bracketed text — that is what makes agents like Claude Code and Codex
//! recognise the image and offer to attach it. Mirrors
//! the Mac UI's `PasteImage.swift`.
//!
//! A host tab takes the other half of this: [`read_clipboard_png`] stops
//! at the bytes, which go over the wire as `session.put_file` and never
//! touch this machine's disk (plan 047 §3.2). [`probe`] picks the half
//! its caller's sink asked for.
//!
//! [`read_clipboard_png`] BLOCKS: `arboard` talks to the display server
//! (or NSPasteboard) synchronously, and a large paste also spends real
//! time in the PNG encoder. Callers run it on the blocking pool — see
//! `UiTask::PasteImageProbe` — never on the UI thread.
//!
//! Failures are strings, like `screenshot.rs` — roost-iced carries no
//! `thiserror` — wrapped in [`ProbeError`] only far enough to separate
//! "there was no image" from "there was one and it did not work": the
//! first is the ordinary end of a text paste that found nothing and says
//! nothing, the second is worth a toast.

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

use roost_engine::ipc::HostOpFailure;
use roost_ipc::messages::MAX_PUT_FILE_BYTES;

/// Decoded-pixel cap. Unlike the now-removed GTK UI — which streamed a
/// compressed payload into gdk-pixbuf and could bail from `size-prepared`
/// before the RGBA buffer existed — arboard hands us pixels already
/// allocated, so this cap bounds the *encode* rather than the decode.
/// 40 MP comfortably covers 5K and 8K screenshots.
pub(crate) const MAX_PIXELS: u64 = 40 * 1024 * 1024;

/// Why a probe produced no image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProbeError {
    /// The clipboard holds no image — an empty clipboard, or one
    /// carrying only text. Not a failure: a paste that found neither
    /// text nor an image is over, quietly.
    Empty,
    /// There was an image and it did not become a PNG: over the caps,
    /// a clipboard held by someone else, an encode that failed.
    Failed(String),
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProbeError::Empty => f.write_str("the clipboard holds no image"),
            ProbeError::Failed(message) => f.write_str(message),
        }
    }
}

/// What a probe produced, by the sink its target asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Materialized {
    /// A local tab's temp PNG, to be pasted as a bare path.
    Path(String),
    /// A host tab's bytes, to be uploaded under `name`. They never reach
    /// this machine's disk.
    Png { name: String, png: Vec<u8> },
}

/// The whole blocking half of a clipboard-image paste, by the sink its
/// target asked for.
///
/// The host name is minted here rather than on the UI thread:
/// [`temp_png_name`] reads `/dev/urandom`, which has no business
/// blocking a frame.
pub(crate) fn probe(sink: crate::app::ProbeSink) -> Result<Materialized, ProbeError> {
    let png = read_clipboard_png()?;
    match sink {
        crate::app::ProbeSink::TempFile => write_temp_png(&png)
            .map(|path| Materialized::Path(path.to_string_lossy().into_owned()))
            .map_err(ProbeError::Failed),
        crate::app::ProbeSink::Bytes => {
            let name = temp_png_name().map_err(ProbeError::Failed)?;
            Ok(Materialized::Png { name, png })
        }
    }
}

/// The whole blocking half of `clipboard.write { image_png }` (plan 047
/// §3.5): the caller's PNG onto this machine's clipboard, answered with
/// the code the op reports.
pub(crate) fn write_png(png: &[u8]) -> Result<(), HostOpFailure> {
    let (width, height, rgba) =
        decode_png_rgba(png).map_err(|message| HostOpFailure::new("invalid-param", message))?;
    write_clipboard_image(width, height, rgba)
        .map_err(|error| write_failure(&error, std::env::var_os("WAYLAND_DISPLAY").is_some()))
}

/// What a failed platform write is worth telling the caller.
///
/// Whether a Wayland session can be written at all is the compositor's
/// answer, not the environment's — arboard's `wayland-data-control`
/// path works on one that implements the protocol (COSMIC does) and
/// fails on one that does not (the headless compositors the lanes run
/// under) — so the write is attempted and only its failure is
/// classified here. Under Wayland that is `not-supported` naming #302,
/// which is a lane's cue to skip rather than to report a paste bug;
/// anywhere else the failure is ours to explain.
fn write_failure(error: &str, wayland_display: bool) -> HostOpFailure {
    if wayland_display {
        HostOpFailure::new(
            "not-supported",
            format!("{error} — this Wayland session offers no clipboard to write (issue #302)"),
        )
    } else {
        HostOpFailure::new("internal", error)
    }
}

/// Read the system clipboard's image and encode it, stopping at the
/// bytes.
///
/// Blocking — see the module docs.
pub(crate) fn read_clipboard_png() -> Result<Vec<u8>, ProbeError> {
    let mut clipboard = arboard::Clipboard::new()
        .map_err(|error| ProbeError::Failed(format!("clipboard image: open: {error}")))?;
    let image = clipboard.get_image().map_err(read_failure)?;
    encode_rgba(image.width, image.height, &image.bytes).map_err(ProbeError::Failed)
}

/// [`MAX_PIXELS`], applied wherever dimensions are known and before
/// anything the size of the image is allocated.
///
/// `u64` rather than the callers' own widths so the multiplication that
/// reaches the cap cannot wrap on the way: a 32-bit PNG header and a
/// pair of `usize` from arboard both fit, and an overflow is its own
/// refusal rather than a small product that passes.
fn within_pixel_cap(width: u64, height: u64) -> Result<(), String> {
    let pixels = width
        .checked_mul(height)
        .ok_or_else(|| format!("clipboard image: dimensions overflow: {width}x{height}"))?;
    if pixels > MAX_PIXELS {
        return Err(format!(
            "clipboard image: {width}x{height} exceeds {MAX_PIXELS} pixels"
        ));
    }
    Ok(())
}

/// Decode a PNG to the RGBA8 arboard's `set_image` wants.
///
/// `EXPAND | ALPHA` is what makes a palette or a truecolor PNG arrive
/// as opaque RGBA without a conversion of our own. What it does *not*
/// widen — 16-bit samples, and grayscale, which expands to
/// grayscale-with-alpha rather than to RGBA — is refused rather than
/// converted here: the caller chose these bytes, and quietly
/// re-interpreting them would make the paste assertion downstream
/// meaningless.
///
/// Blocking-pool work like the rest of this module: a 40 MP decode is
/// not frame work.
pub(crate) fn decode_png_rgba(png: &[u8]) -> Result<(usize, usize, Vec<u8>), String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(png));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::ALPHA);
    let mut reader = decoder
        .read_info()
        .map_err(|error| format!("clipboard image: not a PNG: {error}"))?;
    let info = reader.info();
    within_pixel_cap(u64::from(info.width), u64::from(info.height))?;
    // `output_color_type` is what the transformations above will
    // produce, so both refusals are settled before the buffer they
    // would have filled is allocated.
    let (color_type, bit_depth) = reader.output_color_type();
    if bit_depth != png::BitDepth::Eight {
        return Err(format!(
            "clipboard image: PNG must be 8 bits per sample, got {bit_depth:?}"
        ));
    }
    if color_type != png::ColorType::Rgba {
        return Err(format!(
            "clipboard image: PNG must reduce to RGBA, got {color_type:?}"
        ));
    }
    let mut buffer = vec![
        0;
        reader.output_buffer_size().ok_or_else(|| {
            "clipboard image: PNG output buffer overflows".to_string()
        })?
    ];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| format!("clipboard image: undecodable PNG: {error}"))?;
    buffer.truncate(frame.buffer_size());
    Ok((frame.width as usize, frame.height as usize, buffer))
}

/// The `arboard::Clipboard` an image write leaves alive.
///
/// X11 has no clipboard *content*, only an owning window that answers
/// requests — and arboard's `Drop` tears its owning window down as soon
/// as the last handle goes, offering the data to a clipboard manager on
/// the way out. Under the headless X server the lanes run on there is no
/// manager, so a handle created and dropped inside one call writes an
/// image that is gone before the next call can read it. Holding one
/// handle for the life of the process is arboard's own advice, and is
/// what makes a paste issued right after the write find the image.
///
/// The read side does not touch it — [`read_clipboard_png`] opens its
/// own handle, exactly as an ordinary paste does — so what this static
/// buys is only that the image outlives the call that wrote it.
///
/// Only ever populated by [`write_clipboard_image`], which
/// `ROOST_TEST_MODE=1` gates — an ordinary run never opens it.
static IMAGE_CLIPBOARD: std::sync::Mutex<Option<arboard::Clipboard>> = std::sync::Mutex::new(None);

/// How long [`write_clipboard_image`] waits for what it wrote to become
/// readable. Generous for a local display server, and bounded because
/// the op's caller is blocked on it.
const OWNERSHIP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Hand decoded pixels to the platform clipboard, blocking until a
/// reader can see them.
///
/// arboard re-encodes to its own PNG on the way out, so what a reader
/// gets back is pixel-identical to `rgba` and byte-identical to nothing.
///
/// Blocking — see the module docs.
pub(crate) fn write_clipboard_image(
    width: usize,
    height: usize,
    rgba: Vec<u8>,
) -> Result<(), String> {
    let mut held = IMAGE_CLIPBOARD
        .lock()
        .map_err(|_| "clipboard image: the held clipboard is poisoned".to_string())?;
    let clipboard = match held.as_mut() {
        Some(clipboard) => clipboard,
        None => held.insert(
            arboard::Clipboard::new().map_err(|error| format!("clipboard image: open: {error}"))?,
        ),
    };
    clipboard
        .set_image(arboard::ImageData {
            width,
            height,
            bytes: std::borrow::Cow::Owned(rgba),
        })
        .map_err(|error| format!("clipboard image: write: {error}"))?;
    confirm_readable()
}

/// Wait until the image just written can actually be read back.
///
/// `set_image` returns once the write is on its way, not once the
/// display server has acted on it: the X11 backend flushes a
/// `SetSelectionOwner` without a round trip, and the Wayland one hands
/// the data to a helper that serves it. Answering the op on that alone
/// lets the paste issued in the next breath read the *previous*
/// clipboard — an intermittent failure of the exact lane this seam
/// exists for.
///
/// The check is a fresh handle plus `get_image`, which is
/// [`read_clipboard_png`]'s own call path (an ownership round trip
/// included) rather than a question the writing handle could answer out
/// of the data it just cached. Dropping the handle taken here is safe:
/// arboard hands the selection away only when its *last* handle goes,
/// and [`IMAGE_CLIPBOARD`] is still holding one.
fn confirm_readable() -> Result<(), String> {
    let deadline = std::time::Instant::now() + OWNERSHIP_TIMEOUT;
    loop {
        match arboard::Clipboard::new().and_then(|mut fresh| fresh.get_image()) {
            Ok(_) => return Ok(()),
            Err(error) if std::time::Instant::now() >= deadline => {
                return Err(format!(
                    "clipboard image: nothing readable {OWNERSHIP_TIMEOUT:?} after the write: {error}"
                ));
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    }
}

/// arboard reports an empty clipboard and one carrying only text with
/// the same variant, which is exactly the distinction we want: neither
/// is an image, and neither is worth telling the user about.
fn read_failure(error: arboard::Error) -> ProbeError {
    match error {
        arboard::Error::ContentNotAvailable => ProbeError::Empty,
        other => ProbeError::Failed(format!("clipboard image: read: {other}")),
    }
}

/// The caps and the encoder, with nothing written anywhere.
fn encode_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<Vec<u8>, String> {
    if width == 0 || height == 0 {
        return Err(format!("clipboard image: empty payload ({width}x{height})"));
    }
    within_pixel_cap(width as u64, height as u64)?;
    // arboard documents RGBA8, but a mismatch here would slice out of
    // bounds inside the encoder — reject it as an error, never a panic.
    let expected = width as u64 * height as u64 * 4;
    if rgba.len() as u64 != expected {
        return Err(format!(
            "clipboard image: RGBA length mismatch: expected {expected}, got {}",
            rgba.len()
        ));
    }
    // Default compression, unlike the screenshot encoder's `Fast` — a
    // paste is one-shot and off the UI thread either way, so there's no
    // reason to trade ratio for speed here.
    let png = crate::png_encode::encode_rgba8(
        width as u32,
        height as u32,
        rgba,
        png::Compression::default(),
    )
    .map_err(|error| format!("clipboard image: {error}"))?;
    ensure_encoded_size(png.len())?;
    Ok(png)
}

/// The per-file ceiling `session.put_file` enforces is the same one a
/// local temp PNG has always had (the Mac's, and the removed GTK UI's),
/// so both routes read it from the one place that owns it now.
fn ensure_encoded_size(len: usize) -> Result<(), String> {
    if len as u64 > MAX_PUT_FILE_BYTES {
        return Err(format!(
            "clipboard image: encoded PNG exceeds {MAX_PUT_FILE_BYTES} bytes ({len})"
        ));
    }
    Ok(())
}

/// Write `data` to `roost-image-{unix_nanos}-{16 hex}.png` in the temp
/// dir. Byte-for-byte the now-removed GTK UI's scheme: `create_new` so
/// a collision fails rather than clobbering, and mode `0o600` so the
/// file is unreadable by other users on a shared box.
pub(crate) fn write_temp_png(data: &[u8]) -> Result<PathBuf, String> {
    let path = std::env::temp_dir().join(temp_png_name()?);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&path)
        .map_err(|error| format!("clipboard image: create {}: {error}", path.display()))?;
    file.write_all(data)
        .map_err(|error| format!("clipboard image: write {}: {error}", path.display()))?;
    Ok(path)
}

/// The minted name, which a host upload needs on its own: the bytes go
/// over the wire under it, so `session.put_file` lands
/// `roost-image-<nanos>-<16hex>.png` on the far side too.
pub(crate) fn temp_png_name() -> Result<String, String> {
    let mut rnd = [0u8; 8];
    {
        // /dev/urandom is POSIX-portable and avoids pulling in
        // `getrandom` for 8 bytes — the same trade the now-removed GTK
        // UI made. This path only ever runs on macOS and Linux.
        use std::io::Read;
        std::fs::File::open("/dev/urandom")
            .and_then(|mut file| file.read_exact(&mut rnd))
            .map_err(|error| format!("clipboard image: random bytes: {error}"))?;
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(16);
    for byte in rnd {
        let _ = write!(hex, "{byte:02x}");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    Ok(format!("roost-image-{nanos}-{hex}.png"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgba(width: usize, height: usize) -> Vec<u8> {
        (0..width * height)
            .flat_map(|index| [index as u8, 0x40, 0x80, 0xff])
            .collect()
    }

    fn cleanup(path: &PathBuf) {
        let _ = std::fs::remove_file(path);
    }

    /// Encode RGBA8 pixels to a temp PNG and return its path — the two
    /// halves of [`probe`]'s `TempFile` sink with the clipboard read
    /// taken out, so the caps and the naming are testable without a
    /// display server.
    fn materialize_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<PathBuf, String> {
        let png = encode_rgba(width, height, rgba)?;
        write_temp_png(&png)
    }

    /// The generated name is also the defense-in-depth pin for #282: the
    /// charset is `roost-image-<digits>-<16 lowercase hex>.png`, so a
    /// materialized path can never carry a rejected control scalar or a
    /// shell metacharacter into the bracketed paste.
    fn name_is_safe(name: &str) -> bool {
        let Some(rest) = name.strip_prefix("roost-image-") else {
            return false;
        };
        let Some(rest) = rest.strip_suffix(".png") else {
            return false;
        };
        let Some((nanos, hex)) = rest.rsplit_once('-') else {
            return false;
        };
        !nanos.is_empty()
            && nanos.bytes().all(|byte| byte.is_ascii_digit())
            && hex.len() == 16
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }

    #[test]
    fn materialized_png_round_trips_dimensions_with_a_private_mode_and_safe_name() {
        let pixels = rgba(4, 3);
        let path = materialize_rgba(4, 3, &pixels).expect("materialize");

        let written = std::fs::read(&path).expect("read back");
        assert_eq!(&written[..8], b"\x89PNG\r\n\x1a\n");
        let mut reader = png::Decoder::new(std::io::Cursor::new(&written))
            .read_info()
            .expect("decode header");
        let mut decoded = vec![0; reader.output_buffer_size().expect("output size")];
        let info = reader.next_frame(&mut decoded).expect("decode pixels");
        assert_eq!((info.width, info.height), (4, 3));
        assert_eq!(&decoded[..info.buffer_size()], pixels.as_slice());

        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name_is_safe(&name), "unexpected temp name {name}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        cleanup(&path);
    }

    /// Just over 40 MP, with no pixel buffer to match: the cap is checked
    /// before the length sanity check and long before `write_temp_png`,
    /// the only thing in this module that creates a file.
    #[test]
    fn pixel_cap_rejects_before_any_file_is_created() {
        let error = materialize_rgba(7_000, 6_000, &[]).expect_err("over the pixel cap");
        assert!(error.contains("exceeds"), "{error}");
        assert_eq!(
            MAX_PIXELS, 41_943_040,
            "byte-identical to the removed GTK UI's ceiling"
        );
    }

    #[test]
    fn malformed_payloads_are_errors_not_panics() {
        assert!(materialize_rgba(0, 4, &[])
            .expect_err("zero width")
            .contains("empty payload"));
        assert!(materialize_rgba(4, 0, &[])
            .expect_err("zero height")
            .contains("empty payload"));
        assert!(materialize_rgba(2, 2, &[0; 12])
            .expect_err("short buffer")
            .contains("length mismatch"));
    }

    /// The output cap is a seam of its own because a real 10 MiB PNG is
    /// expensive to build; the now-removed GTK UI applied the same
    /// ceiling to its re-encoded bytes.
    #[test]
    fn encoded_size_cap_matches_the_removed_gtk_uis_ceiling() {
        let cap = MAX_PUT_FILE_BYTES as usize;
        assert_eq!(cap, 10 * 1024 * 1024);
        assert!(ensure_encoded_size(cap).is_ok());
        assert!(ensure_encoded_size(cap + 1)
            .expect_err("over the byte cap")
            .contains("exceeds"));
    }

    /// The split the host route needs: the bytes on their own, and the
    /// file written from exactly those bytes.
    #[test]
    fn the_encoded_bytes_and_the_written_file_are_the_same_png() {
        let pixels = rgba(4, 3);
        let png = encode_rgba(4, 3, &pixels).expect("encode");
        let path = write_temp_png(&png).expect("write");
        assert_eq!(std::fs::read(&path).expect("read back"), png);
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(name_is_safe(&name), "unexpected temp name {name}");
        cleanup(&path);
    }

    /// Encode `rgba` at some other colour type / bit depth than our own
    /// encoder emits, so the decoder's refusals can be provoked.
    fn encode_as(
        width: u32,
        height: u32,
        color: png::ColorType,
        depth: png::BitDepth,
        samples: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(color);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().expect("header");
        writer.write_image_data(samples).expect("pixels");
        writer.finish().expect("finish");
        out
    }

    /// A write the display server refused, sorted into the two codes a
    /// caller can act on — deterministically, in both directions,
    /// which is the point of taking the environment as an argument
    /// rather than reading it here. Naming the issue in the Wayland
    /// message matters too: a `not-supported` with no reason reads as a
    /// broken build.
    #[test]
    fn a_refused_write_is_not_supported_only_under_wayland() {
        let wayland = write_failure("clipboard image: write: no data control", true);
        assert_eq!(wayland.code, "not-supported");
        assert!(wayland.message.contains("#302"), "{wayland:?}");
        assert!(wayland.message.contains("no data control"), "{wayland:?}");

        let elsewhere = write_failure("clipboard image: write: no data control", false);
        assert_eq!(elsewhere.code, "internal");
        assert!(
            elsewhere.message.contains("no data control"),
            "{elsewhere:?}"
        );
    }

    /// One ceiling, both halves: the header a write is about to decode
    /// and the pixels a probe is about to encode. The overflow arm is
    /// unreachable from a PNG header and reachable from arboard's
    /// `usize` dimensions, which is why it lives here rather than at
    /// either caller.
    #[test]
    fn the_image_pixel_cap_holds_for_both_halves() {
        assert!(within_pixel_cap(8192, 5120).is_ok(), "exactly the cap");
        let over = within_pixel_cap(8192, 5121).expect_err("one row over the cap");
        assert!(over.contains("exceeds"), "{over}");
        let wrapped = within_pixel_cap(u64::MAX, 2).expect_err("a product that does not fit");
        assert!(wrapped.contains("overflow"), "{wrapped}");
    }

    /// The seam's whole claim: the pixels a caller hands
    /// `clipboard.write` are the pixels arboard is given. Byte equality
    /// is deliberately NOT asserted — arboard re-encodes with its own
    /// PNG writer, so only the pixels survive the crossing.
    #[test]
    fn decoding_a_png_recovers_the_pixels_it_was_encoded_from() {
        let pixels = rgba(9, 7);
        let png = encode_rgba(9, 7, &pixels).expect("encode");
        assert_eq!(decode_png_rgba(&png).expect("decode"), (9, 7, pixels));
    }

    /// A truecolor PNG without an alpha channel is the other shape a
    /// real screenshot arrives in, and the transformation set is chosen
    /// so it widens to opaque RGBA instead of being refused. Pinned
    /// because dropping `ALPHA` from that set would still compile and
    /// would still decode — into RGB arboard cannot take.
    #[test]
    fn decoding_widens_an_rgb_png_to_opaque_rgba() {
        let rgb: Vec<u8> = (0..4 * 2 * 3).map(|index| index as u8).collect();
        let png = encode_as(4, 2, png::ColorType::Rgb, png::BitDepth::Eight, &rgb);
        let (width, height, rgba) = decode_png_rgba(&png).expect("decode");
        assert_eq!((width, height), (4, 2));
        assert_eq!(rgba.len(), 4 * 2 * 4);
        for (pixel, source) in rgba.chunks_exact(4).zip(rgb.chunks_exact(3)) {
            assert_eq!(&pixel[..3], source);
            assert_eq!(pixel[3], 0xff);
        }
    }

    /// Both refusals the seam owes a caller `invalid-param` for. The
    /// 16-bit arm is the one worth pinning: nothing here strips it down
    /// silently, because the caller chose those bytes.
    #[test]
    fn decoding_refuses_a_non_png_and_a_16_bit_png() {
        let garbage = decode_png_rgba(b"not a png at all").expect_err("not a PNG");
        assert!(garbage.contains("not a PNG"), "{garbage}");

        let deep = encode_as(
            2,
            2,
            png::ColorType::Rgb,
            png::BitDepth::Sixteen,
            &[0x11; 2 * 2 * 3 * 2],
        );
        let refused = decode_png_rgba(&deep).expect_err("16 bits per sample");
        assert!(refused.contains("8 bits per sample"), "{refused}");
    }

    /// An empty clipboard is not a failure and must not become one: the
    /// probe's `Err` arm toasts, and "you pasted with nothing on the
    /// clipboard" is not news.
    #[test]
    fn only_a_missing_image_is_silent() {
        assert_eq!(
            read_failure(arboard::Error::ContentNotAvailable),
            ProbeError::Empty
        );
        assert!(matches!(
            read_failure(arboard::Error::ClipboardOccupied),
            ProbeError::Failed(_)
        ));
    }
}
