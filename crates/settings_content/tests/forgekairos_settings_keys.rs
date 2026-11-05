//! The settings structs that deserialize through `flattened_deserialize!` read each key by the
//! name written in the macro invocation, not by the field's serde attributes. A field placed in
//! `sections` without `#[serde(flatten)]`, or whose JSON key differs from its name without an
//! `as "key"` (the fork's `remote_host` is written as `"remote"`), still compiles and is then
//! silently never read. Upstream introduced the macro in v1.19, after the fork had added fields
//! to these structs. The JSON schema is derived from the serde attributes, so it is the reference
//! for which keys exist.

use std::collections::BTreeSet;

use settings_content::{
    AllLanguageSettingsContent, EditorSettingsContent, ExtensionSettingsContent,
    LanguageSettingsContent, ParseStatus, PlatformOverrides, ProjectSettingsContent,
    ProjectTerminalSettingsContent, ReleaseChannelOverrides, RemoteSettingsContent,
    SettingsContent, TerminalSettingsContent, ThemeSettingsContent, UserSettingsContent,
    WorkspaceSettingsContent, WorktreeSettingsContent,
};

fn property_names<T: schemars::JsonSchema>() -> BTreeSet<String> {
    let schema = schemars::schema_for!(T);
    let value = serde_json::to_value(&schema).expect("schema serializes");
    value
        .get("properties")
        .and_then(|properties| properties.as_object())
        .map(|object| object.keys().cloned().collect())
        .unwrap_or_default()
}

fn assert_keys_match(
    container: &str,
    schema_keys: BTreeSet<String>,
    named_keys: &[&str],
    sections: Vec<BTreeSet<String>>,
) {
    let mut read_keys: BTreeSet<String> = named_keys.iter().map(|key| key.to_string()).collect();
    for section in sections {
        read_keys.extend(section);
    }
    let never_read: Vec<_> = schema_keys.difference(&read_keys).collect();
    let not_in_schema: Vec<_> = read_keys.difference(&schema_keys).collect();
    assert!(
        never_read.is_empty() && not_in_schema.is_empty(),
        "{container}: the keys read by `flattened_deserialize!` differ from the keys in the \
         struct's JSON schema.\n  in the schema but never read: {never_read:?}\n  read but not \
         in the schema: {not_in_schema:?}\nA field is missing from `options`/`defaults`, sits in \
         `sections` without `#[serde(flatten)]`, or has a `#[serde(rename)]` without a matching \
         `as \"key\"` in the macro invocation."
    );
}

#[test]
fn flattened_deserialize_reads_every_key_of_the_schema() {
    assert_keys_match(
        "SettingsContent",
        property_names::<SettingsContent>(),
        SettingsContent::NAMED_DESERIALIZE_KEYS,
        vec![
            property_names::<ProjectSettingsContent>(),
            property_names::<ThemeSettingsContent>(),
            property_names::<ExtensionSettingsContent>(),
            property_names::<WorkspaceSettingsContent>(),
            property_names::<EditorSettingsContent>(),
            property_names::<RemoteSettingsContent>(),
        ],
    );
    assert_keys_match(
        "UserSettingsContent",
        property_names::<UserSettingsContent>(),
        UserSettingsContent::NAMED_DESERIALIZE_KEYS,
        vec![
            property_names::<SettingsContent>(),
            property_names::<ReleaseChannelOverrides>(),
            property_names::<PlatformOverrides>(),
        ],
    );
    assert_keys_match(
        "ProjectSettingsContent",
        property_names::<ProjectSettingsContent>(),
        ProjectSettingsContent::NAMED_DESERIALIZE_KEYS,
        vec![
            property_names::<AllLanguageSettingsContent>(),
            property_names::<WorktreeSettingsContent>(),
        ],
    );
    assert_keys_match(
        "AllLanguageSettingsContent",
        property_names::<AllLanguageSettingsContent>(),
        AllLanguageSettingsContent::NAMED_DESERIALIZE_KEYS,
        vec![property_names::<LanguageSettingsContent>()],
    );
    assert_keys_match(
        "TerminalSettingsContent",
        property_names::<TerminalSettingsContent>(),
        TerminalSettingsContent::NAMED_DESERIALIZE_KEYS,
        vec![property_names::<ProjectTerminalSettingsContent>()],
    );
}

fn parse(json: &str) -> (SettingsContent, ParseStatus) {
    let (settings, status) = settings_content::parse_json::<SettingsContent>(json);
    (settings.expect("the document is an object"), status)
}

