//! Clipboard image-paste extraction for `terminal_view::paste_from_clipboard`.
//!
//! Two input shapes feed this module:
//!
//!   1. A `text/uri-list` payload (Nautilus/Files/Dolphin/etc. file
//!      copy) — we parse the URI list, keep `file://` URIs whose path
//!      has an image extension, and the existing paths get pasted
//!      verbatim (no temp copy).
//!   2. Raw image bytes — PNG passes through; other formats
//!      (`image/jpeg`, `image/gif`, `image/tiff`, `image/webp`,
//!      …) are decoded once by gdk-pixbuf and re-encoded to PNG so
//!      the agent always sees a `.png` extension. The temp file
//!      lives in `std::env::temp_dir()` with mode `0o600`.
//!
//! The encoded path is then pasted as ordinary bracketed-paste text.
//! Mirrors the legacy Go implementation at `cmd/roost/paste_image.go`
//! and the Mac counterpart in `mac/Sources/Roost/PasteImage.swift`.

use std::cell::Cell;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::rc::Rc;

use gdk_pixbuf::prelude::*;
use gdk_pixbuf::Pixbuf;

/// Maximum clipboard payload we'll materialize. Matches the legacy
/// Go cap (cmd/roost/paste_image.go:27) and the Mac port's ceiling.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;

/// Decoded-megapixel cap. A 10 MiB JPEG can describe an 8000×8000
/// image whose RGBA buffer is 256 MiB — we check dimensions before
/// re-encoding so a compression-bomb input can't OOM the renderer.
/// 40 MP comfortably covers 5K and 8K screenshots.
pub const MAX_PIXELS: usize = 40 * 1024 * 1024;

/// MIME types we'll negotiate with the clipboard, in priority order.
/// PNG is first because it needs no re-encoding. The rest are decoded
/// by gdk-pixbuf and re-encoded.
pub const IMAGE_MIMES: &[&str] = &[
    "image/png",
    "image/jpeg",
    "image/gif",
    "image/tiff",
    "image/webp",
];

/// File extensions accepted in the file-URL fast path. The check is
/// case-insensitive; the agent recognises any of these.
pub const IMAGE_EXTENSIONS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "tiff", "tif", "webp", "heic", "heif", "bmp",
];

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("clipboard image: exceeds {} bytes ({len})", MAX_BYTES)]
    TooLarge { len: usize },
    #[error("clipboard image: {width}x{height} exceeds {} megapixels", MAX_PIXELS)]
    TooManyPixels { width: i32, height: i32 },
    #[error("clipboard image: empty payload")]
    Empty,
    #[error("clipboard image: decode failed: {0}")]
    Decode(String),
    #[error("clipboard image: encode failed: {0}")]
    Encode(String),
    #[error("clipboard image: write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("clipboard image: random bytes: {0}")]
    Random(#[from] std::io::Error),
}

/// Top-level entry — write `bytes` (in `mime` format) to a temp PNG
/// and return the absolute path. PNG inputs pass through; everything
/// else is decoded by gdk-pixbuf and re-encoded.
pub fn materialize(bytes: &[u8], mime: &str) -> Result<PathBuf, Error> {
    if bytes.is_empty() {
        return Err(Error::Empty);
    }
    if bytes.len() > MAX_BYTES {
        return Err(Error::TooLarge { len: bytes.len() });
    }
    let png_bytes = if mime == "image/png" {
        if let Some((w, h)) = png_dimensions(bytes) {
            if !pixel_count_fits(w, h) {
                return Err(Error::TooManyPixels {
                    width: w,
                    height: h,
                });
            }
        }
        bytes.to_vec()
    } else {
        reencode_to_png(bytes)?
    };
    write_temp_png(&png_bytes)
}

/// Convert a non-PNG image to PNG via gdk-pixbuf. The dimension
/// check runs from `size-prepared` (which fires after the header
/// parses, before pixel decode starts) so a compression-bomb input
/// can't force a megapixel-sized RGBA allocation before we reject
/// it — that requires feeding the loader in small chunks rather
/// than handing it the whole buffer in one `write`. We also cap the
/// encoded PNG size so a benign decode that re-encodes huge can't
/// slip past the source-byte cap.
fn reencode_to_png(bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    // size-prepared captures the header-reported dimensions exactly
    // once; the closure can't bail out of `write` directly, so the
    // outer loop reads this cell after each chunk and aborts.
    let oversized: Rc<Cell<Option<(i32, i32)>>> = Rc::new(Cell::new(None));
    let cb_state = oversized.clone();
    loader.connect_size_prepared(move |_, w, h| {
        if !pixel_count_fits(w, h) {
            cb_state.set(Some((w, h)));
        }
    });
    const CHUNK: usize = 4096;
    let mut pos = 0;
    while pos < bytes.len() {
        let end = (pos + CHUNK).min(bytes.len());
        loader
            .write(&bytes[pos..end])
            .map_err(|e| Error::Decode(e.to_string()))?;
        pos = end;
        if let Some((w, h)) = oversized.get() {
            let _ = loader.close();
            return Err(Error::TooManyPixels {
                width: w,
                height: h,
            });
        }
    }
    loader.close().map_err(|e| Error::Decode(e.to_string()))?;
    let pixbuf: Pixbuf = loader
        .pixbuf()
        .ok_or_else(|| Error::Decode("loader produced no pixbuf".to_string()))?;
    let (w, h) = (pixbuf.width(), pixbuf.height());
    // Belt-and-suspenders: size-prepared fired, but some loaders
    // don't emit it (broken plugins). Re-check post-decode.
    if !pixel_count_fits(w, h) {
        return Err(Error::TooManyPixels {
            width: w,
            height: h,
        });
    }
    let encoded = pixbuf
        .save_to_bufferv("png", &[])
        .map_err(|e| Error::Encode(e.to_string()))?;
    if encoded.len() > MAX_BYTES {
        return Err(Error::TooLarge { len: encoded.len() });
    }
    Ok(encoded.to_vec())
}

