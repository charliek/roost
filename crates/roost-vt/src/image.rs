//! The Kitty graphics policy every Roost terminal runs under, and the PNG
//! decoder libghostty calls for `f=100` images.
//!
//! Images reach every client as the raw APC bytes the session parsed, and
//! each copy of the terminal parses them again. The session and every
//! client must therefore agree on the decoder, the storage limit and the
//! media, or their image stores, and with them the cursor a placement
//! moves, diverge.

use std::ffi::c_void;
use std::io::Cursor;
use std::mem::ManuallyDrop;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr::{self, NonNull};
use std::sync::OnceLock;

use crate::sys;
use crate::{Error, Result};

/// Bytes of Kitty image data one screen may hold, about four 1920×1080
/// RGBA frames. Also the most one decoded PNG may take.
pub const KITTY_IMAGE_STORAGE_LIMIT: u64 = 32 * 1024 * 1024;

const MAX_DECODED_BYTES: usize = KITTY_IMAGE_STORAGE_LIMIT as usize;

/// Install the PNG decoder for every terminal in this process. A
/// process-global libghostty setting, so call it before the first
/// terminal is created; later calls return the first one's result.
pub fn install_image_decoder() -> Result<()> {
    static INSTALLED: OnceLock<i32> = OnceLock::new();
    let decode: sys::GhosttySysDecodePngFn = Some(decode_png);
    let rc = *INSTALLED.get_or_init(|| {
        let value = decode.map_or(ptr::null(), |decode| decode as *const c_void);
        // SAFETY: `DECODE_PNG` takes the function pointer itself, and
        // `decode_png` is a `'static` fn of the type it documents.
        unsafe { sys::ghostty_sys_set(sys::GhosttySysOption_GHOSTTY_SYS_OPT_DECODE_PNG, value) }
    });
    Error::from_result(rc)
}

/// libghostty's `GhosttySysDecodePngFn`: RGBA8, top-left first, in a
/// buffer from `allocator` of exactly `width * height * 4` bytes.
///
/// Every failure is an ordinary `false`. A panic would abort both
/// binaries (their hook aborts before unwinding); `catch_unwind` is only
/// what keeps an unwind from crossing FFI in a process without that hook.
/// It touches nothing but its locals and the bytes it is lent, which is
/// what makes concurrent calls from many tab tasks safe.
unsafe extern "C" fn decode_png(
    _userdata: *mut c_void,
    allocator: *const sys::GhosttyAllocator,
    data: *const u8,
    data_len: usize,
    out: *mut sys::GhosttySysImage,
) -> bool {
    if data.is_null() || out.is_null() {
        return false;
    }
    // SAFETY: libghostty lends `data_len` initialized bytes at `data` for
    // the duration of this call.
    let png = unsafe { std::slice::from_raw_parts(data, data_len) };
    let Ok(Some(image)) = catch_unwind(AssertUnwindSafe(|| decode(png, allocator))) else {
        return false;
    };
    let (width, height) = (image.width, image.height);
    let (data, data_len) = image.pixels.hand_off();
    // SAFETY: `out` is libghostty's out-parameter, valid for this call.
    unsafe {
        out.write(sys::GhosttySysImage {
            width,
            height,
            data,
            data_len,
        });
    }
    true
}

struct Decoded {
    width: u32,
    height: u32,
    pixels: Pixels,
}

fn decode(png: &[u8], allocator: *const sys::GhosttyAllocator) -> Option<Decoded> {
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(png),
        png::Limits {
            bytes: MAX_DECODED_BYTES,
        },
    );
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    // `png::Limits` charge text chunks only by payload, not by the records
    // `read_info` keeps for them, and `read_info` sizes the raw frame with
    // unchecked arithmetic. So metadata is skipped, and the dimensions are
    // refused from IHDR alone, before `read_info` and before any buffer.
    decoder.set_ignore_text_chunk(true);
    decoder.set_ignore_iccp_chunk(true);
    let header = decoder.read_header_info().ok()?;
    let (width, height) = (header.width, header.height);
    let len = rgba_len(width, height)?;
    let mut reader = decoder.read_info().ok()?;
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight {
        return None;
    }
    let pixels = match color {
        png::ColorType::Rgba => {
            if reader.output_buffer_size()? != len {
                return None;
            }
            let mut pixels = Pixels::alloc(allocator, len)?;
            let frame = reader.next_frame(pixels.bytes_mut()).ok()?;
            whole(&frame, width, height, len)?;
            pixels
        }
        png::ColorType::GrayscaleAlpha => {
            let gray_len = reader.output_buffer_size()?;
            if gray_len.checked_mul(2)? != len {
                return None;
            }
            let mut gray = Vec::new();
            gray.try_reserve_exact(gray_len).ok()?;
            gray.resize(gray_len, 0);
            let frame = reader.next_frame(&mut gray).ok()?;
            whole(&frame, width, height, gray_len)?;
            let mut pixels = Pixels::alloc(allocator, len)?;
            for (rgba, ga) in pixels
                .bytes_mut()
                .chunks_exact_mut(4)
                .zip(gray.chunks_exact(2))
            {
                if let &[g, a] = ga {
                    rgba.copy_from_slice(&[g, g, g, a]);
                }
            }
            pixels
        }
        _ => return None,
    };
    Some(Decoded {
        width,
        height,
        pixels,
    })
}

