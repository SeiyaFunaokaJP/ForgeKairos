//! The two `_` files whose English is not in the sources as a literal: `_action_docs.po`
//! translates the doc comments of actions under their names, and `_settings_schema.po` the
//! descriptions of the settings file's JSON schema under their English text. `check` compares
//! them with what Zed describes now, read from `zed --dump-all-actions` and
//! `zed --dump-settings-schema`.

use anyhow::{Context as _, Result};
use serde::Deserialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub const ACTION_DOCS_STEM: &str = "_action_docs";
pub const SETTINGS_SCHEMA_STEM: &str = "_settings_schema";

/// Action name → its doc comment, as `zed --dump-all-actions` prints them.
pub fn read_action_documentation(path: &Path) -> Result<BTreeMap<String, String>> {
    #[derive(Deserialize)]
    struct Dump {
        actions: Vec<Action>,
    }
    #[derive(Deserialize)]
    struct Action {
        name: String,
        documentation: Option<String>,
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let dump: Dump = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not the output of zed --dump-all-actions",
            path.display()
        )
    })?;
    Ok(dump
        .actions
        .into_iter()
        .filter_map(|action| Some((action.name, action.documentation?)))
        .collect())
}

/// The descriptions in the argument schemas of `zed --dump-all-actions`. The keymap file's
/// schema holds them, and `json_schema_store` translates them by their English text like the
/// settings schema's, so their entries live in `_settings_schema.po` too.
pub fn read_action_argument_descriptions(path: &Path) -> Result<BTreeSet<String>> {
    #[derive(Deserialize)]
    struct Dump {
        actions: Vec<Action>,
        // The argument schemas refer to the types they share (`#/$defs/<name>`) rather than
        // holding them, so those types' descriptions are only here.
        #[serde(default)]
        schema_definitions: serde_json::Map<String, Value>,
    }
    #[derive(Deserialize)]
    struct Action {
        schema: Option<Value>,
    }
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let dump: Dump = serde_json::from_str(&text).with_context(|| {
        format!(
            "{} is not the output of zed --dump-all-actions",
            path.display()
        )
    })?;
    let mut descriptions = BTreeSet::new();
    for schema in dump
        .actions
        .iter()
        .filter_map(|action| action.schema.as_ref())
    {
        collect_descriptions(schema, &mut descriptions);
    }
    for definition in dump.schema_definitions.values() {
        collect_descriptions(definition, &mut descriptions);
    }
    Ok(descriptions)
}

/// Every description of a JSON schema file, as `json_schema_store` translates them.
pub fn read_schema_descriptions(path: &Path) -> Result<BTreeSet<String>> {
    let text = fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let schema: Value = serde_json::from_str(&text)
        .with_context(|| format!("{} is not a JSON schema", path.display()))?;
    let mut descriptions = BTreeSet::new();
    collect_descriptions(&schema, &mut descriptions);
    // Another JSON file given by mistake would otherwise report every entry as obsolete.
    anyhow::ensure!(
        !descriptions.is_empty(),
        "{} has no descriptions; is it the output of zed --dump-settings-schema?",
        path.display()
    );
    Ok(descriptions)
}

// The same walk as `localized_schema` in `json_schema_store`: values under these keys are data,
// and the values of these maps are schemas whatever their names.
const DESCRIPTION_KEYS: [&str; 2] = ["description", "markdownDescription"];
const DATA_KEYS: [&str; 4] = ["default", "examples", "const", "enum"];
const NAME_MAP_KEYS: [&str; 4] = ["properties", "patternProperties", "$defs", "definitions"];

fn collect_descriptions(value: &Value, descriptions: &mut BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            let names_an_action = map
                .get("const")
                .and_then(Value::as_str)
                .is_some_and(|name| name.contains("::"));
            if !names_an_action {
                for key in DESCRIPTION_KEYS {
                    if let Some(Value::String(description)) = map.get(key)
                        && !description.is_empty()
                    {
                        descriptions.insert(description.clone());
                    }
                }
            }
            for (key, child) in map {
                if NAME_MAP_KEYS.contains(&key.as_str()) {
                    if let Value::Object(schemas) = child {
                        for schema in schemas.values() {
                            collect_descriptions(schema, descriptions);
                        }
                    }
                } else if !DATA_KEYS.contains(&key.as_str()) {
                    collect_descriptions(child, descriptions);
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_descriptions(item, descriptions);
            }
        }
        _ => {}
    }
}

