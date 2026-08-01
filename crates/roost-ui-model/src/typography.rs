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
    live: Option<&str>,
    selected: &str,
) -> FamilyConfirmation {
    let opened_primary = opened.and_then(primary_family);
    if opened_primary.is_some_and(|primary| primary.eq_ignore_ascii_case(selected)) {
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

fn primary_family(value: &str) -> Option<&str> {
    value
        .split(',')
        .map(str::trim)
        .find(|part| !part.is_empty())
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
            confirm_family(Some("Hack, Monospace"), Some("Hack"), "Fira Code"),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Fira Code".into())),
                persist: Some("Fira Code".into()),
            }
        );
        assert_eq!(
            confirm_family(None, None, "Monospace"),
            FamilyConfirmation {
                apply: FamilyApply::Set(Some("Monospace".into())),
                persist: Some("Monospace".into()),
            }
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
