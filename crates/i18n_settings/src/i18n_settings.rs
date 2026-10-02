//! Connects the translation catalogs to the `ui_language` setting: picks the catalog
//! before the first frame, swaps it when the setting changes, and tells the user when
//! the translation files have problems or a restart is needed.

#[macro_use]
extern crate i18n;

use gpui::{App, AppContext as _, Global, SharedString};
use i18n_catalog::{LocaleInfo, PoCatalog};
use parking_lot::Mutex;
use serde::Deserialize;
use settings::{RegisterSetting, Settings, SettingsStore};
use std::path::{Path, PathBuf};
use ui::IconName;
use workspace::notifications::{
    NotificationId, show_app_notification, simple_message_notification::MessageNotification,
};

/// The most problems listed in one notification; the rest are in the log.
const MAX_LISTED_PROBLEMS: usize = 5;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UiLanguage {
    /// Follow the operating system's preferred languages.
    #[default]
    System,
    /// A BCP 47 tag such as "ja" or "pt-BR". "en" is the source text, with only the
    /// product's name put in place of upstream's.
    Tag(String),
}

impl UiLanguage {
    pub fn from_setting(value: Option<&str>) -> Self {
        match value.map(str::trim) {
            None | Some("") => UiLanguage::System,
            Some(value) if value.eq_ignore_ascii_case("system") => UiLanguage::System,
            Some(value) => UiLanguage::Tag(value.to_string()),
        }
    }
}

#[derive(Clone, Debug, RegisterSetting)]
pub struct UiLanguageSettings {
    pub ui_language: UiLanguage,
}

impl Settings for UiLanguageSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        Self {
            ui_language: UiLanguage::from_setting(
                content.ui_language.as_ref().map(|name| name.0.as_str()),
            ),
        }
    }
}

/// The language the UI is currently shown in. Views that cache translated text can
/// observe this global, or compare `generation`, to know when to rebuild it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveUiLanguage {
    pub locale: SharedString,
    /// `i18n::generation()` when the catalog was last installed.
    pub generation: u64,
}

impl Global for ActiveUiLanguage {}

impl ActiveUiLanguage {
    fn current() -> Self {
        Self {
            locale: SharedString::new(i18n::current_locale()),
            generation: i18n::generation(),
        }
    }
}

struct UiLanguageState {
    applied: UiLanguage,
    user_locales_directory: Option<PathBuf>,
    /// Whether the settings store has delivered the user's file yet.
    user_settings_arrived: bool,
}

impl Global for UiLanguageState {}

struct EarlyResult {
    language: UiLanguage,
    problems: Vec<String>,
}

/// What `init_early` applied, handed over to `init` once the app is running.
static EARLY_RESULT: Mutex<Option<EarlyResult>> = Mutex::new(None);

/// Where the user puts translations that are read without rebuilding Zed.
pub fn user_locales_directory() -> PathBuf {
    paths::config_dir().join("locales")
}

/// Installs the catalog for the language in the user's settings file.
///
/// Call this in `main` before the app runs. The settings store only receives the
/// user's settings asynchronously, and without this the first frame would be drawn
/// in English and then switch.
pub fn init_early() {
    init_early_with(paths::settings_file(), Some(&user_locales_directory()));
}

fn init_early_with(settings_file: &Path, user_locales_directory: Option<&Path>) {
    gpui::PromptLevel::set_title_provider(prompt_title);
    let language = read_ui_language(settings_file);
    let problems = apply(&language, user_locales_directory);
    *EARLY_RESULT.lock() = Some(EarlyResult { language, problems });
}

fn prompt_title(level: gpui::PromptLevel) -> SharedString {
    match level {
        gpui::PromptLevel::Info => tc!("dialog title", "Info").into(),
        gpui::PromptLevel::Warning => tc!("dialog title", "Warning").into(),
        // Critical also asks before destructive actions (replacing a file, discarding
        // changes), so translations may word it as a warning rather than an error;
        // the dialog's icon still sets it apart.
        gpui::PromptLevel::Critical => tc!("dialog title", "Critical").into(),
    }
}

