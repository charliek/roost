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
    let pixels = (width as u64)
        .checked_mul(height as u64)
        .ok_or_else(|| format!("clipboard image: dimensions overflow: {width}x{height}"))?;
    if pixels > MAX_PIXELS {
        return Err(format!(
            "clipboard image: {width}x{height} exceeds {MAX_PIXELS} pixels"
        ));
    }
    // arboard documents RGBA8, but a mismatch here would slice out of
    // bounds inside the encoder — reject it as an error, never a panic.
    let expected = pixels * 4;
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
