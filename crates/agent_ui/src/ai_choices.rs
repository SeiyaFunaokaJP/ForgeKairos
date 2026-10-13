//! ADR-F94: the agent panel's "Choose AI" menu (the `+` button).
//!
//! The menu used to be organized by how an AI runs — the built-in agent, the
//! official CLI in a tab, external ACP agents — and asked which vendor and
//! which sign-in each of those used to be remembered. It is now organized by
//! whose AI it is, with how it signs in, what runs it and whether it is
//! sandboxed shown beside each entry. [`build_ai_choice_sections`] decides the
//! entries from plain data so that what the menu offers can be tested without
//! a window; the rest of this file turns them into menu rows and acts on them.

use std::{rc::Rc, sync::Arc};

use agent_settings::AgentSettings;
use agent_settings::ai_vendor::{
    ANTHROPIC_AGENT_ID, AiVendor, GOOGLE_AGENT_ID, OPENAI_AGENT_ID, ProviderSignIn,
    is_language_model_provider_offered_before_setup, sign_in_of_language_model_provider,
    vendor_of_external_agent, vendor_of_language_model_provider,
};
use db::kvp::KeyValueStore;
use fs::Fs;
use gpui::{App, Entity, FocusHandle, SharedString, WeakEntity, Window};
use language_model::{ConfiguredModel, IconOrSvg, LanguageModelRegistry};
use project::{AgentId, Project};
use settings::{LanguageModelSelection, Settings as _, update_settings_file};
use ui::{
    ContextMenu, ContextMenuItem, DocumentationAside, KeyBinding, ListItem, ListItemSpacing,
    prelude::*,
};
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
    /// The provider still needs its consent, key or sign-in: set it up in
    /// place. `None` stands for "a local model", whichever local server it is.
    SetUpProvider {
        provider_id: Option<SharedString>,
    },
    /// The provider is signed in but offers no model yet (a local server with
    /// nothing downloaded, say): say why in place rather than start a thread
    /// that would silently keep the previous provider's model.
    SetUpModels {
        provider_id: SharedString,
    },
    /// A vendor's own agent from the ACP registry that is not added yet:
    /// install it, then start a thread with it.
    InstallExternalAgent {
        agent_id: AgentId,
    },
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
            AiChoiceTarget::SetUpProvider { .. }
                | AiChoiceTarget::SetUpModels { .. }
                | AiChoiceTarget::InstallExternalAgent { .. }
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

