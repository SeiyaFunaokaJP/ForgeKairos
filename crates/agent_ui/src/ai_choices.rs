//! ADR-F94: the agent panel's "Choose AI" menu (the `+` button).
//!
//! The menu used to be organized by how an AI runs — the built-in agent, the
//! official CLI in a tab, external ACP agents — and asked which vendor and
//! which sign-in each of those used to be remembered. It is now organized by
//! whose AI it is, with how it signs in, what runs it and whether it is
//! sandboxed shown beside each entry. [`build_ai_choice_sections`] decides the
//! entries from plain data so that what the menu offers can be tested without
//! a window; the rest of this file turns them into menu rows and acts on them.

use std::sync::Arc;

use agent_settings::AgentSettings;
use agent_settings::ai_vendor::{
    AiVendor, ProviderSignIn, is_language_model_provider_offered_before_setup,
    sign_in_of_language_model_provider, vendor_of_external_agent,
    vendor_of_language_model_provider,
};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{App, Entity, FocusHandle, SharedString, WeakEntity, Window};
use language_model::{ConfiguredModel, IconOrSvg, LanguageModelRegistry};
use project::{AgentId, Project};
use settings::{LanguageModelSelection, Settings as _, update_settings_file};
use ui::{ContextMenuItem, DocumentationAside, KeyBinding, prelude::*};
use util::ResultExt as _;
use workspace::Workspace;

use crate::{Agent, AgentThreadSource, NewExternalAgentThread, NewThread, agent_panel::AgentPanel};

