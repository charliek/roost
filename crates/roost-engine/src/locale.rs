//! A tab's `LANG` when nothing names a locale (issue #697).
//!
//! An app launched from Finder or the Dock inherits launchd's
//! environment, which carries no `LANG` or `LC_*`, so its tab shells would
//! run in the C locale: Apple's bash 3.2 readline then reads a multi-byte
//! character as Meta keys. Like Ghostty (`src/os/locale.zig`), a macOS
//! tab gets `LANG` from the user's Cocoa locale, falling back to
//! `en_US.UTF-8`. It is added to each spawned tab's environment, never
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

/// `<language>_<region>.UTF-8` when that locale is installed, else
/// [`FALLBACK`] (Ghostty falls back the same way when `setlocale`
/// rejects the Cocoa pair, such as `en_DE`).
#[cfg(any(target_os = "macos", test))]
fn utf8_lang(language: &str, region: Option<&str>, installed: impl Fn(&str) -> bool) -> String {
    region
        .map(|region| format!("{language}_{region}.UTF-8"))
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
            let locale = NSLocale::currentLocale();
            let language = locale.languageCode().to_string();
            let region = locale.regionCode().map(|region| region.to_string());
            utf8_lang(&language, region.as_deref(), |name| {
                std::path::Path::new("/usr/share/locale")
                    .join(name)
                    .is_dir()
            })
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

    #[test]
    fn an_installed_cocoa_pair_is_used_and_anything_else_falls_back() {
        let installed = |name: &str| name == "sv_SE.UTF-8";
        assert_eq!(utf8_lang("sv", Some("SE"), installed), "sv_SE.UTF-8");
        assert_eq!(utf8_lang("en", Some("DE"), installed), FALLBACK);
        assert_eq!(utf8_lang("sv", None, installed), FALLBACK);
    }
}