/// The English an entry of `_action_docs.po` was translated from: its `#.` comments, one per
/// line of the doc comment.
pub fn english_original(comments: &[String]) -> Option<String> {
    let lines: Vec<&str> = comments
        .iter()
        .filter_map(|comment| comment.strip_prefix("#."))
        .map(|line| line.strip_prefix(' ').unwrap_or(line))
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

/// The new description that most likely replaced `old`: the one sharing the largest part of
/// its words, if that is at least half of them.
pub fn closest_replacement<'a>(
    old: &str,
    candidates: impl IntoIterator<Item = &'a String>,
) -> Option<&'a String> {
    let old_words = words(old);
    if old_words.is_empty() {
        return None;
    }
    candidates
        .into_iter()
        .filter_map(|candidate| {
            let new_words = words(candidate);
            let shared = old_words.intersection(&new_words).count();
            let all = old_words.union(&new_words).count();
            (all > 0 && shared * 2 >= all).then_some((shared * 1000 / all, candidate))
        })
        .max_by_key(|(score, _)| *score)
        .map(|(_, candidate)| candidate)
}

fn words(text: &str) -> BTreeSet<String> {
    text.split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn descriptions_skip_data_and_action_names_but_not_settings_named_like_data() {
        let schema = serde_json::json!({
            "description": "Root",
            "properties": {
                "default": { "description": "A setting named default" },
                "tab_size": {
                    "description": "Tab size",
                    "default": { "description": "Part of a default value" },
                    "examples": [{ "description": "Part of an example" }],
                },
            },
            "$defs": {
                "ActionName": {
                    "anyOf": [
                        { "const": "editor::Copy", "description": "Copy the selection" },
                        { "const": "all", "description": "Everything" },
                    ],
                },
            },
            "allOf": [{ "markdownDescription": "In markdown" }],
        });
        let mut descriptions = BTreeSet::new();
        collect_descriptions(&schema, &mut descriptions);
        assert_eq!(
            descriptions.into_iter().collect::<Vec<_>>(),
            vec![
                "A setting named default",
                "Everything",
                "In markdown",
                "Root",
                "Tab size",
            ]
        );
    }

    #[test]
    fn a_schema_without_descriptions_is_refused() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let actions_dump = directory.path().join("actions.json");
        fs::write(
            &actions_dump,
            r#"{"actions": [{"name": "editor::Copy", "documentation": "Copy."}]}"#,
        )
        .expect("write");
        assert!(read_schema_descriptions(&actions_dump).is_err());

        let schema = directory.path().join("schema.json");
        fs::write(&schema, r#"{"properties": {"a": {"description": "A"}}}"#).expect("write");
        assert_eq!(
            read_schema_descriptions(&schema).expect("a schema").len(),
            1
        );
    }

    #[test]
    fn argument_descriptions_are_read_from_the_actions_dump() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let actions_dump = directory.path().join("actions.json");
        fs::write(
            &actions_dump,
            r##"{"actions": [
                {"name": "editor::Copy", "documentation": "Copy.", "schema": null},
                {"name": "git::Reset", "documentation": "Reset.", "schema": {
                    "properties": {"sha": {"description": "The SHA.", "default": ""}},
                    "$defs": {"Mode": {"oneOf": [{"const": "soft", "description": "Keep."}]}}
                }},
                {"name": "git::ResetHard", "documentation": "Reset.", "schema": {
                    "properties": {"mode": {"$ref": "#/$defs/SharedMode"}}
                }}
            ],
            "schema_definitions": {
                "SharedMode": {"oneOf": [{"const": "hard", "description": "Discard."}]}
            }}"##,
        )
        .expect("write");
        assert_eq!(
            read_action_argument_descriptions(&actions_dump)
                .expect("an actions dump")
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["Discard.", "Keep.", "The SHA."]
        );
    }

    #[test]
    fn english_originals_are_read_from_comments() {
        let comments: Vec<String> = ["#. First line", "#.", "#. - an item", "#.  indented"]
            .into_iter()
            .map(str::to_string)
            .collect();
        assert_eq!(
            english_original(&comments).as_deref(),
            Some("First line\n\n- an item\n indented")
        );
        assert_eq!(english_original(&["# translator".to_string()]), None);
    }

    #[test]
    fn replacements_need_half_of_the_words() {
        let candidates = vec![
            "Whether to show the tab bar in the editor.".to_string(),
            "Something else entirely.".to_string(),
        ];
        assert_eq!(
            closest_replacement("Whether to show the tab bar.", &candidates),
            Some(&candidates[0])
        );
        assert_eq!(
            closest_replacement("Unrelated text here.", &candidates),
            None
        );
        // A quarter of the words in common is a different description, not a reworded one.
        assert_eq!(
            closest_replacement(
                "Show the tab bar.",
                &["Hide the tab strip in editors.".to_string()]
            ),
            None
        );
    }
}
