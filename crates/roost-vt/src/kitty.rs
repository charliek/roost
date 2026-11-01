//! Kitty graphics as a renderer draws them: the placements a
//! [`crate::RenderState`] lists, and the pixels [`Terminal::kitty_image`]
//! lends. Toolkit-neutral; the iced UI and any other painter build on it.

use std::marker::PhantomData;
use std::os::raw::{c_int, c_void};
use std::ptr;

use crate::sys;
use crate::{Error, Result, Terminal};

/// Where a placement draws relative to the text. Variants are in draw
/// order: below the cell backgrounds, then above them, then above the
/// text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ImageLayer {
    BelowBackground,
    BelowText,
    AboveText,
}

impl ImageLayer {
    pub(crate) fn from_raw(raw: c_int) -> Result<Self> {
        use sys::{
            GhosttyRenderStateImageLayer_GHOSTTY_RENDER_STATE_IMAGE_LAYER_ABOVE_TEXT as ABOVE_TEXT,
            GhosttyRenderStateImageLayer_GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_BACKGROUND as BELOW_BACKGROUND,
            GhosttyRenderStateImageLayer_GHOSTTY_RENDER_STATE_IMAGE_LAYER_BELOW_TEXT as BELOW_TEXT,
        };
        match raw as sys::GhosttyRenderStateImageLayer {
            BELOW_BACKGROUND => Ok(Self::BelowBackground),
            BELOW_TEXT => Ok(Self::BelowText),
            ABOVE_TEXT => Ok(Self::AboveText),
            _ => Err(Error::InvalidValue),
        }
    }
}

/// How the program placed the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum PlacementKind {
    /// One row of a `U=1` placement's placeholder cells.
    Virtual,
    /// Positioned from a parent placement.
    Relative,
    /// At a cell; scrolls with the text.
    Fixed,
}

impl PlacementKind {
    pub(crate) fn from_raw(raw: c_int) -> Result<Self> {
        use sys::{
            GhosttyRenderStateKittyPlacementKind_GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_FIXED as FIXED,
            GhosttyRenderStateKittyPlacementKind_GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_RELATIVE as RELATIVE,
            GhosttyRenderStateKittyPlacementKind_GHOSTTY_RENDER_STATE_KITTY_PLACEMENT_KIND_VIRTUAL as VIRTUAL,
        };
        match raw as sys::GhosttyRenderStateKittyPlacementKind {
            VIRTUAL => Ok(Self::Virtual),
            RELATIVE => Ok(Self::Relative),
            FIXED => Ok(Self::Fixed),
            _ => Err(Error::InvalidValue),
        }
    }
}

/// A rectangle in image pixels.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PixelRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One image placement to draw, from [`crate::RenderState::image_placements`].
///
/// Pixel values are in the terminal's cell pixels (the last
/// [`Terminal::resize`]'s). The image's top-left lands at
/// `(col * cell_width + offset_x, row * cell_height + offset_y)` in the
/// viewport, `pixel_width` × `pixel_height` large, showing `source` of
/// the image scaled to fit. Placements are listed at full size whenever
/// any of their cells is in view, so `col` and `row` can be negative and
/// the painter clips to the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImagePlacement {
    pub image_id: u32,
    /// The image's pixel generation: never 0, never reused in a process,
    /// changes whenever the pixels do. A texture cache key with
    /// `image_id`.
    pub generation: u64,
    pub layer: ImageLayer,
    /// Orders placements within a layer, lowest first.
    pub z: i32,
    pub col: i32,
    pub row: i32,
    pub grid_cols: u32,
    pub grid_rows: u32,
    pub offset_x: u32,
    pub offset_y: u32,
    pub pixel_width: u32,
    pub pixel_height: u32,
    pub source: PixelRect,
    /// The program's placement id, or 0 when it gave none.
    pub placement_id: u32,
    pub kind: PlacementKind,
}

