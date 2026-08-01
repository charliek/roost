//! Process-lifetime bridge from Iced's system font database to owned UI rows.

use iced::advanced::graphics::text::font_system;
use iced::Font;
use roost_ui_model::typography;
use std::collections::HashMap;
use std::sync::OnceLock;

#[derive(Debug)]
struct InstalledFamily {
    name: String,
    monospace: bool,
}

/// One resolved family token suitable for Iced paragraph construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedFont {
    pub name: &'static str,
    pub font: Font,
}

/// Immutable renderer metadata shared by every App constructed in this process.
#[derive(Debug)]
pub struct FontRegistry {
    installed: Vec<InstalledFamily>,
    installed_names: Vec<String>,
    by_name: HashMap<String, usize>,
    picker_names: Vec<String>,
}

impl FontRegistry {
    fn from_facts(facts: impl IntoIterator<Item = (String, bool)>) -> Self {
        let mut facts = facts
            .into_iter()
            .filter(|(name, _)| typography::font_family_name_is_safe(name))
            .collect::<Vec<_>>();
        facts.sort_by(|(left, _), (right, _)| {
            left.to_lowercase()
                .cmp(&right.to_lowercase())
                .then_with(|| left.cmp(right))
        });

        let mut installed: Vec<InstalledFamily> = Vec::new();
        let mut by_name: HashMap<String, usize> = HashMap::new();
        for (name, monospace) in facts {
            let key = name.to_lowercase();
            if let Some(index) = by_name.get(&key).copied() {
                installed[index].monospace |= monospace;
                continue;
            }
            let index = installed.len();
            installed.push(InstalledFamily { name, monospace });
            by_name.insert(key, index);
        }
        let picker_names = typography::ordered_monospace_families(
            installed
                .iter()
                .map(|family| (family.name.clone(), family.monospace)),
        );
        let installed_names = installed.iter().map(|family| family.name.clone()).collect();
        Self {
            installed,
            installed_names,
            by_name,
            picker_names,
        }
    }

    /// System monospace rows in shared curated order.
    pub fn picker_names(&self) -> &[String] {
        &self.picker_names
    }

    fn resolve_name(&self, chain: &str) -> (&str, Option<usize>) {
        let canonical = typography::resolve_family_name(chain, &self.installed_names);
        if canonical == "Monospace" {
            return ("Monospace", None);
        }
        let index = self
            .by_name
            .get(&canonical.to_lowercase())
            .copied()
            .expect("shared resolver returns only an installed canonical name");
        (&self.installed[index].name, Some(index))
    }

    /// Resolve a configured fallback chain against the renderer's own database.
    pub fn resolve(&'static self, chain: &str) -> ResolvedFont {
        let (name, index) = self.resolve_name(chain);
        let font = index
            .map(|_| Font::with_name(name))
            .unwrap_or(Font::MONOSPACE);
        ResolvedFont { name, font }
    }
}

/// The renderer and picker share exactly one system-font scan and name arena.
pub fn system_font_registry() -> &'static FontRegistry {
    static REGISTRY: OnceLock<FontRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let facts = {
            let mut system = font_system()
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            system
                .raw()
                .db()
                .faces()
                .flat_map(|face| {
                    face.families
                        .iter()
                        .map(move |(name, _)| (name.clone(), face.monospaced))
                })
                .collect::<Vec<_>>()
        };
        FontRegistry::from_facts(facts)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_deduplicates_canonical_names_and_filters_picker_rows() {
        let registry = FontRegistry::from_facts([
            ("fira code".to_string(), true),
            ("Fira Code".to_string(), false),
            ("Arial".to_string(), false),
            ("Alpha Mono".to_string(), true),
            ("bad,name".to_string(), true),
        ]);
        assert_eq!(registry.installed.len(), 3);
        assert_eq!(
            registry.picker_names(),
            ["Fira Code", "Alpha Mono", "Monospace"]
        );
        assert_eq!(registry.resolve_name("Missing, FIRA CODE").0, "Fira Code");
        assert_eq!(registry.resolve_name("Arial").0, "Arial");
        assert_eq!(registry.resolve_name("Missing"), ("Monospace", None));
        for chain in [
            "Missing, FIRA CODE",
            "Arial",
            "Missing, Monospace, Arial",
            "Missing",
        ] {
            assert_eq!(
                registry.resolve_name(chain).0,
                typography::resolve_family_name(chain, &registry.installed_names),
                "Iced must map the shared chain result for {chain:?}"
            );
        }
    }

    #[test]
    fn process_registry_is_stable_and_bounded_across_acquisition() {
        let first = system_font_registry();
        let installed = first.installed.len();
        let picker = first.picker_names.len();
        let second = system_font_registry();
        assert!(std::ptr::eq(first, second));
        assert_eq!(second.installed.len(), installed);
        assert_eq!(second.picker_names.len(), picker);
        assert!(!second.picker_names.is_empty());
    }
}