/// `width * height * 4`, if that is a non-empty image within the cap.
fn rgba_len(width: u32, height: u32) -> Option<usize> {
    let len = u64::from(width)
        .checked_mul(u64::from(height))?
        .checked_mul(4)?;
    if len == 0 || len > KITTY_IMAGE_STORAGE_LIMIT {
        return None;
    }
    usize::try_from(len).ok()
}

/// The frame filled the whole buffer: an APNG's first frame need not.
fn whole(frame: &png::OutputInfo, width: u32, height: u32, len: usize) -> Option<()> {
    (frame.width == width && frame.height == height && frame.buffer_size() == len).then_some(())
}

/// A `ghostty_alloc` buffer, freed with `ghostty_free` unless handed off.
struct Pixels {
    allocator: *const sys::GhosttyAllocator,
    ptr: NonNull<u8>,
    len: usize,
}

impl Pixels {
    fn alloc(allocator: *const sys::GhosttyAllocator, len: usize) -> Option<Self> {
        // SAFETY: `allocator` is the one libghostty passed this call.
        let ptr = NonNull::new(unsafe { sys::ghostty_alloc(allocator, len) })?;
        // Zeroed so it can be lent out as `&mut [u8]`: `ghostty_alloc`
        // memory is uninitialized.
        // SAFETY: `ptr` is a fresh allocation of `len` bytes.
        unsafe { ptr::write_bytes(ptr.as_ptr(), 0, len) };
        Some(Self {
            allocator,
            ptr,
            len,
        })
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `len` initialized bytes owned by `self`.
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }

    /// Give the buffer to libghostty, which frees it from here on.
    fn hand_off(self) -> (*mut u8, usize) {
        let this = ManuallyDrop::new(self);
        (this.ptr.as_ptr(), this.len)
    }
}

impl Drop for Pixels {
    fn drop(&mut self) {
        // SAFETY: allocated from `allocator` with exactly `len` bytes and
        // not handed off.
        unsafe { sys::ghostty_free(self.allocator, self.ptr.as_ptr(), self.len) };
    }
}

#[cfg(test)]
mod tests {
    use std::alloc::Layout;
    use std::cell::Cell;

    use super::*;

    /// Live allocations, counted through the vtable libghostty's own
    /// `ghostty_alloc` / `ghostty_free` call back into.
    #[derive(Default)]
    struct Counts {
        refuse: bool,
        allocs: Cell<usize>,
        frees: Cell<usize>,
        live_bytes: Cell<usize>,
    }

    unsafe extern "C" fn count_alloc(
        ctx: *mut c_void,
        len: usize,
        alignment: u8,
        _ret_addr: usize,
    ) -> *mut c_void {
        // SAFETY: `ctx` is the test's `Counts`, alive for the call.
        let counts = unsafe { &*ctx.cast::<Counts>() };
        if counts.refuse {
            return ptr::null_mut();
        }
        let Ok(layout) = Layout::from_size_align(len, 1 << alignment) else {
            return ptr::null_mut();
        };
        // SAFETY: `len` is non-zero (`ghostty_alloc` refuses 0 itself).
        let memory = unsafe { std::alloc::alloc(layout) };
        if !memory.is_null() {
            counts.allocs.set(counts.allocs.get() + 1);
            counts.live_bytes.set(counts.live_bytes.get() + len);
        }
        memory.cast()
    }

    unsafe extern "C" fn count_free(
        ctx: *mut c_void,
        memory: *mut c_void,
        memory_len: usize,
        alignment: u8,
        _ret_addr: usize,
    ) {
        // SAFETY: as in `count_alloc`.
        let counts = unsafe { &*ctx.cast::<Counts>() };
        counts.frees.set(counts.frees.get() + 1);
        counts
            .live_bytes
            .set(counts.live_bytes.get().wrapping_sub(memory_len));
        let layout = Layout::from_size_align(memory_len, 1 << alignment).expect("alloc's layout");
        // SAFETY: allocated by `count_alloc` with this layout.
        unsafe { std::alloc::dealloc(memory.cast(), layout) };
    }