/// Sort into draw order: Ghostty's `(layer, z, image_id)`, then a
/// tiebreak over every other field, so two terminals holding the same
/// placements (a session and its mirror) list them identically whatever
/// order libghostty iterated them in.
pub(crate) fn sort_placements(placements: &mut [ImagePlacement]) {
    placements.sort_unstable_by_key(|p| {
        (
            p.layer,
            p.z,
            p.image_id,
            p.placement_id,
            p.row,
            p.col,
            p.source,
            (
                p.kind,
                p.grid_cols,
                p.grid_rows,
                p.offset_x,
                p.offset_y,
                p.pixel_width,
                p.pixel_height,
                p.generation,
            ),
        )
    });
}

/// How a stored image's pixels are laid out. Rows are tight (no stride).
/// PNG never appears: libghostty decodes it to RGBA before storing it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageFormat {
    Rgb,
    Rgba,
    Gray,
    GrayAlpha,
}

impl ImageFormat {
    fn from_raw(raw: c_int) -> Result<Self> {
        use sys::{
            GhosttyKittyImageFormat_GHOSTTY_KITTY_IMAGE_FORMAT_GRAY as GRAY,
            GhosttyKittyImageFormat_GHOSTTY_KITTY_IMAGE_FORMAT_GRAY_ALPHA as GRAY_ALPHA,
            GhosttyKittyImageFormat_GHOSTTY_KITTY_IMAGE_FORMAT_RGB as RGB,
            GhosttyKittyImageFormat_GHOSTTY_KITTY_IMAGE_FORMAT_RGBA as RGBA,
        };
        match raw as sys::GhosttyKittyImageFormat {
            RGB => Ok(Self::Rgb),
            RGBA => Ok(Self::Rgba),
            GRAY => Ok(Self::Gray),
            GRAY_ALPHA => Ok(Self::GrayAlpha),
            _ => Err(Error::InvalidValue),
        }
    }

    pub fn bytes_per_pixel(self) -> usize {
        match self {
            Self::Rgb => 3,
            Self::Rgba => 4,
            Self::Gray => 1,
            Self::GrayAlpha => 2,
        }
    }
}

/// A stored Kitty image's current pixels, borrowed from the terminal.
///
/// # Lifetime
///
/// The pixels belong to libghostty's image storage, which frees or
/// rewrites them on the next call that changes the terminal. The view
/// borrows the [`Terminal`], and while it is alive no safe operation can
/// invalidate them: every call that can change the storage takes
/// `&mut Terminal`. The two `&Terminal` calls into libghostty that do
/// change state, [`crate::RenderState::update`] and [`Terminal::track`],
/// never touch the storage (`update` only reads it; `track` allocates a
/// pin). [`Terminal::as_ffi`] callers must uphold this themselves. The
/// view is `!Send`, like the `&Terminal` it stands for.
///
/// Feeding the terminal while a view is alive does not compile:
///
/// ```compile_fail,E0502
/// let mut terminal = roost_vt::Terminal::new(Default::default()).unwrap();
/// let image = terminal.kitty_image(1).unwrap();
/// terminal.vt_write(b"\x1b_Ga=d,d=I,i=1\x1b\\");
/// drop(image);
/// ```
///
/// Nor does keeping one past the terminal:
///
/// ```compile_fail,E0505
/// let terminal = roost_vt::Terminal::new(Default::default()).unwrap();
/// let image = terminal.kitty_image(1).unwrap();
/// drop(terminal);
/// drop(image);
/// ```
#[derive(Debug, Clone, Copy)]
pub struct KittyImage<'a> {
    id: u32,
    width: u32,
    height: u32,
    format: ImageFormat,
    generation: u64,
    data: &'a [u8],
    _not_send: PhantomData<&'a Terminal>,
}

impl<'a> KittyImage<'a> {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    pub fn format(&self) -> ImageFormat {
        self.format
    }

    /// Same value as [`ImagePlacement::generation`] for this image.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// `width * height * format.bytes_per_pixel()` bytes, rows tight;
    /// [`Terminal::kitty_image`] checked that before handing it out,
    /// which is why the fields are private.
    pub fn data(&self) -> &'a [u8] {
        self.data
    }

    /// The whole image as straight (non-premultiplied) RGBA8,
    /// `width * height * 4` bytes.
    pub fn to_rgba(&self) -> Vec<u8> {
        rgba_from(self.format, self.data)
    }
}

