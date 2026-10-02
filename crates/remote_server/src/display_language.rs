//! zed-jp-enhance: the text this host writes for the client's screen (RPC
//! errors, language server statuses, terminal notices) in the language the
//! client shows its UI in (ADR-F64).
//!
//! The daemon learns the language from the wire revision check, which every
//! connection and reconnection starts with; the `terminal-session` processes
//! from [`DISPLAY_LANGUAGE_ENV_VAR`] on their command line. Only the
//! translations bundled with this binary are used: the PO files in the
//! client's own configuration directory stay on the client.

use i18n::Catalog as _;
use i18n_catalog::PoCatalog;
use remote::{DISPLAY_LANGUAGE_ENV_VAR, DisplayLanguageHandler};

/// Makes the connections of this process apply the display language their
/// client sends.
pub fn init(cx: &mut gpui::App) {
    install_english();
    cx.set_global(DisplayLanguageHandler(install));
}

/// `install` compares languages, and a process starts in English without a
/// catalog, so the English one (the product's own name) is put in place first.
fn install_english() {
    i18n::install(
        i18n_catalog::english_catalog(None).map(|catalog| catalog as &'static dyn i18n::Catalog),
    );
}

/// Shows `tag` from here on, or English for `None`, for a tag this binary has
/// no translation for, and for anything that is not a language tag.
pub fn install(tag: Option<&str>) {
    let catalog = tag
        .and_then(bundled_catalog)
        .or_else(|| i18n_catalog::english_catalog(None));
    if catalog.map_or("en", |catalog| catalog.locale()) == i18n::current_locale() {
        return;
    }
    i18n::install(catalog.map(|catalog| catalog as &'static dyn i18n::Catalog));
}

/// Applies the display language the client put in the environment of a
/// `terminal-session` command. Without one, the process stays in English.
pub fn install_from_environment() {
    install_english();
    if let Ok(tag) = std::env::var(DISPLAY_LANGUAGE_ENV_VAR) {
        install(Some(&tag));
    }
}

/// The tag comes from the other end of a connection or from the
/// environment, so it is checked before anything is looked up by it, and it
/// is resolved to one of the bundled languages first: the catalogs are
/// cached for the life of the process by their tag, and `ja-X1`, `ja-X2`, …
/// would otherwise each keep a copy of the Japanese one.
fn bundled_catalog(tag: &str) -> Option<&'static PoCatalog> {
    if !i18n_catalog::is_valid_tag(tag) {
        log::warn!("the client's display language is not a language tag; showing English");
        return None;
    }
    let bundled =
        i18n_catalog::negotiate(&[tag.to_string()], &i18n_catalog::available_locales(None))?;
    match i18n_catalog::load(&bundled, None) {
        Ok(catalog) => catalog,
        Err(error) => {
            log::warn!("could not load the translation for {bundled}: {error:#}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bundled_language_is_installed_and_anything_else_is_english() {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();

        install(Some("ja"));
        assert_eq!(i18n::current_locale(), "ja");
        assert_eq!(
            i18n::t!("No operation was specified"),
            "操作が指定されていません"
        );

        // A region the bundle does not have falls back to its language.
        install(Some("ja-JP"));
        assert_eq!(i18n::current_locale(), "ja");

        for not_japanese in [
            None,
            Some("en"),
            Some("en-GB"),
            Some("xx"),
            Some(""),
            Some("../ja"),
            Some("ja/../../etc"),
            Some("ja\n"),
            Some("ja.UTF-8"),
        ] {
            install(Some("ja"));
            let before = i18n::generation();
            install(not_japanese);
            assert_eq!(i18n::current_locale(), "en", "{not_japanese:?}");
            assert_ne!(i18n::generation(), before, "{not_japanese:?}");
            assert_eq!(
                i18n::t!("No operation was specified"),
                "No operation was specified"
            );
            assert_eq!(
                i18n::tr_ctx(i18n::context::MENU, "About Zed"),
                "About ForgeKairos",
                "{not_japanese:?} names the product as English does"
            );
        }

        let longest_valid = format!("ja-{}", ["abcdefgh"; 7].join("-"));
        assert_eq!(longest_valid.len(), 65);
        install(Some(&longest_valid));
        assert_eq!(i18n::current_locale(), "en", "longer than a tag may be");
        install(Some(&"x".repeat(1024 * 1024)));
        assert_eq!(i18n::current_locale(), "en");
    }

    #[gpui::test]
    fn a_client_without_a_language_sees_the_product_name_from_the_start(
        cx: &mut gpui::TestAppContext,
    ) {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        cx.update(init);
        // The first connection asks for English, which `install` takes for what is
        // already shown, so the English catalog has to be in place before it.
        install(None);
        assert_eq!(
            i18n::tr_ctx(i18n::context::MENU, "About Zed"),
            "About ForgeKairos"
        );
    }

    #[test]
    fn installing_the_language_already_shown_changes_nothing() {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        install(Some("ja"));
        let installed = i18n::generation();
        install(Some("ja"));
        assert_eq!(i18n::generation(), installed);

        install(None);
        let installed = i18n::generation();
        install(Some("en"));
        install(None);
        assert_eq!(i18n::generation(), installed);
    }

    #[test]
    fn a_tag_that_differs_only_in_its_region_shares_one_catalog() {
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        install(Some("ja"));
        let japanese = i18n::t!("No operation was specified");
        install(None);
        install(Some("ja-X1"));
        let first = i18n::current_locale();
        install(None);
        install(Some("ja-X2"));
        assert_eq!(i18n::current_locale(), first);
        assert_eq!(first, "ja");
        assert_eq!(i18n::t!("No operation was specified"), japanese);
    }
}
