//! The interface language.
//!
//! Strings live in `locales/ui.yml` and are looked up with `rust_i18n::t!`,
//! the same mechanism gpui-kit's components use for theirs. Keys under
//! `gpui_component` in that file override the components' own strings;
//! [`init`] registers them. Both share one process-wide locale, so
//! [`set_language`] switches the pages and the components together.
//!
//! The locale is outside GPUI's change tracking: switching it repaints
//! nothing by itself, which is why [`set_language`] refreshes the windows.
//! Anything that caches a translated string outside `render` has to be
//! rebuilt by whoever switches.
//!
//! Amounts, dates and relative times are formatted by `ui::fmt` and are not
//! translated here; a message that places a value keeps it inside one
//! translated string (`%{n}`), never concatenated from fragments.

use gpui_kit::App;

/// A language the interface is translated into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    English,
    SimplifiedChinese,
}

impl Language {
    pub const ALL: &'static [Language] = &[Language::English, Language::SimplifiedChinese];

    /// The locale code `locales/ui.yml` is keyed by.
    pub fn code(self) -> &'static str {
        match self {
            Language::English => "en",
            Language::SimplifiedChinese => "zh-CN",
        }
    }

    /// The language's name in that language, so it can be found by
    /// someone who cannot read the current one.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::English => "English",
            Language::SimplifiedChinese => "简体中文",
        }
    }

    /// The translation for a stored code or a system locale tag
    /// ("zh-Hans-CN", "zh_CN.UTF-8", "en-US"), or `None` for one there is
    /// no translation for.
    pub fn from_tag(tag: &str) -> Option<Language> {
        let tag = tag.trim().to_ascii_lowercase().replace('_', "-");
        let primary = tag.split(['-', '.']).next().unwrap_or_default();
        match primary {
            "en" => Some(Language::English),
            // Traditional Chinese is not translated; Simplified is closer to
            // a reader of it than English, but the guess is only made for
            // the Simplified scripts and regions.
            "zh" if !(tag.contains("hant")
                || tag.contains("-tw")
                || tag.contains("-hk")
                || tag.contains("-mo")) =>
            {
                Some(Language::SimplifiedChinese)
            }
            _ => None,
        }
    }

    /// The language the system asks for, falling back to English.
    pub fn system() -> Language {
        sys_locale::get_locales()
            .find_map(|tag| Language::from_tag(&tag))
            .unwrap_or(Language::English)
    }

    /// What a stored setting means: the language it names, or the
    /// system's when it names none (or one no longer translated).
    pub fn resolve(setting: Option<&str>) -> Language {
        setting
            .and_then(Language::from_tag)
            .unwrap_or_else(Language::system)
    }
}

/// The language the interface is in now.
pub fn current() -> Language {
    Language::from_tag(&rust_i18n::locale()).unwrap_or(Language::English)
}

/// The language after the current one, wrapping round: what a one-button
/// switch offers.
pub fn next_language() -> Language {
    let all = Language::ALL;
    let at = all.iter().position(|l| *l == current()).unwrap_or(0);
    all[(at + 1) % all.len()]
}

/// Register the component overrides and set the language the config asks
/// for. Call once, before `gpui_kit::init`.
pub fn init() {
    use gpui_kit::component as gpui_component;
    rust_i18n::extend!(gpui_component);

    let setting = crate::config::load_config()
        .ok()
        .and_then(|config| config.language);
    rust_i18n::set_locale(Language::resolve(setting.as_deref()).code());
}

/// A bucket name as shown. The data layer groups charges under fixed
/// English placeholders ("Other", "Unallocated", "Uncategorized") that the
/// pages also match on, so they stay English in the data and are
/// translated only here, at display.
pub fn bucket_label(label: &str) -> gpui_kit::SharedString {
    match label {
        "Other" => tr!("common.other"),
        "Unallocated" => tr!("common.unallocated"),
        "Uncategorized" => tr!("common.uncategorized"),
        other => gpui_kit::SharedString::from(other.to_string()),
    }
}

/// Switch the interface language and repaint every window.
pub fn set_language(language: Language, cx: &mut App) {
    rust_i18n::set_locale(language.code());
    cx.refresh_windows();
    // Some labels are built with the data a page loads; reloading the page
    // on screen rebuilds them. Any other page rebuilds on its next visit.
    if cx.has_global::<crate::app::GlobalAppState>() {
        crate::app::request_reload(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_tags_map_to_a_translation() {
        assert_eq!(Language::from_tag("en-US"), Some(Language::English));
        assert_eq!(
            Language::from_tag("zh-Hans-CN"),
            Some(Language::SimplifiedChinese)
        );
        assert_eq!(
            Language::from_tag("zh_CN.UTF-8"),
            Some(Language::SimplifiedChinese)
        );
        assert_eq!(
            Language::from_tag("zh-CN"),
            Some(Language::SimplifiedChinese)
        );
        assert_eq!(Language::from_tag("zh-TW"), None);
        assert_eq!(Language::from_tag("zh-Hant"), None);
        assert_eq!(Language::from_tag("fr-FR"), None);
    }

    #[test]
    fn a_stored_code_reads_back_as_itself() {
        for language in Language::ALL {
            assert_eq!(Language::from_tag(language.code()), Some(*language));
        }
    }

    /// Every key resolves in every language, so a missing translation is
    /// a test failure rather than a raw key on screen.
    #[test]
    fn every_key_is_translated_in_every_language() {
        let backend = crate::_rust_i18n_backend();
        let keys = backend
            .messages_for_locale("en")
            .expect("English is the source language");
        assert!(!keys.is_empty());
        let mut missing = Vec::new();
        for (key, _) in keys {
            if key.starts_with("gpui_component.") {
                continue;
            }
            for language in Language::ALL {
                if backend.translate(language.code(), &key).is_none() {
                    missing.push(format!("{}: {key}", language.code()));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "untranslated keys:\n{}",
            missing.join("\n")
        );
    }
}
