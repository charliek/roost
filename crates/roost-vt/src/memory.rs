//! `GHOSTTY_TERMINAL_DATA_MEMORY_USAGE`: what one terminal's screens hold.

use crate::sys;
use crate::{Error, Result, Terminal};

/// One screen's figures. `resident_bytes` is the one to budget against;
/// image bytes are not part of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenMemory {
    pub pages: u64,
    pub virtual_bytes: u64,
    pub resident_bytes: u64,
    pub compressed_pages: u64,
    pub compressed_bytes: u64,
    pub image_bytes: u64,
}

/// A terminal's memory, per screen. The alternate screen is all zero
/// until a program first switches to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryUsage {
    pub compression_supported: bool,
    pub primary: ScreenMemory,
    pub alternate: ScreenMemory,
}

impl Terminal {
    /// Read the terminal's memory usage. libghostty walks every page, so
    /// this is for on-demand reads, not for every write.
    pub fn memory_usage(&self) -> Result<MemoryUsage> {
        let mut out = sys::GhosttyTerminalMemoryUsage {
            size: std::mem::size_of::<sys::GhosttyTerminalMemoryUsage>(),
            ..Default::default()
        };
        // SAFETY: handle non-null per the constructor; `out` is a real
        // local of the sized struct this data key fills, with `size` set.
        let rc = unsafe {
            sys::ghostty_terminal_get(
                self.handle(),
                sys::GhosttyTerminalData_GHOSTTY_TERMINAL_DATA_MEMORY_USAGE,
                (&raw mut out).cast(),
            )
        };
        Error::from_result(rc)?;
        Ok(MemoryUsage {
            compression_supported: out.compression_supported,
            primary: ScreenMemory {
                pages: out.primary_pages,
                virtual_bytes: out.primary_virtual_bytes,
                resident_bytes: out.primary_resident_bytes,
                compressed_pages: out.primary_compressed_pages,
                compressed_bytes: out.primary_compressed_bytes,
                image_bytes: out.primary_image_bytes,
            },
            alternate: ScreenMemory {
                pages: out.alternate_pages,
                virtual_bytes: out.alternate_virtual_bytes,
                resident_bytes: out.alternate_resident_bytes,
                compressed_pages: out.alternate_compressed_pages,
                compressed_bytes: out.alternate_compressed_bytes,
                image_bytes: out.alternate_image_bytes,
            },
        })
    }
}