/// Where an AI's work runs, as far as the sandbox is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiPlacement {
    Sandbox,
    /// The user chose, in their own settings file, to always start it outside.
    OutsideByChoice,
    /// No sandbox is available for this project; each start asks first.
    OutsideAfterConfirmation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiSignIn {
    Subscription,
    SubscriptionOrApiKey,
    ApiKey,
    Account,
    Local,
    /// Whatever the agent itself offers; nothing this IDE can name.
    AgentsOwn,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiRunner {
    BuiltInAgent,
    ExternalAgent,
    OfficialCli,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AiChoiceTarget {
    /// A thread of the built-in agent on this provider's models.
    NativeThread {
        provider_id: SharedString,
    },
    ExternalAgentThread {
        agent_id: AgentId,
    },
    LaunchClaudeCode,
    /// The provider still needs its key or sign-in: open its settings.
    SetUpProvider,
    /// The provider is signed in but offers no model yet (a local server with
    /// nothing downloaded, say): open its settings rather than start a thread
    /// that would silently keep the previous provider's model.
    SetUpModels,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AiChoiceIcon {
    Icon(IconName),
    Svg(SharedString),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AiChoice {
    pub(crate) label: SharedString,
    pub(crate) icon: AiChoiceIcon,
    pub(crate) sign_in: AiSignIn,
    pub(crate) runner: AiRunner,
    pub(crate) placement: Option<AiPlacement>,
    pub(crate) target: AiChoiceTarget,
    /// What `NewThread` (the `+` button's shortcut) opens: the AI used last.
    pub(crate) is_current: bool,
}

impl AiChoice {
    pub(crate) fn needs_setup(&self) -> bool {
        matches!(
            self.target,
            AiChoiceTarget::SetUpProvider | AiChoiceTarget::SetUpModels
        )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct AiChoiceSection {
    pub(crate) vendor: AiVendor,
    pub(crate) choices: Vec<AiChoice>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct NativeProviderInput {
    pub(crate) id: SharedString,
    pub(crate) name: SharedString,
    pub(crate) icon: AiChoiceIcon,
    pub(crate) authenticated: bool,
    pub(crate) has_models: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct ExternalAgentInput {
    pub(crate) id: AgentId,
    pub(crate) display_name: SharedString,
    pub(crate) icon_path: Option<SharedString>,
    pub(crate) from_registry: bool,
    pub(crate) outside_by_choice: bool,
    /// ADR-F55 `allow_subscription_sign_in`; only Claude's is read.
    pub(crate) subscription_sign_in_allowed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AiChoiceInputs {
    pub(crate) native_providers: Vec<NativeProviderInput>,
    /// The provider of the built-in agent's default model.
    pub(crate) native_default_provider: Option<SharedString>,
    /// Empty for a collaborator's guest, who can only use the built-in agent.
    pub(crate) external_agents: Vec<ExternalAgentInput>,
    /// `None` where this project cannot run a terminal.
    pub(crate) claude_code: Option<AiPlacement>,
    pub(crate) selected_agent: Agent,
}

pub(crate) fn build_ai_choice_sections(inputs: &AiChoiceInputs) -> Vec<AiChoiceSection> {
    let mut sections = AiVendor::ALL
        .iter()
        .map(|vendor| AiChoiceSection {
            vendor: *vendor,
            choices: Vec::new(),
        })
        .collect::<Vec<_>>();
    let mut push = |vendor: AiVendor, choice: AiChoice| {
        if let Some(section) = sections.iter_mut().find(|section| section.vendor == vendor) {
            section.choices.push(choice);
        }
    };

    if let Some(placement) = inputs.claude_code {
        push(
            AiVendor::Anthropic,
            AiChoice {
                label: "Claude Code".into(),
                icon: AiChoiceIcon::Icon(IconName::AiClaude),
                sign_in: AiSignIn::SubscriptionOrApiKey,
                runner: AiRunner::OfficialCli,
                placement: Some(placement),
                target: AiChoiceTarget::LaunchClaudeCode,
                // A tab of its own rather than a thread: `NewThread` never
                // opens it, so it is never what the shortcut repeats.
                is_current: false,
            },
        );
    }

    for agent in &inputs.external_agents {
        let vendor = vendor_of_external_agent(agent.id.as_ref(), agent.from_registry);
        let sign_in = match vendor {
            AiVendor::Anthropic if agent.subscription_sign_in_allowed => {
                AiSignIn::SubscriptionOrApiKey
            }
            AiVendor::Anthropic => AiSignIn::ApiKey,
            AiVendor::OpenAi | AiVendor::Google => AiSignIn::SubscriptionOrApiKey,
            AiVendor::Local | AiVendor::Other => AiSignIn::AgentsOwn,
        };
        push(
            vendor,
            AiChoice {
                label: agent.display_name.clone(),
                icon: match &agent.icon_path {
                    Some(path) => AiChoiceIcon::Svg(path.clone()),
                    None => AiChoiceIcon::Icon(IconName::Sparkle),
                },
                sign_in,
                runner: AiRunner::ExternalAgent,
                placement: Some(if agent.outside_by_choice {
                    AiPlacement::OutsideByChoice
                } else {
                    AiPlacement::Sandbox
                }),
                target: AiChoiceTarget::ExternalAgentThread {
                    agent_id: agent.id.clone(),
                },
                is_current: inputs.selected_agent
                    == Agent::Custom {
                        id: agent.id.clone(),
                    },
            },
        );
    }

    let mut local_offered = false;
    for provider in &inputs.native_providers {
        let vendor = vendor_of_language_model_provider(provider.id.as_ref());
        if !provider.authenticated
            && !is_language_model_provider_offered_before_setup(provider.id.as_ref())
        {
            continue;
        }
        local_offered |= vendor == AiVendor::Local;
        let target = if !provider.authenticated {
            AiChoiceTarget::SetUpProvider
        } else if !provider.has_models {
            AiChoiceTarget::SetUpModels
        } else {
            AiChoiceTarget::NativeThread {
                provider_id: provider.id.clone(),
            }
        };
        let is_current = matches!(target, AiChoiceTarget::NativeThread { .. })
            && inputs.selected_agent == Agent::NativeAgent
            && inputs.native_default_provider.as_ref() == Some(&provider.id);
        push(
            vendor,
            AiChoice {
                label: provider.name.clone(),
                icon: provider.icon.clone(),
                sign_in: match sign_in_of_language_model_provider(provider.id.as_ref()) {
                    ProviderSignIn::ApiKey => AiSignIn::ApiKey,
                    ProviderSignIn::Subscription => AiSignIn::Subscription,
                    ProviderSignIn::Account => AiSignIn::Account,
                    ProviderSignIn::Local => AiSignIn::Local,
                },
                runner: AiRunner::BuiltInAgent,
                placement: None,
                target,
                is_current,
            },
        );
    }
    if !local_offered {
        push(
            AiVendor::Local,
            AiChoice {
                label: SharedString::from(i18n::t!("Local Model (Ollama, LM Studio, …)")),
                icon: AiChoiceIcon::Icon(IconName::AiOllama),
                sign_in: AiSignIn::Local,
                runner: AiRunner::BuiltInAgent,
                placement: None,
                target: AiChoiceTarget::SetUpProvider,
                is_current: false,
            },
        );
    }

    sections.retain(|section| !section.choices.is_empty());
    for section in &mut sections {
        // Ready entries first, so that what works is never below what does not.
        section.choices.sort_by_key(|choice| choice.needs_setup());
    }
    sections
}

/// Gathers [`AiChoiceInputs`] from the running app.
pub(crate) fn gather_ai_choice_inputs(
    project: &Entity<Project>,
    selected_agent: Agent,
    supports_terminal: bool,
    cx: &App,
) -> AiChoiceInputs {
    let registry = LanguageModelRegistry::read_global(cx);
    let native_providers = registry
        .visible_providers()
        .into_iter()
        .map(|provider| NativeProviderInput {
            id: provider.id().0,
            name: provider.name().0,
            icon: match provider.icon() {
                IconOrSvg::Icon(icon) => AiChoiceIcon::Icon(icon),
                IconOrSvg::Svg(path) => AiChoiceIcon::Svg(path),
            },
            authenticated: provider.is_authenticated(cx),
            has_models: !provider.provided_models(cx).is_empty(),
        })
        .collect();
    let native_default_provider = registry
        .default_model()
        .map(|configured| configured.provider.id().0);

    let project_ref = project.read(cx);
    let external_agents = if project_ref.is_via_collab() {
        Vec::new()
    } else {
        let store = project_ref.agent_server_store().read(cx);
        crate::agent_panel::external_agent_menu_items(store, cx)
            .into_iter()
            .map(|item| {
                let from_registry = store.agent_source(&item.id)
                    == Some(project::agent_server_store::ExternalAgentSource::Registry);
                let id = item.id.as_ref().to_string();
                ExternalAgentInput {
                    outside_by_choice: project::agent_sandbox::runs_outside_sandbox_by_user_choice(
                        &id, cx,
                    ),
                    subscription_sign_in_allowed: !agent_servers::subscription_sign_in_disabled(
                        &id, cx,
                    ),
                    id: item.id,
                    display_name: item.display_name,
                    icon_path: item.icon_path,
                    from_registry,
                }
            })
            .collect()
    };

    AiChoiceInputs {
        native_providers,
        native_default_provider,
        external_agents,
        claude_code: supports_terminal
            .then(|| crate::sandboxed_claude_code::claude_code_placement(project_ref, cx)),
        selected_agent,
    }
}

pub(crate) fn vendor_label(vendor: AiVendor) -> SharedString {
    match vendor {
        AiVendor::Anthropic => i18n::t!("Claude (Anthropic)").into(),
        AiVendor::OpenAi => i18n::t!("OpenAI").into(),
        AiVendor::Google => i18n::t!("Google").into(),
        AiVendor::Local => i18n::t!("Local Models").into(),
        AiVendor::Other => i18n::t!("Other").into(),
    }
}

/// The short facts shown beside an entry: sign-in, runner, sandbox.
pub(crate) fn choice_badges(choice: &AiChoice) -> Vec<SharedString> {
    let mut badges = Vec::with_capacity(3);
    if choice.needs_setup() {
        badges.push(match (&choice.target, choice.sign_in) {
            (AiChoiceTarget::SetUpModels, _) | (_, AiSignIn::Local) => i18n::t!("Set Up…").into(),
            (_, AiSignIn::Subscription | AiSignIn::Account) => i18n::t!("Sign In…").into(),
            _ => i18n::t!("Add API Key…").into(),
        });
        return badges;
    }
    match choice.sign_in {
        AiSignIn::Subscription => badges.push(i18n::t!("Subscription").into()),
        AiSignIn::SubscriptionOrApiKey => badges.push(i18n::t!("Subscription or API Key").into()),
        AiSignIn::ApiKey => badges.push(i18n::t!("API Key").into()),
        AiSignIn::Account => badges.push(i18n::t!("Account").into()),
        AiSignIn::Local => badges.push(i18n::t!("Local").into()),
        AiSignIn::AgentsOwn => {}
    }
    badges.push(match choice.runner {
        AiRunner::BuiltInAgent => agent::NATIVE_AGENT_DISPLAY_NAME.into(),
        AiRunner::ExternalAgent => i18n::t!("ACP Agent").into(),
        AiRunner::OfficialCli => i18n::t!("Official CLI Tab").into(),
    });
    match choice.placement {
        Some(AiPlacement::Sandbox) => badges.push(i18n::t!("Sandbox").into()),
        Some(AiPlacement::OutsideByChoice) => badges.push(i18n::t!("Outside the Sandbox").into()),
        Some(AiPlacement::OutsideAfterConfirmation) => {
            badges.push(i18n::t!("Outside the Sandbox (Asks First)").into())
        }
        None => {}
    }
    badges
}

fn choice_explanation(choice: &AiChoice) -> SharedString {
    match &choice.target {
        AiChoiceTarget::LaunchClaudeCode => i18n::t!(
            "Launches the official Claude Code CLI in a tab of this panel. You sign in on the CLI's screen."
        )
        .into(),
        AiChoiceTarget::ExternalAgentThread { .. } => i18n::t!(
            "Starts a thread with {name} through the Agent Client Protocol. You sign in with the agent's own sign-in.",
            name = choice.label
        )
        .into(),
        AiChoiceTarget::NativeThread { .. } => i18n::t!(
            "Starts a thread of {agent} on {provider}'s models. The key or sign-in is the one in Settings > AI > LLM Providers.",
            agent = agent::NATIVE_AGENT_DISPLAY_NAME,
            provider = choice.label
        )
        .into(),
        AiChoiceTarget::SetUpProvider => i18n::t!(
            "Not set up yet. Opens Settings > AI > LLM Providers, where you add the key or sign in."
        )
        .into(),
        AiChoiceTarget::SetUpModels => i18n::t!(
            "No model is available from this provider yet. Opens Settings > AI > LLM Providers."
        )
        .into(),
    }
}

/// A menu row for `choice`: its icon and name, then its badges, and the
/// `NewThread` shortcut on the entry the shortcut opens.
pub(crate) fn ai_choice_menu_item(
    choice: AiChoice,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    cx: &App,
) -> ContextMenuItem {
    let badges = choice_badges(&choice);
    let explanation = choice_explanation(&choice);
    let needs_setup = choice.needs_setup();
    let launches_claude_code = choice.target == AiChoiceTarget::LaunchClaudeCode;
    let AiChoice {
        label,
        icon,
        is_current,
        target,
        ..
    } = choice;
    ContextMenuItem::custom_entry(
        move |_window, cx| {
            let icon = match &icon {
                AiChoiceIcon::Icon(icon) => Icon::new(*icon),
                AiChoiceIcon::Svg(path) => Icon::from_external_svg(path.clone()),
            }
            .size(IconSize::Small)
            .color(Color::Muted);
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .justify_between()
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(icon)
                        .child(Label::new(label.clone()).color(if needs_setup {
                            Color::Muted
                        } else {
                            Color::Default
                        }))
                        .children(badges.iter().map(|badge| {
                            Label::new(badge.clone()).size(LabelSize::XSmall).color(
                                if needs_setup {
                                    Color::Accent
                                } else {
                                    Color::Muted
                                },
                            )
                        })),
                )
                .when(is_current, |this| {
                    this.child(KeyBinding::for_action_in(&NewThread, &focus_handle, cx))
                })
                .when(launches_claude_code, |this| {
                    this.child(KeyBinding::for_action_in(
                        &zed_actions::terminal::LaunchClaudeCode::default(),
                        &focus_handle,
                        cx,
                    ))
                })
                .into_any_element()
        },
        move |window, cx| open_ai_choice(&target, &workspace, window, cx),
        Some(DocumentationAside::new(
            crate::ui::documentation_aside_side(cx),
            std::rc::Rc::new(move |_| Label::new(explanation.clone()).into_any_element()),
        )),
    )
}

pub(crate) fn open_ai_choice(
    target: &AiChoiceTarget,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    match target {
        AiChoiceTarget::LaunchClaudeCode => {
            window.dispatch_action(
                Box::new(zed_actions::terminal::LaunchClaudeCode::default()),
                cx,
            );
        }
        AiChoiceTarget::SetUpProvider | AiChoiceTarget::SetUpModels => {
            open_llm_providers(window, cx);
        }
        AiChoiceTarget::ExternalAgentThread { agent_id } => {
            with_panel(workspace, window, cx, |panel, window, cx| {
                panel.new_external_agent_thread(
                    &NewExternalAgentThread {
                        agent: agent_id.clone(),
                    },
                    window,
                    cx,
                );
            });
        }
        AiChoiceTarget::NativeThread { provider_id } => {
            let Some(fs) = workspace
                .upgrade()
                .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                .map(|panel| panel.read(cx).fs())
            else {
                return;
            };
            let switch = switch_default_model_to_provider(provider_id, fs, cx);
            if matches!(switch, ModelSwitch::NoModels) {
                open_llm_providers(window, cx);
                return;
            }
            with_panel(workspace, window, cx, move |panel, window, cx| {
                panel.select_native_agent_for_new_thread();
                panel.activate_new_thread(true, AgentThreadSource::AgentPanel, window, cx);
                // The empty draft the panel reuses was made with the model of
                // its time, and a new one reads the settings file before the
                // switch reaches it; the draft has to follow the choice too.
                // Only a draft: where no draft could be opened (no folder),
                // the view still shows a conversation whose model is not this
                // menu's.
                if let ModelSwitch::Switched {
                    model,
                    selection: Some(selection),
                } = switch
                    && panel.active_thread_is_draft(cx)
                    && let Some(thread) = panel.active_native_agent_thread(cx)
                {
                    thread.update(cx, |thread, cx| {
                        thread.set_model(model.model, cx);
                        thread.set_thinking_enabled(selection.enable_thinking, cx);
                        thread.set_thinking_effort(selection.effort, cx);
                        if let Some(speed) = selection.speed {
                            thread.set_speed(speed, cx);
                        }
                    });
                }
            });
        }
    }
}

fn open_llm_providers(window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(zed_actions::OpenSettingsAt {
            path: "llm_providers".to_string(),
            target: None,
        }),
        cx,
    );
}

fn with_panel(
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
    update: impl FnOnce(&mut AgentPanel, &mut Window, &mut gpui::Context<AgentPanel>),
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };
    workspace.update(cx, |workspace, cx| {
        if let Some(panel) = workspace.panel::<AgentPanel>(cx) {
            panel.update(cx, |panel, cx| update(panel, window, cx));
        }
    });
}

enum ModelSwitch {
    /// `selection` is `None` when the default model already was this
    /// provider's and stays as the user chose it.
    Switched {
        model: ConfiguredModel,
        selection: Option<LanguageModelSelection>,
    },
    NoModels,
    UnknownProvider,
}

/// Where the model last used with each provider is kept, so that coming back
/// to a provider through this menu returns to it. In the key-value store
/// rather than the settings file: it is a memory of what was used, not a
/// setting, and the settings file already holds the one that is in use.
/// One key per provider, so that two quick switches never write over each
/// other's entry.
const LAST_MODEL_OF_PROVIDER_KEY_PREFIX: &str = "agent_ui_last_model_of_provider:";

fn last_model_of_provider(provider_id: &str, cx: &App) -> Option<LanguageModelSelection> {
    KeyValueStore::global(cx)
        .read_kvp(&format!("{LAST_MODEL_OF_PROVIDER_KEY_PREFIX}{provider_id}"))
        .log_err()
        .flatten()
        .and_then(|json| serde_json::from_str(&json).log_err())
}

fn remember_last_model_of_provider(selection: &LanguageModelSelection, cx: &App) {
    let Some(json) = serde_json::to_string(selection).log_err() else {
        return;
    };
    let key = format!(
        "{LAST_MODEL_OF_PROVIDER_KEY_PREFIX}{}",
        selection.provider.0
    );
    let kvp = KeyValueStore::global(cx);
    cx.background_spawn(async move { kvp.write_kvp(key, json).await.log_err() })
        .detach();
}

/// Makes a model of `provider_id` the built-in agent's default, unless the
/// current one already is that provider's: picking "Anthropic" while on one
/// of its models keeps the model the user chose. Otherwise the model last used
/// with that provider comes back, with its thinking and effort, then one of
/// the user's favorites from it, then the provider's own default.
fn switch_default_model_to_provider(
    provider_id: &SharedString,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) -> ModelSwitch {
    let registry = LanguageModelRegistry::global(cx);
    if let Some(current) = registry.read(cx).default_model()
        && current.provider.id().0 == *provider_id
    {
        return ModelSwitch::Switched {
            model: current,
            selection: None,
        };
    }
    let Some(provider) = registry
        .read(cx)
        .provider(&language_model::LanguageModelProviderId(
            provider_id.clone(),
        ))
    else {
        return ModelSwitch::UnknownProvider;
    };
    let models = provider.provided_models(cx);
    let settings = AgentSettings::get_global(cx);
    let leaving = settings.default_model.clone();
    let remembered = last_model_of_provider(provider_id, cx)
        .into_iter()
        .chain(
            settings
                .favorite_models
                .iter()
                .filter(|favorite| favorite.provider.0 == provider_id.as_ref())
                .cloned(),
        )
        .find_map(|selection| {
            models
                .iter()
                .find(|model| model.id().0.as_ref() == selection.model)
                .map(|model| (model.clone(), Some(selection)))
        });
    let Some((model, earlier_selection)) = remembered.or_else(|| {
        provider
            .default_model(cx)
            .or_else(|| provider.recommended_models(cx).into_iter().next())
            .or_else(|| models.first().cloned())
            .map(|model| (model, None))
    }) else {
        return ModelSwitch::NoModels;
    };
    let selection = agent_settings::language_model_to_selection(&model, earlier_selection.as_ref());
    if let Some(leaving) = &leaving {
        remember_last_model_of_provider(leaving, cx);
    }
    let configured = ConfiguredModel { provider, model };
    registry.update(cx, |registry, cx| {
        registry.set_default_model(Some(configured.clone()), cx)
    });
    update_settings_file(fs, cx, {
        let selection = selection.clone();
        move |settings, _| {
            settings.agent.get_or_insert_default().set_model(selection);
        }
    });
    ModelSwitch::Switched {
        model: configured,
        selection: Some(selection),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(id: &str, name: &str, authenticated: bool) -> NativeProviderInput {
        NativeProviderInput {
            id: id.to_string().into(),
            name: name.to_string().into(),
            icon: AiChoiceIcon::Icon(IconName::Sparkle),
            authenticated,
            has_models: true,
        }
    }

    fn agent(id: &str, name: &str, from_registry: bool) -> ExternalAgentInput {
        ExternalAgentInput {
            id: AgentId::new(id.to_string()),
            display_name: name.to_string().into(),
            icon_path: None,
            from_registry,
            outside_by_choice: false,
            subscription_sign_in_allowed: true,
        }
    }

    fn labels(sections: &[AiChoiceSection], vendor: AiVendor) -> Vec<String> {
        sections
            .iter()
            .find(|section| section.vendor == vendor)
            .map(|section| {
                section
                    .choices
                    .iter()
                    .map(|choice| choice.label.to_string())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn choice<'a>(sections: &'a [AiChoiceSection], label: &str) -> &'a AiChoice {
        sections
            .iter()
            .flat_map(|section| &section.choices)
            .find(|choice| choice.label.as_ref() == label)
            .unwrap_or_else(|| panic!("no entry {label:?} in {sections:#?}"))
    }

    fn everything() -> AiChoiceInputs {
        AiChoiceInputs {
            native_providers: vec![
                provider("anthropic", "Anthropic", true),
                provider("openai", "OpenAI", false),
                provider("openai-subscribed", "ChatGPT Subscription", true),
                provider("google", "Google AI", false),
                provider("ollama", "Ollama", true),
                provider("openrouter", "OpenRouter", false),
                provider("mistral", "Mistral", true),
            ],
            native_default_provider: Some("anthropic".into()),
            external_agents: vec![
                agent("claude-acp", "Claude Agent", true),
                agent("codex-acp", "Codex", true),
                agent("gemini", "Gemini CLI", true),
                agent("opencode", "OpenCode", true),
            ],
            claude_code: Some(AiPlacement::Sandbox),
            selected_agent: Agent::NativeAgent,
        }
    }

    #[test]
    fn entries_are_grouped_by_whose_ai_they_are() {
        let sections = build_ai_choice_sections(&everything());
        assert_eq!(
            sections
                .iter()
                .map(|section| section.vendor)
                .collect::<Vec<_>>(),
            AiVendor::ALL.to_vec()
        );
        assert_eq!(
            labels(&sections, AiVendor::Anthropic),
            ["Claude Code", "Claude Agent", "Anthropic"]
        );
        assert_eq!(
            labels(&sections, AiVendor::OpenAi),
            ["Codex", "ChatGPT Subscription", "OpenAI"],
            "the provider that still needs its key goes below the ones that work"
        );
        assert_eq!(
            labels(&sections, AiVendor::Google),
            ["Gemini CLI", "Google AI"]
        );
        assert_eq!(labels(&sections, AiVendor::Local), ["Ollama"]);
        assert_eq!(
            labels(&sections, AiVendor::Other),
            ["OpenCode", "Mistral"],
            "a provider of another vendor is listed only once it works"
        );
    }

    #[test]
    fn each_entry_says_how_it_signs_in_what_runs_it_and_where() {
        let sections = build_ai_choice_sections(&everything());

        let claude_code = choice(&sections, "Claude Code");
        assert_eq!(claude_code.sign_in, AiSignIn::SubscriptionOrApiKey);
        assert_eq!(claude_code.runner, AiRunner::OfficialCli);
        assert_eq!(claude_code.placement, Some(AiPlacement::Sandbox));
        assert_eq!(claude_code.target, AiChoiceTarget::LaunchClaudeCode);

        let claude_agent = choice(&sections, "Claude Agent");
        assert_eq!(claude_agent.sign_in, AiSignIn::SubscriptionOrApiKey);
        assert_eq!(claude_agent.runner, AiRunner::ExternalAgent);
        assert_eq!(
            claude_agent.target,
            AiChoiceTarget::ExternalAgentThread {
                agent_id: AgentId::new("claude-acp")
            }
        );

        let anthropic = choice(&sections, "Anthropic");
        assert_eq!(anthropic.sign_in, AiSignIn::ApiKey);
        assert_eq!(anthropic.runner, AiRunner::BuiltInAgent);
        assert_eq!(anthropic.placement, None);
        assert_eq!(
            anthropic.target,
            AiChoiceTarget::NativeThread {
                provider_id: "anthropic".into()
            }
        );

        assert_eq!(
            choice(&sections, "ChatGPT Subscription").sign_in,
            AiSignIn::Subscription
        );
        assert_eq!(choice(&sections, "Ollama").sign_in, AiSignIn::Local);
        assert_eq!(choice(&sections, "OpenCode").sign_in, AiSignIn::AgentsOwn);
    }

    #[test]
    fn a_provider_without_its_key_opens_its_settings() {
        let sections = build_ai_choice_sections(&everything());
        let openai = choice(&sections, "OpenAI");
        assert_eq!(openai.target, AiChoiceTarget::SetUpProvider);
        assert!(openai.needs_setup());
        assert_eq!(choice_badges(openai), ["Add API Key…"]);
        assert!(!openai.is_current);
    }

    #[test]
    fn the_badges_follow_the_settings_that_change_them() {
        let mut inputs = everything();
        inputs.claude_code = Some(AiPlacement::OutsideAfterConfirmation);
        for agent in &mut inputs.external_agents {
            if agent.id.as_ref() == "claude-acp" {
                agent.subscription_sign_in_allowed = false;
                agent.outside_by_choice = true;
            }
        }
        let sections = build_ai_choice_sections(&inputs);

        let claude_code = choice(&sections, "Claude Code");
        assert_eq!(
            choice_badges(claude_code),
            [
                "Subscription or API Key",
                "Official CLI Tab",
                "Outside the Sandbox (Asks First)"
            ]
        );
        let claude_agent = choice(&sections, "Claude Agent");
        assert_eq!(claude_agent.sign_in, AiSignIn::ApiKey);
        assert_eq!(
            choice_badges(claude_agent),
            ["API Key", "ACP Agent", "Outside the Sandbox"]
        );
        assert_eq!(
            choice_badges(choice(&sections, "Codex")),
            ["Subscription or API Key", "ACP Agent", "Sandbox"]
        );
        assert_eq!(
            choice_badges(choice(&sections, "Anthropic")),
            ["API Key", agent::NATIVE_AGENT_DISPLAY_NAME]
        );
    }

    #[test]
    fn only_registry_agents_are_filed_under_a_vendor() {
        let mut inputs = everything();
        inputs.external_agents = vec![agent("claude-acp", "My Claude Wrapper", false)];
        let sections = build_ai_choice_sections(&inputs);
        assert!(labels(&sections, AiVendor::Other).contains(&"My Claude Wrapper".to_string()));
        assert_eq!(
            choice(&sections, "My Claude Wrapper").sign_in,
            AiSignIn::AgentsOwn,
            "a command the user named claude-acp is not taken for Anthropic's"
        );
    }

    #[test]
    fn the_shortcut_marks_the_ai_used_last() {
        let sections = build_ai_choice_sections(&everything());
        let current = sections
            .iter()
            .flat_map(|section| &section.choices)
            .filter(|choice| choice.is_current)
            .map(|choice| choice.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(current, ["Anthropic"]);

        let mut inputs = everything();
        inputs.selected_agent = Agent::Custom {
            id: AgentId::new("codex-acp"),
        };
        let sections = build_ai_choice_sections(&inputs);
        let current = sections
            .iter()
            .flat_map(|section| &section.choices)
            .filter(|choice| choice.is_current)
            .map(|choice| choice.label.to_string())
            .collect::<Vec<_>>();
        assert_eq!(current, ["Codex"]);
    }

    #[test]
    fn without_a_terminal_or_external_agents_only_the_built_in_agent_is_offered() {
        let mut inputs = everything();
        inputs.claude_code = None;
        inputs.external_agents.clear();
        let sections = build_ai_choice_sections(&inputs);
        assert!(
            sections
                .iter()
                .flat_map(|section| &section.choices)
                .all(|choice| choice.runner == AiRunner::BuiltInAgent)
        );
    }

    fn switched_to(switch: &ModelSwitch) -> Option<(String, Option<LanguageModelSelection>)> {
        match switch {
            ModelSwitch::Switched { model, selection } => {
                Some((model.model.id().0.to_string(), selection.clone()))
            }
            ModelSwitch::NoModels | ModelSwitch::UnknownProvider => None,
        }
    }

    fn default_model_id(cx: &mut gpui::TestAppContext) -> Option<String> {
        cx.update(|cx| {
            LanguageModelRegistry::read_global(cx)
                .default_model()
                .map(|configured| configured.model.id().0.to_string())
        })
    }

    /// What the settings file would say after a switch: the store of a test
    /// does not watch the fake file system.
    fn settings_say_default_model(
        selection: LanguageModelSelection,
        cx: &mut gpui::TestAppContext,
    ) {
        cx.update(|cx| {
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.agent.get_or_insert_default().set_model(selection);
                });
            });
        });
    }

    /// Picking the provider the default model is already on keeps the model
    /// the user chose and leaves the settings file alone. Another provider
    /// becomes the default with one of the user's favorites from it; coming
    /// back returns to the model last used there, thinking included. A
    /// provider with no model is not switched to.
    #[gpui::test]
    async fn switching_providers_comes_back_to_the_model_used_last(cx: &mut gpui::TestAppContext) {
        use language_model::fake_provider::{FakeLanguageModel, FakeLanguageModelProvider};
        use language_model::{
            LanguageModel, LanguageModelProvider, LanguageModelProviderId,
            LanguageModelProviderName,
        };

        let fs = fs::FakeFs::new(cx.executor());
        fs.create_dir(::paths::settings_file().parent().expect("settings folder"))
            .await
            .expect("the settings folder can be made");
        let fake_model = |provider: &str, id: &str, thinking: bool| -> Arc<dyn LanguageModel> {
            Arc::new(FakeLanguageModel::with_id_and_thinking(
                provider, id, id, thinking,
            ))
        };
        let fake_provider = |id: &str, models: Vec<Arc<dyn LanguageModel>>| {
            Arc::new(
                FakeLanguageModelProvider::new(
                    LanguageModelProviderId::from(id.to_string()),
                    LanguageModelProviderName::from(id.to_string()),
                )
                .with_models(models),
            )
        };
        let anthropic = fake_provider(
            "anthropic",
            vec![
                fake_model("anthropic", "claude-default", true),
                fake_model("anthropic", "claude-chosen", true),
            ],
        );
        let openai = fake_provider(
            "openai",
            vec![
                fake_model("openai", "gpt-default", false),
                fake_model("openai", "gpt-favorite", false),
            ],
        );
        let ollama = fake_provider("ollama", Vec::new());
        let chosen = LanguageModelSelection {
            provider: "anthropic".to_string().into(),
            model: "claude-chosen".to_string(),
            // Off on a model that supports it: the provider default would turn it on.
            enable_thinking: false,
            effort: None,
            speed: None,
        };
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            cx.set_global(db::AppDatabase::test_new());
            AgentSettings::register(cx);
            LanguageModelRegistry::test(cx);
            let chosen_model = anthropic
                .provided_models(cx)
                .get(1)
                .cloned()
                .expect("the model the user chose");
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(anthropic.clone(), cx);
                registry.register_provider(openai.clone(), cx);
                registry.register_provider(ollama.clone(), cx);
                registry.set_default_model(
                    Some(ConfiguredModel {
                        provider: anthropic.clone(),
                        model: chosen_model,
                    }),
                    cx,
                );
            });
            cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                store.update_user_settings(cx, |settings| {
                    let agent = settings.agent.get_or_insert_default();
                    agent.set_model(chosen.clone());
                    agent.favorite_models = vec![LanguageModelSelection {
                        provider: "openai".to_string().into(),
                        model: "gpt-favorite".to_string(),
                        enable_thinking: false,
                        effort: None,
                        speed: None,
                    }];
                });
            });
        });

        let kept =
            cx.update(|cx| switch_default_model_to_provider(&"anthropic".into(), fs.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            switched_to(&kept),
            Some(("claude-chosen".to_string(), None))
        );
        assert!(
            fs.load(::paths::settings_file()).await.is_err(),
            "nothing is written when the provider does not change"
        );

        let to_openai =
            cx.update(|cx| switch_default_model_to_provider(&"openai".into(), fs.clone(), cx));
        cx.run_until_parked();
        let (model, selection) = switched_to(&to_openai).expect("switched to OpenAI");
        assert_eq!(model, "gpt-favorite", "a favorite comes before the default");
        let selection = selection.expect("a new selection");
        assert_eq!(default_model_id(cx), Some("gpt-favorite".to_string()));
        let written = fs
            .load(::paths::settings_file())
            .await
            .expect("the switch is written to the settings file");
        assert!(
            written.contains("\"openai\"") && written.contains("\"gpt-favorite\""),
            "{written}"
        );
        settings_say_default_model(selection, cx);

        let back =
            cx.update(|cx| switch_default_model_to_provider(&"anthropic".into(), fs.clone(), cx));
        cx.run_until_parked();
        assert_eq!(
            switched_to(&back),
            Some(("claude-chosen".to_string(), Some(chosen))),
            "the model last used with Anthropic comes back, thinking included"
        );
        assert_eq!(default_model_id(cx), Some("claude-chosen".to_string()));

        let to_ollama =
            cx.update(|cx| switch_default_model_to_provider(&"ollama".into(), fs.clone(), cx));
        cx.run_until_parked();
        assert!(matches!(to_ollama, ModelSwitch::NoModels));
        assert_eq!(
            default_model_id(cx),
            Some("claude-chosen".to_string()),
            "a provider without a model does not take the default"
        );
    }

    #[test]
    fn a_provider_without_a_model_opens_its_settings() {
        let mut inputs = everything();
        inputs.native_default_provider = Some("ollama".into());
        for provider in &mut inputs.native_providers {
            if provider.id.as_ref() == "ollama" {
                provider.has_models = false;
            }
        }
        let sections = build_ai_choice_sections(&inputs);
        let ollama = choice(&sections, "Ollama");
        assert_eq!(ollama.target, AiChoiceTarget::SetUpModels);
        assert_eq!(choice_badges(ollama), ["Set Up…"]);
        assert!(
            !ollama.is_current,
            "the shortcut would not open it, so it does not show there"
        );
    }

    #[test]
    fn an_unauthenticated_default_provider_is_not_marked_current() {
        let mut inputs = everything();
        inputs.native_default_provider = Some("openai".into());
        let sections = build_ai_choice_sections(&inputs);
        assert!(
            sections
                .iter()
                .flat_map(|section| &section.choices)
                .all(|choice| !choice.is_current),
            "OpenAI still needs its key, so no entry carries the shortcut"
        );
    }

    #[test]
    fn local_models_are_offered_even_before_one_is_set_up() {
        let mut inputs = everything();
        inputs
            .native_providers
            .retain(|provider| provider.id.as_ref() != "ollama");
        inputs
            .native_providers
            .push(provider("lmstudio", "LM Studio", false));
        let sections = build_ai_choice_sections(&inputs);
        let local = &sections
            .iter()
            .find(|section| section.vendor == AiVendor::Local)
            .expect("a local section")
            .choices;
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].target, AiChoiceTarget::SetUpProvider);
        assert_eq!(choice_badges(&local[0]), ["Set Up…"]);
    }
}