/// Reads only `ui_language` from a settings file. A missing or broken file, or a
/// missing key, means `System`: the full settings loader reports such problems later.
fn read_ui_language(settings_file: &Path) -> UiLanguage {
    #[derive(Deserialize)]
    struct UiLanguageOnly {
        ui_language: Option<String>,
    }

    let text = match std::fs::read_to_string(settings_file) {
        Ok(text) => text,
        Err(error) => {
            if error.kind() != std::io::ErrorKind::NotFound {
                log::warn!(target: "i18n", "cannot read {settings_file:?} for ui_language: {error}");
            }
            return UiLanguage::System;
        }
    };
    match settings::parse_json_with_comments::<UiLanguageOnly>(&text) {
        Ok(settings) => UiLanguage::from_setting(settings.ui_language.as_deref()),
        Err(error) => {
            log::info!(target: "i18n", "cannot read ui_language from {settings_file:?}: {error:#}");
            UiLanguage::System
        }
    }
}

/// Watches the `ui_language` setting and swaps the catalog when it changes.
pub fn init(cx: &mut App) {
    init_with(Some(user_locales_directory()), cx);
}

/// `init` reading the user's translations from `user_locales_directory` instead of
/// the config directory, for tests in the crates that react to the language.
#[cfg(any(test, feature = "test-support"))]
pub fn init_for_tests(user_locales_directory: Option<PathBuf>, cx: &mut App) {
    init_with(user_locales_directory, cx);
}

fn init_with(user_locales_directory: Option<PathBuf>, cx: &mut App) {
    gpui::PromptLevel::set_title_provider(prompt_title);
    let (applied, problems) = match EARLY_RESULT.lock().take() {
        Some(early) => (early.language, early.problems),
        None => {
            let language = UiLanguageSettings::get_global(cx).ui_language.clone();
            let problems = apply(&language, user_locales_directory.as_deref());
            (language, problems)
        }
    };
    cx.set_global(UiLanguageState {
        applied,
        user_locales_directory,
        user_settings_arrived: false,
    });
    cx.set_global(ActiveUiLanguage::current());
    show_problems(problems, cx);

    cx.observe_global::<SettingsStore>(|cx| {
        // Until the user's file has been read the store holds only the defaults, which
        // would undo what `init_early` installed.
        if cx.global::<SettingsStore>().raw_user_settings().is_none() {
            return;
        }
        let language = UiLanguageSettings::get_global(cx).ui_language.clone();
        let state = cx.global_mut::<UiLanguageState>();
        let first_arrival = !std::mem::replace(&mut state.user_settings_arrived, true);
        if state.applied == language {
            return;
        }
        let user_locales_directory = state.user_locales_directory.clone();
        let previous_locale = i18n::current_locale();
        let problems = apply(&language, user_locales_directory.as_deref());
        cx.global_mut::<UiLanguageState>().applied = language;
        let active = ActiveUiLanguage::current();
        let locale_changed = active.locale.as_ref() != previous_locale;
        cx.set_global(active);
        cx.refresh_windows();
        show_problems(problems, cx);
        // `init_early` reads only the top-level `ui_language`, so an override for
        // the platform or release channel switches the language as Zed starts;
        // offering a restart then would do so on every launch.
        if locale_changed && !first_arrival {
            show_restart_notification(cx);
        }
    })
    .detach();
}

/// The languages the user can choose from, for the settings UI.
pub fn language_options(cx: &App) -> Vec<LocaleInfo> {
    let user_locales_directory = match cx.try_global::<UiLanguageState>() {
        Some(state) => state.user_locales_directory.clone(),
        None => Some(user_locales_directory()),
    };
    i18n_catalog::available_locales(user_locales_directory.as_deref())
}

