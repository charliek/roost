//! A tab's `LANG` when nothing names a locale (issue #697).
//!
//! An app launched from Finder or the Dock inherits launchd's
//! environment, which carries no `LANG` or `LC_*`, so its tab shells would
//! run in the C locale: Apple's bash 3.2 readline then reads a multi-byte
//! character as Meta keys. Like Ghostty (`src/os/locale.zig`), a macOS
//! tab gets `LANG` from the user's Cocoa settings, falling back to
//! `en_US.UTF-8`. The language comes from the user's preferred languages
//! rather than `currentLocale`: Roost is localized only in English, so
//! its `currentLocale` reports English whatever language the user reads. It is added to each spawned tab's environment, never
//! `set_var`'d on this process, whose tokio threads are already running.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};

#[cfg(any(target_os = "macos", test))]
const FALLBACK: &str = "en_US.UTF-8";

/// Insert `LANG` from `system` when none of `LANG`, `LC_ALL` and
/// `LC_CTYPE` is set to a non-empty value (an empty one selects nothing,
/// as POSIX reads it). A locale the user or a parent chose, even `C`,
/// is kept.
pub(crate) fn fill_lang(
    env: &mut BTreeMap<OsString, OsString>,
    system: impl FnOnce() -> Option<&'static str>,
) {
    let named = ["LANG", "LC_ALL", "LC_CTYPE"].iter().any(|name| {
        env.get(OsStr::new(name))
            .is_some_and(|value| !value.is_empty())
    });
    if named {
        return;
    }
    if let Some(lang) = system() {
        env.insert("LANG".into(), lang.into());
    }
}

/// The language and region subtags of a preferred-language tag such as
/// `sv-SE`, `sv` or `zh-Hans-CN`.
#[cfg(any(target_os = "macos", test))]
fn split_tag(tag: &str) -> (&str, Option<String>) {
    let mut subtags = tag.split(['-', '_']);
    let language = subtags.next().unwrap_or_default();
    let region = subtags
        .take_while(|subtag| subtag.len() > 1)
        .find(|subtag| subtag.len() == 2 && subtag.bytes().all(|b| b.is_ascii_alphabetic()))
        .map(str::to_ascii_uppercase);
    (language, region)
}

/// The first installed `<language>_<region>.UTF-8`, trying each of the
/// user's `preferred` languages in order with the user's `region` and then
/// the tag's own region, then the `current` locale's pair, then
/// [`FALLBACK`] (Ghostty falls back the same way when `setlocale` rejects
/// the Cocoa pair, such as `en_DE`).
///
/// An English preference with no installed pair settles on [`FALLBACK`],
/// itself English, rather than moving on to a lower-ranked language: an
/// English reader living in Germany who also lists German wants
/// `en_US.UTF-8`, not German messages.
#[cfg(any(target_os = "macos", test))]
fn utf8_lang<'a>(
    preferred: impl IntoIterator<Item = &'a str>,
    current: (&str, Option<&str>),
    installed: impl Fn(&str) -> bool,
) -> String {
    let (current_language, region) = current;
    let fallback_language = FALLBACK.split('_').next().unwrap_or_default();
    for tag in preferred {
        let (language, tag_region) = split_tag(tag);
        if language.is_empty() {
            continue;
        }
        let pairs = [region.map(str::to_owned), tag_region]
            .into_iter()
            .flatten()
            .map(|region| format!("{language}_{region}.UTF-8"));
        if let Some(found) = pairs.into_iter().find(|candidate| installed(candidate)) {
            return found;
        }
        if language == fallback_language {
            return FALLBACK.to_string();
        }
    }
    region
        .map(|region| format!("{current_language}_{region}.UTF-8"))
        .filter(|candidate| installed(candidate))
        .unwrap_or_else(|| FALLBACK.to_string())
}