fn rgba_from(format: ImageFormat, data: &[u8]) -> Vec<u8> {
    let bpp = format.bytes_per_pixel();
    match format {
        ImageFormat::Rgba => data.to_vec(),
        ImageFormat::Rgb => widen(data, bpp, |s| [s[0], s[1], s[2], 0xff]),
        ImageFormat::Gray => widen(data, bpp, |s| [s[0], s[0], s[0], 0xff]),
        ImageFormat::GrayAlpha => widen(data, bpp, |s| [s[0], s[0], s[0], s[1]]),
    }
}

fn widen(data: &[u8], bpp: usize, pixel: impl Fn(&[u8]) -> [u8; 4]) -> Vec<u8> {
    let mut out = vec![0; data.len() / bpp * 4];
    for (dst, src) in out.chunks_exact_mut(4).zip(data.chunks_exact(bpp)) {
        dst.copy_from_slice(&pixel(src));
    }
    out
}

impl Terminal {
    /// The active screen's image `id`, as it is now.
    ///
    /// `Ok(None)` when no such image is stored, its pixels are still on
    /// the way (a snapshot's image whose payload hasn't arrived), or it is
    /// empty. An `Err` means libghostty reported something inconsistent.
    pub fn kitty_image(&self, id: u32) -> Result<Option<KittyImage<'_>>> {
        let mut graphics: sys::GhosttyKittyGraphics = ptr::null_mut();
        // SAFETY: handle non-null; `graphics` is a real local of the
        // KITTY_GRAPHICS out type.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle(),
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_KITTY_GRAPHICS,
                (&raw mut graphics).cast(),
            )
        };
        match Error::from_result(rc) {
            Ok(()) => {}
            Err(Error::NoValue) => return Ok(None),
            Err(err) => return Err(err),
        }
        if graphics.is_null() {
            return Err(Error::NullHandle);
        }
        // SAFETY: `graphics` is the live storage handle just read.
        let image = unsafe { sys::ghostty_kitty_graphics_image(graphics, id) };
        if image.is_null() {
            return Ok(None);
        }

        let mut width: u32 = 0;
        let mut height: u32 = 0;
        let mut format: c_int = 0;
        let mut generation: u64 = 0;
        let mut len: usize = 0;
        let fields: [(sys::GhosttyKittyGraphicsImageData, *mut c_void); 5] = [
            (
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_WIDTH,
                (&raw mut width).cast(),
            ),
            (
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_HEIGHT,
                (&raw mut height).cast(),
            ),
            (
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_FORMAT,
                (&raw mut format).cast(),
            ),
            (
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_GENERATION,
                (&raw mut generation).cast(),
            ),
            (
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_DATA_LEN,
                (&raw mut len).cast(),
            ),
        ];
        let keys = fields.map(|(key, _)| key);
        let mut values = fields.map(|(_, value)| value);
        let mut written: usize = 0;
        // SAFETY: `image` is non-null and valid until the terminal
        // mutates, which `&self` rules out for this call; each value
        // points at a live local of its key's documented type.
        let rc = unsafe {
            sys::ghostty_kitty_graphics_image_get_multi(
                image,
                keys.len(),
                keys.as_ptr(),
                values.as_mut_ptr(),
                &mut written,
            )
        };
        Error::from_result(rc)?;
        if written != keys.len() {
            return Err(Error::InvalidValue);
        }

        let mut data: *const u8 = ptr::null();
        // SAFETY: as above; `data` is a local of DATA_PTR's out type.
        let rc = unsafe {
            sys::ghostty_kitty_graphics_image_get(
                image,
                sys::GhosttyKittyGraphicsImageData_GHOSTTY_KITTY_IMAGE_DATA_DATA_PTR,
                (&raw mut data).cast(),
            )
        };
        match Error::from_result(rc) {
            Ok(()) => {}
            Err(Error::NoValue) => return Ok(None),
            Err(err) => return Err(err),
        }

        let format = ImageFormat::from_raw(format)?;
        if len == 0 {
            return Ok(None);
        }
        if data.is_null() {
            return Err(Error::NullHandle);
        }
        let pixels = (width as usize)
            .checked_mul(height as usize)
            .ok_or(Error::InvalidValue)?;
        let expected = pixels
            .checked_mul(format.bytes_per_pixel())
            .ok_or(Error::InvalidValue)?;
        let rgba_len = pixels.checked_mul(4).ok_or(Error::InvalidValue)?;
        if len != expected || len > isize::MAX as usize || rgba_len > isize::MAX as usize {
            return Err(Error::InvalidValue);
        }
        // SAFETY: libghostty lends `len` initialized bytes at the non-null
        // `data` until the terminal next mutates; the returned borrow of
        // `self` keeps every mutator (`&mut self`) out until it ends (see
        // the type's Lifetime section). `len <= isize::MAX` was checked.
        let data = unsafe { std::slice::from_raw_parts(data, len) };
        Ok(Some(KittyImage {
            id,
            width,
            height,
            format,
            generation,
            data,
            _not_send: PhantomData,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placements_equal_on_layer_z_and_id_sort_on_the_tiebreak() {
        let base = ImagePlacement {
            image_id: 1,
            generation: 1,
            layer: ImageLayer::BelowText,
            z: -1,
            col: 0,
            row: 0,
            grid_cols: 1,
            grid_rows: 1,
            offset_x: 0,
            offset_y: 0,
            pixel_width: 8,
            pixel_height: 16,
            source: PixelRect::default(),
            placement_id: 0,
            kind: PlacementKind::Virtual,
        };
        let sorted = [
            base,
            ImagePlacement { col: 3, ..base },
            ImagePlacement { row: 1, ..base },
            ImagePlacement {
                placement_id: 2,
                ..base
            },
            ImagePlacement {
                image_id: 2,
                ..base
            },
        ];
        let mut placements = sorted;
        placements.reverse();
        sort_placements(&mut placements);
        assert_eq!(placements, sorted);
    }

    #[test]
    fn every_format_converts_to_straight_rgba() {
        let cases: [(ImageFormat, &[u8], &[u8]); 4] = [
            (
                ImageFormat::Rgba,
                &[1, 2, 3, 4, 5, 6, 7, 8],
                &[1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (
                ImageFormat::Rgb,
                &[1, 2, 3, 4, 5, 6],
                &[1, 2, 3, 0xff, 4, 5, 6, 0xff],
            ),
            (
                ImageFormat::Gray,
                &[9, 200],
                &[9, 9, 9, 0xff, 200, 200, 200, 0xff],
            ),
            (
                ImageFormat::GrayAlpha,
                &[9, 10, 200, 0],
                &[9, 9, 9, 10, 200, 200, 200, 0],
            ),
        ];
        for (format, data, rgba) in cases {
            assert_eq!(rgba_from(format, data), rgba, "{format:?}");
        }
    }

    #[test]
    fn raw_codes_map_to_the_header_values() {
        assert_eq!(
            ImageLayer::from_raw(0).unwrap(),
            ImageLayer::BelowBackground
        );
        assert_eq!(ImageLayer::from_raw(1).unwrap(), ImageLayer::BelowText);
        assert_eq!(ImageLayer::from_raw(2).unwrap(), ImageLayer::AboveText);
        assert!(matches!(ImageLayer::from_raw(3), Err(Error::InvalidValue)));
        assert_eq!(PlacementKind::from_raw(0).unwrap(), PlacementKind::Virtual);
        assert_eq!(PlacementKind::from_raw(1).unwrap(), PlacementKind::Relative);
        assert_eq!(PlacementKind::from_raw(2).unwrap(), PlacementKind::Fixed);
        assert!(matches!(
            PlacementKind::from_raw(3),
            Err(Error::InvalidValue)
        ));
        assert_eq!(ImageFormat::from_raw(0).unwrap(), ImageFormat::Rgb);
        assert_eq!(ImageFormat::from_raw(1).unwrap(), ImageFormat::Rgba);
        assert!(matches!(ImageFormat::from_raw(2), Err(Error::InvalidValue)));
        assert_eq!(ImageFormat::from_raw(3).unwrap(), ImageFormat::GrayAlpha);
        assert_eq!(ImageFormat::from_raw(4).unwrap(), ImageFormat::Gray);
    }
}