/// A vendor's agent in the ACP registry that is not added yet.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct InstallableAgentInput {
    pub(crate) id: AgentId,
    pub(crate) display_name: SharedString,
    pub(crate) icon_path: Option<SharedString>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct AiChoiceInputs {
    pub(crate) native_providers: Vec<NativeProviderInput>,
    /// The provider of the built-in agent's default model.
    pub(crate) native_default_provider: Option<SharedString>,
    /// Empty for a collaborator's guest, who can only use the built-in agent.
    pub(crate) external_agents: Vec<ExternalAgentInput>,
    /// Empty for a collaborator's guest, like `external_agents`.
    pub(crate) installable_agents: Vec<InstallableAgentInput>,
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

    for agent in &inputs.installable_agents {
        let vendor = vendor_of_external_agent(agent.id.as_ref(), true);
        push(
            vendor,
            AiChoice {
                label: agent.display_name.clone(),
                icon: match &agent.icon_path {
                    Some(path) => AiChoiceIcon::Svg(path.clone()),
                    None => AiChoiceIcon::Icon(IconName::Sparkle),
                },
                sign_in: match vendor {
                    AiVendor::Local | AiVendor::Other => AiSignIn::AgentsOwn,
                    _ => AiSignIn::SubscriptionOrApiKey,
                },
                runner: AiRunner::ExternalAgent,
                placement: None,
                target: AiChoiceTarget::InstallExternalAgent {
                    agent_id: agent.id.clone(),
                },
                is_current: false,
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
            AiChoiceTarget::SetUpProvider {
                provider_id: Some(provider.id.clone()),
            }
        } else if !provider.has_models {
            AiChoiceTarget::SetUpModels {
                provider_id: provider.id.clone(),
            }
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
                target: AiChoiceTarget::SetUpProvider { provider_id: None },
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
    let installable_agents = if project_ref.is_via_collab() {
        Vec::new()
    } else {
        installable_vendor_agents(project_ref.agent_server_store().read(cx), cx)
    };
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
        installable_agents,
        claude_code: supports_terminal
            .then(|| crate::sandboxed_claude_code::claude_code_placement(project_ref, cx)),
        selected_agent,
    }
}

/// The vendors' own agents that the ACP registry offers and that are not
/// added yet. One the user added under the same id from elsewhere counts as
/// added: installing over it would change what that id runs.
fn installable_vendor_agents(
    store: &project::AgentServerStore,
    cx: &App,
) -> Vec<InstallableAgentInput> {
    let Some(registry) = project::AgentRegistryStore::try_global(cx) else {
        return Vec::new();
    };
    let registry = registry.read(cx);
    let added = store.external_agents().cloned().collect::<Vec<_>>();
    installable_agents_among(
        [ANTHROPIC_AGENT_ID, OPENAI_AGENT_ID, GOOGLE_AGENT_ID]
            .into_iter()
            .filter_map(|id| {
                let agent = registry.agent(&AgentId::new(id))?;
                Some(InstallableAgentInput {
                    id: AgentId::new(id),
                    display_name: agent.name().clone(),
                    icon_path: agent.icon_path().cloned(),
                })
            }),
        &added,
    )
}

pub(crate) fn installable_agents_among(
    offered: impl IntoIterator<Item = InstallableAgentInput>,
    added: &[AgentId],
) -> Vec<InstallableAgentInput> {
    offered
        .into_iter()
        .filter(|agent| !added.contains(&agent.id))
        .collect()
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
            (AiChoiceTarget::InstallExternalAgent { .. }, _) => i18n::t!("Install…").into(),
            (AiChoiceTarget::SetUpModels { .. }, _) | (_, AiSignIn::Local) => {
                i18n::t!("Set Up…").into()
            }
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

/// What runs the thread a model picker belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ThreadRunner {
    BuiltInAgent,
    ExternalAgent(AgentId),
}

impl ThreadRunner {
    pub(crate) fn of_agent(agent_id: &AgentId) -> Self {
        if agent_id == &*agent::ZED_AGENT_ID {
            Self::BuiltInAgent
        } else {
            Self::ExternalAgent(agent_id.clone())
        }
    }
}

/// What picking an entry does to the thread the picker belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AiChoiceTransition {
    /// The thread's own runner: the picker lists its models in the entry's
    /// place, and choosing one switches this thread's model.
    SameThread,
    /// Another runner, which cannot take over this thread's conversation.
    NewThread,
    /// The official CLI, in a tab of its own.
    NewTab,
    /// Nothing starts yet: the provider still needs setting up.
    SetUp,
}

/// `runner` is `None` where no thread is open (the sidebar's menu): then
/// every AI starts a thread of its own.
pub(crate) fn choice_transition(
    choice: &AiChoice,
    runner: Option<&ThreadRunner>,
) -> AiChoiceTransition {
    match (&choice.target, runner) {
        (
            AiChoiceTarget::SetUpProvider { .. }
            | AiChoiceTarget::SetUpModels { .. }
            | AiChoiceTarget::InstallExternalAgent { .. },
            _,
        ) => AiChoiceTransition::SetUp,
        (AiChoiceTarget::LaunchClaudeCode, _) => AiChoiceTransition::NewTab,
        (AiChoiceTarget::NativeThread { .. }, Some(ThreadRunner::BuiltInAgent)) => {
            AiChoiceTransition::SameThread
        }
        (
            AiChoiceTarget::ExternalAgentThread { agent_id },
            Some(ThreadRunner::ExternalAgent(current)),
        ) if agent_id == current => AiChoiceTransition::SameThread,
        (AiChoiceTarget::NativeThread { .. } | AiChoiceTarget::ExternalAgentThread { .. }, _) => {
            AiChoiceTransition::NewThread
        }
    }
}

pub(crate) fn transition_label(transition: AiChoiceTransition) -> Option<SharedString> {
    match transition {
        AiChoiceTransition::NewThread => Some(i18n::t!("New Thread").into()),
        AiChoiceTransition::NewTab => Some(i18n::t!("New Tab").into()),
        AiChoiceTransition::SameThread | AiChoiceTransition::SetUp => None,
    }
}

/// What a picker's query is matched against. The vendor is part of it so
/// that 「openai」 finds Codex too; the badges are not, since a fuzzy match
/// over them finds almost anything.
pub(crate) fn choice_search_text(vendor: AiVendor, choice: &AiChoice) -> String {
    let text = format!("{}/{}", vendor_label(vendor), choice.label);
    match &choice.target {
        AiChoiceTarget::ExternalAgentThread { agent_id } => format!("{text} {}", agent_id.0),
        _ => text,
    }
}

fn ai_choice_icon(icon: &AiChoiceIcon) -> Icon {
    match icon {
        AiChoiceIcon::Icon(icon) => Icon::new(*icon),
        AiChoiceIcon::Svg(path) => Icon::from_external_svg(path.clone()),
    }
    .size(IconSize::Small)
    .color(Color::Muted)
}

fn badge_color(needs_setup: bool) -> Color {
    if needs_setup {
        Color::Accent
    } else {
        Color::Muted
    }
}

fn choice_label_color(needs_setup: bool) -> Color {
    if needs_setup {
        Color::Muted
    } else {
        Color::Default
    }
}

/// A model picker's row for `choice`: the same icon, name and badges as the
/// `+` menu's, stacked to fit the picker's width, and what choosing it opens.
pub(crate) fn render_ai_choice_picker_row(
    ix: usize,
    choice: &AiChoice,
    transition: AiChoiceTransition,
    selected: bool,
) -> AnyElement {
    let needs_setup = choice.needs_setup();
    let badges = choice_badges(choice);
    ListItem::new(("ai-choice", ix))
        .inset(true)
        .spacing(ListItemSpacing::Sparse)
        .toggle_state(selected)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(ai_choice_icon(&choice.icon))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            Label::new(choice.label.clone())
                                .color(choice_label_color(needs_setup))
                                .truncate(),
                        )
                        // Wrapped rather than cut: in the picker's width a
                        // truncated line hides the sandbox badge, the one
                        // that tells where the AI runs.
                        .child(
                            h_flex()
                                .flex_wrap()
                                .gap_x_1p5()
                                .children(badges.into_iter().map(|badge| {
                                    Label::new(badge)
                                        .size(LabelSize::XSmall)
                                        .color(badge_color(needs_setup))
                                })),
                        ),
                ),
        )
        .when_some(transition_label(transition), |this, label| {
            this.end_slot(
                h_flex()
                    .pr_2()
                    .gap_0p5()
                    .child(
                        Label::new(label)
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(
                        Icon::new(IconName::ArrowUpRight)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            )
        })
        .into_any_element()
}

pub(crate) fn choice_explanation(choice: &AiChoice) -> SharedString {
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
        AiChoiceTarget::SetUpProvider { .. } => i18n::t!(
            "Not set up yet. Add the key or sign in right here, and a new thread opens once it works."
        )
        .into(),
        AiChoiceTarget::SetUpModels { .. } => i18n::t!(
            "No model is available from this provider yet. Shows why and what to do."
        )
        .into(),
        AiChoiceTarget::InstallExternalAgent { .. } => i18n::t!(
            "Not installed yet. Installs {name} from the ACP registry, after asking about the hosts it needs, and then starts a thread with it.",
            name = choice.label
        )
        .into(),
    }
}

