use std::sync::Arc;

use anyhow::{Result, bail};
use collections::IndexMap;
use convert_case::{Case, Casing as _};
use fs::Fs;
use gpui::{App, SharedString};
use settings::{
    AgentProfileContent, ContextServerPresetContent, LanguageModelSelection, Settings as _,
    SettingsContent, SettingsStore, update_settings_file,
};
use util::ResultExt as _;

use crate::{AgentProfileId, AgentSettings};

pub mod builtin_profiles {
    use super::AgentProfileId;

    pub const WRITE: &str = "write";
    /// Reads and edits files, but runs no commands and reaches no network.
    pub const EDIT: &str = "edit";
    pub const ASK: &str = "ask";
    pub const MINIMAL: &str = "minimal";

    pub fn is_builtin(profile_id: &AgentProfileId) -> bool {
        matches!(profile_id.as_str(), WRITE | EDIT | ASK | MINIMAL)
    }

    // Earlier versions shipped these Japanese names, and a settings file that copied a built-in
    // profile still holds them verbatim, so they are compared as data.
    const EARLIER_DEFAULT_NAMES: [(&str, &str); 4] = [
        (WRITE, "書き込み"),
        (EDIT, "編集のみ"),
        (ASK, "質問"),
        (MINIMAL, "最小"),
    ];