#[test]
fn fork_settings_are_read_from_their_keys() {
    let (settings, status) = parse(
        r#"{
            "acknowledged_external_ai_providers": ["anthropic", "edit_prediction:copilot"],
            "ui_language": "en",
            "ide_server": { "enabled": true, "share_task_output": false },
            "remote": { "relay_http_through_client": false },
            "read_ssh_config": false,
            "browser": { "starts_open": true },
            "tabular_data": { "open_as_grid": false },
            "dev_tools": { "suggest_missing": false, "ignored": ["clangd"] },
            "terminal": {
                "session_log": { "auto": true },
                "remote_persistence": { "detached_timeout_hours": 3 },
                "macro_picker_folders_collapsed": true
            },
            "agent": {
                "sandbox_permissions": { "allow_unsandboxed": false, "allow_outside_sandbox": true }
            },
            "private_files": ["**/.env"]
        }"#,
    );
    assert!(matches!(status, ParseStatus::Success), "{status:?}");
    assert_eq!(
        settings.acknowledged_external_ai_providers,
        Some(vec![
            "anthropic".to_string(),
            "edit_prediction:copilot".to_string()
        ])
    );
    assert_eq!(
        settings.ui_language.map(|language| language.0),
        Some("en".into())
    );
    let ide_server = settings.ide_server.expect("ide_server is read");
    assert_eq!(ide_server.enabled, Some(true));
    assert_eq!(ide_server.share_task_output, Some(false));
    assert_eq!(
        settings
            .remote_host
            .expect("`remote` is read into remote_host")
            .relay_http_through_client,
        Some(false)
    );
    // The same top-level key space also feeds the flattened connection lists.
    assert_eq!(settings.remote.read_ssh_config, Some(false));
    assert_eq!(settings.browser.expect("browser").starts_open, Some(true));
    assert_eq!(
        settings.tabular_data.expect("tabular_data").open_as_grid,
        Some(false)
    );
    let dev_tools = settings.project.dev_tools.expect("dev_tools");
    assert_eq!(dev_tools.suggest_missing, Some(false));
    assert_eq!(dev_tools.ignored, Some(vec!["clangd".to_string()]));
    let terminal = settings.terminal.expect("terminal");
    assert_eq!(terminal.session_log.expect("session_log").auto, Some(true));
    assert_eq!(
        terminal
            .remote_persistence
            .expect("remote_persistence")
            .detached_timeout_hours,
        Some(3)
    );
    assert_eq!(terminal.macro_picker_folders_collapsed, Some(true));
    assert_eq!(
        settings
            .agent
            .as_ref()
            .and_then(|agent| agent.sandbox_permissions.as_ref())
            .and_then(|permissions| permissions.allow_outside_sandbox),
        Some(true)
    );
    assert_eq!(
        settings
            .agent
            .expect("agent")
            .sandbox_permissions
            .expect("sandbox_permissions")
            .allow_unsandboxed,
        Some(false)
    );
    assert_eq!(
        settings.project.worktree.private_files.map(|files| files.0),
        Some(vec!["**/.env".to_string()])
    );
}

#[test]
fn fork_settings_are_written_back_under_the_same_keys() {
    let (settings, _) = parse(
        r#"{
            "acknowledged_external_ai_providers": ["anthropic"],
            "remote": { "relay_http_through_client": false },
            "ui_language": "ja"
        }"#,
    );
    let written = serde_json::to_value(&settings).expect("settings serialize");
    assert_eq!(
        written["acknowledged_external_ai_providers"],
        serde_json::json!(["anthropic"])
    );
    assert_eq!(
        written["remote"],
        serde_json::json!({ "relay_http_through_client": false })
    );
    assert_eq!(written["ui_language"], serde_json::json!("ja"));
    assert!(written.get("remote_host").is_none(), "{written}");

    let (read_again, status) = parse(&written.to_string());
    assert!(matches!(status, ParseStatus::Success), "{status:?}");
    assert_eq!(read_again, settings);
}

#[test]
fn a_consent_list_of_the_wrong_type_reads_as_not_consented() {
    let (settings, status) = parse(
        r#"{
            "acknowledged_external_ai_providers": "anthropic",
            "ide_server": { "enabled": "yes" },
            "vim_mode": true
        }"#,
    );
    assert!(matches!(status, ParseStatus::Failed { .. }), "{status:?}");
    assert_eq!(settings.acknowledged_external_ai_providers, None);
    assert_eq!(settings.ide_server.and_then(|server| server.enabled), None);
    // One bad value does not throw away the rest of the file.
    assert_eq!(settings.vim_mode, Some(true));
}
