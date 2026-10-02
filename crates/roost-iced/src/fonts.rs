//! The terminal font Roost ships, so the default never depends on what is installed.

use iced::advanced::graphics::text::font_system;
use std::borrow::Cow;
use std::sync::{Once, PoisonError};

/// `DEFAULT_FONT_FAMILY`'s primary, and the family the generic `Monospace` maps to.
const BUNDLED_TERMINAL_FAMILY: &str = "JetBrains Mono";

static BUNDLED_TERMINAL_FACES: [&[u8]; 4] = [
    include_bytes!("../../../third_party/jetbrains-mono/JetBrainsMono-Regular.ttf"),
    include_bytes!("../../../third_party/jetbrains-mono/JetBrainsMono-Bold.ttf"),
    include_bytes!("../../../third_party/jetbrains-mono/JetBrainsMono-Italic.ttf"),
    include_bytes!("../../../third_party/jetbrains-mono/JetBrainsMono-BoldItalic.ttf"),
];

/// Loads the bundled JetBrains Mono faces into iced's font database and
/// makes them the generic monospace family.
///
/// Must run before `system_font_registry()` first scans that database: the
/// scan happens once per process, inside `App::bootstrap`, which also
/// measures the terminal cell. Bytes handed to iced's `.font()` load only
/// when iced builds its first compositor, after both.
pub fn install_bundled_terminal_fonts() {
    static INSTALLED: Once = Once::new();
    INSTALLED.call_once(|| {
        let mut system = font_system()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for face in BUNDLED_TERMINAL_FACES {
            system.load_font(Cow::Borrowed(face));
        }
        system
            .raw()
            .db_mut()
            .set_monospace_family(BUNDLED_TERMINAL_FAMILY);
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::font_registry::FontRegistry;
    use iced::advanced::graphics::text::cosmic_text::fontdb::{Source, Style};
    use iced::advanced::graphics::text::Paragraph;
    use iced::advanced::text::{self, Paragraph as _};
    use iced::{alignment, Font, Pixels, Size};
    use roost_ui_model::typography::DEFAULT_FONT_FAMILY;
    use std::collections::BTreeSet;

    fn shaped_face_families(font: Font) -> Vec<String> {
        let paragraph = Paragraph::with_text(text::Text {
            content: "M",
            bounds: Size::INFINITE,
            size: Pixels(13.0),
            line_height: text::LineHeight::default(),
            font,
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Auto,
            wrapping: text::Wrapping::None,
        });
        let face_id = paragraph
            .buffer()
            .layout_runs()
            .find_map(|run| run.glyphs.first().map(|glyph| glyph.font_id))
            .expect("\"M\" shapes to one glyph");
        let mut system = font_system()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let face = system
            .raw()
            .db()
            .face(face_id)
            .expect("the shaped face is in the database");
        face.families.iter().map(|(name, _)| name.clone()).collect()
    }

    #[test]
    fn bundled_faces_back_the_default_chain_and_the_generic_monospace() {
        install_bundled_terminal_fonts();
        // A fresh scan rather than `system_font_registry()`: under a shared
        // test process another test may already have run that one-shot scan.
        let registry: &'static FontRegistry = Box::leak(Box::new(FontRegistry::scan()));

        let default = registry.resolve(DEFAULT_FONT_FAMILY);
        assert_eq!(default.name, BUNDLED_TERMINAL_FAMILY);
        assert!(
            shaped_face_families(default.font)
                .iter()
                .any(|name| name == BUNDLED_TERMINAL_FAMILY),
            "the default chain shapes with a JetBrains Mono face"
        );

        let unknown = registry.resolve("Roost Missing Family");
        assert_eq!(unknown.name, "Monospace");
        assert_eq!(unknown.font, Font::MONOSPACE);
        let generic = shaped_face_families(unknown.font);
        assert!(
            generic.iter().any(|name| name == BUNDLED_TERMINAL_FAMILY),
            "an unresolved family lands on the generic, which shapes with JetBrains Mono, \
             not {generic:?}"
        );

        // A system-wide JetBrains Mono would satisfy everything above on its
        // own; the bundled faces are the ones loaded from memory.
        let mut system = font_system()
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let bundled = system
            .raw()
            .db()
            .faces()
            .filter(|face| matches!(face.source, Source::Binary(_)))
            .filter(|face| {
                face.families
                    .iter()
                    .any(|(name, _)| name == BUNDLED_TERMINAL_FAMILY)
            })
            .map(|face| (face.weight.0, face.style == Style::Italic))
            .collect::<BTreeSet<_>>();
        assert_eq!(
            bundled,
            BTreeSet::from([(400, false), (400, true), (700, false), (700, true)])
        );
    }
}