/// Runs what a menu entry opens. The `+` menu runs it at once; the sidebar
/// first brings the project the menu belongs to forward.
pub type AiChoiceRunner = Rc<dyn Fn(Box<dyn FnOnce(&mut Window, &mut App)>, &mut Window, &mut App)>;

pub(crate) fn run_at_once() -> AiChoiceRunner {
    Rc::new(|open, window, cx| open(window, cx))
}

/// A row of the `+` menu (and of the sidebar's, which shows the same).
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum AiChoiceMenuRow {
    Separator,
    Header(AiVendor),
    Choice(AiChoice),
    /// ADR-F49 決定 19: placed right after Claude Code.
    ClaudeCodeOutsideSandbox,
}

pub(crate) fn ai_choice_menu_rows(sections: Vec<AiChoiceSection>) -> Vec<AiChoiceMenuRow> {
    let mut rows = Vec::new();
    for (index, section) in sections.into_iter().enumerate() {
        if index > 0 {
            rows.push(AiChoiceMenuRow::Separator);
        }
        rows.push(AiChoiceMenuRow::Header(section.vendor));
        for choice in section.choices {
            let is_claude_code = choice.target == AiChoiceTarget::LaunchClaudeCode;
            rows.push(AiChoiceMenuRow::Choice(choice));
            if is_claude_code {
                rows.push(AiChoiceMenuRow::ClaudeCodeOutsideSandbox);
            }
        }
    }
    rows
}

