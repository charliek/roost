//! Clipboard image materialization for the paste path.
//!
//! A paste reads text first; when the system clipboard carries no text
//! the UI probes here. The clipboard's raw RGBA is encoded to a temp PNG
//! (mode `0o600`) and the *path* is what gets pasted, as ordinary
//! bracketed text — that is what makes agents like Claude Code and Codex
//! recognise the image and offer to attach it. Mirrors
//! `roost-linux/src/paste_image.rs` and the Mac UI's `PasteImage.swift`.
//!
//! [`materialize`] BLOCKS: `arboard` talks to the display server (or
//! NSPasteboard) synchronously, and a large paste also spends real time
//! in the PNG encoder. Callers run it on the blocking pool — see
//! `UiTask::PasteImageProbe` — never on the UI thread.
//!
//! Errors are `String`, like `screenshot.rs`: every failure mode here is
//! logged and dropped at one call site, so a typed enum would buy the
//! crate nothing (and roost-iced carries no `thiserror`).

use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;

/// Decoded-pixel cap. Unlike the GTK UI — which streams a compressed
/// payload into gdk-pixbuf and can bail from `size-prepared` before the
/// RGBA buffer exists — arboard hands us pixels already allocated, so
/// this cap bounds the *encode* rather than the decode. 40 MP comfortably
/// covers 5K and 8K screenshots.
pub(crate) const MAX_PIXELS: u64 = 40 * 1024 * 1024;

/// Maximum PNG we'll write. Matches the GTK and Mac ceilings; GTK applies
/// it to the re-encoded output too (`paste_image.rs`), which is the check
/// we mirror here since our input is never compressed.
pub(crate) const MAX_BYTES: usize = 10 * 1024 * 1024;

/// Read the system clipboard's image and write it to a temp PNG.
///
/// Blocking — see the module docs.
pub(crate) fn materialize() -> Result<PathBuf, String> {
    let mut clipboard =
        arboard::Clipboard::new().map_err(|error| format!("clipboard image: open: {error}"))?;
    let image = clipboard
        .get_image()
        .map_err(|error| format!("clipboard image: read: {error}"))?;
    materialize_rgba(image.width, image.height, &image.bytes)
}

/// Encode RGBA8 pixels to a temp PNG and return its path. Split from the
/// clipboard read so the caps and the naming are testable without a
/// display server.
fn materialize_rgba(width: usize, height: usize, rgba: &[u8]) -> Result<PathBuf, String> {
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
    write_temp_png(&png)
}

fn ensure_encoded_size(len: usize) -> Result<(), String> {
    if len > MAX_BYTES {
        return Err(format!(
            "clipboard image: encoded PNG exceeds {MAX_BYTES} bytes ({len})"
        ));
    }
    Ok(())
}

/// Write `data` to `roost-image-{unix_nanos}-{16 hex}.png` in the temp
/// dir. Byte-for-byte the GTK scheme: `create_new` so a collision fails
/// rather than clobbering, and mode `0o600` so the file is unreadable by
/// other users on a shared box.
fn write_temp_png(data: &[u8]) -> Result<PathBuf, String> {
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

fn temp_png_name() -> Result<String, String> {
    let mut rnd = [0u8; 8];
    {
        // /dev/urandom is POSIX-portable and avoids pulling in
        // `getrandom` for 8 bytes — the same trade the GTK UI makes. This
        // path only ever runs on macOS and Linux.
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
        assert_eq!(MAX_PIXELS, 41_943_040, "byte-identical to the GTK ceiling");
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
    /// expensive to build; GTK applies the same ceiling to its re-encoded
    /// bytes.
    #[test]
    fn encoded_size_cap_matches_the_gtk_ceiling() {
        assert_eq!(MAX_BYTES, 10 * 1024 * 1024);
        assert!(ensure_encoded_size(MAX_BYTES).is_ok());
        assert!(ensure_encoded_size(MAX_BYTES + 1)
            .expect_err("over the byte cap")
            .contains("exceeds"));
    }
}