    unsafe extern "C" fn no_resize(
        _ctx: *mut c_void,
        _memory: *mut c_void,
        _memory_len: usize,
        _alignment: u8,
        _new_len: usize,
        _ret_addr: usize,
    ) -> bool {
        false
    }

    unsafe extern "C" fn no_remap(
        _ctx: *mut c_void,
        _memory: *mut c_void,
        _memory_len: usize,
        _alignment: u8,
        _new_len: usize,
        _ret_addr: usize,
    ) -> *mut c_void {
        ptr::null_mut()
    }

    static VTABLE: sys::GhosttyAllocatorVtable = sys::GhosttyAllocatorVtable {
        alloc: Some(count_alloc),
        resize: Some(no_resize),
        remap: Some(no_remap),
        free: Some(count_free),
    };

    /// What one call left behind: its verdict, the image it handed off
    /// (copied out, then freed through the same allocator), and the counts.
    struct Call {
        ok: bool,
        image: Option<(u32, u32, Vec<u8>)>,
        counts: Counts,
    }

    fn call(png: &[u8], refuse: bool) -> Call {
        let counts = Counts {
            refuse,
            ..Counts::default()
        };
        let allocator = sys::GhosttyAllocator {
            ctx: (&raw const counts).cast_mut().cast(),
            vtable: &raw const VTABLE,
        };
        let mut out = sys::GhosttySysImage::default();
        // SAFETY: the callback's contract: valid bytes, a live allocator
        // and a zeroed out-parameter.
        let ok = unsafe {
            decode_png(
                ptr::null_mut(),
                &allocator,
                png.as_ptr(),
                png.len(),
                &mut out,
            )
        };
        let image = (ok && !out.data.is_null()).then(|| {
            // SAFETY: on `true` `out` describes `data_len` bytes from `allocator`.
            let bytes = unsafe { std::slice::from_raw_parts(out.data, out.data_len) }.to_vec();
            // SAFETY: what libghostty does with a handed-off buffer.
            unsafe { sys::ghostty_free(&allocator, out.data, out.data_len) };
            (out.width, out.height, bytes)
        });
        Call { ok, image, counts }
    }

    fn assert_balanced(call: &Call) {
        assert_eq!(
            call.counts.allocs.get(),
            call.counts.frees.get(),
            "every allocation freed or handed off"
        );
        assert_eq!(call.counts.live_bytes.get(), 0, "no bytes left live");
    }

