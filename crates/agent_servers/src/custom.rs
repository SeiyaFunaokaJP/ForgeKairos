use crate::{AgentServer, AgentServerDelegate, load_proxy_env};
use acp_thread::AgentConnection;
use agent_client_protocol::schema::v1 as acp;
use anyhow::{Context as _, Result};
use collections::{HashMap, HashSet};
use fs::Fs;
use gpui::{App, AppContext as _, Entity, Task};
use language_model::{ApiKey, EnvVar};
use project::{
    Project,
    agent_server_store::{AgentId, AgentLaunchTarget, AllAgentServersSettings},
};
use settings::{
    AgentConfigOptionValue, SettingsStore, update_settings_file,
    update_settings_file_with_completion,
};
use std::{rc::Rc, sync::Arc};
use ui::IconName;

pub const GEMINI_ID: &str = "gemini";
pub const CLAUDE_AGENT_ID: &str = "claude-acp";
pub const CODEX_ID: &str = "codex-acp";
pub const CURSOR_ID: &str = "cursor";

/// ADR-F53: the ACP Claude agent's own sign-in, which opens `/login` in a
/// terminal and stores a Claude subscription's credentials.
pub const CLAUDE_SUBSCRIPTION_AUTH_METHOD: &str = "claude-login";

/// ADR-F53 決定 4: API キーが ACP Claude のエージェントへ届く唯一の変数。
pub const CLAUDE_API_KEY_VARIABLE: &str = "ANTHROPIC_API_KEY";

/// ADR-F55: `agent_servers.<id>.allow_subscription_sign_in` の既定。
/// 既定で許すのは、サインインを行うのが**未改変の CLI 自身**であり、この IDE は
/// トークンを見も保存も中継もしないため (ADR-F55 の根拠を参照)。
pub const SUBSCRIPTION_SIGN_IN_ALLOWED_BY_DEFAULT: bool = true;

/// ADR-F53 決定 4: そのエージェントが環境変数から読む API キーの変数名。
/// 読まないエージェントでは `None`。キーそのものは OS の資格情報ストアに
/// 置き、起動のたびにこの変数として渡す (ADR-F88)。
///
/// [`subscription_sign_in_disabled`] とは別物: こちらは「キーを受け取れるか」
/// だけを答え、「サブスクリプションでサインインできるか」には触れない。キーを
/// 入れる UI はこちらだけを見るので、サインインの可否が変わっても入力欄は残る。
pub fn api_key_variable(agent_id: &str) -> Option<&'static str> {
    (agent_id == CLAUDE_AGENT_ID).then_some(CLAUDE_API_KEY_VARIABLE)
}

/// ADR-F55: `method_id` がサブスクリプションでのサインインに見えるか
/// (設定は見ない純関数)。
///
/// ベンダーがメソッドを増やしたり改名したりしても取りこぼさないよう、鍵では
/// なくサインインに読める id は広めに拾う。この判定が効くのは
/// [`subscription_sign_in_disabled`] が真のときだけなので、広く拾って困るのは
/// 「切ったのに残る」場合ではなく「切ったのに落ちない」場合だけ = fail-closed。
///
/// 大文字・小文字と区切り文字はベンダーの書き方次第なので、比べる前にそろえる:
/// `Login` や `claude_login` を取りこぼすと、利用者が切ったはずの設定が黙って
/// 効かなくなる。
pub fn looks_like_subscription_sign_in(method_id: &str) -> bool {
    let normalized = method_id.to_ascii_lowercase().replace('_', "-");
    normalized.contains("login")
        || normalized.contains("oauth")
        || normalized.contains("subscription")
        || normalized.contains("sign-in")
        || normalized.contains("signin")
}

/// ADR-F55: ユーザーが `agent_servers.<agent_id>.allow_subscription_sign_in` を
/// `false` にしているか。
///
/// 既定は「許す」。ACP の Claude で使われるサインインは、未改変の `claude`
/// (Agent SDK) が自分の `/login` を Anthropic のフローで行うもので、この IDE は
/// 認証情報を見ない・保存しない・中継しない。設定を `false` にすると、その
/// エージェントのサインインは認証カードから消え、別経路で要求されても断られる。
pub fn subscription_sign_in_disabled(agent_id: &str, cx: &App) -> bool {
    let allowed = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id)
            .and_then(|agent| agent.allow_subscription_sign_in())
            .unwrap_or(SUBSCRIPTION_SIGN_IN_ALLOWED_BY_DEFAULT)
    });
    !allowed
}

/// ADR-F55: `agent_id` のこのメソッドを、サブスクリプションのサインインとして
/// 断るか。設定で切られているときだけ真になる。
pub fn is_refused_subscription_auth_method(agent_id: &str, method_id: &str, cx: &App) -> bool {
    looks_like_subscription_sign_in(method_id) && subscription_sign_in_disabled(agent_id, cx)
}

/// ADR-F55: `agent_servers.<agent_id>.allow_subscription_sign_in` を書く。
/// 設定 GUI と認証カードのどちらから切り替えても同じ 1 か所に落ちる。
pub fn set_subscription_sign_in_allowed(
    agent_id: &str,
    allowed: bool,
    fs: Arc<dyn Fs>,
    cx: &App,
) -> Task<Result<()>> {
    let agent_id = agent_id.to_string();
    let written = update_settings_file_with_completion(fs, cx, move |settings, _cx| {
        let settings = settings
            .agent_servers
            .get_or_insert_default()
            .entry(agent_id)
            .or_insert_with(default_settings_for_agent);

        match settings {
            settings::CustomAgentServerSettings::Custom {
                allow_subscription_sign_in,
                ..
            }
            | settings::CustomAgentServerSettings::Registry {
                allow_subscription_sign_in,
                ..
            } => {
                *allow_subscription_sign_in = Some(allowed);
            }
        }
    });
    cx.background_spawn(async move {
        written
            .await
            .context(i18n::t!("Writing to the settings file didn't finish"))?
    })
}

/// ADR-F88: the entry of the operating system's credential store an agent's
/// API key is kept under. Spelled so that it can be told apart in the store's
/// own viewer (Windows Credential Manager prefixes it with `zed:url=`); the
/// entry's user name is the variable.
pub fn agent_api_key_credentials_url(agent_id: &str, variable: &str) -> String {
    format!(
        "{}://agent-servers/{agent_id}/env/{variable}",
        branding::CURRENT.url_scheme
    )
}

/// ADR-F88: the name of the credential store the card saves API keys in, as
/// the user knows it on this platform.
pub fn credential_store_name() -> &'static str {
    if cfg!(target_os = "windows") {
        i18n::t!("Windows Credential Manager")
    } else if cfg!(target_os = "macos") {
        i18n::t!("macOS Keychain")
    } else {
        i18n::t!("the system keyring (Secret Service)")
    }
}

/// 1 行の入力欄は貼り付けられた改行を畳んで見せるだけで、中身には残す。
/// 端を落としただけでは改行やタブが値に残り、保存は通るのにエージェントへは
/// 壊れた環境が渡る (NUL ならプロセスの起動自体が失敗する)。空の値は
/// 「消したい」とも「設定したい」とも取れる。どちらも黙って書かずに断る
/// (fail-closed)。
fn validated_api_key<'a>(name: &str, value: &'a str) -> Result<&'a str> {
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), i18n::t!("{name} has no value"));
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        i18n::t!("The value of {name} contains a line break or a control character")
    );
    Ok(value)
}

