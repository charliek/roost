//! Toolkit-neutral terminal typography state and config policy.
//!
//! Glyph discovery and measurement belong to each renderer adapter. This
//! module owns the shared Rust UI transitions so GTK and Iced do not grow
//! competing zoom, reset, preview, and confirmation state machines.

/// Preferred Rust UI terminal family chain when config does not override it.
pub const DEFAULT_FONT_FAMILY: &str = "JetBrains Mono, Monospace";
/// Default Rust UI terminal size in points.
pub const DEFAULT_FONT_SIZE_PT: f64 = 13.0;
/// Smallest size reachable through a zoom command.
pub const MIN_FONT_SIZE_PT: f64 = 6.0;
/// Largest size reachable through a zoom command.
pub const MAX_FONT_SIZE_PT: f64 = 72.0;

/// Programming-oriented picker order shared by the Rust UI adapters.
pub const CURATED_FONT_FAMILIES: &[&str] = &[
    "JetBrains Mono",
    "JetBrainsMono Nerd Font",
    "Fira Code",
    "Fira Mono",
    "Hack",
    "Source Code Pro",
    "Cascadia Code",
    "Cascadia Mono",
    "IBM Plex Mono",
    "Inconsolata",
    "Iosevka",
    "DejaVu Sans Mono",
    "Ubuntu Mono",
    "Liberation Mono",
    "Noto Sans Mono",
    "SF Mono",
    "Menlo",
    "Monaco",
];

const TRANSITION_EPSILON: f64 = 0.01;

/// Owned live terminal typography state.
#[derive(Debug, Clone, PartialEq)]
pub struct TerminalTypography {
    family: Option<String>,
    baseline_size_pt: f64,
    current_size_pt: f64,
}

impl TerminalTypography {
    /// Builds live state from the launch configuration.
    ///
    /// Finite positive sizes are preserved exactly, even outside the zoom
    /// command range, because reset returns to the launch-configured baseline.
    /// Invalid numeric input is normalized here instead of changing the shared
    /// config parser contract.
    pub fn new(family: Option<String>, configured_size_pt: Option<f64>) -> Self {
        let baseline_size_pt = configured_size_pt
            .filter(|size| size.is_finite() && *size > 0.0)
            .unwrap_or(DEFAULT_FONT_SIZE_PT);
        Self {
            family,
            baseline_size_pt,
            current_size_pt: baseline_size_pt,
        }
    }

    /// Raw live family override. `None` means use the default family chain.
    pub fn family(&self) -> Option<&str> {
        self.family.as_deref()
    }

    /// Family string that a renderer should resolve.
    pub fn effective_family(&self) -> &str {
        self.family.as_deref().unwrap_or(DEFAULT_FONT_FAMILY)
    }

    /// Unmodified valid launch baseline, or the Rust UI default.
    pub fn baseline_size_pt(&self) -> f64 {
        self.baseline_size_pt
    }

    /// Live size after any zoom transitions.
    pub fn current_size_pt(&self) -> f64 {
        self.current_size_pt
    }

    /// Replaces the live family, returning whether it changed.
    pub fn set_family(&mut self, family: Option<String>) -> bool {
        if self.family == family {
            return false;
        }
        self.family = family;
        true
    }

    /// Applies a finite zoom delta and returns the new size when it changed.
    pub fn adjust_size(&mut self, delta: f64) -> Option<f64> {
        if !delta.is_finite() || delta == 0.0 {
            return None;
        }
        let next = (self.current_size_pt + delta).clamp(MIN_FONT_SIZE_PT, MAX_FONT_SIZE_PT);
        if (next - self.current_size_pt).abs() < TRANSITION_EPSILON {
            return None;
        }
        self.current_size_pt = next;
        Some(next)
    }

    /// Restores the launch baseline and returns it when state changed.
    pub fn reset_size(&mut self) -> Option<f64> {
        if (self.current_size_pt - self.baseline_size_pt).abs() < TRANSITION_EPSILON {
            return None;
        }
        self.current_size_pt = self.baseline_size_pt;
        Some(self.current_size_pt)
    }
}

/// Whether confirmation needs to update the live renderer state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FamilyApply {
    /// The live family already represents the confirmed state.
    Keep,
    /// Apply the contained raw override; `None` restores the default chain.
    Set(Option<String>),
}

