//! ADR-F94: "AI Connections", the one page that lists every way to use an AI
//! here, by whose AI it is. It configures nothing itself: each row says how
//! that AI signs in and what runs it, whether it is ready, and leads to the
//! page that sets it up (LLM Providers, External Agents, the ACP registry).

use agent_settings::AgentSettings;
use agent_settings::ai_vendor::{
    ANTHROPIC_AGENT_ID, AiVendor, GOOGLE_AGENT_ID, OPENAI_AGENT_ID, ProviderSignIn,
    is_language_model_provider_offered_before_setup, sign_in_of_language_model_provider,
    vendor_of_external_agent, vendor_of_language_model_provider,
};
use gpui::{ScrollHandle, prelude::*};
use language_model::LanguageModelRegistry;
use project::agent_server_store::ExternalAgentSource;
use settings::Settings as _;
use ui::{Divider, prelude::*};
use util::ResultExt as _;

use crate::SettingsWindow;

const LLM_PROVIDERS_PATH: &str = "llm_providers";
const EXTERNAL_AGENTS_PATH: &str = "agent_servers";
const ENABLE_AGENT_PATH: &str = "agent.enabled";

/// The vendors' own ACP agents, offered from their section before they are
/// added. Product names, not translated.
const VENDOR_AGENTS: [(&str, &str, AiVendor); 3] = [
    (ANTHROPIC_AGENT_ID, "Claude Agent", AiVendor::Anthropic),
    (OPENAI_AGENT_ID, "Codex", AiVendor::OpenAi),
    (GOOGLE_AGENT_ID, "Gemini CLI", AiVendor::Google),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionSignIn {
    Subscription,
    SubscriptionOrApiKey,
    ApiKey,
    Account,
    Local,
    AgentsOwn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionRunner {
    BuiltInAgent,
    ExternalAgent,
    OfficialCli,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionStatus {
    Ready,
    NotSetUp,
    /// Whether it is installed is decided on its own page (the official CLI's
    /// lookup runs there).
    SeeDetails,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionDestination {
    LlmProviders,
    ExternalAgents,
    AcpRegistry,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ConnectionRow {
    pub(crate) label: SharedString,
    pub(crate) sign_in: ConnectionSignIn,
    pub(crate) runner: ConnectionRunner,
    pub(crate) status: ConnectionStatus,
    pub(crate) destination: ConnectionDestination,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ProviderState {
    pub(crate) id: SharedString,
    pub(crate) name: SharedString,
    pub(crate) authenticated: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AgentState {
    pub(crate) id: SharedString,
    pub(crate) name: SharedString,
    pub(crate) from_registry: bool,
}

/// The rows of each vendor's section, in [`AiVendor::ALL`]'s order, leaving
/// out vendors with nothing to show.
pub(crate) fn connection_sections(
    providers: &[ProviderState],
    agents: Option<&[AgentState]>,
    claude_subscription_sign_in_allowed: bool,
) -> Vec<(AiVendor, Vec<ConnectionRow>)> {
    let mut sections = AiVendor::ALL
        .iter()
        .map(|vendor| (*vendor, Vec::new()))
        .collect::<Vec<_>>();
    let mut push = |vendor: AiVendor, row: ConnectionRow| {
        if let Some((_, rows)) = sections.iter_mut().find(|(each, _)| *each == vendor) {
            rows.push(row);
        }
    };

    push(
        AiVendor::Anthropic,
        ConnectionRow {
            label: "Claude Code".into(),
            sign_in: ConnectionSignIn::SubscriptionOrApiKey,
            runner: ConnectionRunner::OfficialCli,
            status: ConnectionStatus::SeeDetails,
            destination: ConnectionDestination::LlmProviders,
        },
    );

    // Without a project there is no agent store to read; the vendors' agents
    // are then neither claimed added nor offered for install.
    if let Some(agents) = agents {
        for (id, name, vendor) in VENDOR_AGENTS {
            let added = agents
                .iter()
                .find(|agent| agent.from_registry && agent.id.as_ref() == id);
            push(
                vendor,
                ConnectionRow {
                    label: added
                        .map(|agent| agent.name.clone())
                        .unwrap_or_else(|| name.into()),
                    // ADR-F55: Claude's subscription sign-in can be turned off.
                    sign_in: if vendor == AiVendor::Anthropic
                        && !claude_subscription_sign_in_allowed
                    {
                        ConnectionSignIn::ApiKey
                    } else {
                        ConnectionSignIn::SubscriptionOrApiKey
                    },
                    runner: ConnectionRunner::ExternalAgent,
                    status: if added.is_some() {
                        ConnectionStatus::Ready
                    } else {
                        ConnectionStatus::NotSetUp
                    },
                    destination: if added.is_some() {
                        ConnectionDestination::ExternalAgents
                    } else {
                        ConnectionDestination::AcpRegistry
                    },
                },
            );
        }
        for agent in agents {
            if vendor_of_external_agent(agent.id.as_ref(), agent.from_registry) != AiVendor::Other {
                continue;
            }
            push(
                AiVendor::Other,
                ConnectionRow {
                    label: agent.name.clone(),
                    sign_in: ConnectionSignIn::AgentsOwn,
                    runner: ConnectionRunner::ExternalAgent,
                    status: ConnectionStatus::Ready,
                    destination: ConnectionDestination::ExternalAgents,
                },
            );
        }
    }

    let mut local_listed = false;
    for provider in providers {
        if !provider.authenticated
            && !is_language_model_provider_offered_before_setup(provider.id.as_ref())
        {
            continue;
        }
        let vendor = vendor_of_language_model_provider(provider.id.as_ref());
        local_listed |= vendor == AiVendor::Local;
        push(
            vendor,
            ConnectionRow {
                label: provider.name.clone(),
                sign_in: match sign_in_of_language_model_provider(provider.id.as_ref()) {
                    ProviderSignIn::ApiKey => ConnectionSignIn::ApiKey,
                    ProviderSignIn::Subscription => ConnectionSignIn::Subscription,
                    ProviderSignIn::Account => ConnectionSignIn::Account,
                    ProviderSignIn::Local => ConnectionSignIn::Local,
                },
                runner: ConnectionRunner::BuiltInAgent,
                status: if provider.authenticated {
                    ConnectionStatus::Ready
                } else {
                    ConnectionStatus::NotSetUp
                },
                destination: ConnectionDestination::LlmProviders,
            },
        );
    }
    if !local_listed {
        push(
            AiVendor::Local,
            ConnectionRow {
                label: SharedString::from(i18n::t!("Local Model (Ollama, LM Studio, …)")),
                sign_in: ConnectionSignIn::Local,
                runner: ConnectionRunner::BuiltInAgent,
                status: ConnectionStatus::NotSetUp,
                destination: ConnectionDestination::LlmProviders,
            },
        );
    }

    sections.retain(|(_, rows)| !rows.is_empty());
    sections
}

fn vendor_label(vendor: AiVendor) -> SharedString {
    match vendor {
        AiVendor::Anthropic => i18n::t!("Claude (Anthropic)").into(),
        AiVendor::OpenAi => i18n::t!("OpenAI").into(),
        AiVendor::Google => i18n::t!("Google").into(),
        AiVendor::Local => i18n::t!("Local Models").into(),
        AiVendor::Other => i18n::t!("Other").into(),
    }
}

fn vendor_icon(vendor: AiVendor) -> IconName {
    match vendor {
        AiVendor::Anthropic => IconName::AiClaude,
        AiVendor::OpenAi => IconName::AiOpenAi,
        AiVendor::Google => IconName::AiGoogle,
        AiVendor::Local => IconName::AiOllama,
        AiVendor::Other => IconName::Sparkle,
    }
}

pub(crate) fn row_badges(row: &ConnectionRow) -> Vec<SharedString> {
    let mut badges = Vec::with_capacity(2);
    match row.sign_in {
        ConnectionSignIn::Subscription => badges.push(i18n::t!("Subscription").into()),
        ConnectionSignIn::SubscriptionOrApiKey => {
            badges.push(i18n::t!("Subscription or API Key").into())
        }
        ConnectionSignIn::ApiKey => badges.push(i18n::t!("API Key").into()),
        ConnectionSignIn::Account => badges.push(i18n::t!("Account").into()),
        ConnectionSignIn::Local => badges.push(i18n::t!("Local").into()),
        ConnectionSignIn::AgentsOwn => {}
    }
    badges.push(match row.runner {
        ConnectionRunner::BuiltInAgent => agent::NATIVE_AGENT_DISPLAY_NAME.into(),
        ConnectionRunner::ExternalAgent => i18n::t!("ACP Agent").into(),
        ConnectionRunner::OfficialCli => i18n::t!("Official CLI Tab").into(),
    });
    badges
}

fn gather_sections(
    settings_window: &SettingsWindow,
    cx: &App,
) -> Vec<(AiVendor, Vec<ConnectionRow>)> {
    let providers = LanguageModelRegistry::read_global(cx)
        .visible_providers()
        .into_iter()
        .map(|provider| ProviderState {
            id: provider.id().0,
            name: provider.name().0,
            authenticated: provider.is_authenticated(cx),
        })
        .collect::<Vec<_>>();
    let agents = settings_window.active_project(cx).map(|project| {
        let store = project.read(cx).agent_server_store().read(cx);
        store
            .external_agents()
            .map(|id| AgentState {
                id: id.0.clone(),
                name: store.agent_display_name(id).unwrap_or_else(|| id.0.clone()),
                from_registry: store.agent_source(id) == Some(ExternalAgentSource::Registry),
            })
            .collect::<Vec<_>>()
    });
    connection_sections(
        &providers,
        agents.as_deref(),
        super::external_agents_page::claude_subscription_sign_in_allowed(cx),
    )
}

pub(crate) fn render_ai_connections_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let agent_enabled = AgentSettings::get_global(cx).enabled(cx);
    let sections = gather_sections(settings_window, cx);
    let has_workspace = settings_window.original_window.is_some();

    v_flex()
        .id("ai-connections-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .gap_4()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            v_flex()
                .gap_1()
                .child(Label::new(i18n::t!("AI Connections")))
                .child(
                    Label::new(i18n::t!(
                        "Every AI you can use here, by whose AI it is. Each row shows how you sign in and what runs it. In the agent panel, the + menu lists the same AIs."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .when(!agent_enabled, |this| {
            this.child(
                h_flex()
                    .w_full()
                    .min_w_0()
                    .gap_2()
                    .justify_between()
                    .p_2()
                    .border_1()
                    .border_color(cx.theme().colors().border)
                    .rounded_sm()
                    .child(
                        Label::new(i18n::t!(
                            "AI features are off. Turn on \"Enable Agent\" on the AI page to use them."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    .child(
                        Button::new("ai-connections-enable-agent", i18n::t!("Go to \"Enable Agent\""))
                            .label_size(LabelSize::Small)
                            .style(ButtonStyle::Outlined)
                            .tab_index(0isize)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.navigate_to_setting(ENABLE_AGENT_PATH, window, cx);
                            })),
                    ),
            )
        })
        .children(sections.into_iter().map(|(vendor, rows)| {
            render_section(vendor, rows, has_workspace, cx).into_any_element()
        }))
        .into_any_element()
}

fn render_section(
    vendor: AiVendor,
    rows: Vec<ConnectionRow>,
    has_workspace: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let vendor_key = format!("{vendor:?}");
    v_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(
            h_flex()
                .gap_1p5()
                .child(
                    Icon::new(vendor_icon(vendor))
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .child(Label::new(vendor_label(vendor)).size(LabelSize::Large)),
        )
        .child(Divider::horizontal())
        .children(rows.into_iter().enumerate().map(|(index, row)| {
            render_row(&vendor_key, index, row, has_workspace, cx).into_any_element()
        }))
}

fn render_row(
    vendor_key: &str,
    index: usize,
    row: ConnectionRow,
    has_workspace: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let badges = row_badges(&row);
    let (status_icon, status_color, status_text): (IconName, Color, SharedString) = match row.status
    {
        ConnectionStatus::Ready => (IconName::Check, Color::Success, i18n::t!("Ready").into()),
        ConnectionStatus::NotSetUp => (
            IconName::Circle,
            Color::Muted,
            i18n::t!("Not Set Up").into(),
        ),
        ConnectionStatus::SeeDetails => {
            (IconName::Info, Color::Muted, i18n::t!("See Details").into())
        }
    };
    let (button_label, button_tooltip): (SharedString, Option<SharedString>) = match row.destination
    {
        ConnectionDestination::LlmProviders => (i18n::t!("LLM Providers").into(), None),
        ConnectionDestination::ExternalAgents => (i18n::t!("External Agents").into(), None),
        ConnectionDestination::AcpRegistry => (
            i18n::t!("Install from Registry").into(),
            (!has_workspace)
                .then(|| i18n::t!("A project window is needed to install agents").into()),
        ),
    };
    let destination = row.destination;

    h_flex()
        .w_full()
        .min_w_0()
        .py_1()
        .gap_2()
        .justify_between()
        .child(
            h_flex()
                .min_w_0()
                .flex_wrap()
                .gap_1p5()
                .child(Label::new(row.label))
                .children(badges.into_iter().map(|badge| {
                    Label::new(badge)
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                })),
        )
        .child(
            h_flex()
                .flex_none()
                .gap_2()
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Icon::new(status_icon)
                                .size(IconSize::XSmall)
                                .color(status_color),
                        )
                        .child(
                            Label::new(status_text)
                                .size(LabelSize::Small)
                                .color(status_color),
                        ),
                )
                .child(
                    Button::new(
                        SharedString::from(format!("ai-connection-{vendor_key}-{index}")),
                        button_label,
                    )
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Outlined)
                    .tab_index(0isize)
                    .disabled(destination == ConnectionDestination::AcpRegistry && !has_workspace)
                    .when_some(button_tooltip, |this, tooltip| {
                        this.tooltip(ui::Tooltip::text(tooltip))
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        open_destination(this, destination, window, cx);
                    })),
                ),
        )
}

fn open_destination(
    settings_window: &mut SettingsWindow,
    destination: ConnectionDestination,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    match destination {
        ConnectionDestination::LlmProviders => {
            settings_window.navigate_to_sub_page(LLM_PROVIDERS_PATH, window, cx);
        }
        ConnectionDestination::ExternalAgents => {
            settings_window.navigate_to_sub_page(EXTERNAL_AGENTS_PATH, window, cx);
        }
        ConnectionDestination::AcpRegistry => {
            if let Some(original_window) = settings_window.original_window {
                cx.activate(true);
                original_window
                    .update(cx, |_, window, cx| {
                        window.activate_window();
                        window.dispatch_action(Box::new(zed_actions::AcpRegistry), cx);
                    })
                    .log_err();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, name: &str, authenticated: bool) -> ProviderState {
        ProviderState {
            id: id.to_string().into(),
            name: name.to_string().into(),
            authenticated,
        }
    }

    fn agent(id: &str, name: &str, from_registry: bool) -> AgentState {
        AgentState {
            id: id.to_string().into(),
            name: name.to_string().into(),
            from_registry,
        }
    }

    fn rows(
        sections: &[(AiVendor, Vec<ConnectionRow>)],
        vendor: AiVendor,
    ) -> Vec<(String, ConnectionStatus, ConnectionDestination)> {
        sections
            .iter()
            .find(|(each, _)| *each == vendor)
            .map(|(_, rows)| {
                rows.iter()
                    .map(|row| (row.label.to_string(), row.status, row.destination))
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn each_vendor_lists_its_routes_with_where_to_set_them_up() {
        let providers = [
            provider("anthropic", "Anthropic", false),
            provider("openai", "OpenAI", true),
            provider("google", "Google AI", false),
            provider("ollama", "Ollama", true),
            provider("openrouter", "OpenRouter", false),
            provider("x_ai", "xAI", true),
        ];
        let agents = [
            agent("claude-acp", "Claude Agent", true),
            agent("opencode", "OpenCode", true),
        ];
        let sections = connection_sections(&providers, Some(&agents), true);

        assert_eq!(
            sections
                .iter()
                .map(|(vendor, _)| *vendor)
                .collect::<Vec<_>>(),
            AiVendor::ALL.to_vec()
        );
        use ConnectionDestination::*;
        use ConnectionStatus::*;
        assert_eq!(
            rows(&sections, AiVendor::Anthropic),
            [
                ("Claude Code".to_string(), SeeDetails, LlmProviders),
                ("Claude Agent".to_string(), Ready, ExternalAgents),
                ("Anthropic".to_string(), NotSetUp, LlmProviders),
            ]
        );
        assert_eq!(
            rows(&sections, AiVendor::OpenAi),
            [
                ("Codex".to_string(), NotSetUp, AcpRegistry),
                ("OpenAI".to_string(), Ready, LlmProviders),
            ]
        );
        assert_eq!(
            rows(&sections, AiVendor::Google),
            [
                ("Gemini CLI".to_string(), NotSetUp, AcpRegistry),
                ("Google AI".to_string(), NotSetUp, LlmProviders),
            ]
        );
        assert_eq!(
            rows(&sections, AiVendor::Local),
            [("Ollama".to_string(), Ready, LlmProviders)]
        );
        assert_eq!(
            rows(&sections, AiVendor::Other),
            [
                ("OpenCode".to_string(), Ready, ExternalAgents),
                ("xAI".to_string(), Ready, LlmProviders),
            ],
            "another vendor's provider is listed once it works"
        );
    }

    #[test]
    fn a_custom_agent_under_a_vendor_id_is_not_that_vendors() {
        let sections = connection_sections(&[], Some(&[agent("claude-acp", "Mine", false)]), true);
        assert_eq!(
            rows(&sections, AiVendor::Anthropic)[1],
            (
                "Claude Agent".to_string(),
                ConnectionStatus::NotSetUp,
                ConnectionDestination::AcpRegistry
            )
        );
        assert_eq!(
            rows(&sections, AiVendor::Other),
            [(
                "Mine".to_string(),
                ConnectionStatus::Ready,
                ConnectionDestination::ExternalAgents
            )]
        );
    }

    #[test]
    fn without_a_project_no_agent_is_claimed_or_offered() {
        let sections = connection_sections(&[provider("anthropic", "Anthropic", true)], None, true);
        assert_eq!(
            rows(&sections, AiVendor::Anthropic),
            [
                (
                    "Claude Code".to_string(),
                    ConnectionStatus::SeeDetails,
                    ConnectionDestination::LlmProviders
                ),
                (
                    "Anthropic".to_string(),
                    ConnectionStatus::Ready,
                    ConnectionDestination::LlmProviders
                ),
            ]
        );
        assert!(rows(&sections, AiVendor::OpenAi).is_empty());
        assert_eq!(
            rows(&sections, AiVendor::Local),
            [(
                "Local Model (Ollama, LM Studio, …)".to_string(),
                ConnectionStatus::NotSetUp,
                ConnectionDestination::LlmProviders
            )]
        );
    }

    #[test]
    fn rows_name_how_they_sign_in_and_what_runs_them() {
        let sections = connection_sections(
            &[
                provider("openai-subscribed", "ChatGPT Subscription", true),
                provider("lmstudio", "LM Studio", true),
            ],
            Some(&[agent("claude-acp", "Claude Agent", true)]),
            false,
        );
        let find = |label: &str| {
            sections
                .iter()
                .flat_map(|(_, rows)| rows)
                .find(|row| row.label.as_ref() == label)
                .cloned()
                .unwrap_or_else(|| panic!("no row {label:?}"))
        };
        assert_eq!(
            row_badges(&find("Claude Code")),
            ["Subscription or API Key", "Official CLI Tab"]
        );
        assert_eq!(
            row_badges(&find("ChatGPT Subscription")),
            ["Subscription", agent::NATIVE_AGENT_DISPLAY_NAME]
        );
        assert_eq!(
            row_badges(&find("LM Studio")),
            ["Local", agent::NATIVE_AGENT_DISPLAY_NAME]
        );
        assert_eq!(
            row_badges(&find("Codex")),
            ["Subscription or API Key", "ACP Agent"]
        );
        assert_eq!(
            row_badges(&find("Claude Agent")),
            ["API Key", "ACP Agent"],
            "with Claude's subscription sign-in turned off, only the key is left"
        );
    }
}
