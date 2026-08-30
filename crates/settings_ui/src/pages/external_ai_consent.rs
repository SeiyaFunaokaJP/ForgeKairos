//! 外部 AI プロバイダーの学習リスク警告とオプトイン (`docs/jp-enhance/DESIGN.md`
//! ADR-F10)。
//!
//! Whether a provider counts as "external" is decided by the host its API URL
//! points at, not by a list of vendor names: a provider aimed at loopback (the
//! default Ollama / LM Studio / llama.cpp, or an OpenAI-compatible endpoint the
//! user pointed at `localhost`) is local and never warned about, and everything
//! else is external — including a cloud vendor's own URL and a remote
//! OpenAI-compatible endpoint.
//!
//! Consent is persisted in `acknowledged_external_ai_providers` in the user's
//! `settings.json`, and is read back from the global merge only, so that a
//! project's `.zed/settings.json` cannot grant it. Unticking the checkbox
//! removes the entry and brings the warning back.

use gpui::ScrollHandle;
use language_model::{LanguageModelProviderId, LanguageModelRegistry};
use settings::SettingsStore;
use settings::settings_content::AllLanguageModelSettingsContent;
use ui::{Banner, Checkbox, Divider, ToggleState, Tooltip, prelude::*};

use crate::SettingsWindow;
use crate::pages::network_allowlist;

/// Prefix that keeps edit prediction consent independent from the agent's LLM
/// provider consent. The two surfaces are configured separately (F2-4), and
/// their provider ids would otherwise collide (`ollama`, `copilot`).
const EDIT_PREDICTION_PREFIX: &str = "edit_prediction:";

/// One provider the user has to opt into before it can be configured.
pub(crate) struct ConsentTarget {
    /// Value stored in `acknowledged_external_ai_providers`.
    pub key: SharedString,
    /// Provider name as shown in the UI.
    pub name: SharedString,
    /// Hosts the provider needs in `network_allowlist.json`. The first entry is
    /// derived from the effective API URL; the rest are sign-in / token
    /// endpoints that the API URL does not reveal.
    pub hosts: Vec<SharedString>,
    /// Extra caveat shown under the host list, for providers whose real
    /// endpoint cannot be derived from settings.
    pub note: Option<&'static str>,
}

/// The default endpoint of each built-in provider, plus the hosts its API URL
/// does not reveal.
///
/// `default_api_url` values are copied from the constants the provider itself
/// uses; a user-set `api_url` in `settings.json` always wins over them. Sources
/// (2026-08-28, Zed v1.17.2):
///
/// | provider id | constant |
/// |---|---|
/// | `anthropic` | `anthropic::ANTHROPIC_API_URL` (`crates/anthropic/src/anthropic.rs`) |
/// | `openai` | `open_ai::OPEN_AI_API_URL` (`crates/open_ai/src/open_ai.rs`) |
/// | `google` | `google_ai::API_URL` (`crates/google_ai/src/google_ai.rs`) |
/// | `x_ai` | `x_ai::XAI_API_URL` (`crates/x_ai/src/x_ai.rs`) |
/// | `mistral` | `mistral::MISTRAL_API_URL` (`crates/mistral/src/mistral.rs`) |
/// | `deepseek` | `deepseek::DEEPSEEK_API_URL` (`crates/deepseek/src/deepseek.rs`) |
/// | `openrouter` | `open_router::OPEN_ROUTER_API_URL` (`crates/open_router/src/open_router.rs`) |
/// | `vercel_ai_gateway` | `API_URL` in `crates/language_models/src/provider/vercel_ai_gateway.rs` |
/// | `opencode` | `opencode::OPENCODE_API_URL` (`crates/opencode/src/opencode.rs`) |
/// | `copilot_chat` | `DEFAULT_COPILOT_API_ENDPOINT` in `crates/copilot_chat/src/copilot_chat.rs` |
/// | `ollama` | `ollama::OLLAMA_API_URL` (`crates/ollama/src/ollama.rs`) |
/// | `lmstudio` | `lmstudio::LMSTUDIO_API_URL` (`crates/lmstudio/src/lmstudio.rs`) |
/// | `llama.cpp` | `llama_cpp::LLAMA_CPP_API_URL` (`crates/llama_cpp/src/llama_cpp.rs`) |
///
/// The table is duplicated here rather than pulled in from `language_models`
/// because that would drag a dozen vendor crates into `settings_ui` for three
/// string constants each. `builtin_defaults_are_wellformed` keeps the strings
/// parseable; the mapping itself has to be re-checked when a provider's default
/// endpoint changes upstream — which cannot happen without a deliberate edit in
/// this hard fork (ADR-F01).
struct BuiltinProvider {
    id: &'static str,
    default_api_url: Option<&'static str>,
    extra_hosts: &'static [&'static str],
    /// Providers whose endpoint never appears in `settings.json`: they are
    /// external no matter what the user configures.
    always_external: bool,
    note: Option<&'static str>,
}