/// The user's UTF-8 locale on macOS, read once from `NSLocale`; `None`
/// elsewhere, where a tab keeps whatever locale its starter had.
#[cfg(target_os = "macos")]
pub(crate) fn system_lang() -> Option<&'static str> {
    use std::sync::OnceLock;

    use objc2_foundation::NSLocale;

    static LANG: OnceLock<String> = OnceLock::new();
    let lang = LANG.get_or_init(|| {
        objc2::rc::autoreleasepool(|_| {
            let preferred: Vec<String> = NSLocale::preferredLanguages()
                .to_vec()
                .iter()
                .map(|tag| tag.to_string())
                .collect();
            let locale = NSLocale::currentLocale();
            let language = locale.languageCode().to_string();
            let region = locale.regionCode().map(|region| region.to_string());
            utf8_lang(
                preferred.iter().map(String::as_str),
                (&language, region.as_deref()),
                |name| {
                    std::path::Path::new("/usr/share/locale")
                        .join(name)
                        .is_dir()
                },
            )
        })
    });
    Some(lang.as_str())
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn system_lang() -> Option<&'static str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<OsString, OsString> {
        pairs
            .iter()
            .map(|(name, value)| (name.into(), value.into()))
            .collect()
    }

    fn lang_after(pairs: &[(&str, &str)]) -> Option<String> {
        let mut env = env(pairs);
        fill_lang(&mut env, || Some("de_DE.UTF-8"));
        env.get(OsStr::new("LANG"))
            .map(|value| value.to_string_lossy().into_owned())
    }

    #[test]
    fn an_unset_locale_takes_the_systems() {
        assert_eq!(
            lang_after(&[("PATH", "/bin")]).as_deref(),
            Some("de_DE.UTF-8")
        );
    }

    #[test]
    fn empty_locale_variables_count_as_unset() {
        assert_eq!(
            lang_after(&[("LANG", ""), ("LC_ALL", ""), ("LC_CTYPE", "")]).as_deref(),
            Some("de_DE.UTF-8")
        );
    }

    #[test]
    fn a_locale_already_named_is_kept() {
        assert_eq!(
            lang_after(&[("LANG", "fr_FR.UTF-8")]).as_deref(),
            Some("fr_FR.UTF-8")
        );
        assert_eq!(lang_after(&[("LC_ALL", "C")]), None);
        assert_eq!(lang_after(&[("LC_CTYPE", "UTF-8")]), None);
    }

    #[test]
    fn no_system_locale_adds_nothing() {
        let mut env = env(&[]);
        fill_lang(&mut env, || None);
        assert!(env.is_empty());
    }

    fn installed(name: &str) -> bool {
        [
            "sv_SE.UTF-8",
            "en_US.UTF-8",
            "en_GB.UTF-8",
            "de_DE.UTF-8",
            "zh_CN.UTF-8",
        ]
        .contains(&name)
    }

    /// Roost's English-only `currentLocale` says `en_SE` for a Swedish
    /// reader in Sweden; the preferred language is what names Swedish.
    #[test]
    fn the_preferred_language_beats_the_english_current_locale() {
        assert_eq!(
            utf8_lang(["sv-SE"], ("en", Some("SE")), installed),
            "sv_SE.UTF-8"
        );
        assert_eq!(
            utf8_lang(["sv"], ("en", Some("SE")), installed),
            "sv_SE.UTF-8"
        );
    }

    /// The user's region comes first, then the tag's own: Swedish read
    /// in the US has no `sv_US`, so the tag's `SE` names it.
    #[test]
    fn a_preferred_language_tries_the_users_region_then_its_own() {
        assert_eq!(
            utf8_lang(["sv-SE"], ("en", Some("US")), installed),
            "sv_SE.UTF-8"
        );
        assert_eq!(
            utf8_lang(["en-GB"], ("en", Some("GB")), installed),
            "en_GB.UTF-8"
        );
        assert_eq!(
            utf8_lang(["en-US"], ("en", Some("US")), installed),
            "en_US.UTF-8"
        );
        assert_eq!(
            utf8_lang(["zh-Hans-CN"], ("en", Some("CN")), installed),
            "zh_CN.UTF-8"
        );
    }

    #[test]
    fn an_uninstalled_preference_moves_on_to_the_next() {
        assert_eq!(
            utf8_lang(["gsw-CH", "de-DE"], ("en", Some("CH")), installed),
            "de_DE.UTF-8"
        );
    }

    #[test]
    fn an_english_preference_settles_on_the_fallback_before_a_lower_language() {
        assert_eq!(
            utf8_lang(["en-DE", "de-DE"], ("en", Some("DE")), installed),
            FALLBACK
        );
    }

    #[test]
    fn with_no_usable_preference_the_current_locale_then_the_fallback_decide() {
        assert_eq!(utf8_lang([], ("sv", Some("SE")), installed), "sv_SE.UTF-8");
        assert_eq!(
            utf8_lang(["xx"], ("sv", Some("SE")), installed),
            "sv_SE.UTF-8"
        );
        assert_eq!(utf8_lang([], ("en", Some("DE")), installed), FALLBACK);
        assert_eq!(utf8_lang(["xx"], ("sv", None), installed), FALLBACK);
    }

    #[test]
    fn a_tag_splits_into_language_and_region() {
        assert_eq!(split_tag("sv-SE"), ("sv", Some("SE".into())));
        assert_eq!(split_tag("sv"), ("sv", None));
        assert_eq!(split_tag("zh-Hans-CN"), ("zh", Some("CN".into())));
        assert_eq!(split_tag("en_gb"), ("en", Some("GB".into())));
        assert_eq!(split_tag("es-419"), ("es", None));
        assert_eq!(split_tag("en-u-rg-gbzzzz"), ("en", None));
    }

    /// The real `NSLocale` read, which no other test reaches: whatever
    /// the runner's locale, the result must be a locale macOS installs.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_system_lang_is_an_installed_utf8_locale() {
        let lang = system_lang().expect("macOS always names a locale");
        assert!(lang.ends_with(".UTF-8"), "{lang}");
        assert!(
            std::path::Path::new("/usr/share/locale")
                .join(lang)
                .is_dir(),
            "{lang}"
        );
    }
}
