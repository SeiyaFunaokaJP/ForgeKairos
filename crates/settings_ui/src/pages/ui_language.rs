use gpui::{AnyElement, App, ReadGlobal as _, SharedString, Window};
use settings::{SettingsStore, UiLanguageName};
use ui::{
    ButtonSize, ContextMenu, DropdownMenu, DropdownStyle, FluentBuilder as _, IconPosition,
    IntoElement, px,
};
use util::ResultExt as _;

use crate::{SettingField, SettingsFieldMetadata, SettingsUiFile, update_settings_file};

const SYSTEM: &str = "system";
const ENGLISH: &str = "en";

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct LanguageChoice {
    /// What is written to `ui_language`.
    pub(crate) value: SharedString,
    pub(crate) label: SharedString,
}

/// Following the system, English (which needs no catalog), then every language
/// with PO files, bundled or in the user's directory, named in that language.
pub(crate) fn language_choices(cx: &App) -> Vec<LanguageChoice> {
    let mut choices = vec![
        LanguageChoice {
            value: SYSTEM.into(),
            label: i18n::tc!("settings", "Follow the System Language").into(),
        },
        LanguageChoice {
            value: ENGLISH.into(),
            label: "English".into(),
        },
    ];
    for locale in i18n_settings::language_options(cx) {
        if locale.tag.eq_ignore_ascii_case(ENGLISH) {
            continue;
        }
        choices.push(LanguageChoice {
            value: locale.tag.into(),
            label: locale.display_name.into(),
        });
    }
    choices
}

fn is_selected(choice: &LanguageChoice, current_value: &str) -> bool {
    let current_value = current_value.trim();
    let current_value = if current_value.is_empty() {
        SYSTEM
    } else {
        current_value
    };
    choice.value.eq_ignore_ascii_case(current_value)
}

/// The label of the chosen language, or the raw value when it names a language
/// that has no translation files (Zed then shows English and says so).
pub(crate) fn current_label(choices: &[LanguageChoice], current_value: &str) -> SharedString {
    choices
        .iter()
        .find(|choice| is_selected(choice, current_value))
        .map(|choice| choice.label.clone())
        .unwrap_or_else(|| SharedString::from(current_value.to_string()))
}

pub(crate) fn render_ui_language_dropdown(
    field: SettingField<UiLanguageName>,
    file: SettingsUiFile,
    _metadata: Option<&SettingsFieldMetadata>,
    title: &'static str,
    description: &'static str,
    window: &mut Window,
    cx: &mut App,
) -> AnyElement {
    let (_, current_value) =
        SettingsStore::global(cx).get_value_from_file(file.to_settings(), field.pick);
    let current_value = current_value
        .map(|name| name.0.clone())
        .unwrap_or_else(|| SYSTEM.to_string());

    // Listing the languages reads the user's directory, so it is done once per
    // language shown instead of on every frame.
    let language_key = format!("{}-{}", i18n::generation(), i18n::current_locale());
    let choices = window.use_keyed_state(
        SharedString::from(format!("ui-language-choices-{language_key}")),
        cx,
        |_, cx| language_choices(cx),
    );
    let choices = choices.read(cx).clone();
    let label = current_label(&choices, &current_value);

    let menu = window.use_keyed_state(
        SharedString::from(format!("ui-language-menu-{current_value}-{language_key}")),
        cx,
        move |window, cx| {
            ContextMenu::new(window, cx, move |mut menu, _, _| {
                for choice in choices {
                    let selected = is_selected(&choice, &current_value);
                    let file = file.clone();
                    menu = menu.toggleable_entry(
                        choice.label,
                        selected,
                        IconPosition::End,
                        None,
                        move |window, cx| {
                            if selected {
                                return;
                            }
                            let value = UiLanguageName(choice.value.to_string());
                            update_settings_file(
                                file.clone(),
                                field.json_path,
                                window,
                                cx,
                                move |settings, app| {
                                    (field.write)(settings, Some(value), app);
                                },
                            )
                            .log_err();
                        },
                    );
                }
                menu
            })
        },
    );

    DropdownMenu::new("ui-language-dropdown", label, menu)
        .style(DropdownStyle::Outlined)
        .aria_label(title)
        .when(!description.is_empty(), |this| this.aria_description(description))
        .tab_index(0)
        .trigger_size(ButtonSize::Medium)
        .offset(gpui::Point {
            x: px(0.0),
            y: px(2.0),
        })
        .into_any_element()
}