/// Pure result of confirming one font-family palette row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FamilyConfirmation {
    pub apply: FamilyApply,
    /// Exact unquoted family to persist, or `None` for a no-write confirm.
    pub persist: Option<String>,
}

/// Resolves live and persistence intent for one font-family confirmation.
///
/// Confirming the at-open chain's primary preserves the complete chain and
/// performs no write. A different selection becomes the live override and is
/// persisted. The palette owns the at-open snapshot; this function only owns
/// the deterministic transition.
pub fn confirm_family(
    opened: Option<&str>,
    resolved_opened: &str,
    live: Option<&str>,
    selected: &str,
) -> FamilyConfirmation {
    if resolved_opened.eq_ignore_ascii_case(selected) {
        let apply = if live == opened {
            FamilyApply::Keep
        } else {
            FamilyApply::Set(opened.map(str::to_owned))
        };
        return FamilyConfirmation {
            apply,
            persist: None,
        };
    }

    FamilyConfirmation {
        // Preserve GTK's commit boundary: a newly selected family is applied
        // even when the highlight preview already made it live.
        apply: FamilyApply::Set(Some(selected.to_owned())),
        persist: Some(selected.to_owned()),
    }
}

/// Whether a discovered family can round-trip through Roost's scalar config
/// grammar and comma-delimited family chain without an escape syntax.
pub fn font_family_name_is_safe(name: &str) -> bool {
    !name.is_empty()
        && name.trim() == name
        && !name
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | ','))
}

/// Curated-first, case-insensitively deduplicated installed monospace names.
///
/// Adapters own discovery. Curated entries are retained when installed even
/// if platform metadata fails to mark them monospace, matching the historical
/// GTK/Swift picker policy. The generic alias is always present exactly once
/// so an unavailable configured chain can be represented and preselected.
pub fn ordered_monospace_families(
    installed: impl IntoIterator<Item = (String, bool)>,
) -> Vec<String> {
    let mut installed = installed
        .into_iter()
        .filter(|(name, _)| font_family_name_is_safe(name))
        .collect::<Vec<_>>();
    installed.sort_by(|(left, _), (right, _)| {
        left.to_lowercase()
            .cmp(&right.to_lowercase())
            .then_with(|| left.cmp(right))
    });

    let mut output = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for curated in CURATED_FONT_FAMILIES {
        if let Some((canonical, _)) = installed
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(curated))
        {
            let key = canonical.to_lowercase();
            if seen.insert(key) {
                output.push(canonical.clone());
            }
        }
    }
    for (name, is_monospace) in installed {
        if !is_monospace {
            continue;
        }
        let key = name.to_lowercase();
        if seen.insert(key) {
            output.push(name);
        }
    }
    if !output
        .iter()
        .any(|name| name.eq_ignore_ascii_case("Monospace"))
    {
        output.push("Monospace".to_string());
    }
    output
}

/// Resolves a comma-delimited family chain against adapter-owned installed
/// names. The generic alias is returned when named entries are unavailable.
pub fn resolve_family_name(chain: &str, installed: &[String]) -> String {
    for requested in chain
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        if requested.eq_ignore_ascii_case("monospace") {
            return "Monospace".to_string();
        }
        if let Some(canonical) = installed
            .iter()
            .find(|name| name.eq_ignore_ascii_case(requested))
        {
            return canonical.clone();
        }
    }
    "Monospace".to_string()
}