/// ADR-F88: saves the API key `agent_id` reads ([`api_key_variable`]) in the
/// operating system's credential store, and [`CustomAgentServer::connect`]
/// hands it to the agent from there on every route (this host, the WSL
/// sandbox, a remote host).
///
/// The input is checked before anything is written; the write itself is
/// asynchronous, so whether it took is the returned task's to answer.
pub fn save_agent_api_key(
    agent_id: &str,
    value: &str,
    fs: Arc<dyn Fs>,
    cx: &App,
) -> Result<Task<Result<()>>> {
    let variable = api_key_variable(agent_id)
        .with_context(|| i18n::t!("This agent takes no API key (internal error)"))?;
    let value = validated_api_key(variable, value)?.to_string();
    let url = agent_api_key_credentials_url(agent_id, variable);
    let credentials_provider = zed_credentials_provider::global(cx);
    let agent_id = agent_id.to_string();
    Ok(cx.spawn(async move |cx| {
        credentials_provider
            .write_credentials(&url, variable, value.as_bytes(), cx)
            .await
            .with_context(|| {
                i18n::t!("Couldn't write to {store}", store = credential_store_name())
            })?;
        // A copy left in settings.json would keep the key in plain text, and
        // the settings' `env` is applied after the stored key, so it would
        // also go on being the one the agent gets.
        let removed =
            cx.update(|cx| remove_api_key_from_settings_file(&agent_id, variable, fs, cx));
        if let Some(removed) = removed {
            removed.await?;
        }
        Ok(())
    }))
}

/// ADR-F88: removes `agent_id`'s API key from the credential store and from
/// `agent_servers.<agent_id>.env` in settings.json, wherever it is. Both are
/// tried even when one fails, and the first failure is returned.
pub fn remove_agent_api_key(agent_id: &str, fs: Arc<dyn Fs>, cx: &App) -> Task<Result<()>> {
    let Some(variable) = api_key_variable(agent_id) else {
        return Task::ready(Ok(()));
    };
    let url = agent_api_key_credentials_url(agent_id, variable);
    let credentials_provider = zed_credentials_provider::global(cx);
    let from_settings = remove_api_key_from_settings_file(agent_id, variable, fs, cx);
    cx.spawn(async move |cx| {
        // Deleting an entry that isn't there is an error on Windows and macOS.
        let from_store = async {
            if credentials_provider
                .read_credentials(&url, cx)
                .await?
                .is_some()
            {
                credentials_provider.delete_credentials(&url, cx).await?;
            }
            anyhow::Ok(())
        }
        .await
        .with_context(|| {
            i18n::t!(
                "Couldn't remove the key from {store}",
                store = credential_store_name()
            )
        });
        let from_settings = match from_settings {
            Some(removed) => removed.await,
            None => Ok(()),
        };
        from_store.and(from_settings)
    })
}

/// ADR-F88: whether the credential store holds an API key for `agent_id`.
/// Only that is answered, never the key: nothing that shows this status can
/// leak it.
pub fn load_agent_api_key_status(agent_id: &str, cx: &App) -> Task<Result<bool>> {
    let Some(variable) = api_key_variable(agent_id) else {
        return Task::ready(Ok(false));
    };
    let url = agent_api_key_credentials_url(agent_id, variable);
    let credentials_provider = zed_credentials_provider::global(cx);
    cx.spawn(async move |cx| {
        let stored = credentials_provider
            .read_credentials(&url, cx)
            .await
            .with_context(|| {
                i18n::t!(
                    "Couldn't read from {store}",
                    store = credential_store_name()
                )
            })?;
        Ok(stored.is_some_and(|(_, key)| !key.is_empty()))
    })
}

/// ADR-F88: whether `agent_servers.<agent_id>.env` in the settings still
/// spells out the API key in plain text — written by hand, or left by a
/// migration that could not write to the credential store. That copy is the
/// one the agent gets (the settings' `env` is applied last), so the card warns
/// about it. Only whether it is there is answered, never the value.
pub fn api_key_in_settings_file(agent_id: &str, cx: &App) -> bool {
    let Some(variable) = api_key_variable(agent_id) else {
        return false;
    };
    cx.read_global(|settings: &SettingsStore, _| {
        let Some(agent) = settings.get::<AllAgentServersSettings>(None).get(agent_id) else {
            return false;
        };
        let value = match agent {
            project::agent_server_store::CustomAgentServerSettings::Registry { env, .. } => {
                env.get(variable)
            }
            project::agent_server_store::CustomAgentServerSettings::Custom { command, .. } => {
                command.env.as_ref().and_then(|env| env.get(variable))
            }
        };
        value.is_some_and(|value| !value.trim().is_empty())
    })
}

fn settings_env(settings: &settings::CustomAgentServerSettings) -> &HashMap<String, String> {
    match settings {
        settings::CustomAgentServerSettings::Custom { env, .. }
        | settings::CustomAgentServerSettings::Registry { env, .. } => env,
    }
}

/// Removes `agent_servers.<agent_id>.env.<variable>` from the user's
/// settings file, or `None` when the file has no such entry: the file is not
/// rewritten (or created) only to find that out. The agent's other settings
/// and the rest of its `env` are the user's and stay.
fn remove_api_key_from_settings_file(
    agent_id: &str,
    variable: &'static str,
    fs: Arc<dyn Fs>,
    cx: &App,
) -> Option<Task<Result<()>>> {
    let in_file = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .raw_user_settings()
            .and_then(|user| user.content.agent_servers.as_ref())
            .and_then(|agent_servers| agent_servers.get(agent_id))
            .is_some_and(|agent| settings_env(agent).contains_key(variable))
    });
    if !in_file {
        return None;
    }
    let agent_id = agent_id.to_string();
    let written = update_settings_file_with_completion(fs, cx, move |settings, _cx| {
        if let Some(agent) = settings
            .agent_servers
            .as_mut()
            .and_then(|agent_servers| agent_servers.get_mut(&agent_id))
        {
            match agent {
                settings::CustomAgentServerSettings::Custom { env, .. }
                | settings::CustomAgentServerSettings::Registry { env, .. } => {
                    env.remove(variable);
                }
            }
        }
    });
    Some(cx.background_spawn(async move {
        written
            .await
            .context(i18n::t!("Writing to the settings file didn't finish"))?
    }))
}

/// ADR-F88: moves the API keys an earlier build left in plain text in the
/// user's settings file (`agent_servers.<id>.env.<variable>`) into the
/// credential store. A key leaves the file only once the store has it — written
/// and read back the same — so a store that cannot be written loses nothing;
/// the key then stays where it was, and the card warns about it.
///
/// Only the user's own settings file: a project's settings are not the user's
/// to have their secrets moved out of, and this fork does not read an agent's
/// key from them.
pub fn migrate_agent_api_keys_out_of_settings_file(fs: Arc<dyn Fs>, cx: &App) -> Task<Result<()>> {
    use settings::RootUserSettings as _;

    cx.spawn(async move |cx| {
        let settings_path = paths::settings_file();
        if !fs.is_file(settings_path).await {
            return Ok(());
        }
        let text = fs.load(settings_path).await?;
        let (Some(user_settings), _) = settings::UserSettingsContent::parse_json(&text) else {
            return Ok(());
        };
        let keys = user_settings
            .content
            .agent_servers
            .iter()
            .flat_map(|agent_servers| agent_servers.iter())
            .filter_map(|(agent_id, agent)| {
                let variable = api_key_variable(agent_id)?;
                let value = settings_env(agent).get(variable)?;
                (!value.trim().is_empty()).then(|| (agent_id.clone(), variable, value.clone()))
            })
            .collect::<Vec<_>>();
        for (agent_id, variable, value) in keys {
            match move_api_key_to_credential_store(&agent_id, variable, value, fs.clone(), cx)
                .await
            {
                Ok(()) => log::info!(
                    "moved agent_servers.{agent_id}.env.{variable} from the settings file to the credential store"
                ),
                Err(error) => log::warn!(
                    "agent_servers.{agent_id}.env.{variable} stays in the settings file: {error:#}"
                ),
            }
        }
        Ok(())
    })
}

