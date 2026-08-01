//! Renderer-independent sRGB color value used by terminal configuration.

/// sRGB triple, layout-compatible with libghostty's `GhosttyColorRgb` when
/// the optional FFI feature is enabled.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ColorRgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl ColorRgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub fn to_f64(self) -> (f64, f64, f64) {
        (
            self.r as f64 / 255.0,
            self.g as f64 / 255.0,
            self.b as f64 / 255.0,
        )
    }

    pub fn relative_luminance(self) -> f64 {
        let (r, g, b) = self.to_f64();
        0.2126 * r + 0.7152 * g + 0.0722 * b
    }

    pub fn is_light(self) -> bool {
        self.relative_luminance() > 0.5
    }
}

#[cfg(test)]
mod tests {
    use super::ColorRgb;

    #[test]
    fn luminance_classifies_terminal_backgrounds() {
        assert!(!ColorRgb::new(0x1e, 0x1e, 0x1e).is_light());
        assert!(ColorRgb::new(0xff, 0xff, 0xff).is_light());
        assert!(ColorRgb::new(0x00, 0xff, 0x00).is_light());
        assert!(!ColorRgb::new(0x00, 0x00, 0xff).is_light());
    }
}
