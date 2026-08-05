//! Shared RGBA8 → PNG encoder, used by [`crate::screenshot`] (renderer
//! capture) and [`crate::paste_image`] (clipboard image paste). Both
//! callers hand it an already-decoded, already-bounds-checked RGBA8
//! buffer; this just owns the one `png::Encoder` incantation the two
//! call sites shared byte-for-byte.

/// Encode `rgba` (must be exactly `width * height * 4` bytes) as an
/// 8-bit RGBA PNG.
pub(crate) fn encode_rgba8(
    width: u32,
    height: u32,
    rgba: &[u8],
    compression: png::Compression,
) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(compression);
        let mut writer = encoder
            .write_header()
            .map_err(|error| format!("encode PNG header: {error}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|error| format!("encode PNG pixels: {error}"))?;
    }
    Ok(bytes)
}