async fn move_api_key_to_credential_store(
    agent_id: &str,
    variable: &'static str,
    value: String,
    fs: Arc<dyn Fs>,
    cx: &mut gpui::AsyncApp,
) -> Result<()> {
    let url = agent_api_key_credentials_url(agent_id, variable);
    let credentials_provider = cx.update(|cx| zed_credentials_provider::global(cx));
    let stored_value = value.trim();
    // A different key already in the store was saved from the card (possibly
    // while this ran, or by another build sharing the store), so it is newer
    // than the plain-text one; neither is dropped until the user picks.
    match credentials_provider
        .read_credentials(&url, cx)
        .await
        .context("reading the credential store")?
    {
        Some((_, key)) if key == stored_value.as_bytes() => {}
        Some(_) => anyhow::bail!(
            "the credential store already holds a different key; remove one of the two"
        ),
        None => credentials_provider
            .write_credentials(&url, variable, stored_value.as_bytes(), cx)
            .await
            .context("writing to the credential store")?,
    }
    let read_back = credentials_provider
        .read_credentials(&url, cx)
        .await
        .context("reading the key back from the credential store")?;
    anyhow::ensure!(
        read_back.is_some_and(|(_, key)| key == stored_value.as_bytes()),
        "the credential store did not give back the key that was written"
    );

    let agent_id = agent_id.to_string();
    let written = cx.update(|cx| {
        update_settings_file_with_completion(fs, cx, move |settings, _cx| {
            if let Some(agent) = settings
                .agent_servers
                .as_mut()
                .and_then(|agent_servers| agent_servers.get_mut(&agent_id))
            {
                match agent {
                    settings::CustomAgentServerSettings::Custom { env, .. }
                    | settings::CustomAgentServerSettings::Registry { env, .. } => {
                        // Only the value that was stored: one the user changed
                        // in the meantime is theirs and stays.
                        if env.get(variable) == Some(&value) {
                            env.remove(variable);
                        }
                    }
                }
            }
        })
    });
    written
        .await
        .context("the settings file update didn't finish")?
}

/// ADR-F49 決定 19: whether `agent_id` starts outside its sandbox this time —
/// the one start the user confirmed (`delegate_outside`), or every start,
/// because the user chose so in their own settings file
/// (`agent_servers.<id>.run_outside_sandbox`, which a project's settings
/// cannot set). The only place the choice is read, so a new thread, a thread
/// read back from the history and the reconnection after the idle disconnect
/// all go the same way — except a thread read back from the history whose
/// session was started in the sandbox, which
/// [`AgentServerDelegate::in_sandbox_whatever_the_settings`] keeps there.
///
/// ADR-F92: nothing starts outside while running outside the sandbox is not
/// allowed — neither the confirmed start nor the settings.
pub fn starts_outside_sandbox(delegate_outside: bool, agent_id: &str, cx: &App) -> bool {
    (delegate_outside && project::agent_sandbox::outside_sandbox_allowed(cx))
        || project::agent_sandbox::runs_outside_sandbox_by_user_choice(agent_id, cx)
}

/// ADR-F92: why a start outside the sandbox was refused, for the card it
/// fails on.
pub const OUTSIDE_SANDBOX_NOT_ALLOWED: &str = i18n::msg!(
    "Running outside the sandbox is not allowed. To allow it, turn on \"Allow Running Outside the Sandbox\" in Settings > AI > Sandbox."
);

/// ADR-F49 決定 19: whether the user chose, in their own settings file, to
/// start `agent_id` outside its sandbox every time.
pub fn runs_outside_sandbox_by_user_choice(agent_id: &str, cx: &App) -> bool {
    project::agent_sandbox::runs_outside_sandbox_by_user_choice(agent_id, cx)
}

fn log_overridden_legacy_outside_sandbox_choice(agent_id: &str, cx: &App) {
    let Some(choice) = project::agent_sandbox::user_choice_to_run_outside_sandbox(agent_id, cx)
    else {
        return;
    };
    if let Some(legacy) = choice.ignored_legacy_entry {
        log::warn!(
            "agent_servers.{agent_id}.run_outside_sandbox ({}) is used; agent_servers.{}.run_outside_sandbox ({}) under the old extension id is ignored",
            choice.run_outside_sandbox,
            legacy.legacy_id,
            legacy.run_outside_sandbox,
        );
    }
}

/// What the IDE adds to a registry agent's environment for that agent in
/// particular.
///
/// Nothing for the ACP Claude agent (8c, the rest of ADR-F55): upstream
/// blanked `ANTHROPIC_API_KEY` here to push the agent onto its OAuth sign-in,
/// and the fork then pointed the start outside the sandbox at a folder of its
/// own. Both are gone — outside the sandbox the agent can read the account's
/// `~/.claude` anyway, so a folder of the fork's guarded nothing and only made
/// the user sign in twice; the sandbox lays its per-project folder over the
/// agent's environment regardless (`profile.environment`), and a
/// `CLAUDE_CONFIG_DIR` the user writes in the agent's settings wins over both.
fn add_registry_agent_environment(agent_id: &str, extra_env: &mut HashMap<String, String>) {
    match agent_id {
        CODEX_ID => {
            if let Ok(api_key) = std::env::var("CODEX_API_KEY") {
                extra_env.insert("CODEX_API_KEY".into(), api_key);
            }
            if let Ok(api_key) = std::env::var("OPEN_AI_API_KEY") {
                extra_env.insert("OPEN_AI_API_KEY".into(), api_key);
            }
        }
        GEMINI_ID => {
            extra_env.insert("SURFACE".to_owned(), "zed".to_owned());
        }
        _ => {}
    }
}

/// A generic agent server implementation for custom user-defined agents
pub struct CustomAgentServer {
    agent_id: AgentId,
}

impl CustomAgentServer {
    pub fn new(agent_id: AgentId) -> Self {
        Self { agent_id }
    }
}

impl AgentServer for CustomAgentServer {
    fn agent_id(&self) -> AgentId {
        self.agent_id.clone()
    }

    fn logo(&self) -> IconName {
        IconName::Terminal
    }

    fn starts_on_first_message(&self) -> bool {
        true
    }

    fn reads_session_options_before_start(&self) -> bool {
        true
    }