/// Formats a point size for the config file.
pub fn format_font_size(size_pt: f64) -> String {
    if is_effectively_whole_font_size(size_pt) {
        format!("{}", size_pt.round() as i64)
    } else {
        let formatted = format!("{size_pt:.2}");
        formatted
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn is_effectively_whole_font_size(size_pt: f64) -> bool {
    (size_pt.round() - size_pt).abs() < 0.001
}

/// Serializes a family name for the scalar config editor.
pub fn quote_font_family(name: &str) -> String {
    format!("\"{name}\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_and_configured_values_preserve_launch_baseline() {
        let default = TerminalTypography::new(None, None);
        assert_eq!(default.family(), None);
        assert_eq!(default.effective_family(), DEFAULT_FONT_FAMILY);
        assert_eq!(default.baseline_size_pt(), DEFAULT_FONT_SIZE_PT);
        assert_eq!(default.current_size_pt(), DEFAULT_FONT_SIZE_PT);

        for configured in [2.5, 13.25, 90.0] {
            let typography = TerminalTypography::new(Some("Fira Code".into()), Some(configured));
            assert_eq!(typography.family(), Some("Fira Code"));
            assert_eq!(typography.effective_family(), "Fira Code");
            assert_eq!(typography.baseline_size_pt(), configured);
            assert_eq!(typography.current_size_pt(), configured);
        }
    }

    #[test]
    fn invalid_sizes_normalize_at_consumption() {
        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -5.0] {
            let typography = TerminalTypography::new(None, Some(invalid));
            assert_eq!(typography.baseline_size_pt(), DEFAULT_FONT_SIZE_PT);
            assert_eq!(typography.current_size_pt(), DEFAULT_FONT_SIZE_PT);
        }
    }

    #[test]
    fn adjustment_clamps_and_ignores_non_finite_or_tiny_changes() {
        let mut typography = TerminalTypography::new(None, Some(71.5));
        assert_eq!(typography.adjust_size(1.0), Some(MAX_FONT_SIZE_PT));
        assert_eq!(typography.adjust_size(1.0), None);
        assert_eq!(typography.adjust_size(f64::NAN), None);
        assert_eq!(typography.adjust_size(f64::INFINITY), None);

        let mut tiny = TerminalTypography::new(None, Some(13.0));
        assert_eq!(tiny.adjust_size(0.009), None);
        assert_eq!(tiny.current_size_pt(), 13.0);

        // Preserve the former GTK transition boundary exactly: because the
        // computed binary difference is just below 0.01, 0.01 is a no-op;
        // the next representable user-scale delta crosses the threshold.
        assert_eq!(tiny.adjust_size(0.01), None);
        assert_eq!(tiny.adjust_size(0.010_001), Some(13.010_001));

        let mut below = TerminalTypography::new(None, Some(2.5));
        assert_eq!(below.adjust_size(0.0), None);
        assert_eq!(below.current_size_pt(), 2.5);
        assert_eq!(below.adjust_size(1.0), Some(MIN_FONT_SIZE_PT));
        assert_eq!(below.adjust_size(-1.0), None);

        let mut above = TerminalTypography::new(None, Some(90.0));
        assert_eq!(above.adjust_size(0.0), None);
        assert_eq!(above.current_size_pt(), 90.0);
    }

    #[test]
    fn reset_returns_to_the_unclamped_launch_baseline() {
        let mut typography = TerminalTypography::new(None, Some(90.0));
        assert_eq!(typography.adjust_size(-1.0), Some(MAX_FONT_SIZE_PT));
        assert_eq!(typography.reset_size(), Some(90.0));
        assert_eq!(typography.reset_size(), None);
    }

    #[test]
    fn family_mutation_preserves_the_unconfigured_state() {
        let mut typography = TerminalTypography::new(None, None);
        assert!(!typography.set_family(None));
        assert!(typography.set_family(Some("Hack".into())));
        assert_eq!(typography.family(), Some("Hack"));
        assert!(typography.set_family(None));
        assert_eq!(typography.family(), None);
        assert_eq!(typography.effective_family(), DEFAULT_FONT_FAMILY);
    }

    #[test]
    fn confirming_the_opened_primary_restores_its_chain_without_a_write() {
        assert_eq!(
            confirm_family(
                Some("JetBrains Mono, Monospace"),
                "JetBrains Mono",
                Some("Fira Code"),
                "jetbrains mono",
            ),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("JetBrains Mono, Monospace".into())),
                persist: None,
            }
        );
        assert_eq!(
            confirm_family(
                Some("JetBrains Mono, Monospace"),
                "JetBrains Mono",
                Some("JetBrains Mono, Monospace"),
                "JetBrains Mono",
            ),
            FamilyConfirmation {
                apply: FamilyApply::Keep,
                persist: None,
            }
        );
    }

    #[test]
    fn confirming_a_different_or_unconfigured_family_applies_and_persists() {
        assert_eq!(
            confirm_family(Some("Hack, Monospace"), "Hack", Some("Hack"), "Fira Code",),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Fira Code".into())),
                persist: Some("Fira Code".into()),
            }
        );
        assert_eq!(
            confirm_family(None, "Menlo", None, "Monospace"),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Monospace".into())),
                persist: Some("Monospace".into()),
            }
        );
    }

    #[test]
    fn confirming_the_resolved_default_or_fallback_preserves_raw_policy() {
        assert_eq!(
            confirm_family(None, "Monospace", Some("Fira Code"), "monospace"),
            FamilyConfirmation {
                apply: FamilyApply::Set(None),
                persist: None,
            }
        );
        assert_eq!(
            confirm_family(
                Some("Missing, Fira Code, Monospace"),
                "Fira Code",
                Some("Fira Code"),
                "fira code",
            ),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Missing, Fira Code, Monospace".into())),
                persist: None,
            }
        );
        assert_eq!(
            confirm_family(
                Some("Missing One, Missing Two"),
                "Monospace",
                Some("Hack"),
                "Monospace",
            ),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Missing One, Missing Two".into())),
                persist: None,
            }
        );
    }

    #[test]
    fn installed_family_order_is_curated_safe_and_deduplicated() {
        let ordered = ordered_monospace_families([
            ("zeta Mono".to_string(), true),
            ("fira code".to_string(), true),
            ("Fira Code".to_string(), true),
            ("Arial".to_string(), false),
            ("Alpha Mono".to_string(), true),
            ("Hack".to_string(), false),
            ("bad,name".to_string(), true),
            ("bad\nname".to_string(), true),
            ("bad\"name".to_string(), true),
            (" padded family ".to_string(), true),
        ]);
        assert_eq!(
            ordered,
            ["Fira Code", "Hack", "Alpha Mono", "zeta Mono", "Monospace"]
        );
    }

    #[test]
    fn installed_family_order_has_a_generic_empty_fallback() {
        assert_eq!(
            ordered_monospace_families([
                ("Arial".to_string(), false),
                ("bad,name".to_string(), true),
            ]),
            ["Monospace"]
        );
        for safe in ["JetBrains Mono", "日本語等幅", "Back\\slash"] {
            assert!(font_family_name_is_safe(safe));
            let quoted = quote_font_family(safe);
            assert_eq!(
                quoted.strip_prefix('"').and_then(|v| v.strip_suffix('"')),
                Some(safe)
            );
        }
        for unsafe_name in [
            "",
            "bad,name",
            "bad\"name",
            "bad\nname",
            "bad\0name",
            " padded family ",
        ] {
            assert!(!font_family_name_is_safe(unsafe_name));
        }
    }

    #[test]
    fn family_chain_resolution_preserves_canonical_names_and_fallbacks() {
        let installed = vec![
            "Arial".to_string(),
            "Fira Code".to_string(),
            "Menlo".to_string(),
        ];
        assert_eq!(
            resolve_family_name("Missing, fira code, Monospace", &installed),
            "Fira Code"
        );
        assert_eq!(
            resolve_family_name("Missing, Monospace, Menlo", &installed),
            "Monospace"
        );
        assert_eq!(resolve_family_name("Missing", &installed), "Monospace");
        assert_eq!(resolve_family_name("", &installed), "Monospace");
        assert_eq!(
            resolve_family_name("Arial, Monospace", &installed),
            "Arial",
            "an adapter diagnostic must retain an installed proportional primary"
        );
    }

    #[test]
    fn config_serialization_matches_existing_rust_ui_bytes() {
        assert_eq!(format_font_size(14.0), "14");
        assert_eq!(format_font_size(14.0 + f64::EPSILON), "14");
        assert_eq!(format_font_size(14.5), "14.5");
        assert_eq!(format_font_size(13.25), "13.25");
        assert_eq!(format_font_size(13.20), "13.2");
        assert_eq!(quote_font_family("JetBrains Mono"), "\"JetBrains Mono\"");
    }

    #[test]
    fn whole_number_formatting_boundary_matches_existing_rust_ui_policy() {
        assert!(is_effectively_whole_font_size(14.000_999));
        // Keep the existing floating-point behavior at the decimal boundary:
        // 14.001 is represented just inside the strict 0.001 tolerance.
        assert!(is_effectively_whole_font_size(14.001));
        assert!(!is_effectively_whole_font_size(14.001_001));
    }
}