/// Adds the AI sections to `menu`, the same way for the `+` menu and the
/// sidebar's: what each opens goes through `run`.
pub(crate) fn add_ai_choice_rows(
    mut menu: ContextMenu,
    sections: Vec<AiChoiceSection>,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    run: AiChoiceRunner,
    cx: &App,
) -> ContextMenu {
    for row in ai_choice_menu_rows(sections) {
        menu = match row {
            AiChoiceMenuRow::Separator => menu.separator(),
            AiChoiceMenuRow::Header(vendor) => menu.header(vendor_label(vendor)),
            AiChoiceMenuRow::Choice(choice) => menu.item(ai_choice_menu_item(
                choice,
                workspace.clone(),
                focus_handle.clone(),
                run.clone(),
                cx,
            )),
            AiChoiceMenuRow::ClaudeCodeOutsideSandbox => menu.item(
                crate::agent_panel::claude_code_outside_sandbox_menu_entry(cx).handler({
                    let run = run.clone();
                    let workspace = workspace.clone();
                    move |window, cx| {
                        let workspace = workspace.clone();
                        run(
                            Box::new(move |window, cx| {
                                if let Some(workspace) = workspace.upgrade() {
                                    crate::sandboxed_claude_code::launch_claude_code_in(
                                        &workspace, true, window, cx,
                                    );
                                }
                            }),
                            window,
                            cx,
                        )
                    }
                }),
            ),
        };
    }
    menu
}

/// The AI sections for a new thread in `workspace`'s project, as its agent
/// panel's `+` menu shows them. The sidebar's project menu asks this too, so
/// that the two never differ.
pub(crate) fn ai_choice_sections_for_workspace(
    workspace: &Entity<Workspace>,
    cx: &App,
) -> Option<Vec<AiChoiceSection>> {
    let panel = workspace.read(cx).panel::<AgentPanel>(cx)?;
    Some(panel.read(cx).new_thread_ai_context(cx).sections(cx))
}

/// What the AI sections for a new thread are gathered from.
#[derive(Clone)]
pub(crate) struct NewThreadAiContext {
    pub(crate) project: Entity<Project>,
    pub(crate) selected_agent: Agent,
    pub(crate) supports_terminal: bool,
}

impl NewThreadAiContext {
    pub(crate) fn sections(&self, cx: &App) -> Vec<AiChoiceSection> {
        build_ai_choice_sections(&gather_ai_choice_inputs(
            &self.project,
            self.selected_agent.clone(),
            self.supports_terminal,
            cx,
        ))
    }
}

/// The sidebar's share of the `+` menu: the AI sections for a new thread in
/// `workspace`, opened through `run`, which brings that project forward first.
pub fn add_ai_choices_for_workspace(
    menu: ContextMenu,
    workspace: &Entity<Workspace>,
    focus_handle: FocusHandle,
    run: AiChoiceRunner,
    cx: &App,
) -> ContextMenu {
    let Some(sections) = ai_choice_sections_for_workspace(workspace, cx) else {
        return menu;
    };
    add_ai_choice_rows(menu, sections, workspace.downgrade(), focus_handle, run, cx)
}

