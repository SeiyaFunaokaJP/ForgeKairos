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

/// ADR-F53 決定 4: そのエージェントが `agent_servers.<id>.env` から読む
/// API キーの変数名。読まないエージェントでは `None`。
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
            .context("設定ファイルへの書き込みが終わりませんでした")?
    })
}

/// ADR-F53 決定 4: `agent_servers.<agent_id>.env.<name>` を書く。
/// [`project::agent_server_store`] の環境の組み立てでは設定の `env` が最後に
/// 載る = いちばん強いので、ここへ書けば起動するエージェントに必ず届く。
///
/// 値は settings.json にそのまま (平文で) 残る。キーチェーンへは移さない:
/// この変数は「エージェントのプロセスに渡す環境」であって IDE 自身の資格情報
/// ではなく、WSL のサンドボックスを含むどの起動経路も設定の `env` からしか
/// 読まないため。呼び出し側はユーザーにその旨を伝えること。
///
/// 空の値は「消したい」とも「設定したい」とも取れるので、黙って `""` を
/// 書かずに断る (fail-closed)。書き込み自体は非同期なので、その成否は返した
/// タスクが答える。
pub fn set_env_var(
    agent_id: &str,
    name: &str,
    value: &str,
    fs: Arc<dyn Fs>,
    cx: &App,
) -> Result<Task<Result<()>>> {
    anyhow::ensure!(!name.is_empty(), "環境変数の名前が空です (内部エラー)");
    let value = value.trim();
    anyhow::ensure!(!value.is_empty(), "{name} の値が空です");
    // 1 行の入力欄は貼り付けられた改行を畳んで見せるだけで、中身には残す。
    // 端を落としただけでは改行やタブが値に残り、settings.json への書き込みは
    // 通るのにエージェントへは壊れた環境が渡る (NUL ならプロセスの起動自体が
    // 失敗する)。黙って壊れた値を持たせないよう断る。
    anyhow::ensure!(
        !value.chars().any(char::is_control),
        "{name} の値に改行や制御文字が含まれています"
    );

    let agent_id = agent_id.to_string();
    let name = name.to_string();
    let value = value.to_string();
    let written = update_settings_file_with_completion(fs, cx, move |settings, _cx| {
        let settings = settings
            .agent_servers
            .get_or_insert_default()
            .entry(agent_id)
            .or_insert_with(default_settings_for_agent);

        match settings {
            settings::CustomAgentServerSettings::Custom { env, .. }
            | settings::CustomAgentServerSettings::Registry { env, .. } => {
                env.insert(name, value);
            }
        }
    });
    Ok(cx.background_spawn(async move {
        written
            .await
            .context("設定ファイルへの書き込みが終わりませんでした")?
    }))
}

/// ADR-F55: `agent_servers.<agent_id>.env.<name>` を消す。
///
/// 消すだけで、エージェントの項目そのものは残す (`env` 以外の設定はユーザーの
/// もの)。エントリーが無ければ何も書かずに成功する: 「消えている」という結果は
/// 同じで、消すために設定を作りに行く理由が無いため。
pub fn remove_env_var(agent_id: &str, name: &str, fs: Arc<dyn Fs>, cx: &App) -> Task<Result<()>> {
    let agent_id = agent_id.to_string();
    let name = name.to_string();
    let written = update_settings_file_with_completion(fs, cx, move |settings, _cx| {
        let Some(settings) = settings
            .agent_servers
            .as_mut()
            .and_then(|agent_servers| agent_servers.get_mut(&agent_id))
        else {
            return;
        };

        match settings {
            settings::CustomAgentServerSettings::Custom { env, .. }
            | settings::CustomAgentServerSettings::Registry { env, .. } => {
                env.remove(&name);
            }
        }
    });
    cx.background_spawn(async move {
        written
            .await
            .context("設定ファイルへの書き込みが終わりませんでした")?
    })
}

/// ADR-F53 決定 4: `agent_servers.<agent_id>.env.<name>` に値が入っているか。
///
/// 値そのものは返さない。API キーがここから UI やログへ漏れる経路を作らない
/// ため、「設定済みか」だけを答える。
pub fn has_env_var(agent_id: &str, name: &str, cx: &App) -> bool {
    cx.read_global(|settings: &SettingsStore, _| {
        let Some(agent) = settings.get::<AllAgentServersSettings>(None).get(agent_id) else {
            return false;
        };
        let value = match agent {
            project::agent_server_store::CustomAgentServerSettings::Registry { env, .. } => {
                env.get(name)
            }
            project::agent_server_store::CustomAgentServerSettings::Custom { command, .. } => {
                command.env.as_ref().and_then(|env| env.get(name))
            }
        };
        value.is_some_and(|value| !value.trim().is_empty())
    })
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
pub fn starts_outside_sandbox(delegate_outside: bool, agent_id: &str, cx: &App) -> bool {
    delegate_outside || project::agent_sandbox::runs_outside_sandbox_by_user_choice(agent_id, cx)
}

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
        let store = delegate.store.downgrade();
        log_overridden_legacy_outside_sandbox_choice(agent_id.as_ref(), cx);
        let run_outside_sandbox = delegate.starts_outside_sandbox(agent_id.as_ref(), cx);
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

    const OUTSIDE_FOR_CLAUDE: &str = r#"{ "agent_servers": { "claude-acp": { "type": "registry", "run_outside_sandbox": true } } }"#;

    /// ADR-F49 決定 19: the user's own settings file makes every start of that
    /// agent one outside its sandbox — and only that agent's.
    #[gpui::test]
    fn test_the_users_choice_starts_the_agent_outside_its_sandbox(cx: &mut TestAppContext) {
        init_test(cx);
        cx.update(|cx| {
            assert!(!starts_outside_sandbox(false, CLAUDE_AGENT_ID, cx));
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
                r#"{ "agent_servers": {
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
                r#"{ "agent_servers": {
                    "codebuddy": { "type": "registry", "run_outside_sandbox": false },
                    "codebuddy-code": { "type": "registry", "run_outside_sandbox": true }
                } }"#,
                cx,
            );
            assert!(runs_outside_sandbox_by_user_choice("codebuddy-code", cx));

            set_user_settings(
                r#"{ "agent_servers": {
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
}