    /// Whether `name` is the name an earlier version shipped for the built-in profile
    /// `profile_id`, which counts as that profile's default name.
    pub fn is_earlier_default_name(profile_id: &AgentProfileId, name: &str) -> bool {
        EARLIER_DEFAULT_NAMES
            .iter()
            .any(|(id, earlier_name)| *id == profile_id.as_str() && *earlier_name == name)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentProfile {
    id: AgentProfileId,
}

pub type AvailableProfiles = IndexMap<AgentProfileId, SharedString>;

impl AgentProfile {
    pub fn new(id: AgentProfileId) -> Self {
        Self { id }
    }

    pub fn id(&self) -> &AgentProfileId {
        &self.id
    }

    /// Saves a new profile to the settings.
    pub fn create(
        name: String,
        base_profile_id: Option<AgentProfileId>,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) -> AgentProfileId {
        let id = AgentProfileId(name.to_case(Case::Kebab).into());

        let base_profile =
            base_profile_id.and_then(|id| AgentSettings::get_global(cx).profiles.get(&id).cloned());

        // Copy toggles from the base profile so the new profile starts with familiar defaults.
        let tools = base_profile
            .as_ref()
            .map(|profile| profile.tools.clone())
            .unwrap_or_default();
        let enable_all_context_servers = base_profile
            .as_ref()
            .map(|profile| profile.enable_all_context_servers)
            .unwrap_or_default();
        let context_servers = base_profile
            .as_ref()
            .map(|profile| profile.context_servers.clone())
            .unwrap_or_default();
        // Preserve the base profile's model preference when cloning into a new profile.
        let default_model = base_profile
            .as_ref()
            .and_then(|profile| profile.default_model.clone());

        let profile_settings = AgentProfileSettings {
            name: name.into(),
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
        };

        update_settings_file(fs, cx, {
            let id = id.clone();
            move |settings, _cx| {
                profile_settings.save_to_settings(id, settings).log_err();
            }
        });

        id
    }

    /// Returns a map of AgentProfileIds to their names
    pub fn available_profiles(cx: &App) -> AvailableProfiles {
        let mut profiles = AvailableProfiles::default();
        for (id, profile) in AgentSettings::get_global(cx).profiles.iter() {
            profiles.insert(id.clone(), profile.name.clone());
        }
        profiles
    }
}

/// A profile for the Zed Agent that controls its behavior.
#[derive(Debug, Clone)]
pub struct AgentProfileSettings {
    /// The name of the profile.
    pub name: SharedString,
    pub tools: IndexMap<Arc<str>, bool>,
    pub enable_all_context_servers: bool,
    pub context_servers: IndexMap<Arc<str>, ContextServerPreset>,
    /// Default language model to apply when this profile becomes active.
    pub default_model: Option<LanguageModelSelection>,
}

impl AgentProfileSettings {
    pub fn is_tool_enabled(&self, tool_name: &str) -> bool {
        self.tools.get(tool_name) == Some(&true)
    }

    /// Whether the built-in profile with the given id still matches the shipped
    /// default — i.e. the user has neither customized the built-in profile nor
    /// shadowed it with a custom profile of the same id. Custom profile ids are
    /// never considered unmodified defaults.
    pub fn is_unmodified_default(profile_id: &AgentProfileId, cx: &App) -> bool {
        if !builtin_profiles::is_builtin(profile_id) {
            return false;
        }
        let store = cx.global::<SettingsStore>();
        let profile_in = |content: &SettingsContent| {
            content
                .agent
                .as_ref()
                .and_then(|agent| agent.profiles.as_ref())
                .and_then(|profiles| profiles.get(profile_id.as_str()))
                .cloned()
        };
        match (
            profile_in(store.merged_settings()),
            profile_in(store.raw_default_settings()),
        ) {
            (Some(mut merged), Some(default)) => {
                if builtin_profiles::is_earlier_default_name(profile_id, &merged.name) {
                    merged.name = default.name.clone();
                }
                merged == default
            }
            _ => false,
        }
    }

    pub fn is_context_server_tool_enabled(&self, server_id: &str, tool_name: &str) -> bool {
        self.context_servers
            .get(server_id)
            .and_then(|preset| preset.tools.get(tool_name).copied())
            .unwrap_or(self.enable_all_context_servers)
    }

    pub fn save_to_settings(
        &self,
        profile_id: AgentProfileId,
        content: &mut SettingsContent,
    ) -> Result<()> {
        let profiles = content
            .agent
            .get_or_insert_default()
            .profiles
            .get_or_insert_default();
        if profiles.contains_key(&profile_id.0) {
            bail!("profile with ID '{profile_id}' already exists");
        }

        profiles.insert(
            profile_id.0,
            AgentProfileContent {
                name: self.name.clone().into(),
                tools: self.tools.clone(),
                enable_all_context_servers: Some(self.enable_all_context_servers),
                context_servers: self
                    .context_servers
                    .clone()
                    .into_iter()
                    .map(|(server_id, preset)| {
                        (
                            server_id,
                            ContextServerPresetContent {
                                tools: preset.tools,
                            },
                        )
                    })
                    .collect(),
                default_model: self.default_model.clone(),
            },
        );

        Ok(())
    }
}

impl From<AgentProfileContent> for AgentProfileSettings {
    fn from(content: AgentProfileContent) -> Self {
        let AgentProfileContent {
            name,
            tools,
            enable_all_context_servers,
            context_servers,
            default_model,
        } = content;

        Self {
            name: name.into(),
            tools,
            enable_all_context_servers: enable_all_context_servers.unwrap_or_default(),
            context_servers: context_servers
                .into_iter()
                .map(|(server_id, preset)| (server_id, preset.into()))
                .collect(),
            default_model,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ContextServerPreset {
    pub tools: IndexMap<Arc<str>, bool>,
}

impl From<settings::ContextServerPresetContent> for ContextServerPreset {
    fn from(content: settings::ContextServerPresetContent) -> Self {
        Self {
            tools: content.tools,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(
        enable_all_context_servers: bool,
        context_servers: IndexMap<Arc<str>, ContextServerPreset>,
    ) -> AgentProfileSettings {
        AgentProfileSettings {
            name: "test".into(),
            tools: IndexMap::default(),
            enable_all_context_servers,
            context_servers,
            default_model: None,
        }
    }

    fn preset(tools: &[(&str, bool)]) -> ContextServerPreset {
        ContextServerPreset {
            tools: tools
                .iter()
                .map(|(name, enabled)| (Arc::from(*name), *enabled))
                .collect(),
        }
    }

    #[test]
    fn explicit_false_disables_tool_when_enable_all_is_true() {
        let mut servers = IndexMap::default();
        servers.insert(Arc::from("server"), preset(&[("disabled_tool", false)]));
        let profile = profile(true, servers);

        assert!(!profile.is_context_server_tool_enabled("server", "disabled_tool"));
        assert!(profile.is_context_server_tool_enabled("server", "other_tool"));
        assert!(profile.is_context_server_tool_enabled("other_server", "any_tool"));
    }

    #[test]
    fn explicit_true_enables_tool_when_enable_all_is_false() {
        let mut servers = IndexMap::default();
        servers.insert(Arc::from("server"), preset(&[("enabled_tool", true)]));
        let profile = profile(false, servers);

        assert!(profile.is_context_server_tool_enabled("server", "enabled_tool"));
        assert!(!profile.is_context_server_tool_enabled("server", "other_tool"));
        assert!(!profile.is_context_server_tool_enabled("other_server", "any_tool"));
    }

    /// 決定 4: the shipped "read and edit" profile. Its point is what it
    /// leaves out, so the disabled half is asserted too — a tool silently
    /// gaining a `true` here would hand the model a capability the profile
    /// promises it doesn't have.
    #[gpui::test]
    fn shipped_edit_profile_reads_and_edits_only(cx: &mut gpui::App) {
        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let edit_profile_id = AgentProfileId(builtin_profiles::EDIT.into());
        assert!(builtin_profiles::is_builtin(&edit_profile_id));
        assert!(AgentProfileSettings::is_unmodified_default(
            &edit_profile_id,
            cx
        ));

        let profile = AgentSettings::get_global(cx)
            .profiles
            .get(&edit_profile_id)
            .expect("the edit profile ships in default.json")
            .clone();

        for tool in [
            "read_file",
            "list_directory",
            "grep",
            "find_path",
            "diagnostics",
            "repo_map",
            "git_diff",
            "find_references",
            "go_to_definition",
            "get_code_actions",
            "edit_file",
            "write_file",
            "create_directory",
        ] {
            assert!(profile.is_tool_enabled(tool), "{tool} should be enabled");
        }
        for tool in [
            "terminal",
            "sandboxed_terminal",
            "fetch",
            "search_web",
            "delete_path",
            "move_path",
            "copy_path",
            "skill",
            "spawn_agent",
            "apply_code_action",
            "rename_symbol",
            "ask_user",
            "git",
        ] {
            assert!(!profile.is_tool_enabled(tool), "{tool} should be disabled");
        }
        assert!(
            !profile.enable_all_context_servers,
            "context server tools are not vetted for this profile"
        );
    }

    #[gpui::test]
    fn unmodified_default_detection(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let write = AgentProfileId(builtin_profiles::WRITE.into());
        let minimal = AgentProfileId(builtin_profiles::MINIMAL.into());
        let custom = AgentProfileId("custom".into());

        // Fresh defaults: the shipped built-in profiles are unmodified.
        assert!(AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(AgentProfileSettings::is_unmodified_default(&minimal, cx));
        // Custom (non-built-in) ids are never considered unmodified defaults.
        assert!(!AgentProfileSettings::is_unmodified_default(&custom, cx));

        // The user customizes the `write` profile; `minimal` stays untouched.
        SettingsStore::update_global(cx, |store, cx| {
            store
                .set_user_settings(
                    r#"{ "agent": { "profiles": { "write": { "name": "Write", "tools": { "fetch": false } } } } }"#,
                    cx,
                )
                .unwrap();
        });

        assert!(!AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(AgentProfileSettings::is_unmodified_default(&minimal, cx));
    }

    /// A user settings file holding a copy of the shipped profile `id`, as the tool picker writes
    /// one, under the name `name`.
    fn copy_of_default_profile(id: &str, name: &str, cx: &App) -> serde_json::Value {
        let mut profile = cx
            .global::<SettingsStore>()
            .raw_default_settings()
            .agent
            .as_ref()
            .and_then(|agent| agent.profiles.as_ref())
            .and_then(|profiles| profiles.get(id))
            .cloned()
            .expect("the profile ships in default.json");
        profile.name = name.into();
        serde_json::to_value(profile).expect("a profile serializes")
    }

    #[gpui::test]
    fn copies_named_by_an_earlier_version_still_count_as_defaults(cx: &mut gpui::App) {
        use gpui::UpdateGlobal as _;

        let store = SettingsStore::test(cx);
        cx.set_global(store);
        project::DisableAiSettings::register(cx);
        AgentSettings::register(cx);

        let write = AgentProfileId(builtin_profiles::WRITE.into());
        let ask = AgentProfileId(builtin_profiles::ASK.into());
        let minimal = AgentProfileId(builtin_profiles::MINIMAL.into());
        let set_user_profiles = |profiles: serde_json::Value, cx: &mut App| {
            let settings = serde_json::json!({ "agent": { "profiles": profiles } }).to_string();
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&settings, cx)
                    .expect("valid user settings");
            });
        };

        let profiles = serde_json::json!({
            "write": copy_of_default_profile("write", "書き込み", cx),
            "minimal": copy_of_default_profile("minimal", "最小", cx),
            // The name `write` used to have, on another built-in profile, is the user's own.
            "ask": copy_of_default_profile("ask", "書き込み", cx),
        });
        set_user_profiles(profiles, cx);
        assert!(AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(AgentProfileSettings::is_unmodified_default(&minimal, cx));
        assert!(!AgentProfileSettings::is_unmodified_default(&ask, cx));

        // A name the user chose, Japanese or not, still makes the profile their own.
        let profiles = serde_json::json!({
            "write": copy_of_default_profile("write", "書き込み (自分用)", cx),
            "minimal": copy_of_default_profile("minimal", "Bare", cx),
        });
        set_user_profiles(profiles, cx);
        assert!(!AgentProfileSettings::is_unmodified_default(&write, cx));
        assert!(!AgentProfileSettings::is_unmodified_default(&minimal, cx));

        // The rest of the profile is still compared exactly.
        let mut profiles = serde_json::json!({
            "write": copy_of_default_profile("write", "書き込み", cx),
        });
        profiles["write"]["tools"]["fetch"] = serde_json::json!(false);
        set_user_profiles(profiles, cx);
        assert!(!AgentProfileSettings::is_unmodified_default(&write, cx));
    }
}
