use serde_json::Value;

const DESCRIPTION_KEYS: [&str; 2] = ["description", "markdownDescription"];

/// Keys of a schema whose values are data the user writes, not schemas, so a
/// `description` inside them is part of a value and stays as it is.
const DATA_KEYS: [&str; 4] = ["default", "examples", "const", "enum"];

/// Keys of a schema whose values map names (of settings, of definitions) to schemas, so
/// a setting named like a data key is still a schema.
const NAME_MAP_KEYS: [&str; 4] = ["properties", "patternProperties", "$defs", "definitions"];

/// Replaces the descriptions of a settings or keymap schema with their translations in
/// the display language, for the hover and completion text of the JSON language server.
///
/// An alternative whose `const` is an action name (`editor::Copy`) is described by the
/// action's doc comment, so it is translated like the keymap editor translates it, by
/// the action name; every other description is looked up by its English text.
pub fn localize_descriptions(schema: &mut Value) {
    if i18n::current_locale() == "en" {
        return;
    }
    localize_schema(schema);
}

fn localize_schema(value: &mut Value) {
    match value {
        Value::Object(map) => {
            let action_documentation = map
                .get("const")
                .and_then(Value::as_str)
                .filter(|name| name.contains("::"))
                .and_then(|action_name| {
                    i18n::tr_ctx_opt(i18n::context::ACTION_DOCUMENTATION, action_name)
                });
            for key in DESCRIPTION_KEYS {
                if let Some(Value::String(description)) = map.get_mut(key) {
                    let translation = action_documentation
                        .or_else(|| i18n::tr_ctx_opt(i18n::context::SETTINGS_SCHEMA, description));
                    if let Some(translation) = translation {
                        *description = translation.to_string();
                    }
                }
            }
            for (key, child) in map.iter_mut() {
                if NAME_MAP_KEYS.contains(&key.as_str()) {
                    if let Value::Object(schemas) = child {
                        schemas.values_mut().for_each(localize_schema);
                    }
                } else if !DATA_KEYS.contains(&key.as_str()) {
                    localize_schema(child);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(localize_schema),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i18n::Catalog as _;
    use i18n::test_support::{LocaleOverride, ThreadLocalInstalls};
    use serde_json::json;
    use std::time::Instant;

    const ICON_THEME: &str = "The name of the icon theme to use.";
    const ICON_THEME_JA: &str = "使用するアイコンテーマの名前です。";

    fn japanese() -> &'static i18n_catalog::PoCatalog {
        i18n_catalog::test_support::bundled_catalog("ja")
    }

    fn sample_schema() -> Value {
        json!({
            "description": ICON_THEME,
            "properties": {
                "default": { "markdownDescription": ICON_THEME },
                "icon_theme": {
                    "description": ICON_THEME,
                    "default": { "description": ICON_THEME },
                    "examples": [{ "description": ICON_THEME }],
                },
            },
            "$defs": {
                "ActionName": {
                    "anyOf": [
                        { "const": "editor::Copy", "description": "Copies selected text to the clipboard." },
                        { "const": "editor::NotAnAction", "description": ICON_THEME },
                    ],
                },
            },
        })
    }

    #[test]
    fn descriptions_follow_the_display_language_but_values_do_not() {
        let mut english = sample_schema();
        localize_descriptions(&mut english);
        assert_eq!(english, sample_schema());

        let _locale = LocaleOverride::new(Some(japanese()));
        let mut schema = sample_schema();
        localize_descriptions(&mut schema);
        assert_eq!(
            schema,
            json!({
                "description": ICON_THEME_JA,
                "properties": {
                    "default": { "markdownDescription": ICON_THEME_JA },
                    "icon_theme": {
                        "description": ICON_THEME_JA,
                        "default": { "description": ICON_THEME },
                        "examples": [{ "description": ICON_THEME }],
                    },
                },
                "$defs": {
                    "ActionName": {
                        "anyOf": [
                            { "const": "editor::Copy", "description": "選択テキストをクリップボードにコピーします。" },
                            { "const": "editor::NotAnAction", "description": ICON_THEME_JA },
                        ],
                    },
                },
            })
        );
    }

    /// On the real schemas, with languages, fonts and themes named like schema keywords or like
    /// translated descriptions, only the text of descriptions may change: a translated value
    /// (an `enum` or `const` the user writes, a `default`) or a translated name would make the
    /// language server reject or suggest settings that Zed does not read.
    #[test]
    fn only_descriptions_change_in_the_real_schemas() {
        let tricky_names = [
            "description".to_string(),
            "default".to_string(),
            "enum".to_string(),
            ICON_THEME.to_string(),
        ];
        let tricky_theme_names = tricky_names
            .iter()
            .map(|name| gpui::SharedString::from(name.clone()))
            .collect::<Vec<_>>();
        let action_documentation = collections::HashMap::from_iter([("editor::Copy", ICON_THEME)]);
        let settings_schema =
            settings::SettingsStore::json_schema(&settings::SettingsJsonSchemaParams {
                language_names: &tricky_names,
                font_names: &tricky_names,
                theme_names: &tricky_theme_names,
                icon_theme_names: &tricky_theme_names,
                lsp_adapter_names: &tricky_names,
                action_names: &["editor::Copy", ICON_THEME],
                action_documentation: &action_documentation,
                deprecations: &collections::HashMap::default(),
                deprecation_messages: &collections::HashMap::default(),
            });
        let keymap_schema = settings::KeymapFile::generate_json_schema_from_inventory();

        let _locale = LocaleOverride::new(Some(japanese()));
        for english in [settings_schema, keymap_schema] {
            let mut localized = english.clone();
            localize_descriptions(&mut localized);
            let mut changed = 0;
            compare_outside_descriptions(&english, &localized, false, "#", &mut changed);
            assert!(changed > 0, "nothing was translated");

            let reparsed: Value = serde_json::from_str(
                &serde_json::to_string(&localized).expect("the localized schema serializes"),
            )
            .expect("the localized schema is valid JSON");
            assert_eq!(reparsed, localized);
        }
    }

    fn compare_outside_descriptions(
        english: &Value,
        localized: &Value,
        names_schemas: bool,
        path: &str,
        changed: &mut usize,
    ) {
        match (english, localized) {
            (Value::Object(english_map), Value::Object(localized_map)) => {
                assert_eq!(
                    english_map.keys().collect::<Vec<_>>(),
                    localized_map.keys().collect::<Vec<_>>(),
                    "keys changed at {path}"
                );
                for (key, english_child) in english_map {
                    let localized_child = &localized_map[key];
                    let child_path = format!("{path}/{key}");
                    if names_schemas {
                        compare_outside_descriptions(
                            english_child,
                            localized_child,
                            false,
                            &child_path,
                            changed,
                        );
                    } else if DATA_KEYS.contains(&key.as_str()) {
                        assert_eq!(
                            english_child, localized_child,
                            "a value changed at {child_path}"
                        );
                    } else if DESCRIPTION_KEYS.contains(&key.as_str()) && english_child.is_string()
                    {
                        assert!(localized_child.is_string(), "{child_path}");
                        if english_child != localized_child {
                            *changed += 1;
                        }
                    } else {
                        compare_outside_descriptions(
                            english_child,
                            localized_child,
                            NAME_MAP_KEYS.contains(&key.as_str()),
                            &child_path,
                            changed,
                        );
                    }
                }
            }
            (Value::Array(english_items), Value::Array(localized_items)) => {
                assert_eq!(english_items.len(), localized_items.len(), "{path}");
                for (index, (english_item, localized_item)) in
                    english_items.iter().zip(localized_items).enumerate()
                {
                    compare_outside_descriptions(
                        english_item,
                        localized_item,
                        false,
                        &format!("{path}/{index}"),
                        changed,
                    );
                }
            }
            _ => assert_eq!(english, localized, "a value changed at {path}"),
        }
    }

    /// The bundled `_settings_schema.po` is keyed by the schema's own descriptions, so most of
    /// them must be found; a change in how the schema is built or walked would leave them all
    /// English. Also reports how long the replacement takes, since the schema is rebuilt on
    /// every settings change.
    #[test]
    fn bundled_translations_cover_the_settings_schema() {
        let schema = settings::SettingsStore::json_schema(&settings::SettingsJsonSchemaParams {
            language_names: &[],
            font_names: &[],
            theme_names: &[],
            icon_theme_names: &[],
            lsp_adapter_names: &[],
            action_names: &[],
            action_documentation: &collections::HashMap::default(),
            deprecations: &collections::HashMap::default(),
            deprecation_messages: &collections::HashMap::default(),
        });
        let mut descriptions = Vec::new();
        collect_descriptions(&schema, &mut descriptions);

        let _locale = LocaleOverride::new(Some(japanese()));
        let mut localized = schema.clone();
        let started = Instant::now();
        localize_descriptions(&mut localized);
        let elapsed = started.elapsed();
        let mut localized_descriptions = Vec::new();
        collect_descriptions(&localized, &mut localized_descriptions);

        assert_eq!(descriptions.len(), localized_descriptions.len());
        let translated = descriptions
            .iter()
            .zip(&localized_descriptions)
            .filter(|(english, localized)| english != localized)
            .count();
        eprintln!(
            "translated {translated} of {} descriptions in {elapsed:?}",
            descriptions.len()
        );
        assert!(
            translated * 10 >= descriptions.len() * 9,
            "only {translated} of {} descriptions are translated",
            descriptions.len()
        );
    }

    fn collect_descriptions(value: &Value, descriptions: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for key in DESCRIPTION_KEYS {
                    if let Some(Value::String(description)) = map.get(key) {
                        descriptions.push(description.clone());
                    }
                }
                map.values()
                    .for_each(|child| collect_descriptions(child, descriptions));
            }
            Value::Array(items) => items
                .iter()
                .for_each(|item| collect_descriptions(item, descriptions)),
            _ => {}
        }
    }

    /// The language server keeps the schemas it was given, so after a language change it
    /// has to be told about every schema whose descriptions are translated, including the
    /// keymap's, which is not in the cache of dynamic schemas.
    #[test]
    fn a_language_change_reloads_every_localized_schema() {
        let settings_uri = format!(
            "{}settings/lsp/a-test-only-server",
            crate::SCHEMA_URI_PREFIX
        );
        crate::DYNAMIC_SCHEMA_CACHE
            .write()
            .insert(settings_uri.clone(), "{}".to_string());

        let reloaded = crate::invalidate_localized_schemas();

        assert!(reloaded.contains(&settings_uri), "{reloaded:?}");
        assert!(
            reloaded.contains(&format!("{}keymap", crate::SCHEMA_URI_PREFIX)),
            "{reloaded:?}"
        );
        assert!(
            !crate::DYNAMIC_SCHEMA_CACHE
                .read()
                .contains_key(&settings_uri)
        );
    }

    #[test]
    fn keymap_schema_is_translated_again_when_the_language_changes() {
        let _installs = ThreadLocalInstalls::new();
        let (english, japanese_text) = gpui::generate_list_of_all_registered_actions()
            .find_map(|action| {
                let english = action.documentation?;
                let translation =
                    japanese().lookup(Some(i18n::context::ACTION_DOCUMENTATION), action.name)?;
                (translation != english && !english.contains('"') && !english.contains('\n'))
                    .then_some((english, translation))
            })
            .expect("an action with a Japanese description is linked into this test");

        i18n::install(Some(japanese()));
        let schema = crate::localized_keymap_schema();
        assert!(schema.contains(japanese_text), "{japanese_text}");

        i18n::install(None);
        let schema = crate::localized_keymap_schema();
        assert!(!schema.contains(japanese_text), "{japanese_text}");
        assert!(schema.contains(english), "{english}");
    }
}