/// A menu row for `choice`: its icon and name, then its badges, and the
/// `NewThread` shortcut on the entry the shortcut opens.
pub(crate) fn ai_choice_menu_item(
    choice: AiChoice,
    workspace: WeakEntity<Workspace>,
    focus_handle: FocusHandle,
    run: AiChoiceRunner,
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
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .justify_between()
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_1p5()
                        .child(ai_choice_icon(&icon))
                        .child(Label::new(label.clone()).color(choice_label_color(needs_setup)))
                        .children(badges.iter().map(|badge| {
                            Label::new(badge.clone())
                                .size(LabelSize::XSmall)
                                .color(badge_color(needs_setup))
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
        move |window, cx| {
            let target = target.clone();
            let workspace = workspace.clone();
            run(
                Box::new(move |window, cx| open_ai_choice(&target, &workspace, window, cx)),
                window,
                cx,
            )
        },
        Some(DocumentationAside::new(
            crate::ui::documentation_aside_side(cx),
            Rc::new(move |_| Label::new(explanation.clone()).into_any_element()),
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
            if let Some(workspace) = workspace.upgrade() {
                crate::sandboxed_claude_code::launch_claude_code_in(&workspace, false, window, cx);
            }
        }
        AiChoiceTarget::SetUpProvider { provider_id } => {
            set_up_provider(provider_id.clone(), window, cx);
        }
        AiChoiceTarget::SetUpModels { provider_id } => {
            set_up_provider(Some(provider_id.clone()), window, cx);
        }
        AiChoiceTarget::InstallExternalAgent { agent_id } => {
            crate::ai_choice_install::install_from_menu(agent_id.clone(), workspace, window, cx);
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
            if !may_start_native_thread(provider_id, cx) {
                set_up_provider(Some(provider_id.clone()), window, cx);
                return;
            }
            let Some(fs) = workspace
                .upgrade()
                .and_then(|workspace| workspace.read(cx).panel::<AgentPanel>(cx))
                .map(|panel| panel.read(cx).fs())
            else {
                return;
            };
            let switch = switch_default_model_to_provider(provider_id, fs, cx);
            if matches!(switch, ModelSwitch::NoModels) {
                set_up_provider(Some(provider_id.clone()), window, cx);
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

/// ADR-F94 step 4: the setup window over the workspace (in `settings_ui`),
/// which comes back with `NewThreadWithLanguageModelProvider` once it works.
fn set_up_provider(provider_id: Option<SharedString>, window: &mut Window, cx: &mut App) {
    window.dispatch_action(
        Box::new(zed_actions::agent::SetUpLanguageModelProvider {
            provider: provider_id.map(|id| id.to_string()),
        }),
        cx,
    );
}

/// ADR-F09 / F10: a thread on a provider needs AI turned on and, for an
/// external provider, the user's consent. Whatever is missing is the setup
/// window's to ask for (or, for AI being off, to point at).
fn may_start_native_thread(provider_id: &SharedString, cx: &App) -> bool {
    AgentSettings::get_global(cx).enabled(cx)
        && agent_settings::ai_vendor::language_model_provider_is_consented(provider_id, cx)
}

pub(crate) fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _, _| {
        workspace.register_action(
            |workspace,
             action: &zed_actions::agent::NewThreadWithLanguageModelProvider,
             window,
             cx| {
                if workspace.panel::<AgentPanel>(cx).is_none() {
                    return;
                }
                let provider_id = SharedString::from(action.provider.clone());
                // The action can be bound to a key, so it does not take the
                // setup window's word that the provider is ready.
                if !may_start_native_thread(&provider_id, cx) {
                    set_up_provider(Some(provider_id), window, cx);
                    return;
                }
                workspace.focus_panel::<AgentPanel>(window, cx);
                let target = AiChoiceTarget::NativeThread { provider_id };
                let workspace = cx.entity().downgrade();
                // The panel is updated through the workspace, which this
                // handler is still updating.
                window.defer(cx, move |window, cx| {
                    open_ai_choice(&target, &workspace, window, cx);
                });
            },
        );
    })
    .detach();
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
            installable_agents: Vec::new(),
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
        assert_eq!(
            openai.target,
            AiChoiceTarget::SetUpProvider {
                provider_id: Some("openai".into())
            },
            "the setup window is told which provider to set up"
        );
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
        assert_eq!(
            ollama.target,
            AiChoiceTarget::SetUpModels {
                provider_id: "ollama".into()
            }
        );
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
    fn only_the_thread_s_own_runner_switches_in_place() {
        let sections = build_ai_choice_sections(&everything());
        let transition = |label: &str, runner: Option<&ThreadRunner>| {
            choice_transition(choice(&sections, label), runner)
        };
        let built_in = ThreadRunner::BuiltInAgent;
        let claude_agent = ThreadRunner::of_agent(&AgentId::new("claude-acp"));
        assert_eq!(
            claude_agent,
            ThreadRunner::ExternalAgent(AgentId::new("claude-acp"))
        );
        assert_eq!(
            ThreadRunner::of_agent(&agent::ZED_AGENT_ID),
            ThreadRunner::BuiltInAgent
        );

        assert_eq!(
            transition("Anthropic", Some(&built_in)),
            AiChoiceTransition::SameThread,
            "another provider's model is a model switch of the built-in agent"
        );
        assert_eq!(
            transition("Mistral", Some(&built_in)),
            AiChoiceTransition::SameThread
        );
        assert_eq!(
            transition("Claude Agent", Some(&built_in)),
            AiChoiceTransition::NewThread
        );
        assert_eq!(
            transition("Claude Agent", Some(&claude_agent)),
            AiChoiceTransition::SameThread
        );
        assert_eq!(
            transition("Codex", Some(&claude_agent)),
            AiChoiceTransition::NewThread,
            "another external agent cannot take over this agent's session"
        );
        assert_eq!(
            transition("Anthropic", Some(&claude_agent)),
            AiChoiceTransition::NewThread,
            "the built-in agent cannot continue an external agent's thread"
        );
        for runner in [None, Some(&built_in), Some(&claude_agent)] {
            assert_eq!(
                transition("Claude Code", runner),
                AiChoiceTransition::NewTab
            );
            assert_eq!(transition("OpenAI", runner), AiChoiceTransition::SetUp);
        }
        assert_eq!(
            transition("Anthropic", None),
            AiChoiceTransition::NewThread,
            "with no thread open, everything starts one"
        );
        assert_eq!(
            transition_label(AiChoiceTransition::NewThread).as_deref(),
            Some("New Thread")
        );
        assert_eq!(transition_label(AiChoiceTransition::SameThread), None);
    }

    #[test]
    fn the_menu_rows_are_the_sections_with_the_outside_the_sandbox_entry_after_claude_code() {
        let rows = ai_choice_menu_rows(build_ai_choice_sections(&everything()));
        let described = rows
            .iter()
            .map(|row| match row {
                AiChoiceMenuRow::Separator => "---".to_string(),
                AiChoiceMenuRow::Header(vendor) => format!("# {}", vendor_label(*vendor)),
                AiChoiceMenuRow::Choice(choice) => choice.label.to_string(),
                AiChoiceMenuRow::ClaudeCodeOutsideSandbox => "outside".to_string(),
            })
            .collect::<Vec<_>>();
        assert_eq!(
            described,
            [
                "# Claude (Anthropic)",
                "Claude Code",
                "outside",
                "Claude Agent",
                "Anthropic",
                "---",
                "# OpenAI",
                "Codex",
                "ChatGPT Subscription",
                "OpenAI",
                "---",
                "# Google",
                "Gemini CLI",
                "Google AI",
                "---",
                "# Local Models",
                "Ollama",
                "---",
                "# Other",
                "OpenCode",
                "Mistral",
            ]
        );
    }

    #[test]
    fn a_vendor_s_name_is_part_of_what_a_query_matches() {
        let sections = build_ai_choice_sections(&everything());
        assert_eq!(
            choice_search_text(AiVendor::OpenAi, choice(&sections, "Codex")),
            "OpenAI/Codex codex-acp"
        );
        assert_eq!(
            choice_search_text(AiVendor::Anthropic, choice(&sections, "Claude Code")),
            "Claude (Anthropic)/Claude Code"
        );
    }

    /// The sidebar's project menu and the agent panel's `+` menu are made
    /// from the same panel's choices.
    #[gpui::test]
    async fn the_sidebar_offers_what_the_panel_s_plus_menu_offers(cx: &mut gpui::TestAppContext) {
        crate::conversation_view::tests::init_test(cx);
        cx.update(|cx| {
            agent::ThreadStore::init_global(cx);
            LanguageModelRegistry::test(cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        fs.insert_tree("/project", serde_json::json!({ "file.txt": "" }))
            .await;
        let project = Project::test(fs, [std::path::Path::new("/project")], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        let workspace = multi_workspace
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let cx = &mut gpui::VisualTestContext::from_window(multi_workspace.into(), cx);

        assert_eq!(
            cx.update(|_, cx| ai_choice_sections_for_workspace(&workspace, cx)),
            None,
            "without the panel there is nothing to start a thread in"
        );

        let panel = workspace.update_in(cx, |workspace, window, cx| {
            let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
            workspace.add_panel(panel.clone(), window, cx);
            panel
        });
        let (from_sidebar, from_plus_menu) = cx.update(|_, cx| {
            (
                ai_choice_sections_for_workspace(&workspace, cx),
                panel.read(cx).new_thread_ai_context(cx).sections(cx),
            )
        });
        assert!(!from_plus_menu.is_empty());
        assert_eq!(from_sidebar, Some(from_plus_menu));
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
        assert_eq!(
            local[0].target,
            AiChoiceTarget::SetUpProvider { provider_id: None },
            "the one local entry stands for whichever local server is set up"
        );
        assert_eq!(choice_badges(&local[0]), ["Set Up…"]);
    }

    fn installable(id: &str, name: &str) -> InstallableAgentInput {
        InstallableAgentInput {
            id: AgentId::new(id.to_string()),
            display_name: name.to_string().into(),
            icon_path: None,
        }
    }

    #[test]
    fn a_vendor_agent_not_added_yet_is_offered_for_install_in_its_section() {
        let mut inputs = everything();
        inputs
            .external_agents
            .retain(|agent| agent.id.as_ref() != "codex-acp");
        inputs.installable_agents = vec![installable("codex-acp", "Codex")];
        let sections = build_ai_choice_sections(&inputs);

        assert_eq!(
            labels(&sections, AiVendor::OpenAi),
            ["ChatGPT Subscription", "Codex", "OpenAI"],
            "an install goes below what already works"
        );
        let codex = choice(&sections, "Codex");
        assert_eq!(
            codex.target,
            AiChoiceTarget::InstallExternalAgent {
                agent_id: AgentId::new("codex-acp")
            }
        );
        assert!(codex.needs_setup());
        assert!(!codex.is_current);
        assert_eq!(codex.runner, AiRunner::ExternalAgent);
        assert_eq!(choice_badges(codex), ["Install…"]);
    }

    #[test]
    fn only_agents_not_added_are_offered_for_install() {
        let offered = || {
            vec![
                installable("claude-acp", "Claude Agent"),
                installable("codex-acp", "Codex"),
                installable("gemini", "Gemini CLI"),
            ]
        };
        let ids = |agents: Vec<InstallableAgentInput>| {
            agents
                .into_iter()
                .map(|agent| agent.id.as_ref().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            ids(installable_agents_among(offered(), &[])),
            ["claude-acp", "codex-acp", "gemini"]
        );
        assert_eq!(
            ids(installable_agents_among(
                offered(),
                &[AgentId::new("claude-acp"), AgentId::new("gemini")]
            )),
            ["codex-acp"],
            "an agent added under the same id, from anywhere, is not installed over"
        );
    }

    mod native_thread_gate {
        use std::cell::RefCell;
        use std::path::Path;
        use std::rc::Rc;

        use agent_settings::ai_vendor::LanguageModelProviderConsentCheck;
        use fs::FakeFs;
        use gpui::{TestAppContext, VisualTestContext};
        use serde_json::json;
        use workspace::MultiWorkspace;

        use super::super::*;

        struct Gate {
            set_ups: Rc<RefCell<Vec<Option<String>>>>,
            workspace: WeakEntity<Workspace>,
            project: WeakEntity<Project>,
            cx: VisualTestContext,
        }

        async fn gate(cx: &mut TestAppContext) -> Gate {
            crate::test_support::init_test(cx);
            cx.update(|cx| {
                agent::ThreadStore::init_global(cx);
                crate::thread_metadata_store::ThreadMetadataStore::init_global(cx);
                language_model::LanguageModelRegistry::test(cx);
                init(cx);
            });
            let fs = FakeFs::new(cx.executor());
            cx.update(|cx| <dyn Fs>::set_global(fs.clone(), cx));
            fs.insert_tree("/project", json!({})).await;
            let project = Project::test(fs, [Path::new("/project")], cx).await;
            let weak_project = project.downgrade();
            let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
            let workspace = window
                .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
                .expect("the window is open");
            let mut cx = VisualTestContext::from_window(window.into(), cx);
            let set_ups = Rc::new(RefCell::new(Vec::new()));
            workspace.update_in(&mut cx, |workspace, window, cx| {
                let panel = cx.new(|cx| AgentPanel::new(workspace, window, cx));
                workspace.add_panel(panel, window, cx);
                let set_ups = set_ups.clone();
                workspace.register_action(
                    move |_, action: &zed_actions::agent::SetUpLanguageModelProvider, _, _| {
                        set_ups.borrow_mut().push(action.provider.clone());
                    },
                );
            });
            cx.run_until_parked();
            Gate {
                set_ups,
                workspace: workspace.downgrade(),
                project: weak_project,
                cx,
            }
        }

        impl Gate {
            fn set_ai_enabled(&mut self, enabled: bool) {
                self.cx.update(|_, cx| {
                    AgentSettings::override_global(
                        AgentSettings {
                            enabled,
                            ..AgentSettings::get_global(cx).clone()
                        },
                        cx,
                    );
                });
            }

            fn set_consent(&mut self, check: fn(&str, &App) -> bool) {
                self.cx.update(|_, cx| {
                    cx.set_global(LanguageModelProviderConsentCheck(check));
                });
            }

            /// What the action, as a key binding would send it, asked the
            /// setup window for.
            fn start(&mut self) -> Vec<Option<String>> {
                self.set_ups.borrow_mut().clear();
                self.cx
                    .dispatch_action(zed_actions::agent::NewThreadWithLanguageModelProvider {
                        provider: "fake".to_string(),
                    });
                self.cx.run_until_parked();
                self.set_ups.borrow().clone()
            }

            /// The same, chosen from the `+` menu.
            fn choose(&mut self) -> Vec<Option<String>> {
                self.set_ups.borrow_mut().clear();
                let workspace = self.workspace.clone();
                self.cx.update(|window, cx| {
                    open_ai_choice(
                        &AiChoiceTarget::NativeThread {
                            provider_id: "fake".into(),
                        },
                        &workspace,
                        window,
                        cx,
                    );
                });
                self.cx.run_until_parked();
                self.set_ups.borrow().clone()
            }

            fn fake_choice() -> AiChoice {
                AiChoice {
                    label: "Fake".into(),
                    icon: AiChoiceIcon::Icon(IconName::Sparkle),
                    sign_in: AiSignIn::ApiKey,
                    runner: AiRunner::BuiltInAgent,
                    placement: None,
                    target: AiChoiceTarget::NativeThread {
                        provider_id: "fake".into(),
                    },
                    is_current: false,
                }
            }

            /// The same, chosen in the model picker of an external agent's
            /// thread, where it starts a thread of its own.
            fn choose_in_picker(&mut self) -> Vec<Option<String>> {
                self.set_ups.borrow_mut().clear();
                let source = crate::model_selector::agent_launch_source(
                    self.project.clone(),
                    self.workspace.clone(),
                    AgentId::new("test-agent"),
                );
                let entry = crate::model_selector::AgentLaunchEntry {
                    vendor: AiVendor::Other,
                    choice: Self::fake_choice(),
                    transition: AiChoiceTransition::NewThread,
                };
                self.cx.update(|window, cx| {
                    crate::model_selector::open_agent_launch_entry(
                        Some(&source),
                        &entry,
                        window,
                        cx,
                    );
                });
                self.cx.run_until_parked();
                self.set_ups.borrow().clone()
            }

            /// The same, chosen in the rows the sidebar's project menu adds
            /// (through a runner, as the sidebar's brings its project forward).
            fn choose_in_project_menu(&mut self) -> Vec<Option<String>> {
                self.set_ups.borrow_mut().clear();
                let workspace = self.workspace.clone();
                let ran = Rc::new(std::cell::Cell::new(0));
                let run: AiChoiceRunner = Rc::new({
                    let ran = ran.clone();
                    move |open, window, cx| {
                        ran.set(ran.get() + 1);
                        open(window, cx)
                    }
                });
                let menu = self.cx.update(|window, cx| {
                    ContextMenu::build(window, cx, |menu, _, cx| {
                        add_ai_choice_rows(
                            menu,
                            vec![AiChoiceSection {
                                vendor: AiVendor::Other,
                                choices: vec![Self::fake_choice()],
                            }],
                            workspace,
                            cx.focus_handle(),
                            run,
                            cx,
                        )
                    })
                });
                menu.update_in(&mut self.cx, |menu, window, cx| {
                    menu.select_first(&menu::SelectFirst, window, cx);
                    menu.confirm(&menu::Confirm, window, cx);
                });
                self.cx.run_until_parked();
                assert_eq!(ran.get(), 1, "the entry went through the menu's runner");
                self.set_ups.borrow().clone()
            }
        }

        #[gpui::test]
        async fn the_picker_and_the_project_menu_start_no_thread_without_the_consent(
            cx: &mut TestAppContext,
        ) {
            let mut gate = gate(cx).await;
            let to_setup = vec![Some("fake".to_string())];
            gate.set_ai_enabled(true);
            gate.set_consent(|_, _| false);
            assert_eq!(gate.choose_in_picker(), to_setup);
            assert_eq!(gate.choose_in_project_menu(), to_setup);

            gate.set_consent(|provider_id, _| provider_id == "fake");
            gate.set_ai_enabled(false);
            assert_eq!(gate.choose_in_picker(), to_setup, "AI off (ADR-F09)");
            assert_eq!(gate.choose_in_project_menu(), to_setup, "AI off (ADR-F09)");

            gate.set_ai_enabled(true);
            assert_eq!(gate.choose_in_picker(), Vec::<Option<String>>::new());
            assert_eq!(gate.choose_in_project_menu(), Vec::<Option<String>>::new());
        }

        #[gpui::test]
        async fn the_action_starts_no_thread_without_ai_on_and_the_consent(
            cx: &mut TestAppContext,
        ) {
            let mut gate = gate(cx).await;
            let to_setup = vec![Some("fake".to_string())];

            gate.set_ai_enabled(true);
            assert_eq!(
                gate.start(),
                to_setup,
                "with nothing to say the consent was given, it is the setup window's to ask"
            );

            gate.set_consent(|_, _| false);
            assert_eq!(gate.start(), to_setup);
            assert_eq!(
                gate.choose(),
                to_setup,
                "a provider signed in by an environment variable still asks first"
            );

            gate.set_consent(|provider_id, _| provider_id == "fake");
            gate.set_ai_enabled(false);
            assert_eq!(
                gate.start(),
                to_setup,
                "AI off (ADR-F09) is the setup window's to point at, consent or not"
            );

            gate.set_ai_enabled(true);
            assert_eq!(
                gate.start(),
                Vec::<Option<String>>::new(),
                "AI on and the consent given: the thread starts without the setup window"
            );
        }
    }
}