/// Overflow-safe `width * height <= MAX_PIXELS`. Uses division form
/// to dodge the trapping multiplication; rejects non-positive
/// dimensions (broken loaders, hand-crafted headers).
pub fn pixel_count_fits(width: i32, height: i32) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    (width as usize) <= MAX_PIXELS / (height as usize)
}

/// Peek a PNG IHDR for width/height without decoding pixels. PNG
/// layout: 8-byte signature, then `length(4) + type(4) + payload +
/// crc(4)`. The first chunk MUST be IHDR with length 13
/// (RFC 2083 §11.2.2); we validate both the type bytes and the
/// length before trusting the dimension fields, so a hand-crafted
/// PNG-shaped blob with garbage in that slot can't mislead the
/// caller's bounds check.
pub fn png_dimensions(data: &[u8]) -> Option<(i32, i32)> {
    if data.len() < 24 {
        return None;
    }
    let sig = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if data[..8] != sig {
        return None;
    }
    let ihdr_len = i32::from_be_bytes(data[8..12].try_into().ok()?);
    if ihdr_len != 13 {
        return None;
    }
    if &data[12..16] != b"IHDR" {
        return None;
    }
    let w = i32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = i32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

fn write_temp_png(data: &[u8]) -> Result<PathBuf, Error> {
    let mut rnd = [0u8; 8];
    // /dev/urandom is POSIX-portable (macOS dev build of roost-linux
    // works too); avoids pulling in `getrandom` for 8 bytes.
    {
        use std::io::Read;
        std::fs::File::open("/dev/urandom")?.read_exact(&mut rnd)?;
    }
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(16);
    for b in rnd {
        let _ = write!(hex, "{b:02x}");
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let name = format!("roost-image-{nanos}-{hex}.png");
    let path = std::env::temp_dir().join(name);

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|source| Error::Write {
            path: path.clone(),
            source,
        })?;
    file.write_all(data).map_err(|source| Error::Write {
        path: path.clone(),
        source,
    })?;
    Ok(path)
}

/// Parse a `text/uri-list` payload and return the local file paths
/// whose extensions match `IMAGE_EXTENSIONS`. RFC 2483: comments
/// start with `#`, lines are CRLF-terminated; we accept LF too for
/// robustness against poorly-behaved producers.
pub fn file_uris_to_paths(uri_list: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in uri_list.split('\n') {
        let line = raw.trim_end_matches('\r').trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(path) = file_uri_to_path(line) else {
            continue;
        };
        let lower_ext = std::path::Path::new(&path)
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase())
            .unwrap_or_default();
        if IMAGE_EXTENSIONS.iter().any(|e| *e == lower_ext) {
            out.push(path);
        }
    }
    out
}

/// Strip the `file://` scheme + optional host from a single URI and
/// percent-decode the path. Returns None for non-file URIs.
fn file_uri_to_path(uri: &str) -> Option<String> {
    let rest = uri.strip_prefix("file://")?;
    // `file:///path` after stripping scheme leaves `/path` — empty
    // host. `file://host/path` is rare on local clipboards; the
    // first '/' starts the path either way.
    let path_part = match rest.find('/') {
        Some(0) => rest,
        Some(i) => &rest[i..],
        None => return None,
    };
    Some(percent_decode(path_part))
}

