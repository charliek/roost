//! A libghostty terminal's memory usage, in `tab.memory_usage`'s wire
//! types — shared by the session tab task and roost-iced's terminals.

use roost_ipc::messages::{MemoryUsage, ScreenMemory};

/// Read `terminal`'s memory usage as the wire reports it.
pub fn of(terminal: &roost_vt::Terminal) -> roost_vt::Result<MemoryUsage> {
    terminal.memory_usage().map(from_vt)
}

fn from_vt(usage: roost_vt::MemoryUsage) -> MemoryUsage {
    MemoryUsage::new(
        usage.compression_supported,
        screen(usage.primary),
        screen(usage.alternate),
    )
}

fn screen(screen: roost_vt::ScreenMemory) -> ScreenMemory {
    ScreenMemory {
        pages: screen.pages,
        virtual_bytes: screen.virtual_bytes,
        resident_bytes: screen.resident_bytes,
        compressed_pages: screen.compressed_pages,
        compressed_bytes: screen.compressed_bytes,
        image_bytes: screen.image_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_screen_field_lands_on_its_own_wire_field() {
        let vt = |base: u64| roost_vt::ScreenMemory {
            pages: base + 1,
            virtual_bytes: base + 2,
            resident_bytes: base + 3,
            compressed_pages: base + 4,
            compressed_bytes: base + 5,
            image_bytes: base + 6,
        };
        let wire = from_vt(roost_vt::MemoryUsage {
            compression_supported: true,
            primary: vt(10),
            alternate: vt(20),
        });
        let want = |base: u64| ScreenMemory {
            pages: base + 1,
            virtual_bytes: base + 2,
            resident_bytes: base + 3,
            compressed_pages: base + 4,
            compressed_bytes: base + 5,
            image_bytes: base + 6,
        };
        assert_eq!(wire, MemoryUsage::new(true, want(10), want(20)));
        assert_eq!(
            (wire.resident_bytes, wire.virtual_bytes),
            (13 + 23, 12 + 22)
        );
    }
}