    fn default_mode(&self, cx: &App) -> Option<acp::SessionModeId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_mode().map(acp::SessionModeId::new))
    }

    fn favorite_config_option_value_ids(
        &self,
        config_id: &acp::SessionConfigId,
        cx: &mut App,
    ) -> HashSet<acp::SessionConfigValueId> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().0.as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.favorite_config_option_values(config_id.0.as_ref()))
            .map(|values| {
                values
                    .iter()
                    .cloned()
                    .map(acp::SessionConfigValueId::new)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        config_id: acp::SessionConfigId,
        value_id: acp::SessionConfigValueId,
        should_be_favorite: bool,
        fs: Arc<dyn Fs>,
        cx: &App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        let value_id = value_id.to_string();

        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    favorite_config_option_values,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    favorite_config_option_values,
                    ..
                } => {
                    let entry = favorite_config_option_values
                        .entry(config_id.clone())
                        .or_insert_with(Vec::new);

                    if should_be_favorite {
                        if !entry.iter().any(|v| v == &value_id) {
                            entry.push(value_id.clone());
                        }
                    } else {
                        entry.retain(|v| v != &value_id);
                        if entry.is_empty() {
                            favorite_config_option_values.remove(&config_id);
                        }
                    }
                }
            }
        });
    }

    fn set_default_mode(&self, mode_id: Option<acp::SessionModeId>, fs: Arc<dyn Fs>, cx: &mut App) {
        let agent_id = self.agent_id();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom { default_mode, .. }
                | settings::CustomAgentServerSettings::Registry { default_mode, .. } => {
                    *default_mode = mode_id.map(|m| m.to_string());
                }
            }
        });
    }

    fn default_config_option(&self, config_id: &str, cx: &App) -> Option<AgentConfigOptionValue> {
        let settings = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .cloned()
        });

        settings
            .as_ref()
            .and_then(|s| s.default_config_option(config_id).cloned())
    }

    fn set_default_config_option(
        &self,
        config_id: &str,
        value: Option<AgentConfigOptionValue>,
        fs: Arc<dyn Fs>,
        cx: &mut App,
    ) {
        let agent_id = self.agent_id();
        let config_id = config_id.to_string();
        update_settings_file(fs, cx, move |settings, _cx| {
            let settings = settings
                .agent_servers
                .get_or_insert_default()
                .entry(agent_id.0.to_string())
                .or_insert_with(default_settings_for_agent);

            match settings {
                settings::CustomAgentServerSettings::Custom {
                    default_config_options,
                    ..
                }
                | settings::CustomAgentServerSettings::Registry {
                    default_config_options,
                    ..
                } => {
                    if let Some(value) = value {
                        default_config_options.insert(config_id.clone(), value);
                    } else {
                        default_config_options.remove(&config_id);
                    }
                }
            }
        });
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let agent_id = self.agent_id();
        let default_mode = self.default_mode(cx);
        let is_registry_agent = is_registry_agent(agent_id.clone(), cx);
        let default_config_options = cx.read_global(|settings: &SettingsStore, _| {
            settings
                .get::<AllAgentServersSettings>(None)
                .get(self.agent_id().as_ref())
                .map(|s| match s {
                    project::agent_server_store::CustomAgentServerSettings::Custom {
                        default_config_options,
                        ..
                    }
                    | project::agent_server_store::CustomAgentServerSettings::Registry {
                        default_config_options,
                        ..
                    } => default_config_options.clone(),
                })
                .unwrap_or_default()
        });

        if is_registry_agent {
            if let Some(registry_store) = project::AgentRegistryStore::try_global(cx) {
                registry_store.update(cx, |store, cx| store.refresh_if_stale(cx));
            }
        }

        let mut extra_env = load_proxy_env(cx);
        if delegate.store.read(cx).no_browser() {
            extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
        }
        if is_registry_agent {
            add_registry_agent_environment(agent_id.as_ref(), &mut extra_env);
        }
        let stored_api_key = api_key_variable(agent_id.as_ref()).map(|variable| {
            (
                variable,
                agent_api_key_credentials_url(agent_id.as_ref(), variable),
                zed_credentials_provider::global(cx),
            )
        });
        let store = delegate.store.downgrade();
        log_overridden_legacy_outside_sandbox_choice(agent_id.as_ref(), cx);
        // ADR-F92: a confirmed start outside that is no longer allowed fails
        // rather than quietly going into the sandbox the user chose to leave.
        if delegate.runs_outside_sandbox() && !project::agent_sandbox::outside_sandbox_allowed(cx) {
            return Task::ready(Err(anyhow::anyhow!(
                "{}",
                i18n::tr(OUTSIDE_SANDBOX_NOT_ALLOWED)
            )));
        }
        let run_outside_sandbox = delegate.starts_outside_sandbox(agent_id.as_ref(), cx);
        let lift_agent_hosts = delegate.lifts_agent_hosts(cx);
        // ADR-F49 第 2b 段: on Windows the sandbox is the WSL distro of the
        // project's folders, so the command is resolved for it.
        let launch_target = if run_outside_sandbox {
            AgentLaunchTarget::Host
        } else {
            let roots = project::agent_sandbox::project_key_roots(
                project.read(cx).worktree_store().read(cx),
                cx,
            );
            match project::agent_sandbox::wsl_distro_for_roots(&roots) {
                Ok(wsl_distro) => AgentLaunchTarget::Sandbox { wsl_distro },
                Err(error) => return Task::ready(Err(error)),
            }
        };
        cx.spawn(async move |cx| {
            if is_registry_agent && agent_id.as_ref() == GEMINI_ID {
                if let Some(api_key) = cx.update(api_key_for_gemini_cli).await.ok() {
                    extra_env.insert("GEMINI_API_KEY".into(), api_key);
                }
            }
            if let Some((variable, url, credentials_provider)) = stored_api_key {
                let stored = credentials_provider.read_credentials(&url, cx).await;
                add_stored_api_key(&mut extra_env, agent_id.as_ref(), variable, stored);
            }
            let command = store
                .update(cx, |store, cx| {
                    let agent = store.get_external_agent(&agent_id).with_context(|| {
                        format!("Custom agent server `{}` is not registered", agent_id)
                    })?;
                    if let Some(new_version_available_tx) = delegate.new_version_available {
                        agent.set_new_version_available_tx(new_version_available_tx);
                    }
                    if let Some(loading_status_tx) = delegate.loading_status {
                        agent.set_loading_status_tx(loading_status_tx);
                    }
                    anyhow::Ok(agent.get_command(
                        vec![],
                        extra_env,
                        launch_target,
                        &mut cx.to_async(),
                    ))
                })??
                .await?;
            let connection = crate::acp::connect(
                agent_id,
                project,
                command,
                store.clone(),
                default_mode,
                default_config_options,
                run_outside_sandbox,
                lift_agent_hosts,
                cx,
            )
            .await?;
            Ok(connection)
        })
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn std::any::Any> {
        self
    }
}

/// ADR-F88: puts the API key read from the credential store into what the IDE
/// adds to the agent's environment, which every route that starts it
/// composes (`agent_environment` for this host and the WSL sandbox, the
/// remote host's command otherwise). A key spelled out in the agent's
/// settings is applied after this one and wins: that is the user's own word.
///
/// A store that cannot be read does not stop the agent: it starts without the
/// key and asks for a sign-in, as it would without one.
fn add_stored_api_key(
    extra_env: &mut HashMap<String, String>,
    agent_id: &str,
    variable: &str,
    stored: Result<Option<(String, Vec<u8>)>>,
) {
    match stored {
        Ok(Some((_, key))) => match String::from_utf8(key) {
            Ok(key) if !key.trim().is_empty() => {
                extra_env.insert(variable.to_string(), key);
            }
            Ok(_) => {}
            Err(_) => log::warn!("the stored {variable} for {agent_id} is not UTF-8; not used"),
        },
        Ok(None) => {}
        Err(error) => {
            log::warn!("couldn't read the stored {variable} for {agent_id}: {error:#}")
        }
    }
}

fn api_key_for_gemini_cli(cx: &mut App) -> Task<Result<String>> {
    let env_var = EnvVar::new("GEMINI_API_KEY".into()).or(EnvVar::new("GOOGLE_AI_API_KEY".into()));
    if let Some(key) = env_var.value {
        return Task::ready(Ok(key));
    }
    let credentials_provider = zed_credentials_provider::global(cx);
    let api_url = google_ai::API_URL.to_string();
    cx.spawn(async move |cx| {
        Ok(
            ApiKey::load_from_system_keychain(&api_url, credentials_provider.as_ref(), cx)
                .await?
                .key()
                .to_string(),
        )
    })
}

fn is_registry_agent(agent_id: impl Into<AgentId>, cx: &App) -> bool {
    let agent_id = agent_id.into();
    let is_in_registry = project::AgentRegistryStore::try_global(cx)
        .map(|store| store.read(cx).agent(&agent_id).is_some())
        .unwrap_or(false);
    let is_settings_registry = cx.read_global(|settings: &SettingsStore, _| {
        settings
            .get::<AllAgentServersSettings>(None)
            .get(agent_id.as_ref())
            .is_some_and(|s| {
                matches!(
                    s,
                    project::agent_server_store::CustomAgentServerSettings::Registry { .. }
                )
            })
    });
    is_in_registry || is_settings_registry
}