/// Decode `%HH` escapes in-place. We avoid the `percent-encoding`
/// crate for one call site.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = hex_nibble(bytes[i + 1]);
            let lo = hex_nibble(bytes[i + 2]);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Once;

    static GDK_INIT: Once = Once::new();

    /// gdk-pixbuf loaders need the type system bootstrapped. Calling
    /// `gtk4::init()` is overkill (it tries to open a display); a
    /// bare `gio::resources_register` + `Pixbuf::new` is enough to
    /// nudge glib into initialization. Easiest path: just create a
    /// throwaway pixbuf the first time round.
    fn ensure_gdk() {
        GDK_INIT.call_once(|| {
            let _ = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 1, 1);
        });
    }

    fn tiny_png_bytes() -> Vec<u8> {
        ensure_gdk();
        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, true, 8, 4, 3).unwrap();
        pb.fill(0xff0000ff);
        pb.save_to_bufferv("png", &[]).unwrap().to_vec()
    }

    fn tiny_jpeg_bytes() -> Vec<u8> {
        ensure_gdk();
        let pb = Pixbuf::new(gdk_pixbuf::Colorspace::Rgb, false, 8, 4, 3).unwrap();
        pb.fill(0xff0000ff);
        pb.save_to_bufferv("jpeg", &[]).unwrap().to_vec()
    }

    fn cleanup(p: &PathBuf) {
        let _ = std::fs::remove_file(p);
    }

    #[test]
    fn png_passthrough_writes_temp_file_with_same_bytes() {
        let png = tiny_png_bytes();
        let path = materialize(&png, "image/png").expect("materialize png");
        let written = std::fs::read(&path).expect("read back");
        assert_eq!(written, png);
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("roost-image-"));
        assert_eq!(path.extension().and_then(|e| e.to_str()), Some("png"));
        cleanup(&path);
    }

    #[test]
    fn jpeg_is_reencoded_to_png() {
        let jpg = tiny_jpeg_bytes();
        let path = materialize(&jpg, "image/jpeg").expect("materialize jpeg");
        let written = std::fs::read(&path).expect("read back");
        assert_eq!(
            written[..8],
            [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
        );
        cleanup(&path);
    }

    #[test]
    fn oversized_payload_is_rejected_before_decode() {
        let mut huge = vec![0x89, 0x50, 0x4E, 0x47];
        huge.resize(MAX_BYTES + 1, 0);
        assert!(matches!(
            materialize(&huge, "image/png"),
            Err(Error::TooLarge { .. })
        ));
    }

    #[test]
    fn empty_payload_returns_empty_error() {
        assert!(matches!(materialize(&[], "image/png"), Err(Error::Empty)));
    }

    #[test]
    fn png_dimensions_parses_ihdr() {
        let png = tiny_png_bytes();
        assert_eq!(png_dimensions(&png), Some((4, 3)));
    }

    #[test]
    fn png_dimensions_rejects_non_png() {
        assert_eq!(png_dimensions(&[0xff, 0xd8, 0xff, 0xe0]), None);
        assert_eq!(png_dimensions(&[]), None);
    }

    #[test]
    fn png_dimensions_rejects_bad_ihdr_type() {
        // Signature OK, chunk length = 13, but type "FOOO" — must
        // reject so the caller's bounds check isn't fooled by garbage
        // in the dimension slot.
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x0D]);
        png.extend_from_slice(b"FOOO");
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x0C]);
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]);
        assert_eq!(png_dimensions(&png), None);
    }

    #[test]
    fn png_dimensions_rejects_bad_ihdr_length() {
        // Signature + IHDR type OK, but length lies about its size
        // (claims 99 bytes). Reject defensively.
        let mut png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x63]);
        png.extend_from_slice(b"IHDR");
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x0C]);
        png.extend_from_slice(&[0x00, 0x00, 0x00, 0x07]);
        assert_eq!(png_dimensions(&png), None);
    }

    #[test]
    fn pixel_count_fits_bounds() {
        assert!(pixel_count_fits(1024, 1024));
        assert!(pixel_count_fits(1, MAX_PIXELS as i32));
        assert!(pixel_count_fits(MAX_PIXELS as i32, 1));
        // 100_000 × 100_000 = 10^10 — division-form check rejects
        // without ever materialising the product.
        assert!(!pixel_count_fits(100_000, 100_000));
        assert!(!pixel_count_fits(0, 1));
        assert!(!pixel_count_fits(1, 0));
        assert!(!pixel_count_fits(-1, 1));
    }

    #[test]
    fn file_uri_to_path_strips_scheme_and_decodes() {
        assert_eq!(
            file_uri_to_path("file:///tmp/foo%20bar.png"),
            Some("/tmp/foo bar.png".to_string())
        );
        assert_eq!(
            file_uri_to_path("file:///home/charliek/img.PNG"),
            Some("/home/charliek/img.PNG".to_string())
        );
        assert_eq!(file_uri_to_path("https://example.com/img.png"), None);
    }

    #[test]
    fn file_uris_to_paths_filters_by_extension_and_ignores_comments() {
        let payload = "\
# this is a comment\r\n\
file:///tmp/a.png\r\n\
file:///tmp/b.txt\r\n\
file:///tmp/c.JPG\r\n\
file:///tmp/d.tar.gz\r\n\
file:///tmp/e.gif\r\n\
";
        let paths = file_uris_to_paths(payload);
        assert_eq!(paths, vec!["/tmp/a.png", "/tmp/c.JPG", "/tmp/e.gif"]);
    }

    #[test]
    fn file_uris_to_paths_handles_lf_only() {
        let payload = "file:///tmp/a.png\nfile:///tmp/b.jpg\n";
        let paths = file_uris_to_paths(payload);
        assert_eq!(paths, vec!["/tmp/a.png", "/tmp/b.jpg"]);
    }
}