/// Installs the catalog for `language` and returns the problems to show the user.
fn apply(language: &UiLanguage, user_locales_directory: Option<&Path>) -> Vec<String> {
    let (catalog, problems) = resolve(language, user_locales_directory);
    i18n::install(catalog.map(|catalog| catalog as &'static dyn i18n::Catalog));
    problems
}

fn resolve(
    language: &UiLanguage,
    user_locales_directory: Option<&Path>,
) -> (Option<&'static PoCatalog>, Vec<String>) {
    let english = || i18n_catalog::english_catalog(user_locales_directory);
    let requested = match language {
        UiLanguage::System => i18n_catalog::system_locales(),
        UiLanguage::Tag(tag) => {
            let normalized = i18n_catalog::normalize_tag(tag);
            if normalized.eq_ignore_ascii_case(i18n_catalog::ENGLISH) {
                return (english(), Vec::new());
            }
            if !i18n_catalog::is_valid_tag(&normalized) {
                return (
                    english(),
                    vec![t!("\"{}\" is not a language tag; showing English.", tag)],
                );
            }
            vec![normalized]
        }
    };
    let available = i18n_catalog::available_locales(user_locales_directory);
    let Some(tag) = i18n_catalog::negotiate(&requested, &available) else {
        return match language {
            UiLanguage::Tag(tag)
                if !i18n_catalog::base_language(&i18n_catalog::normalize_tag(tag))
                    .eq_ignore_ascii_case(i18n_catalog::ENGLISH) =>
            {
                (
                    english(),
                    vec![t!(
                        "There is no translation for \"{}\"; showing English.",
                        tag
                    )],
                )
            }
            _ => (english(), Vec::new()),
        };
    };
    match i18n_catalog::load(&tag, user_locales_directory) {
        Ok(Some(catalog)) => {
            let problems = catalog
                .problems()
                .iter()
                .map(|problem| problem.to_string())
                .collect();
            (Some(catalog), problems)
        }
        Ok(None) => (english(), Vec::new()),
        Err(error) => (english(), vec![format!("{error:#}")]),
    }
}

fn show_problems(problems: Vec<String>, cx: &mut App) {
    if problems.is_empty() {
        return;
    }
    for problem in &problems {
        log::warn!(target: "i18n", "{problem}");
    }
    let mut message = problems
        .iter()
        .take(MAX_LISTED_PROBLEMS)
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    if problems.len() > MAX_LISTED_PROBLEMS {
        message.push('\n');
        message.push_str(&t!(
            "…and {} more (see the log).",
            problems.len() - MAX_LISTED_PROBLEMS
        ));
    }
    let message = SharedString::from(message);

    struct TranslationProblemNotification;
    show_app_notification(
        NotificationId::unique::<TranslationProblemNotification>(),
        cx,
        move |cx| {
            let message = message.clone();
            cx.new(|cx| {
                MessageNotification::new(message, cx)
                    .with_title(t!("Problems in the UI translation files"))
            })
        },
    );
}

/// Text that was translated before the switch, such as tab titles, stays in the old
/// language until it is rebuilt, so offer a restart.
fn show_restart_notification(cx: &mut App) {
    struct RestartForLanguageNotification;
    show_app_notification(
        NotificationId::unique::<RestartForLanguageNotification>(),
        cx,
        |cx| {
            cx.new(|cx| {
                MessageNotification::new(
                    t!("Some parts of the UI switch to the new language after a restart."),
                    cx,
                )
                .primary_message(t!("Restart Now"))
                .primary_icon(IconName::ArrowCircle)
                .primary_on_click(|_window, cx| workspace::reload(cx))
            })
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use i18n::Catalog as _;

    // `EARLY_RESULT` is process-wide, so the tests that go through `init_early`
    // hold this lock; their installs are kept to their own thread by
    // `ThreadLocalInstalls`.
    static SERIAL: Mutex<()> = Mutex::new(());

    #[test]
    fn native_dialog_titles_follow_the_ui_language() {
        gpui::PromptLevel::set_title_provider(prompt_title);
        let titles = || {
            [
                gpui::PromptLevel::Info,
                gpui::PromptLevel::Warning,
                gpui::PromptLevel::Critical,
            ]
            .map(gpui::PromptLevel::title)
        };
        {
            let _locale = i18n::test_support::LocaleOverride::new(None);
            assert_eq!(titles(), ["Info", "Warning", "Critical"]);
        }
        let _locale = i18n::test_support::LocaleOverride::new(Some(
            i18n_catalog::test_support::bundled_catalog("ja"),
        ));
        assert_eq!(titles(), ["情報", "警告", "警告"]);
    }

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the directory");
        }
        std::fs::write(path, text).expect("write the file");
    }

    fn set_user_settings(json: &str, cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                let result = store.set_user_settings(json, cx);
                assert_eq!(
                    result.parse_status,
                    settings::ParseStatus::Success,
                    "the test settings should parse"
                );
            });
        });
        cx.run_until_parked();
    }

    fn active(cx: &mut TestAppContext) -> ActiveUiLanguage {
        cx.update(|cx| cx.global::<ActiveUiLanguage>().clone())
    }

    #[test]
    fn setting_values() {
        assert_eq!(UiLanguage::from_setting(None), UiLanguage::System);
        assert_eq!(UiLanguage::from_setting(Some("")), UiLanguage::System);
        assert_eq!(
            UiLanguage::from_setting(Some(" System ")),
            UiLanguage::System
        );
        assert_eq!(
            UiLanguage::from_setting(Some("pt-BR")),
            UiLanguage::Tag("pt-BR".to_string())
        );
    }

    #[test]
    fn reading_ui_language_from_settings_files() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let path = directory.path().join("settings.json");

        assert_eq!(read_ui_language(&path), UiLanguage::System);

        write(
            &path,
            "// comment\n{\n  \"theme\": \"One Dark\",\n  /* block */ \"ui_language\": \"ja\",\n}\n",
        );
        assert_eq!(read_ui_language(&path), UiLanguage::Tag("ja".to_string()));

        for broken in [
            "",
            "{",
            "{ \"ui_language\": 3 }",
            "[1, 2]",
            "{ \"ui_language\": \"ja\" } trailing",
            "\u{0}\u{1}",
        ] {
            write(&path, broken);
            assert_eq!(read_ui_language(&path), UiLanguage::System, "{broken:?}");
        }

        write(&path, "{ \"vim_mode\": true }");
        assert_eq!(read_ui_language(&path), UiLanguage::System);
        std::fs::write(&path, [0xff, 0xfe, 0x00]).expect("write the file");
        assert_eq!(read_ui_language(&path), UiLanguage::System);
    }

    #[test]
    fn init_early_survives_a_broken_settings_file_and_hands_over_to_init() {
        let _serial = SERIAL.lock();
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let path = directory.path().join("settings.json");
        write(&path, "{ \"ui_language\": ");
        init_early_with(&path, Some(directory.path()));
        let early = EARLY_RESULT
            .lock()
            .take()
            .expect("init_early records its result");
        assert_eq!(early.language, UiLanguage::System);

        write(&path, "{ \"ui_language\": \"ja\" }");
        init_early_with(&path, Some(directory.path()));
        assert_eq!(i18n::current_locale(), "ja");
        let early = EARLY_RESULT
            .lock()
            .take()
            .expect("init_early records its result");
        assert_eq!(early.language, UiLanguage::Tag("ja".to_string()));
        assert!(early.problems.is_empty(), "{:?}", early.problems);

        write(&path, "{ \"ui_language\": \"en\" }");
        init_early_with(&path, Some(directory.path()));
        assert_eq!(i18n::current_locale(), "en");
        EARLY_RESULT.lock().take();
    }

    #[gpui::test]
    fn changing_the_setting_swaps_the_catalog(cx: &mut TestAppContext) {
        let _serial = SERIAL.lock();
        let _installs = i18n::test_support::ThreadLocalInstalls::new();
        EARLY_RESULT.lock().take();

        let directory = tempfile::tempdir().expect("create a temporary directory");
        write(
            &directory.path().join("fr/test.po"),
            "msgid \"\"\nmsgstr \"X-Language-Name: Français\\n\"\n\nmsgid \"i18n_settings test: Open\"\nmsgstr \"Ouvrir\"\n",
        );
        write(
            &directory.path().join("zz/broken.po"),
            "msgid \"i18n_settings test: Open\"\nmsgstr \"Zz\"\n\nmsgid \"unterminated\nmsgstr \"x\"\n",
        );

        let settings_file = directory.path().join("settings.json");
        write(&settings_file, "{ \"ui_language\": \"ja\" }");
        init_early_with(&settings_file, Some(directory.path()));
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            init_with(Some(directory.path().to_path_buf()), cx);
        });
        assert_eq!(active(cx).locale.as_ref(), "ja");
        let generation_after_init = i18n::generation();
        // Settings changes before the user's file arrives carry only the defaults and
        // must not undo what `init_early` installed.
        cx.update(|cx| cx.update_global::<SettingsStore, _>(|_, _| {}));
        cx.run_until_parked();
        assert_eq!(active(cx).locale.as_ref(), "ja");
        assert_eq!(i18n::current_locale(), "ja");
        assert_eq!(i18n::generation(), generation_after_init);

        set_user_settings("{ \"ui_language\": \"en\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "en");
        assert_eq!(
            i18n::tr("i18n_settings test: Open"),
            "i18n_settings test: Open"
        );
        assert_eq!(
            i18n::tr_ctx(i18n::context::MENU, "About Zed"),
            "About ForgeKairos",
            "English shows the product's own name"
        );

        let before = active(cx).generation;
        set_user_settings("{ \"ui_language\": \"fr\" }", cx);
        assert_eq!(i18n::current_locale(), "fr");
        assert_eq!(active(cx).locale.as_ref(), "fr");
        assert!(active(cx).generation > before);
        assert_eq!(i18n::tr("i18n_settings test: Open"), "Ouvrir");

        let options = cx.update(|cx| language_options(cx));
        let french = options
            .iter()
            .find(|option| option.tag == "fr")
            .expect("the user's fr is offered");
        assert_eq!(french.display_name, "Français");
        assert!(options.iter().any(|option| option.tag == "ja"));

        // An unrelated change must not reinstall the catalog.
        let generation = active(cx).generation;
        set_user_settings("{ \"ui_language\": \"fr\", \"vim_mode\": true }", cx);
        assert_eq!(active(cx).generation, generation);
        assert_eq!(i18n::generation(), generation);

        set_user_settings("{ \"ui_language\": \"ja\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "ja");
        assert_eq!(i18n::tr("i18n_catalog test: Hello"), "こんにちは (同梱)");
        assert_eq!(
            i18n::tr("Some parts of the UI switch to the new language after a restart."),
            "一部の表示は再起動後に新しい言語に切り替わります。"
        );

        set_user_settings("{ \"ui_language\": \"zz\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "zz");
        assert_eq!(i18n::tr("i18n_settings test: Open"), "Zz");

        set_user_settings("{ \"ui_language\": \"xx-YY\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "en");
        assert_eq!(
            i18n::tr("i18n_settings test: Open"),
            "i18n_settings test: Open"
        );

        set_user_settings("{ \"ui_language\": \"../fr\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "en");

        set_user_settings("{ \"ui_language\": \"en\" }", cx);
        assert_eq!(active(cx).locale.as_ref(), "en");
    }

    #[test]
    fn problems_are_reported_for_unavailable_and_broken_languages() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        write(
            &directory.path().join("zz/broken.po"),
            "msgid \"a\"\nmsgstr \"b\"\n\nmsgid \"unterminated\nmsgstr \"x\"\n",
        );
        let resolve_tag =
            |tag: &str| resolve(&UiLanguage::Tag(tag.to_string()), Some(directory.path()));

        let (catalog, problems) = resolve_tag("zz");
        assert!(catalog.is_some());
        assert_eq!(problems.len(), 1);
        assert!(problems[0].contains("broken.po:4:"), "{problems:?}");

        let is_english =
            |catalog: Option<&PoCatalog>| catalog.is_some_and(|catalog| catalog.locale() == "en");

        let (catalog, problems) = resolve_tag("xx");
        assert!(is_english(catalog));
        assert_eq!(problems.len(), 1);

        let (catalog, problems) = resolve_tag("../zz");
        assert!(is_english(catalog));
        assert_eq!(problems.len(), 1);

        for english in ["en", "EN", "en_US", "en-GB"] {
            let (catalog, problems) = resolve_tag(english);
            assert!(is_english(catalog), "{english}");
            assert!(problems.is_empty(), "{english}: {problems:?}");
        }
    }
}