    fn encode(
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

    fn rgb_2x1() -> Vec<u8> {
        encode(
            2,
            1,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            None,
            &[255, 0, 0, 0, 0, 255],
        )
    }

    /// A PNG whose header declares `width`x`height` RGBA and whose data
    /// is one empty zlib stream: valid through the header, never decodable.
    fn header_only(width: u32, height: u32) -> Vec<u8> {
        header_only_at(width, height, png::BitDepth::Eight)
    }

    fn header_only_at(width: u32, height: u32, depth: png::BitDepth) -> Vec<u8> {
        let mut bytes = Vec::new();
        let mut encoder = png::Encoder::new(&mut bytes, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(depth);
        let mut writer = encoder.write_header().expect("header");
        writer
            .write_chunk(
                png::chunk::IDAT,
                &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
            )
            .expect("IDAT");
        drop(writer);
        bytes
    }

    /// 32x32 of varied RGB: enough compressed data that a cut through
    /// the middle lands in the pixels, after the header.
    fn rgb_32x32() -> Vec<u8> {
        let pixels: Vec<u8> = (0..32u32 * 32 * 3).map(|i| (i * 37 % 251) as u8).collect();
        encode(
            32,
            32,
            png::ColorType::Rgb,
            png::BitDepth::Eight,
            None,
            &pixels,
        )
    }

    #[test]
    fn rgb_decodes_to_opaque_rgba_and_hands_its_buffer_off() {
        let call = call(&rgb_2x1(), false);
        assert!(call.ok);
        assert_eq!(
            call.image,
            Some((2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]))
        );
        assert_eq!(call.counts.allocs.get(), 1, "exactly the w*h*4 buffer");
        assert_balanced(&call);
    }

    #[test]
    fn a_malformed_png_is_refused_and_allocates_nothing() {
        let call = call(b"\x89PNG\r\n\x1a\nnot really a png", false);
        assert!(!call.ok);
        assert_eq!(call.counts.allocs.get(), 0);
        assert_balanced(&call);
    }

    #[test]
    fn a_truncated_stream_is_refused_and_frees_its_buffer() {
        let png = rgb_32x32();
        let call = call(&png[..png.len() / 2], false);
        assert!(!call.ok);
        assert!(
            call.counts.allocs.get() > 0,
            "the cut falls in the pixel data, after the buffer exists"
        );
        assert_balanced(&call);
    }

    #[test]
    fn an_allocator_that_returns_null_is_a_refusal() {
        let call = call(&rgb_2x1(), true);
        assert!(!call.ok);
        assert_eq!(call.counts.allocs.get(), 0);
        assert_balanced(&call);
    }

    #[test]
    fn an_image_over_the_cap_is_refused_before_any_allocation() {
        // 4096 x 2049 x 4 is just over 32 MiB.
        let call = call(&header_only(4096, 2049), false);
        assert!(!call.ok);
        assert_eq!(
            call.counts.allocs.get(),
            0,
            "refused from the header, before the output buffer"
        );
        assert_balanced(&call);
    }

    #[test]
    fn a_header_whose_raw_frame_size_overflows_is_refused_without_a_panic() {
        // RGBA16 at these dimensions passes png's transformed-size check, but
        // its raw frame size, (8w + 1) * h, overflows u64 inside `read_info`.
        // `decode` is called outside the callback's `catch_unwind`, so a
        // panic fails this test instead of turning into `false`.
        let png = header_only_at(1_073_741_825, 2_147_483_646, png::BitDepth::Sixteen);
        let counts = Counts::default();
        let allocator = sys::GhosttyAllocator {
            ctx: (&raw const counts).cast_mut().cast(),
            vtable: &raw const VTABLE,
        };
        assert!(decode(&png, &raw const allocator).is_none());
        assert_eq!(counts.allocs.get(), 0);
    }

    #[test]
    fn the_size_check_takes_the_cap_and_no_more() {
        assert_eq!(rgba_len(4096, 2048), Some(MAX_DECODED_BYTES));
        assert_eq!(rgba_len(4096, 2049), None);
        assert_eq!(rgba_len(0, 10), None);
        assert_eq!(rgba_len(u32::MAX, u32::MAX), None);
    }

    #[test]
    fn grayscale_widens_to_rgba() {
        let png = encode(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::Eight,
            None,
            &[0x10, 0xF0],
        );
        let call = call(&png, false);
        assert_eq!(
            call.image,
            Some((2, 1, vec![0x10, 0x10, 0x10, 0xFF, 0xF0, 0xF0, 0xF0, 0xFF]))
        );
        assert_balanced(&call);
    }

    #[test]
    fn grayscale_with_alpha_widens_to_rgba() {
        let png = encode(
            2,
            1,
            png::ColorType::GrayscaleAlpha,
            png::BitDepth::Eight,
            None,
            &[0x10, 0x80, 0xF0, 0x00],
        );
        let call = call(&png, false);
        assert_eq!(
            call.image,
            Some((2, 1, vec![0x10, 0x10, 0x10, 0x80, 0xF0, 0xF0, 0xF0, 0x00]))
        );
        assert_balanced(&call);
    }

    #[test]
    fn low_bit_depth_grayscale_expands_to_eight_bits() {
        // Two 1-bit pixels, black then white, packed into one byte.
        let png = encode(
            2,
            1,
            png::ColorType::Grayscale,
            png::BitDepth::One,
            None,
            &[0b0100_0000],
        );
        let call = call(&png, false);
        assert_eq!(
            call.image,
            Some((2, 1, vec![0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]))
        );
        assert_balanced(&call);
    }

    #[test]
    fn a_palette_with_trns_takes_its_alpha() {
        let png = encode(
            2,
            1,
            png::ColorType::Indexed,
            png::BitDepth::Eight,
            Some((&[255, 0, 0, 0, 0, 255], &[0x40])),
            &[0, 1],
        );
        let call = call(&png, false);
        assert_eq!(
            call.image,
            Some((2, 1, vec![255, 0, 0, 0x40, 0, 0, 255, 0xFF]))
        );
        assert_balanced(&call);
    }

    #[test]
    fn sixteen_bit_rgba_strips_to_eight() {
        let png = encode(
            1,
            1,
            png::ColorType::Rgba,
            png::BitDepth::Sixteen,
            None,
            &[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0],
        );
        let call = call(&png, false);
        assert_eq!(call.image, Some((1, 1, vec![0x12, 0x56, 0x9A, 0xDE])));
        assert_balanced(&call);
    }
}
