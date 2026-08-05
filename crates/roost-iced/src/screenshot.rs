//! Iced renderer capture normalization for Roost's screenshot IPC port.
//!
//! Iced captures the current viewport in physical pixels. Roost's wire
//! contract instead asks for logical renderer-surface pixels multiplied by an
//! explicit 1x/2x scale, so this adapter normalizes across Retina, X11, and Wayland
//! before encoding an owned PNG.

use iced::window::Screenshot;

const RGBA_BYTES_PER_PIXEL: usize = 4;

pub fn encode(capture: &Screenshot, requested_scale: u32) -> Result<(Vec<u8>, u32, u32), String> {
    if !(1..=2).contains(&requested_scale) {
        return Err(format!(
            "screenshot scale must be 1 or 2, got {requested_scale}"
        ));
    }
    if !capture.scale_factor.is_finite() || capture.scale_factor <= 0.0 {
        return Err(format!(
            "invalid Iced screenshot scale factor {}",
            capture.scale_factor
        ));
    }
    let source_width = capture.size.width;
    let source_height = capture.size.height;
    if source_width == 0 || source_height == 0 {
        return Err("Iced screenshot has zero size".into());
    }
    let source_len = rgba_len(source_width, source_height)?;
    if capture.rgba.len() != source_len {
        return Err(format!(
            "Iced screenshot RGBA length mismatch: expected {source_len}, got {}",
            capture.rgba.len()
        ));
    }

    let width = normalized_dimension(source_width, capture.scale_factor, requested_scale)?;
    let height = normalized_dimension(source_height, capture.scale_factor, requested_scale)?;
    let rgba = if width == source_width && height == source_height {
        capture.rgba.to_vec()
    } else {
        nearest_neighbor_rgba(&capture.rgba, source_width, source_height, width, height)?
    };
    let png_bytes = crate::png_encode::encode_rgba8(width, height, &rgba, png::Compression::Fast)?;
    Ok((png_bytes, width, height))
}

fn normalized_dimension(
    physical: u32,
    native_scale: f32,
    requested_scale: u32,
) -> Result<u32, String> {
    let logical = (f64::from(physical) / f64::from(native_scale)).round();
    if !logical.is_finite() || logical < 1.0 || logical > f64::from(u32::MAX) {
        return Err(format!(
            "invalid normalized screenshot dimension from {physical}px at {native_scale}x"
        ));
    }
    (logical as u32)
        .checked_mul(requested_scale)
        .ok_or_else(|| {
            format!(
                "normalized screenshot dimension overflows at requested scale {requested_scale}"
            )
        })
}

fn nearest_neighbor_rgba(
    source: &[u8],
    source_width: u32,
    source_height: u32,
    target_width: u32,
    target_height: u32,
) -> Result<Vec<u8>, String> {
    let target_len = rgba_len(target_width, target_height)?;
    let mut target = Vec::new();
    target
        .try_reserve_exact(target_len)
        .map_err(|error| format!("allocate normalized screenshot: {error}"))?;
    target.resize(target_len, 0);

    let source_width = source_width as usize;
    let source_height = source_height as usize;
    let target_width = target_width as usize;
    let target_height = target_height as usize;
    for target_y in 0..target_height {
        let source_y = target_y * source_height / target_height;
        for target_x in 0..target_width {
            let source_x = target_x * source_width / target_width;
            let source_offset = (source_y * source_width + source_x) * RGBA_BYTES_PER_PIXEL;
            let target_offset = (target_y * target_width + target_x) * RGBA_BYTES_PER_PIXEL;
            target[target_offset..target_offset + RGBA_BYTES_PER_PIXEL]
                .copy_from_slice(&source[source_offset..source_offset + RGBA_BYTES_PER_PIXEL]);
        }
    }
    Ok(target)
}

fn rgba_len(width: u32, height: u32) -> Result<usize, String> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| format!("screenshot dimensions overflow: {width}x{height}"))?;
    pixels
        .checked_mul(RGBA_BYTES_PER_PIXEL)
        .ok_or_else(|| format!("screenshot RGBA length overflows: {width}x{height}"))
}

#[cfg(test)]
mod tests {
    use iced::Size;

    use super::*;

    fn capture(width: u32, height: u32, scale_factor: f32) -> Screenshot {
        let mut rgba = Vec::new();
        for y in 0..height {
            for x in 0..width {
                rgba.extend_from_slice(&[x as u8, y as u8, 0x7f, 0xff]);
            }
        }
        Screenshot::new(rgba, Size::new(width, height), scale_factor)
    }

    fn png_dimensions(bytes: &[u8]) -> (u32, u32) {
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        (
            u32::from_be_bytes(bytes[16..20].try_into().unwrap()),
            u32::from_be_bytes(bytes[20..24].try_into().unwrap()),
        )
    }

    #[test]
    fn normalizes_native_one_and_two_x_to_requested_dimensions() {
        let (one_x, width, height) = encode(&capture(4, 2, 1.0), 1).unwrap();
        assert_eq!((width, height), (4, 2));
        assert_eq!(png_dimensions(&one_x), (4, 2));

        let (retina_one_x, width, height) = encode(&capture(8, 4, 2.0), 1).unwrap();
        assert_eq!((width, height), (4, 2));
        assert_eq!(png_dimensions(&retina_one_x), (4, 2));

        let (linux_two_x, width, height) = encode(&capture(4, 2, 1.0), 2).unwrap();
        assert_eq!((width, height), (8, 4));
        assert_eq!(png_dimensions(&linux_two_x), (8, 4));

        let (retina_two_x, width, height) = encode(&capture(8, 4, 2.0), 2).unwrap();
        assert_eq!((width, height), (8, 4));
        assert_eq!(png_dimensions(&retina_two_x), (8, 4));
    }

    #[test]
    fn fractional_native_scale_uses_one_canonical_logical_extent() {
        assert_eq!(normalized_dimension(1377, 1.25, 1).unwrap(), 1102);
        assert_eq!(normalized_dimension(1377, 1.25, 2).unwrap(), 2204);
        assert_eq!(normalized_dimension(1000, 1.5, 1).unwrap(), 667);
        assert_eq!(normalized_dimension(1000, 1.5, 2).unwrap(), 1334);
        assert!(normalized_dimension(u32::MAX, 1.0, 2)
            .unwrap_err()
            .contains("overflows"));
    }

    #[test]
    fn rejects_invalid_scale_dimensions_and_rgba_metadata() {
        assert!(encode(&capture(2, 2, 1.0), 3)
            .unwrap_err()
            .contains("scale must be 1 or 2"));
        assert!(encode(&capture(2, 2, 0.0), 1)
            .unwrap_err()
            .contains("scale factor"));
        assert!(
            encode(&Screenshot::new(Vec::new(), Size::new(0, 2), 1.0), 1)
                .unwrap_err()
                .contains("zero size")
        );
        assert!(
            encode(&Screenshot::new(vec![0; 3], Size::new(1, 1), 1.0), 1)
                .unwrap_err()
                .contains("length mismatch")
        );
    }
}