fn default_settings_for_agent() -> settings::CustomAgentServerSettings {
    settings::CustomAgentServerSettings::Registry {
        default_mode: None,
        env: Default::default(),
        default_config_options: Default::default(),
        favorite_config_option_values: Default::default(),
        allow_subscription_sign_in: None,
        run_outside_sandbox: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use collections::HashMap;
    use gpui::{BorrowAppContext as _, TestAppContext};
    use project::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use settings::Settings as _;
    use ui::SharedString;
    use zed_credentials_provider::test_support::InMemoryCredentialsProvider;

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn init_registry_with_agents(cx: &mut TestAppContext, agent_ids: &[&str]) {
        let agents: Vec<RegistryAgent> = agent_ids
            .iter()
            .map(|id| {
                let id = SharedString::from(id.to_string());
                RegistryAgent::Npx(RegistryNpxAgent {
                    metadata: RegistryAgentMetadata {
                        id: AgentId::new(id.clone()),
                        name: id.clone(),
                        description: SharedString::from(""),
                        version: SharedString::from("1.0.0"),
                        repository: None,
                        website: None,
                        license_url: None,
                        icon_path: None,
                    },
                    package: id,
                    args: Vec::new(),
                    env: HashMap::default(),
                })
            })
            .collect();
        cx.update(|cx| {
            AgentRegistryStore::init_test_global(cx, agents);
        });
    }

    fn set_agent_server_settings(
        cx: &mut TestAppContext,
        entries: Vec<(&str, settings::CustomAgentServerSettings)>,
    ) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                project::agent_server_store::AllAgentServersSettings(
                    entries
                        .into_iter()
                        .map(|(name, settings)| (name.to_string(), settings.into()))
                        .collect(),
                ),
                cx,
            );
        });
    }

    /// ADR-F55: サインインらしい id は広く拾い、鍵のメソッドは拾わない。
    /// この判定はメソッド名だけを見るので、設定とは独立して確かめられる。
    #[test]
    fn a_sign_in_is_told_from_a_key_by_its_name() {
        assert!(looks_like_subscription_sign_in(
            CLAUDE_SUBSCRIPTION_AUTH_METHOD
        ));
        for renamed in [
            "login",
            "claude-code-login",
            "claude-oauth",
            "subscription",
            // 大文字・区切り違いも同じもの: ここを取りこぼすと、切った設定が
            // 黙って効かなくなる (fail-open)。
            "Login",
            "claude_login",
            "ClaudeLogin",
            "oauth2",
            "OAuth",
            "sign-in",
            "signIn",
            "sign_in",
            "claude-subscription",
        ] {
            assert!(
                looks_like_subscription_sign_in(renamed),
                "a sign-in under another name is one too: {renamed}"
            );
        }
        for kept in [
            "anthropic-api-key",
            "api-key",
            "bedrock",
            "vertex",
            "ANTHROPIC_API_KEY",
            "logout",
            "",
            // 多バイトの id でも境界で割らずに答える。
            "サブスク",
        ] {
            assert!(
                !looks_like_subscription_sign_in(kept),
                "an API-key method is not a sign-in: {kept}"
            );
        }
    }

    /// ADR-F55: サインインを行うのは未改変の `claude` 自身 (Anthropic の
    /// フロー) なので、既定では落とさない。落とすのは
    /// `agent_servers.<id>.allow_subscription_sign_in` を `false` にした
    /// エージェントのメソッドだけ。
    #[gpui::test]
    fn only_a_turned_off_agents_sign_in_is_refused(cx: &mut TestAppContext) {
        init_test(cx);

        cx.update(|cx| {
            assert!(
                !subscription_sign_in_disabled(CLAUDE_AGENT_ID, cx),
                "設定が無ければサインインは使える (既定)"
            );
            assert!(!is_refused_subscription_auth_method(
                CLAUDE_AGENT_ID,
                CLAUDE_SUBSCRIPTION_AUTH_METHOD,
                cx
            ));
        });

        set_agent_server_settings(
            cx,
            vec![(
                CLAUDE_AGENT_ID,
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                    allow_subscription_sign_in: Some(false),
                    run_outside_sandbox: None,
                },
            )],
        );

        cx.update(|cx| {
            assert!(subscription_sign_in_disabled(CLAUDE_AGENT_ID, cx));
            assert!(is_refused_subscription_auth_method(
                CLAUDE_AGENT_ID,
                CLAUDE_SUBSCRIPTION_AUTH_METHOD,
                cx
            ));
            for kept in ["anthropic-api-key", "api-key", "bedrock", "vertex"] {
                assert!(
                    !is_refused_subscription_auth_method(CLAUDE_AGENT_ID, kept, cx),
                    "鍵のメソッドは切っても残る: {kept}"
                );
            }
            for other_agent in [CODEX_ID, GEMINI_ID, CURSOR_ID, "my-custom-agent"] {
                assert!(
                    !is_refused_subscription_auth_method(
                        other_agent,
                        CLAUDE_SUBSCRIPTION_AUTH_METHOD,
                        cx
                    ),
                    "ほかのエージェントは自分の設定にだけ従う: {other_agent}"
                );
            }
        });

        set_agent_server_settings(
            cx,
            vec![(
                CLAUDE_AGENT_ID,
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                    allow_subscription_sign_in: Some(true),
                    run_outside_sandbox: None,
                },
            )],
        );
        cx.update(|cx| {
            assert!(
                !is_refused_subscription_auth_method(
                    CLAUDE_AGENT_ID,
                    CLAUDE_SUBSCRIPTION_AUTH_METHOD,
                    cx
                ),
                "戻せば使える"
            );
        });
    }

    /// ADR-F55: 同じ設定を `Registry` と `Custom` のどちらの形で書いても同じ
    /// ように効き、書かなかった (`null`) ときの既定はどちらでも「許す」。
    /// 利用者は `command` を足した瞬間に `Custom` 側へ移るので、片方だけ効く
    /// と設定が黙って無視されることになる。
    #[gpui::test]
    fn the_setting_is_read_from_either_shape_of_entry(cx: &mut TestAppContext) {
        init_test(cx);

        let custom = |allow_subscription_sign_in| settings::CustomAgentServerSettings::Custom {
            path: std::path::PathBuf::from("/usr/bin/claude-agent-acp"),
            args: Vec::new(),
            env: HashMap::default(),
            default_mode: None,
            default_config_options: HashMap::default(),
            favorite_config_option_values: HashMap::default(),
            allow_subscription_sign_in,
            run_outside_sandbox: None,
        };
        let registry = |allow_subscription_sign_in| settings::CustomAgentServerSettings::Registry {
            env: HashMap::default(),
            default_mode: None,
            default_config_options: HashMap::default(),
            favorite_config_option_values: HashMap::default(),
            allow_subscription_sign_in,
            run_outside_sandbox: None,
        };

        for entry in [custom(None), registry(None)] {
            set_agent_server_settings(cx, vec![(CLAUDE_AGENT_ID, entry)]);
            cx.update(|cx| {
                assert!(
                    !subscription_sign_in_disabled(CLAUDE_AGENT_ID, cx),
                    "書かなければ既定 (許す)"
                );
            });
        }

        for entry in [custom(Some(false)), registry(Some(false))] {
            set_agent_server_settings(cx, vec![(CLAUDE_AGENT_ID, entry)]);
            cx.update(|cx| {
                assert!(
                    subscription_sign_in_disabled(CLAUDE_AGENT_ID, cx),
                    "どちらの形でも切れる"
                );
            });
        }
    }

    /// 8c (the rest of ADR-F55): the ACP Claude agent is pointed at no
    /// configuration folder of the fork's. Outside the sandbox it uses the
    /// account's `~/.claude`, like the official CLI there; in the sandbox the
    /// profile lays the project's folder over whatever this adds.
    #[test]
    fn the_acp_claude_is_pointed_at_no_folder_of_the_forks() {
        let mut env = HashMap::default();
        add_registry_agent_environment(CLAUDE_AGENT_ID, &mut env);
        assert!(!env.contains_key("CLAUDE_CONFIG_DIR"), "{env:?}");

        let mut gemini = HashMap::default();
        add_registry_agent_environment(GEMINI_ID, &mut gemini);
        assert_eq!(
            gemini.get("SURFACE").map(String::as_str),
            Some("zed"),
            "the other agents keep what they are given"
        );
    }

    const OUTSIDE_FOR_CLAUDE: &str = r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } }, "agent_servers": { "claude-acp": { "type": "registry", "run_outside_sandbox": true } } }"#;
    const OUTSIDE_ALLOWED: &str =
        r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } } }"#;

    /// ADR-F49 決定 19: the user's own settings file makes every start of that
    /// agent one outside its sandbox — and only that agent's.
    #[gpui::test]
    fn test_the_users_choice_starts_the_agent_outside_its_sandbox(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(OUTSIDE_ALLOWED, cx)
                    .result()
                    .expect("valid user settings");
            });
            assert!(
                starts_outside_sandbox(true, CLAUDE_AGENT_ID, cx),
                "the one start the user confirmed"
            );
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(OUTSIDE_FOR_CLAUDE, cx)
                    .result()
                    .expect("valid user settings");
            });
            assert!(starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
            assert!(runs_outside_sandbox_by_user_choice(CLAUDE_AGENT_ID, cx));
            assert!(
                !starts_outside_sandbox(false, CODEX_ID, cx),
                "another agent still starts in its sandbox"
            );

            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        r#"{ "agent_servers": { "claude-acp": { "type": "custom", "command": "/usr/bin/claude-agent-acp", "run_outside_sandbox": false } } }"#,
                        cx,
                    )
                    .result()
                    .expect("valid user settings");
            });
            assert!(
                !starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx),
                "turned off again, in the other shape of the entry"
            );
        });
    }

    /// Both the registry id and the old extension id carry a value: the
    /// registry id's wins, whichever way round the two disagree, and the
    /// overridden entry is reported. Only the old id: it still counts.
    #[gpui::test]
    fn test_the_registry_ids_choice_wins_over_the_old_extension_ids(cx: &mut TestAppContext) {
        init_test(cx);
        let set_user_settings = |settings: &str, cx: &mut App| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(settings, cx)
                    .result()
                    .expect("valid user settings");
            });
        };
        cx.update(|cx| {
            set_user_settings(
                r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } }, "agent_servers": {
                    "codebuddy": { "type": "registry", "run_outside_sandbox": true },
                    "codebuddy-code": { "type": "registry", "run_outside_sandbox": false }
                } }"#,
                cx,
            );
            assert!(!runs_outside_sandbox_by_user_choice("codebuddy-code", cx));
            assert_eq!(
                project::agent_sandbox::user_choice_to_run_outside_sandbox("codebuddy-code", cx),
                Some(project::agent_sandbox::OutsideSandboxChoice {
                    run_outside_sandbox: false,
                    ignored_legacy_entry: Some(project::agent_sandbox::IgnoredLegacyChoice {
                        legacy_id: "codebuddy".to_string(),
                        run_outside_sandbox: true,
                    }),
                })
            );

            set_user_settings(
                r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } }, "agent_servers": {
                    "codebuddy": { "type": "registry", "run_outside_sandbox": false },
                    "codebuddy-code": { "type": "registry", "run_outside_sandbox": true }
                } }"#,
                cx,
            );
            assert!(runs_outside_sandbox_by_user_choice("codebuddy-code", cx));

            set_user_settings(
                r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } }, "agent_servers": {
                    "codebuddy": { "type": "registry", "run_outside_sandbox": true },
                    "codebuddy-code": { "type": "registry" }
                } }"#,
                cx,
            );
            assert!(runs_outside_sandbox_by_user_choice("codebuddy-code", cx));
            assert_eq!(
                project::agent_sandbox::user_choice_to_run_outside_sandbox("codebuddy-code", cx),
                Some(project::agent_sandbox::OutsideSandboxChoice {
                    run_outside_sandbox: true,
                    ignored_legacy_entry: None,
                }),
                "a value only under the old id overrides nothing"
            );
        });
    }

    /// ADR-F49 決定 19 / ADR-F50: a project's own settings cannot let an agent
    /// out of its sandbox — the agent that would be let out can write them.
    #[gpui::test]
    fn test_a_projects_settings_cannot_start_an_agent_outside_its_sandbox(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        settings::WorktreeId::from_usize(1),
                        settings::LocalSettingsPath::InWorktree(
                            util::rel_path::RelPath::empty().into(),
                        ),
                        settings::LocalSettingsKind::Settings,
                        Some(OUTSIDE_FOR_CLAUDE),
                        cx,
                    )
                    .ok();
                store
                    .set_local_settings(
                        settings::WorktreeId::from_usize(1),
                        settings::LocalSettingsPath::InWorktree(
                            util::rel_path::RelPath::empty().into(),
                        ),
                        settings::LocalSettingsKind::Settings,
                        Some(r#"{ "agent": { "claude_code_run_outside_sandbox": true } }"#),
                        cx,
                    )
                    .ok();
            });
            assert!(!starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
            assert!(!runs_outside_sandbox_by_user_choice(CLAUDE_AGENT_ID, cx));
            assert!(!project::agent_sandbox::outside_sandbox_allowed(cx));
        });
    }

    /// ADR-F92: while running outside the sandbox is not allowed, neither the
    /// start the user confirmed nor the "always outside" settings let an
    /// agent or Claude Code out, and turning it on brings both back.
    #[gpui::test]
    fn test_nothing_starts_outside_while_it_is_not_allowed(cx: &mut TestAppContext) {
        init_test(cx);
        let set_user_settings = |settings: &str, cx: &mut App| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(settings, cx)
                    .result()
                    .expect("valid user settings");
            });
        };
        cx.update(|cx| {
            for allowed in [None, Some(false)] {
                let allowed = allowed.map_or(String::new(), |allowed| {
                    format!(r#""sandbox_permissions": {{ "allow_outside_sandbox": {allowed} }},"#)
                });
                set_user_settings(
                    &format!(
                        r#"{{ "agent": {{ {allowed} "claude_code_run_outside_sandbox": true }}, "agent_servers": {{ "claude-acp": {{ "type": "registry", "run_outside_sandbox": true }} }} }}"#
                    ),
                    cx,
                );
                assert!(!project::agent_sandbox::outside_sandbox_allowed(cx));
                assert!(!starts_outside_sandbox(true, CLAUDE_AGENT_ID, cx));
                assert!(!starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
                assert!(!runs_outside_sandbox_by_user_choice(CLAUDE_AGENT_ID, cx));
                assert!(!project::agent_sandbox::claude_code_runs_outside_sandbox_by_user_choice(cx));
            }

            set_user_settings(
                r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true }, "claude_code_run_outside_sandbox": true }, "agent_servers": { "claude-acp": { "type": "registry", "run_outside_sandbox": true } } }"#,
                cx,
            );
            assert!(project::agent_sandbox::outside_sandbox_allowed(cx));
            assert!(starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
            assert!(runs_outside_sandbox_by_user_choice(CLAUDE_AGENT_ID, cx));
            assert!(project::agent_sandbox::claude_code_runs_outside_sandbox_by_user_choice(cx));
        });
    }

    /// ADR-F92: a project's settings cannot allow running outside the sandbox.
    #[gpui::test]
    fn test_a_projects_settings_cannot_allow_running_outside_the_sandbox(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        r#"{ "agent_servers": { "claude-acp": { "type": "registry", "run_outside_sandbox": true } } }"#,
                        cx,
                    )
                    .result()
                    .expect("valid user settings");
                store
                    .set_local_settings(
                        settings::WorktreeId::from_usize(1),
                        settings::LocalSettingsPath::InWorktree(
                            util::rel_path::RelPath::empty().into(),
                        ),
                        settings::LocalSettingsKind::Settings,
                        Some(r#"{ "agent": { "sandbox_permissions": { "allow_outside_sandbox": true } } }"#),
                        cx,
                    )
                    .ok();
            });
            assert!(!project::agent_sandbox::outside_sandbox_allowed(cx));
            assert!(!starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
        });
    }

    /// ADR-F92: a start outside the sandbox that the user confirmed, made after
    /// running outside it stopped being allowed, fails rather than quietly
    /// starting the agent in the sandbox the user chose to leave.
    #[gpui::test]
    async fn test_a_confirmed_start_outside_fails_once_it_is_not_allowed(cx: &mut TestAppContext) {
        init_test(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().unwrap();
        let project = project::Project::example([temp_dir.path()], &mut cx.to_async()).await;
        let store = project.read_with(cx, |project, _| project.agent_server_store().clone());

        let connect = cx.update(|cx| {
            assert!(!project::agent_sandbox::outside_sandbox_allowed(cx));
            let delegate = AgentServerDelegate::new(store, None, None).outside_sandbox_once();
            CustomAgentServer::new(AgentId::new(CLAUDE_AGENT_ID)).connect(
                delegate,
                project.clone(),
                cx,
            )
        });
        match connect.await {
            Ok(_) => panic!("the agent must not start"),
            Err(error) => assert_eq!(error.to_string(), i18n::tr(OUTSIDE_SANDBOX_NOT_ALLOWED)),
        }
    }

    /// ADR-F96 stage 3: a start asked to lift the agent's own hosts lifts them
    /// only while the user's settings allow it — a start asked for before the
    /// setting was turned off does not outlive it, and a project's settings
    /// cannot turn it on.
    #[gpui::test]
    async fn test_a_start_lifts_the_agents_hosts_only_while_that_is_allowed(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        cx.executor().allow_parking();
        let temp_dir = tempfile::tempdir().expect("tempdir");
        let project = project::Project::example([temp_dir.path()], &mut cx.to_async()).await;
        let store = project.read_with(cx, |project, _| project.agent_server_store().clone());
        let set_user_settings = |allowed: bool, cx: &mut App| {
            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_user_settings(
                        &format!(
                            r#"{{ "agent": {{ "sandbox_permissions": {{ "allow_unrestricted_network": {allowed} }} }} }}"#
                        ),
                        cx,
                    )
                    .result()
                    .expect("valid user settings");
            });
        };
        cx.update(|cx| {
            let lifting = AgentServerDelegate::new(store.clone(), None, None).lift_agent_hosts();
            let not_lifting = AgentServerDelegate::new(store, None, None);

            set_user_settings(true, cx);
            assert!(lifting.lifts_agent_hosts(cx));
            assert!(!not_lifting.lifts_agent_hosts(cx));

            set_user_settings(false, cx);
            assert!(!lifting.lifts_agent_hosts(cx));

            cx.update_global::<SettingsStore, _>(|store, cx| {
                store
                    .set_local_settings(
                        settings::WorktreeId::from_usize(1),
                        settings::LocalSettingsPath::InWorktree(
                            util::rel_path::RelPath::empty().into(),
                        ),
                        settings::LocalSettingsKind::Settings,
                        Some(r#"{ "agent": { "sandbox_permissions": { "allow_unrestricted_network": true } } }"#),
                        cx,
                    )
                    .ok();
            });
            assert!(!lifting.lifts_agent_hosts(cx));
        });
    }

    #[gpui::test]
    fn test_unknown_agent_is_not_registry(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!is_registry_agent("my-custom-agent", cx));
        });
    }

    #[gpui::test]
    fn test_agent_in_registry_store_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        init_registry_with_agents(cx, &["some-new-registry-agent"]);
        cx.update(|cx| {
            assert!(is_registry_agent("some-new-registry-agent", cx));
            assert!(!is_registry_agent("not-in-registry", cx));
        });
    }

    #[gpui::test]
    fn test_agent_with_registry_settings_type_is_registry(cx: &mut TestAppContext) {
        init_test(cx);
        set_agent_server_settings(
            cx,
            vec![(
                "agent-from-settings",
                settings::CustomAgentServerSettings::Registry {
                    env: HashMap::default(),
                    default_mode: None,
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                    allow_subscription_sign_in: None,
                    run_outside_sandbox: None,
                },
            )],
        );
        cx.update(|cx| {
            assert!(is_registry_agent("agent-from-settings", cx));
        });
    }

    const PLAIN_TEXT_KEY: &str = "sk-ant-api03-plain-text";
    const SETTINGS_WITH_A_PLAIN_TEXT_KEY: &str = r#"{
  "buffer_font_size": 15,
  "agent_servers": {
    "claude-acp": {
      "type": "registry",
      "env": {
        "ANTHROPIC_API_KEY": "sk-ant-api03-plain-text",
        "CLAUDE_CONFIG_DIR": "/tmp/keep-me"
      }
    }
  }
}"#;

    async fn settings_file_with(text: &str, cx: &mut TestAppContext) -> std::sync::Arc<fs::FakeFs> {
        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(
            paths::settings_file()
                .parent()
                .expect("the settings file has a parent"),
        )
        .await
        .expect("the settings directory can be created");
        fs.atomic_write(paths::settings_file().to_path_buf(), text.to_string())
            .await
            .expect("the settings file can be written");
        fs
    }

    fn claude_key_url() -> String {
        agent_api_key_credentials_url(CLAUDE_AGENT_ID, CLAUDE_API_KEY_VARIABLE)
    }

    /// ADR-F88: someone reading the credential store's own viewer can tell
    /// which product and which agent the entry is for.
    #[test]
    fn the_stored_key_is_named_for_the_product_and_the_agent() {
        assert_eq!(
            claude_key_url(),
            "forgekairos://agent-servers/claude-acp/env/ANTHROPIC_API_KEY"
        );
    }

    /// ADR-F88: a key left in plain text by an earlier build moves into the
    /// credential store, and leaves the settings file only after that — the
    /// rest of the file stays as it was.
    #[gpui::test]
    async fn the_migration_moves_a_plain_text_key_into_the_credential_store(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        assert_eq!(
            credentials.get(&claude_key_url()),
            Some((
                CLAUDE_API_KEY_VARIABLE.to_string(),
                PLAIN_TEXT_KEY.as_bytes().to_vec()
            ))
        );
        let written = fs
            .load(paths::settings_file())
            .await
            .expect("the settings file can be read");
        assert!(!written.contains(PLAIN_TEXT_KEY), "{written}");
        assert!(!written.contains(CLAUDE_API_KEY_VARIABLE), "{written}");
        for kept in ["\"buffer_font_size\": 15", "/tmp/keep-me"] {
            assert!(written.contains(kept), "{kept} was lost:\n{written}");
        }
    }

    /// ADR-F88: fail-safe. A credential store that cannot be written leaves the
    /// key where it was: the settings file is not touched.
    #[gpui::test]
    async fn the_migration_keeps_the_key_when_the_store_cannot_be_written(cx: &mut TestAppContext) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        credentials.set_fail_writes(true);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("a key that could not be moved is not an error of the migration");
        cx.run_until_parked();

        assert!(credentials.is_empty());
        assert_eq!(
            fs.load(paths::settings_file())
                .await
                .expect("the settings file can be read"),
            SETTINGS_WITH_A_PLAIN_TEXT_KEY,
            "the key stays in the settings file"
        );
    }

    /// ADR-F88: a different key already in the store was saved from the card
    /// (or by another build sharing the store) and is not overwritten by the
    /// older plain-text one; both stay until the user picks.
    #[gpui::test]
    async fn the_migration_keeps_both_when_the_store_holds_a_different_key(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        let url = agent_api_key_credentials_url(CLAUDE_AGENT_ID, CLAUDE_API_KEY_VARIABLE);
        credentials.insert(&url, CLAUDE_API_KEY_VARIABLE, b"sk-ant-api03-from-the-card");
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        assert_eq!(
            credentials.get(&url).map(|(_, key)| key),
            Some(b"sk-ant-api03-from-the-card".to_vec()),
            "the key saved from the card is kept"
        );
        assert_eq!(
            fs.load(paths::settings_file())
                .await
                .expect("the settings file can be read"),
            SETTINGS_WITH_A_PLAIN_TEXT_KEY,
            "the plain-text key stays in the settings file"
        );
    }

    /// ADR-F88: the same key already in the store needs no write, and the
    /// plain-text copy goes.
    #[gpui::test]
    async fn the_migration_drops_a_plain_text_copy_the_store_already_holds(
        cx: &mut TestAppContext,
    ) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        let url = agent_api_key_credentials_url(CLAUDE_AGENT_ID, CLAUDE_API_KEY_VARIABLE);
        credentials.insert(&url, CLAUDE_API_KEY_VARIABLE, PLAIN_TEXT_KEY.as_bytes());
        credentials.set_fail_writes(true);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        let settings = fs
            .load(paths::settings_file())
            .await
            .expect("the settings file can be read");
        assert!(
            !settings.contains(PLAIN_TEXT_KEY),
            "the plain-text copy is removed: {settings}"
        );
    }

    /// ADR-F88: a store that takes the write but does not give the key back
    /// (a keyring that is locked again, say) is not trusted with it either.
    #[gpui::test]
    async fn the_migration_keeps_the_key_when_it_cannot_be_read_back(cx: &mut TestAppContext) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        credentials.set_fail_reads(true);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        assert_eq!(
            fs.load(paths::settings_file())
                .await
                .expect("the settings file can be read"),
            SETTINGS_WITH_A_PLAIN_TEXT_KEY,
            "the key stays in the settings file"
        );
    }

    /// ADR-F88: a store that reports the write as done but keeps nothing is
    /// caught by reading the key back.
    #[gpui::test]
    async fn the_migration_keeps_the_key_when_the_store_loses_it(cx: &mut TestAppContext) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        credentials.set_lose_writes(true);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        assert_eq!(
            fs.load(paths::settings_file())
                .await
                .expect("the settings file can be read"),
            SETTINGS_WITH_A_PLAIN_TEXT_KEY,
            "the key stays in the settings file"
        );
    }

    /// ADR-F88: a key the user changed in the settings file while the old one
    /// was being moved is theirs, and is not taken out with the old one.
    #[gpui::test]
    async fn the_migration_keeps_a_key_changed_while_it_ran(cx: &mut TestAppContext) {
        const CHANGED: &str = "sk-ant-api03-changed-meanwhile";
        init_test(cx);
        let fs = settings_file_with(SETTINGS_WITH_A_PLAIN_TEXT_KEY, cx).await;
        let credentials = std::sync::Arc::new(ChangesTheFileOnWrite {
            store: InMemoryCredentialsProvider::default(),
            fs: fs.clone(),
            text: SETTINGS_WITH_A_PLAIN_TEXT_KEY.replace(PLAIN_TEXT_KEY, CHANGED),
        });
        cx.update(|cx| {
            cx.set_global(zed_credentials_provider::ZedCredentialsProvider(
                credentials.clone(),
            ))
        });

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        let written = fs
            .load(paths::settings_file())
            .await
            .expect("the settings file can be read");
        assert!(written.contains(CHANGED), "{written}");
    }

    struct ChangesTheFileOnWrite {
        store: InMemoryCredentialsProvider,
        fs: std::sync::Arc<fs::FakeFs>,
        text: String,
    }

    impl credentials_provider::CredentialsProvider for ChangesTheFileOnWrite {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            cx: &'a gpui::AsyncApp,
        ) -> futures::future::LocalBoxFuture<'a, Result<Option<(String, Vec<u8>)>>> {
            self.store.read_credentials(url, cx)
        }

        fn write_credentials<'a>(
            &'a self,
            url: &'a str,
            username: &'a str,
            password: &'a [u8],
            cx: &'a gpui::AsyncApp,
        ) -> futures::future::LocalBoxFuture<'a, Result<()>> {
            use fs::Fs as _;
            use futures::FutureExt as _;
            async move {
                self.fs
                    .atomic_write(paths::settings_file().to_path_buf(), self.text.clone())
                    .await?;
                self.store
                    .write_credentials(url, username, password, cx)
                    .await
            }
            .boxed_local()
        }

        fn delete_credentials<'a>(
            &'a self,
            url: &'a str,
            cx: &'a gpui::AsyncApp,
        ) -> futures::future::LocalBoxFuture<'a, Result<()>> {
            self.store.delete_credentials(url, cx)
        }
    }

    /// ADR-F88: no settings file, or one without a key, is nothing to do — the
    /// file is not created or rewritten.
    #[gpui::test]
    async fn the_migration_leaves_a_file_without_a_key_alone(cx: &mut TestAppContext) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        const WITHOUT_A_KEY: &str = r#"{ "agent_servers": { "claude-acp": { "type": "registry", "env": { "ANTHROPIC_API_KEY": "  " } } } }"#;
        let fs = settings_file_with(WITHOUT_A_KEY, cx).await;

        cx.update(|cx| migrate_agent_api_keys_out_of_settings_file(fs.clone(), cx))
            .await
            .expect("the migration runs");
        cx.run_until_parked();

        assert!(credentials.is_empty());
        assert_eq!(
            fs.load(paths::settings_file())
                .await
                .expect("the settings file can be read"),
            WITHOUT_A_KEY
        );
    }

    /// ADR-F88: the routes that would let the stored key override the agent's
    /// own settings (a custom agent, a remote host) hold back exactly these
    /// names, so every variable a key is stored for must be one of them.
    #[test]
    fn every_stored_key_only_fills_in_on_every_route() {
        for agent_id in [CLAUDE_AGENT_ID, CODEX_ID, GEMINI_ID, "my-agent"] {
            if let Some(variable) = api_key_variable(agent_id) {
                assert!(
                    project::agent_server_store::STORED_API_KEY_VARIABLES.contains(&variable),
                    "{variable} of {agent_id}"
                );
            }
        }
    }

    /// ADR-F88: what `connect` hands the agent. A stored key goes into the
    /// environment under its variable; nothing stored, a store that could not
    /// be read or an empty entry add nothing (the agent starts and asks for a
    /// sign-in).
    #[test]
    fn a_stored_key_reaches_the_agents_environment() {
        let mut env = HashMap::default();
        add_stored_api_key(
            &mut env,
            CLAUDE_AGENT_ID,
            CLAUDE_API_KEY_VARIABLE,
            Ok(Some((
                CLAUDE_API_KEY_VARIABLE.to_string(),
                b"sk-ant-api03-stored".to_vec(),
            ))),
        );
        assert_eq!(
            env.get(CLAUDE_API_KEY_VARIABLE).map(String::as_str),
            Some("sk-ant-api03-stored")
        );

        for stored in [
            Ok(None),
            Err(anyhow::anyhow!("the store is locked")),
            Ok(Some((CLAUDE_API_KEY_VARIABLE.to_string(), Vec::new()))),
            Ok(Some((
                CLAUDE_API_KEY_VARIABLE.to_string(),
                vec![0xff, 0xfe],
            ))),
        ] {
            let mut env = HashMap::default();
            add_stored_api_key(&mut env, CLAUDE_AGENT_ID, CLAUDE_API_KEY_VARIABLE, stored);
            assert!(env.is_empty(), "{env:?}");
        }
    }

    /// ADR-F88: the key is saved in the credential store and nothing is
    /// written to settings.json; removing it takes it out of both.
    #[gpui::test]
    async fn a_saved_key_goes_to_the_credential_store_only(cx: &mut TestAppContext) {
        init_test(cx);
        let credentials = cx.update(InMemoryCredentialsProvider::install);
        let fs = fs::FakeFs::new(cx.executor());

        let saved = cx
            .update(|cx| {
                save_agent_api_key(CLAUDE_AGENT_ID, "  sk-ant-api03-saved ", fs.clone(), cx)
            })
            .expect("a valid key");
        saved.await.expect("saved");
        cx.run_until_parked();

        assert_eq!(
            credentials.get(&claude_key_url()),
            Some((
                CLAUDE_API_KEY_VARIABLE.to_string(),
                b"sk-ant-api03-saved".to_vec()
            ))
        );
        assert!(
            !fs.is_file(paths::settings_file()).await,
            "settings.json is not written"
        );
        assert!(
            cx.update(|cx| load_agent_api_key_status(CLAUDE_AGENT_ID, cx))
                .await
                .expect("readable")
        );

        cx.update(|cx| remove_agent_api_key(CLAUDE_AGENT_ID, fs.clone(), cx))
            .await
            .expect("removed");
        assert!(credentials.is_empty());
        assert!(
            !cx.update(|cx| load_agent_api_key_status(CLAUDE_AGENT_ID, cx))
                .await
                .expect("readable")
        );
        cx.update(|cx| remove_agent_api_key(CLAUDE_AGENT_ID, fs.clone(), cx))
            .await
            .expect("removing what isn't there is not an error");

        for refused in ["", "   ", "sk-ant\nsecond-line"] {
            assert!(
                cx.update(|cx| save_agent_api_key(CLAUDE_AGENT_ID, refused, fs.clone(), cx))
                    .is_err(),
                "{refused:?}"
            );
        }
        assert!(
            cx.update(|cx| save_agent_api_key(CODEX_ID, "sk-anything", fs.clone(), cx))
                .is_err(),
            "an agent that takes no key"
        );
        assert!(credentials.is_empty());
    }
}