const BUILTIN_PROVIDERS: &[BuiltinProvider] = &[
    BuiltinProvider {
        // The Zed Cloud provider builds its URL from `server_url` via
        // `HttpClientWithUrl` (`crates/http_client/src/http_client.rs`), not
        // from a `language_models` setting.
        id: "zed.dev",
        default_api_url: None,
        extra_hosts: &["cloud.zed.dev", "api.zed.dev", "zed.dev"],
        always_external: true,
        note: None,
    },
    BuiltinProvider {
        id: "anthropic",
        default_api_url: Some("https://api.anthropic.com"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "openai",
        default_api_url: Some("https://api.openai.com/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        // ChatGPT subscription sign-in; endpoints are the `CODEX_BASE_URL` /
        // `OPENAI_AUTHORIZE_URL` constants in
        // `crates/openai_subscribed/src/openai_subscribed.rs`.
        id: "openai-subscribed",
        default_api_url: None,
        extra_hosts: &["chatgpt.com", "auth.openai.com"],
        always_external: true,
        note: None,
    },
    BuiltinProvider {
        id: "google",
        default_api_url: Some("https://generativelanguage.googleapis.com"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "x_ai",
        default_api_url: Some("https://api.x.ai/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "mistral",
        default_api_url: Some("https://api.mistral.ai/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "deepseek",
        default_api_url: Some("https://api.deepseek.com/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "openrouter",
        default_api_url: Some("https://openrouter.ai/api/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "vercel_ai_gateway",
        default_api_url: Some("https://ai-gateway.vercel.sh/v1"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "opencode",
        default_api_url: Some("https://opencode.ai/zen"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "amazon-bedrock",
        default_api_url: None,
        extra_hosts: &[],
        always_external: true,
        note: Some(
            "接続先は AWS のリージョンによって変わります (例: bedrock-runtime.us-east-1.amazonaws.com)。実際に使用するリージョンのホストを許可リストに登録してください。",
        ),
    },
    BuiltinProvider {
        id: "copilot_chat",
        default_api_url: Some("https://api.githubcopilot.com"),
        extra_hosts: &["github.com", "api.github.com"],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "ollama",
        default_api_url: Some("http://localhost:11434"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "lmstudio",
        default_api_url: Some("http://localhost:1234/api/v0"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
    BuiltinProvider {
        id: "llama.cpp",
        default_api_url: Some("http://localhost:8080"),
        extra_hosts: &[],
        always_external: false,
        note: None,
    },
];

/// Whether the warning for this provider has already been acknowledged.
pub(crate) fn is_acknowledged(key: &str, cx: &App) -> bool {
    settings::external_ai_provider_is_acknowledged(key, cx)
}

fn apply_acknowledgement(settings: &mut settings::SettingsContent, key: &str, acknowledged: bool) {
    let acknowledged_providers = settings
        .acknowledged_external_ai_providers
        .get_or_insert_default();
    if acknowledged {
        if !acknowledged_providers.iter().any(|entry| entry == key) {
            acknowledged_providers.push(key.to_string());
        }
    } else {
        acknowledged_providers.retain(|entry| entry != key);
    }
}

/// Records or withdraws consent in the user's `settings.json`.
fn set_acknowledged(key: SharedString, acknowledged: bool, cx: &mut App) {
    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |settings, _| {
        apply_acknowledgement(settings, &key, acknowledged);
    });
}

/// Records consent as part of a settings update the caller is already making,
/// so that adding a provider and consenting to it land in one write.
pub(crate) fn acknowledge_in(settings: &mut settings::SettingsContent, key: &str) {
    apply_acknowledgement(settings, key, true);
}

/// The host of an API URL, or `None` when it has no host at all (a relative or
/// malformed URL).
fn host_of(api_url: &str) -> Option<SharedString> {
    let url = url::Url::parse(api_url.trim()).ok()?;
    let host = url.host_str()?;
    Some(http_client::RequestHost::parse(host)?.to_string().into())
}

fn is_loopback(host: &str) -> bool {
    http_client::RequestHost::parse(host).is_some_and(|host| host.is_loopback())
}

/// Builds a target from an endpoint the user typed or configured. Returns
/// `None` when the endpoint is loopback, i.e. a local LLM that needs no warning.
fn target_for_api_url(
    key: SharedString,
    name: SharedString,
    api_url: &str,
    extra_hosts: &[&'static str],
    note: Option<&'static str>,
) -> Option<ConsentTarget> {
    let host = host_of(api_url)?;
    if is_loopback(&host) {
        return None;
    }
    let mut hosts = vec![host];
    hosts.extend(extra_hosts.iter().map(|host| SharedString::from(*host)));
    Some(ConsentTarget {
        key,
        name,
        hosts,
        note,
    })
}

fn language_model_settings(cx: &App) -> Option<&AllLanguageModelSettingsContent> {
    cx.global::<SettingsStore>()
        .merged_settings()
        .language_models
        .as_ref()
}

/// The `api_url` the user configured for a built-in provider, if any. An empty
/// string means "use the default", the same way the providers themselves read
/// it.
fn configured_api_url(provider_id: &str, cx: &App) -> Option<String> {
    let settings = language_model_settings(cx)?;
    let api_url = match provider_id {
        "anthropic" => settings.anthropic.as_ref()?.api_url.clone(),
        "openai" => settings.openai.as_ref()?.api_url.clone(),
        "google" => settings.google.as_ref()?.api_url.clone(),
        "x_ai" => settings.x_ai.as_ref()?.api_url.clone(),
        "mistral" => settings.mistral.as_ref()?.api_url.clone(),
        "deepseek" => settings.deepseek.as_ref()?.api_url.clone(),
        "openrouter" => settings.open_router.as_ref()?.api_url.clone(),
        "vercel_ai_gateway" => settings.vercel_ai_gateway.as_ref()?.api_url.clone(),
        "opencode" => settings.opencode.as_ref()?.api_url.clone(),
        "ollama" => settings.ollama.as_ref()?.api_url.clone(),
        "lmstudio" => settings.lmstudio.as_ref()?.api_url.clone(),
        "llama.cpp" => settings.llama_cpp.as_ref()?.api_url.clone(),
        _ => None,
    }?;
    (!api_url.trim().is_empty()).then_some(api_url)
}

/// The `api_url` of a user-defined OpenAI/Anthropic-compatible provider, which
/// is keyed in settings by the name the user gave it.
fn compatible_provider_api_url(provider_id: &str, cx: &App) -> Option<String> {
    let settings = language_model_settings(cx)?;
    let from_openai = settings
        .openai_compatible
        .as_ref()
        .and_then(|providers| providers.get(provider_id))
        .map(|provider| provider.api_url.clone());
    let from_anthropic = || {
        settings
            .anthropic_compatible
            .as_ref()
            .and_then(|providers| providers.get(provider_id))
            .map(|provider| provider.api_url.clone())
    };
    from_openai.or_else(from_anthropic)
}

/// The consent target for an agent (LLM) provider, or `None` when it only talks
/// to loopback and so needs no warning.
pub(crate) fn llm_provider_target(
    provider_id: &str,
    provider_name: SharedString,
    cx: &App,
) -> Option<ConsentTarget> {
    let key = SharedString::from(provider_id.to_string());

    if let Some(builtin) = BUILTIN_PROVIDERS
        .iter()
        .find(|builtin| builtin.id == provider_id)
    {
        if builtin.always_external {
            return Some(ConsentTarget {
                key,
                name: provider_name,
                hosts: builtin
                    .extra_hosts
                    .iter()
                    .map(|host| SharedString::from(*host))
                    .collect(),
                note: builtin.note,
            });
        }
        let api_url = configured_api_url(provider_id, cx)
            .or_else(|| builtin.default_api_url.map(str::to_string))?;
        return target_for_api_url(
            key,
            provider_name,
            &api_url,
            builtin.extra_hosts,
            builtin.note,
        );
    }

    if let Some(api_url) = compatible_provider_api_url(provider_id, cx) {
        return target_for_api_url(key, provider_name, &api_url, &[], None);
    }

    // An extension-provided provider, whose endpoint is not visible from here.
    // Treated as external: the warning is the safe direction to fail in.
    Some(ConsentTarget {
        key,
        name: provider_name,
        hosts: Vec::new(),
        note: Some(
            "このプロバイダーの接続先は設定から特定できません。送信先を確認したうえで有効にしてください。",
        ),
    })
}

/// The consent target for the provider being defined in the "add compatible
/// provider" form, whose endpoint is whatever is currently typed.
pub(crate) fn form_target(
    provider_name: &str,
    api_url: &str,
    fallback_name: &'static str,
) -> Option<ConsentTarget> {
    let name = if provider_name.trim().is_empty() {
        SharedString::from(fallback_name)
    } else {
        SharedString::from(provider_name.trim().to_string())
    };
    target_for_api_url(name.clone(), name, api_url, &[], None)
}

/// The consent target for an edit prediction provider. `api_url` is the
/// endpoint the provider is actually configured with, so that an OpenAI
/// compatible or Ollama endpoint aimed at loopback stays local.
pub(crate) fn edit_prediction_target(
    provider: &'static str,
    provider_name: &'static str,
    api_url: Option<&str>,
    extra_hosts: &[&'static str],
    note: Option<&'static str>,
) -> Option<ConsentTarget> {
    let key = SharedString::from(format!("{EDIT_PREDICTION_PREFIX}{provider}"));
    let name = SharedString::from(provider_name);
    match api_url {
        Some(api_url) => target_for_api_url(key, name, api_url, extra_hosts, note),
        // No endpoint in settings: sign-in based, and always external.
        None => Some(ConsentTarget {
            key,
            name,
            hosts: extra_hosts
                .iter()
                .map(|host| SharedString::from(*host))
                .collect(),
            note,
        }),
    }
}

fn warning_headline(name: &SharedString) -> SharedString {
    format!("{name} は外部の AI サービスです").into()
}

fn warning_body(name: &SharedString) -> SharedString {
    format!(
        "有効にすると、入力したコードや会話の内容が {name} のサーバーへ送信されます。事業者の方針によっては保存され、モデルの学習に利用される可能性があります。業務で利用する場合は、所属組織のポリシーを必ず確認してください。"
    )
    .into()
}

/// The warning text, the hosts the provider needs, and the one-click allowlist
/// action. Shared by the provider sections and the "add provider" form, which
/// supply their own confirmation checkbox.
pub(crate) fn render_warning_content(
    settings_window: &SettingsWindow,
    target: &ConsentTarget,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    v_flex()
        .gap_1p5()
        .child(Label::new(warning_headline(&target.name)))
        .child(
            Label::new(warning_body(&target.name))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(render_hosts(settings_window, target, cx))
        .when_some(target.note, |this, note| {
            this.child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
        })
        .into_any_element()
}

/// The hosts this provider needs in `network_allowlist.json`, with a button
/// that adds the missing ones. Declining is simply not pressing it: egress stays
/// closed and the provider will fail to connect, which is the intended default.
pub(crate) fn render_hosts(
    settings_window: &SettingsWindow,
    target: &ConsentTarget,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    if target.hosts.is_empty() {
        return div().into_any_element();
    }

    let allowlist = &settings_window.network_allowlist;
    let is_loaded = allowlist.is_loaded();
    let missing: Vec<SharedString> = target
        .hosts
        .iter()
        .filter(|host| !allowlist.allows(host))
        .cloned()
        .collect();
    let all_allowed = is_loaded && missing.is_empty();
    let hosts_label = target
        .hosts
        .iter()
        .map(SharedString::to_string)
        .collect::<Vec<_>>()
        .join("、");
    let button_id = SharedString::from(format!("external-ai-allowlist-{}", target.key));

    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .flex_wrap()
                .gap_1p5()
                .child(
                    Label::new("許可が必要なホスト:")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(hosts_label).size(LabelSize::Small).buffer_font(cx))
                .child(
                    Label::new(if !is_loaded {
                        "(許可リストを読み込み中です)"
                    } else if all_allowed {
                        "(登録済み)"
                    } else {
                        "(未登録)"
                    })
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(
                    Button::new(button_id, "許可リストに追加")
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Outlined)
                        .disabled(!is_loaded || all_allowed)
                        .start_icon(
                            Icon::new(IconName::Plus)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(
                            "network_allowlist.json に追加します。追加しない場合、このプロバイダーへの通信は拒否されたままになります",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            network_allowlist::add_allowed_hosts(this, &missing, cx);
                        })),
                ),
        )
        .when_some(allowlist.consent_error(), |this, error| {
            this.child(
                h_flex()
                    .gap_2()
                    .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                    .child(
                        Button::new(
                            SharedString::from(format!(
                                "external-ai-allowlist-dismiss-{}",
                                target.key
                            )),
                            "閉じる",
                        )
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(|this, _, _, cx| {
                            network_allowlist::dismiss_error(this, cx);
                        })),
                    ),
            )
        })
        .into_any_element()
}

/// Shown in place of the provider's configuration UI until the box is ticked.
pub(crate) fn render_consent_gate(
    settings_window: &SettingsWindow,
    target: &ConsentTarget,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let key = target.key.clone();
    let name = target.name.clone();

    v_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .child(
            Banner::new().severity(Severity::Warning).child(
                v_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .child(render_warning_content(settings_window, target, cx))
                    .child(Divider::horizontal())
                    .child(
                        Checkbox::new(
                            SharedString::from(format!("external-ai-consent-{key}")),
                            ToggleState::Unselected,
                        )
                        .label(format!(
                            "上記のリスクを理解したうえで、{name} を有効にします"
                        ))
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |_, checked: &ToggleState, _, cx| {
                                set_acknowledged(key.clone(), checked.selected(), cx);
                                cx.notify();
                            },
                        )),
                    ),
            ),
        )
        .into_any_element()
}

/// The button that withdraws a consent. Every surface that offers this — the
/// per-provider summary, the acknowledged-providers list and the edit
/// prediction "Zed" card — goes through here, so there is one control and one
/// wording for it.
pub(crate) fn render_revoke_button(
    id: SharedString,
    key: SharedString,
    cx: &mut Context<SettingsWindow>,
) -> Button {
    Button::new(id, "承諾を取り消す")
        .label_size(LabelSize::Small)
        .style(ButtonStyle::Outlined)
        .tooltip(Tooltip::text(
            "承諾を取り消すと、再び警告が表示され設定できなくなります",
        ))
        .on_click(cx.listener(move |_, _, _, cx| {
            set_acknowledged(key.clone(), false, cx);
            cx.notify();
        }))
}

/// The "already acknowledged" line and its revoke button, without the host
/// list. Shared by [`render_consent_summary`] and the edit prediction "Zed"
/// card, which shows its hosts unconditionally and so cannot use the summary as
/// a whole.
pub(crate) fn render_acknowledged_row(
    target: &ConsentTarget,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let key = target.key.clone();

    h_flex()
        .gap_2()
        .flex_wrap()
        .child(
            Icon::new(IconName::Check)
                .size(IconSize::XSmall)
                .color(Color::Muted),
        )
        .child(
            Label::new("外部の AI サービスとして利用することを承諾済みです。")
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(render_revoke_button(
            SharedString::from(format!("external-ai-revoke-{key}")),
            key,
            cx,
        ))
        .into_any_element()
}

/// Shown above the provider's configuration UI once consent has been given, so
/// that it can be withdrawn from the same place it was granted.
pub(crate) fn render_consent_summary(
    settings_window: &SettingsWindow,
    target: &ConsentTarget,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let allowlist = &settings_window.network_allowlist;
    let has_missing_hosts = target.hosts.iter().any(|host| !allowlist.allows(host));
    let needs_hosts = allowlist.is_loaded() && has_missing_hosts;

    v_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(render_acknowledged_row(target, cx))
        .when(needs_hosts, |this| {
            this.child(render_hosts(settings_window, target, cx))
        })
        .into_any_element()
}

/// Every key currently in `acknowledged_external_ai_providers`, in storage
/// order. Read from the global merge, matching `is_acknowledged` above (a
/// project's `.zed/settings.json` cannot add to this list).
fn acknowledged_provider_keys(cx: &App) -> Vec<SharedString> {
    cx.global::<SettingsStore>()
        .merged_settings()
        .acknowledged_external_ai_providers
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(SharedString::from)
        .collect()
}

/// Best-effort display name for an acknowledged provider key, for the
/// "AI" page's consent list. Falls back to the raw key when no name
/// can be resolved, which is an acceptable outcome for this cosmetic lookup.
fn display_name_for_acknowledged_key(key: &SharedString, cx: &App) -> SharedString {
    if key.as_ref() == settings::ZED_CLOUD_AI_CONSENT_KEY {
        return "Zed".into();
    }

    if let Some(id) = key.as_ref().strip_prefix(EDIT_PREDICTION_PREFIX) {
        // Mirrors the display names passed to `edit_prediction_target` at each
        // call site in `edit_prediction_provider_setup.rs`; there is no shared
        // registry of edit prediction provider names to look these up from.
        let name = match id {
            "copilot" => "GitHub Copilot",
            "mercury" => "Mercury",
            "codestral" => "Codestral",
            "open_ai_compatible_api" => "OpenAI Compatible API",
            "ollama" => "Ollama",
            _ => id,
        };
        return format!("{name} (編集予測)").into();
    }

    LanguageModelRegistry::read_global(cx)
        .provider(&LanguageModelProviderId(key.clone()))
        .map(|provider| provider.name().0)
        .unwrap_or_else(|| key.clone())
}

/// The "外部 AI プロバイダーの承諾" sub-page: every provider the user has
/// acknowledged the training-risk warning for, with a button to withdraw
/// consent. Revoking reuses `set_acknowledged`, the same function the
/// per-provider consent gate (`render_consent_summary` above) uses, so there
/// is exactly one place that writes `acknowledged_external_ai_providers`.
pub(crate) fn render_acknowledged_ai_providers_page(
    _settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let keys = acknowledged_provider_keys(cx);

    v_flex()
        .id("acknowledged-ai-providers-page")
        .size_full()
        .pt_2()
        .pb_16()
        .map(|this| {
            if keys.is_empty() {
                this.px_8()
                    .items_center()
                    .justify_center()
                    .child(Label::new("承諾済みのプロバイダーはありません。").color(Color::Muted))
            } else {
                let row_count = keys.len();
                this.track_scroll(scroll_handle)
                    .overflow_y_scroll()
                    .children(keys.into_iter().enumerate().flat_map(|(index, key)| {
                        let mut elements: Vec<AnyElement> =
                            vec![render_acknowledged_provider_row(key, cx)];
                        if index + 1 < row_count {
                            elements.push(
                                div()
                                    .px_8()
                                    .child(Divider::horizontal().flex_grow_1())
                                    .into_any_element(),
                            );
                        }
                        elements
                    }))
            }
        })
        .into_any_element()
}

fn render_acknowledged_provider_row(
    key: SharedString,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let name = display_name_for_acknowledged_key(&key, cx);
    let show_raw_key = name != key;

    h_flex()
        .w_full()
        .justify_between()
        .py_3()
        .px_8()
        .gap_4()
        .child(
            v_flex()
                .gap_0p5()
                .min_w_0()
                .flex_1()
                .child(Label::new(name))
                .when(show_raw_key, |this| {
                    this.child(
                        Label::new(key.clone())
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    )
                }),
        )
        .child(render_revoke_button(
            SharedString::from(format!("external-ai-revoke-list-{key}")),
            key,
            cx,
        ))
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use gpui::UpdateGlobal as _;
    use settings::settings_content::ParseStatus;

    use super::*;

    #[track_caller]
    fn set_user_settings(json: &str, cx: &mut App) {
        let result = settings::SettingsStore::update_global(cx, |store, cx| {
            store.set_user_settings(json, cx)
        });
        assert_eq!(
            result.parse_status,
            ParseStatus::Success,
            "the test settings should parse"
        );
    }

    #[test]
    fn builtin_defaults_are_wellformed() {
        for builtin in BUILTIN_PROVIDERS {
            if let Some(api_url) = builtin.default_api_url {
                assert!(
                    host_of(api_url).is_some(),
                    "{}: {api_url:?} has no host",
                    builtin.id
                );
            } else {
                assert!(
                    builtin.always_external,
                    "{}: a provider with no default endpoint has to be marked always_external",
                    builtin.id
                );
            }
            for host in builtin.extra_hosts {
                assert!(
                    http_client::HostPattern::parse(host).is_ok(),
                    "{}: {host:?} is not a valid allowlist entry",
                    builtin.id
                );
            }
        }
    }

    #[test]
    fn local_endpoints_need_no_consent() {
        for api_url in [
            "http://localhost:11434",
            "http://127.0.0.1:1234/api/v0",
            "http://[::1]:8080/v1",
            "http://127.0.0.1:8080/v1/completions",
        ] {
            assert!(
                target_for_api_url("k".into(), "n".into(), api_url, &[], None).is_none(),
                "{api_url} should count as local"
            );
        }
    }

    #[test]
    fn remote_endpoints_need_consent_and_list_their_host() {
        let target = target_for_api_url(
            "openai".into(),
            "OpenAI".into(),
            "https://api.openai.com/v1",
            &[],
            None,
        )
        .expect("a remote endpoint should need consent");
        assert_eq!(target.hosts, vec![SharedString::from("api.openai.com")]);

        let target = target_for_api_url(
            "copilot_chat".into(),
            "GitHub Copilot Chat".into(),
            "https://api.githubcopilot.com",
            &["github.com"],
            None,
        )
        .expect("a remote endpoint should need consent");
        assert_eq!(
            target.hosts,
            vec![
                SharedString::from("api.githubcopilot.com"),
                SharedString::from("github.com")
            ]
        );
    }

    /// The whole point of deciding by host: an OpenAI-compatible provider is
    /// local or external depending only on where the user pointed it.
    #[test]
    fn a_compatible_provider_follows_its_url() {
        assert!(form_target("MyServer", "http://localhost:8000/v1", "OpenAI").is_none());
        let target = form_target("MyServer", "https://llm.corp.example/v1", "OpenAI")
            .expect("a remote compatible provider should need consent");
        assert_eq!(target.key, SharedString::from("MyServer"));
        assert_eq!(target.hosts, vec![SharedString::from("llm.corp.example")]);
    }

    /// The decision has to follow the *configured* endpoint, not the vendor
    /// name: a cloud provider aimed at a local proxy is local, and a local
    /// provider aimed at a remote host is external.
    #[gpui::test]
    fn a_providers_endpoint_decides(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });

        cx.update(|cx| {
            assert!(
                llm_provider_target("ollama", "Ollama".into(), cx).is_none(),
                "the default Ollama endpoint is loopback"
            );
            assert!(llm_provider_target("lmstudio", "LM Studio".into(), cx).is_none());
            assert!(llm_provider_target("llama.cpp", "llama.cpp".into(), cx).is_none());

            let anthropic = llm_provider_target("anthropic", "Anthropic".into(), cx)
                .expect("Anthropic's default endpoint is remote");
            assert_eq!(
                anthropic.hosts,
                vec![SharedString::from("api.anthropic.com")]
            );

            let zed = llm_provider_target("zed.dev", "Zed".into(), cx)
                .expect("Zed Cloud has no configurable endpoint and is always external");
            assert!(zed.hosts.contains(&SharedString::from("cloud.zed.dev")));
        });

        cx.update(|cx| {
            set_user_settings(
                r#"{
                    "language_models": {
                        "ollama": { "api_url": "https://ollama.corp.example" },
                        "anthropic": { "api_url": "http://127.0.0.1:8787" }
                    }
                }"#,
                cx,
            );
        });

        cx.update(|cx| {
            let ollama = llm_provider_target("ollama", "Ollama".into(), cx)
                .expect("a remote Ollama has to be warned about");
            assert_eq!(
                ollama.hosts,
                vec![SharedString::from("ollama.corp.example")]
            );
            assert!(
                llm_provider_target("anthropic", "Anthropic".into(), cx).is_none(),
                "Anthropic pointed at a local proxy needs no warning"
            );
        });
    }

    #[gpui::test]
    fn consent_is_remembered_and_can_be_withdrawn(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });

        cx.update(|cx| {
            assert!(!is_acknowledged("anthropic", cx));
        });

        let with_consent = cx.update(|cx| {
            let mut content = cx
                .global::<settings::SettingsStore>()
                .merged_settings()
                .clone();
            apply_acknowledgement(&mut content, "anthropic", true);
            content.acknowledged_external_ai_providers.clone()
        });
        assert_eq!(with_consent, Some(vec!["anthropic".to_string()]));

        cx.update(|cx| {
            set_user_settings(
                r#"{ "acknowledged_external_ai_providers": ["anthropic"] }"#,
                cx,
            );
        });
        cx.update(|cx| {
            assert!(is_acknowledged("anthropic", cx));
            assert!(!is_acknowledged("openai", cx));
        });

        let without_consent = cx.update(|cx| {
            let mut content = cx
                .global::<settings::SettingsStore>()
                .merged_settings()
                .clone();
            apply_acknowledgement(&mut content, "anthropic", false);
            content.acknowledged_external_ai_providers.clone()
        });
        assert_eq!(without_consent, Some(Vec::new()));
    }

    #[test]
    fn edit_prediction_keys_are_namespaced() {
        let target = edit_prediction_target(
            "copilot",
            "GitHub Copilot",
            Some("https://api.githubcopilot.com"),
            &[],
            None,
        )
        .expect("Copilot should need consent");
        assert_eq!(target.key, SharedString::from("edit_prediction:copilot"));
        assert_eq!(
            target.key,
            SharedString::from(settings::EDIT_PREDICTION_COPILOT_CONSENT_KEY),
            "ai_onboarding's edit prediction upsell checks this same key directly \
             (it cannot depend on settings_ui to call edit_prediction_target); \
             the two must never drift apart"
        );
        assert!(
            edit_prediction_target(
                "ollama",
                "Ollama",
                Some("http://localhost:11434"),
                &[],
                None
            )
            .is_none()
        );
    }

    /// Backs the "AI" page's consent list: it has to see
    /// every acknowledged key and, when the key has one, its Zed Cloud /
    /// edit-prediction display name.
    #[gpui::test]
    fn acknowledged_provider_keys_and_names_are_resolved(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            // The raw-key fallback below goes through `LanguageModelRegistry`,
            // which panics on lookup if nothing has ever installed its global.
            // A real app always calls this during startup; tests have to do it
            // themselves.
            language_model::init(cx);
        });

        cx.update(|cx| {
            assert_eq!(acknowledged_provider_keys(cx), Vec::<SharedString>::new());
        });

        cx.update(|cx| {
            set_user_settings(
                r#"{
                    "acknowledged_external_ai_providers": [
                        "zed.dev",
                        "edit_prediction:copilot",
                        "edit_prediction:mystery-provider",
                        "some-unregistered-provider"
                    ]
                }"#,
                cx,
            );
        });

        cx.update(|cx| {
            let keys = acknowledged_provider_keys(cx);
            assert_eq!(
                keys,
                vec![
                    SharedString::from("zed.dev"),
                    SharedString::from("edit_prediction:copilot"),
                    SharedString::from("edit_prediction:mystery-provider"),
                    SharedString::from("some-unregistered-provider"),
                ]
            );

            assert_eq!(
                display_name_for_acknowledged_key(&keys[0], cx),
                SharedString::from("Zed")
            );
            assert_eq!(
                display_name_for_acknowledged_key(&keys[1], cx),
                SharedString::from("GitHub Copilot (編集予測)")
            );
            assert_eq!(
                display_name_for_acknowledged_key(&keys[2], cx),
                SharedString::from("mystery-provider (編集予測)"),
                "an unknown edit prediction id should still get the (編集予測) suffix"
            );
            assert_eq!(
                display_name_for_acknowledged_key(&keys[3], cx),
                keys[3],
                "an unregistered agent provider key should fall back to the raw key"
            );
        });
    }
}
