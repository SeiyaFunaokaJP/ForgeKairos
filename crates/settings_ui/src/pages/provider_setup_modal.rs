//! ADR-F94 step 4: choosing a language model provider that is not set up yet
//! from the agent panel's `+` menu sets it up right there, in a small window
//! over the workspace, instead of sending the user to the settings window.
//!
//! The window goes through what the LLM Providers page asks for, in the same
//! order and through the same writers: the external-AI consent (ADR-F10,
//! `acknowledged_external_ai_providers`), then the key (`set_api_key`, which
//! keeps it in the OS credential store, ADR-F88) or the provider's own sign-in
//! view. Once the provider works it asks the agent panel for a thread on it and
//! closes. "Open Detailed Settings" leads to the page itself.

use gpui::{
    AnyView, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription, Task,
    prelude::*,
};
use language_model::{
    LanguageModelProvider, LanguageModelProviderId, LanguageModelRegistry, ProviderSettingsView,
};
use settings::{Settings as _, SettingsStore};
use std::sync::Arc;
use ui::{ButtonLink, Checkbox, Modal, ModalFooter, ModalHeader, Section, ToggleState, prelude::*};
use ui_input::InputField;
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::pages::external_ai_consent::{self, ConsentTarget};
use crate::pages::llm_providers_page::{LINK_SLOT, split_around_link};

const LLM_PROVIDERS_PATH: &str = "llm_providers";
const ENABLE_AGENT_PATH: &str = "agent.enabled";

/// The local providers "a local model" stands for, in the order they are
/// offered.
const LOCAL_PROVIDER_IDS: [&str; 3] = ["ollama", "lmstudio", "llama.cpp"];

/// What the window asks for next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SetupStep {
    /// ADR-F09 / F13: AI is off, and this window is not where it is turned on.
    AiDisabled,
    UnknownProvider,
    /// ADR-F10: an external provider the user has not consented to yet.
    Consent,
    ApiKey,
    /// Sign-in, or a local server: the provider's own configuration view.
    SignIn,
    /// Signed in, but the provider offers no model.
    NoModels,
    Ready,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProviderFacts {
    pub(crate) needs_consent: bool,
    pub(crate) authenticated: bool,
    pub(crate) has_models: bool,
    pub(crate) asks_for_api_key: bool,
}

pub(crate) fn setup_step(ai_enabled: bool, provider: Option<ProviderFacts>) -> SetupStep {
    if !ai_enabled {
        return SetupStep::AiDisabled;
    }
    let Some(provider) = provider else {
        return SetupStep::UnknownProvider;
    };
    if provider.needs_consent {
        SetupStep::Consent
    } else if !provider.authenticated {
        if provider.asks_for_api_key {
            SetupStep::ApiKey
        } else {
            SetupStep::SignIn
        }
    } else if !provider.has_models {
        SetupStep::NoModels
    } else {
        SetupStep::Ready
    }
}

fn is_local_provider(provider_id: &str) -> bool {
    LOCAL_PROVIDER_IDS.contains(&provider_id)
}

fn provider_name(provider_id: &LanguageModelProviderId, cx: &App) -> SharedString {
    LanguageModelRegistry::read_global(cx)
        .provider(provider_id)
        .map(|provider| provider.name().0)
        .unwrap_or_else(|| provider_id.0.clone())
}

fn consent_target(provider_id: &LanguageModelProviderId, cx: &App) -> Option<ConsentTarget> {
    external_ai_consent::llm_provider_target(
        provider_id.0.as_ref(),
        provider_name(provider_id, cx),
        cx,
    )
}

/// The consent this provider still needs, or `None` when it needs none or
/// has it.
fn missing_consent(provider_id: &LanguageModelProviderId, cx: &App) -> Option<ConsentTarget> {
    consent_target(provider_id, cx)
        .filter(|target| !external_ai_consent::is_acknowledged(&target.key, cx))
}

/// Registers [`missing_consent`] as the rule the agent panel checks before it
/// starts a thread on a provider, so the two never disagree on what needs one.
pub(crate) fn init(cx: &mut App) {
    cx.set_global(
        agent_settings::ai_vendor::LanguageModelProviderConsentCheck(|provider_id, cx| {
            missing_consent(&LanguageModelProviderId(provider_id.to_string().into()), cx).is_none()
        }),
    );
}

pub(crate) fn provider_step(provider_id: &LanguageModelProviderId, cx: &mut App) -> SetupStep {
    let ai_enabled = agent_settings::AgentSettings::get_global(cx).enabled(cx);
    let provider = LanguageModelRegistry::read_global(cx).provider(provider_id);
    let facts = provider.map(|provider| ProviderFacts {
        needs_consent: missing_consent(provider_id, cx).is_some(),
        authenticated: provider.is_authenticated(cx),
        has_models: !provider.provided_models(cx).is_empty(),
        asks_for_api_key: matches!(
            provider.settings_view(cx),
            Some(ProviderSettingsView::ApiKey(_))
        ),
    });
    setup_step(ai_enabled, facts)
}

pub(crate) fn open_provider_setup_modal(
    workspace: &mut Workspace,
    provider: Option<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let registry = LanguageModelRegistry::read_global(cx);
    let local_choices = LOCAL_PROVIDER_IDS
        .iter()
        .filter_map(|id| {
            registry
                .provider(&LanguageModelProviderId(SharedString::new_static(id)))
                .map(|provider| (provider.id(), provider.name().0))
        })
        .collect::<Vec<_>>();
    let (provider_id, local_choices) = match provider {
        Some(id) => (LanguageModelProviderId(id.into()), Vec::new()),
        None => match local_choices.first() {
            Some((id, _)) => (id.clone(), local_choices),
            None => (
                LanguageModelProviderId(SharedString::new_static(LOCAL_PROVIDER_IDS[0])),
                Vec::new(),
            ),
        },
    };
    // Nothing to set up (the menu was older than the provider's state, say):
    // straight to the thread.
    if provider_step(&provider_id, cx) == SetupStep::Ready {
        window.dispatch_action(
            Box::new(zed_actions::agent::NewThreadWithLanguageModelProvider {
                provider: provider_id.0.to_string(),
            }),
            cx,
        );
        return;
    }
    workspace.toggle_modal(window, cx, |window, cx| {
        ProviderSetupModal::new(provider_id, local_choices, window, cx)
    });
}

pub(crate) struct ProviderSetupModal {
    provider_id: LanguageModelProviderId,
    /// Filled only when the window was opened for "a local model".
    local_choices: Vec<(LanguageModelProviderId, SharedString)>,
    api_key_input: Entity<InputField>,
    configuration_view: Option<(LanguageModelProviderId, AnyView)>,
    saving: bool,
    error: Option<SharedString>,
    allowlist_error: Option<SharedString>,
    allowlist_writing: bool,
    thread_requested: bool,
    focus_handle: FocusHandle,
    _pending: Vec<Task<()>>,
    _subscriptions: Vec<Subscription>,
}

impl ProviderSetupModal {
    fn new(
        provider_id: LanguageModelProviderId,
        local_choices: Vec<(LanguageModelProviderId, SharedString)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let api_key_input = cx.new(|cx| {
            InputField::new(window, cx, "xxxxxxxxxxxxxxxxxxxx")
                .masked(true)
                .tab_index(0)
        });
        let registry = LanguageModelRegistry::global(cx);
        let subscriptions = vec![
            cx.subscribe_in(&registry, window, |this, _, event, window, cx| {
                if let language_model::Event::ProviderStateChanged(_)
                | language_model::Event::AddedProvider(_)
                | language_model::Event::RemovedProvider(_)
                | language_model::Event::ProvidersChanged = event
                {
                    this.open_thread_once_ready(window, cx);
                    cx.notify();
                }
            }),
            cx.observe_global_in::<SettingsStore>(window, |this, window, cx| {
                this.open_thread_once_ready(window, cx);
                cx.notify();
            }),
        ];

        let mut pending = Vec::new();
        // The hosts' "(added)" marks follow the running policy, which takes in
        // the allowlist file a moment after it is written.
        if let Some(policy) = cx.try_global::<egress_policy::EgressAllowlistGlobal>() {
            let mut changes = policy.0.subscribe();
            pending.push(cx.spawn(async move |this, cx| {
                use futures::StreamExt as _;
                while changes.next().await.is_some() {
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }

        Self {
            provider_id,
            local_choices,
            api_key_input,
            configuration_view: None,
            saving: false,
            error: None,
            allowlist_error: None,
            allowlist_writing: false,
            thread_requested: false,
            focus_handle: cx.focus_handle(),
            _pending: pending,
            _subscriptions: subscriptions,
        }
    }

    fn provider(&self, cx: &App) -> Option<Arc<dyn LanguageModelProvider>> {
        LanguageModelRegistry::read_global(cx).provider(&self.provider_id)
    }

    fn provider_name(&self, cx: &App) -> SharedString {
        provider_name(&self.provider_id, cx)
    }

    fn consent_target(&self, cx: &App) -> Option<ConsentTarget> {
        consent_target(&self.provider_id, cx)
    }

    pub(crate) fn step(&self, cx: &mut App) -> SetupStep {
        provider_step(&self.provider_id, cx)
    }

    fn open_thread_once_ready(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.thread_requested || self.step(cx) != SetupStep::Ready {
            return;
        }
        self.thread_requested = true;
        window.dispatch_action(
            Box::new(zed_actions::agent::NewThreadWithLanguageModelProvider {
                provider: self.provider_id.0.to_string(),
            }),
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn acknowledge(&mut self, acknowledged: bool, cx: &mut Context<Self>) {
        if let Some(target) = self.consent_target(cx) {
            external_ai_consent::set_acknowledged(target.key, acknowledged, cx);
        }
        cx.notify();
    }

    fn add_hosts_to_allowlist(&mut self, hosts: Vec<SharedString>, cx: &mut Context<Self>) {
        if self.allowlist_writing || hosts.is_empty() {
            return;
        }
        // A consent is for the hosts it names; a pattern would open more.
        if hosts.iter().any(|host| host.contains('*')) {
            return;
        }
        self.allowlist_writing = true;
        self.allowlist_error = None;
        let fs = <dyn fs::Fs>::global(cx);
        let path = paths::network_allowlist_file().clone();
        let hosts = hosts
            .iter()
            .map(SharedString::to_string)
            .collect::<Vec<_>>();
        let write = cx.background_spawn(async move {
            egress_policy::add_allowed_hosts(&fs, &path, &hosts).await
        });
        self._pending.push(cx.spawn(async move |this, cx| {
            let result = write.await;
            this.update(cx, |this, cx| {
                this.allowlist_writing = false;
                if let Err(error) = result {
                    this.allowlist_error = Some(
                        i18n::t!("Failed to add the hosts to the allowlist: {error:#}").into(),
                    );
                }
                cx.notify();
            })
            .log_err();
        }));
        cx.notify();
    }

    pub(crate) fn save_api_key(
        &mut self,
        key: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.saving {
            return;
        }
        if self.step(cx) != SetupStep::ApiKey {
            return;
        }
        let key = key.trim().to_string();
        if key.is_empty() {
            self.error = Some(i18n::t!("Enter the API key.").into());
            cx.notify();
            return;
        }
        let Some(provider) = self.provider(cx) else {
            return;
        };
        self.saving = true;
        self.error = None;
        let save = provider.set_api_key(Some(key), cx);
        self._pending
            .push(cx.spawn_in(window, async move |this, cx| {
                let result = save.await;
                this.update_in(cx, |this, window, cx| {
                    this.saving = false;
                    match result {
                        Ok(()) => {
                            this.api_key_input
                                .update(cx, |input, cx| input.clear(window, cx));
                            this.open_thread_once_ready(window, cx);
                        }
                        Err(error) => {
                            this.error =
                                Some(i18n::t!("The API key could not be saved: {error:#}").into());
                        }
                    }
                    cx.notify();
                })
                .log_err();
            }));
        cx.notify();
    }

    fn retry(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(provider) = self.provider(cx) else {
            return;
        };
        self.error = None;
        let authenticate = provider.authenticate(cx);
        self._pending
            .push(cx.spawn_in(window, async move |this, cx| {
                let result = authenticate.await;
                this.update_in(cx, |this, window, cx| {
                    if let Err(error) = result {
                        this.error = Some(i18n::t!("Could not connect: {error}").into());
                    }
                    this.open_thread_once_ready(window, cx);
                    cx.notify();
                })
                .log_err();
            }));
        cx.notify();
    }

    fn select_local_provider(
        &mut self,
        provider_id: LanguageModelProviderId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.provider_id = provider_id;
        self.error = None;
        self.open_thread_once_ready(window, cx);
        cx.notify();
    }

    fn open_detailed_settings(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        window.dispatch_action(
            Box::new(zed_actions::OpenSettingsAt {
                path: path.to_string(),
                target: None,
            }),
            cx,
        );
        cx.emit(DismissEvent);
    }

    fn cancel(&mut self, _: &menu::Cancel, _: &mut Window, cx: &mut Context<Self>) {
        cx.emit(DismissEvent);
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut Context<Self>) {
        if self.step(cx) == SetupStep::ApiKey {
            let key = self.api_key_input.read(cx).text(cx);
            self.save_api_key(key, window, cx);
        }
    }

    fn configuration_view(
        &mut self,
        provider: &Arc<dyn LanguageModelProvider>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyView> {
        if let Some((id, view)) = &self.configuration_view
            && *id == self.provider_id
        {
            return Some(view.clone());
        }
        let create_view = match provider.settings_view(cx)? {
            ProviderSettingsView::Inline(settings) => settings.create_view,
            ProviderSettingsView::SubPage(settings) => settings.create_view,
            ProviderSettingsView::ApiKey(_) => return None,
        };
        let view = create_view(window, cx);
        self.configuration_view = Some((self.provider_id.clone(), view.clone()));
        Some(view)
    }

    fn render_hosts(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let target = self.consent_target(cx)?;
        if target.hosts.is_empty() {
            return None;
        }
        let policy = cx.try_global::<egress_policy::EgressAllowlistGlobal>();
        let missing = target
            .hosts
            .iter()
            .filter(|host| {
                !http_client::RequestHost::parse(host)
                    .is_some_and(|host| policy.is_some_and(|policy| policy.0.allows(&host)))
            })
            .cloned()
            .collect::<Vec<_>>();
        let hosts_label = target
            .hosts
            .iter()
            .map(SharedString::to_string)
            .collect::<Vec<_>>()
            .join(i18n::tc!("list separator", ", "));
        Some(
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap_1p5()
                        .child(
                            Label::new(i18n::t!("Hosts that need to be allowed:"))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .child(Label::new(hosts_label).size(LabelSize::Small).buffer_font(cx))
                        .child(
                            Label::new(if missing.is_empty() {
                                i18n::t!("(added)")
                            } else {
                                i18n::t!("(not added)")
                            })
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                        ),
                )
                .when(!missing.is_empty(), |this| {
                    this.child(
                        h_flex().child(
                            Button::new("provider-setup-allowlist", i18n::t!("Add to Allowlist"))
                                .label_size(LabelSize::Small)
                                .style(ButtonStyle::Outlined)
                                .loading(self.allowlist_writing)
                                .disabled(self.allowlist_writing)
                                .tab_index(0isize)
                                .tooltip(ui::Tooltip::text(i18n::t!(
                                    "Adds them to network_allowlist.json. If you don't add them, traffic to this provider stays denied"
                                )))
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.add_hosts_to_allowlist(missing.clone(), cx);
                                })),
                        ),
                    )
                })
                .when_some(self.allowlist_error.clone(), |this, error| {
                    this.child(Label::new(error).size(LabelSize::Small).color(Color::Error))
                })
                .into_any_element(),
        )
    }

    fn render_consent(&self, target: &ConsentTarget, cx: &mut Context<Self>) -> AnyElement {
        let name = target.name.clone();
        v_flex()
            .gap_2()
            .child(
                ui::Banner::new().severity(Severity::Warning).child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1p5()
                        .child(Label::new(external_ai_consent::warning_headline(
                            &target.name,
                        )))
                        .child(
                            Label::new(external_ai_consent::warning_body(&target.name))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when_some(target.note, |this, note| {
                            this.child(
                                Label::new(i18n::tr(note))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                ),
            )
            .child(
                Checkbox::new("provider-setup-consent", ToggleState::Unselected)
                    .label(i18n::t!(
                        "I understand the risks above and want to enable {name}"
                    ))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, checked: &ToggleState, _, cx| {
                        this.acknowledge(checked.selected(), cx);
                    })),
            )
            .into_any_element()
    }

    fn render_api_key(&self, cx: &mut Context<Self>) -> AnyElement {
        let provider_name = self.provider_name(cx);
        let api_key_url =
            self.provider(cx)
                .and_then(|provider| match provider.settings_view(cx)? {
                    ProviderSettingsView::ApiKey(config) => Some(config),
                    _ => None,
                });
        let (before_link, after_link) = split_around_link(&i18n::t!(
            "Visit the {link} to generate an API key.",
            link = LINK_SLOT
        ));
        v_flex()
            .gap_1p5()
            .child(Label::new(i18n::t!("API Key")))
            .when_some(api_key_url, |this, config| {
                this.child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .flex_wrap()
                        .gap_0p5()
                        .when(!before_link.is_empty(), |this| {
                            this.child(
                                Label::new(before_link)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        })
                        .child(
                            ButtonLink::new(
                                i18n::t!("{provider_name} dashboard"),
                                config.api_key_url,
                            )
                            .no_icon(true)
                            .label_size(LabelSize::Small)
                            .label_color(Color::Muted),
                        )
                        .when(!after_link.is_empty(), |this| {
                            this.child(
                                Label::new(after_link)
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        }),
                )
            })
            .child(self.api_key_input.clone())
            .child(
                Label::new(i18n::t!(
                    "The key is kept in this computer's credential store, not in a settings file."
                ))
                .size(LabelSize::XSmall)
                .color(Color::Muted),
            )
            .into_any_element()
    }

    fn render_local_choices(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.local_choices.len() < 2 {
            return None;
        }
        Some(
            h_flex()
                .gap_1()
                .flex_wrap()
                .children(self.local_choices.iter().map(|(id, name)| {
                    let selected = *id == self.provider_id;
                    let id = id.clone();
                    Button::new(
                        SharedString::from(format!("provider-setup-local-{}", id.0)),
                        name.clone(),
                    )
                    .label_size(LabelSize::Small)
                    .toggle_state(selected)
                    .style(if selected {
                        ButtonStyle::Filled
                    } else {
                        ButtonStyle::Subtle
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.select_local_provider(id.clone(), window, cx);
                    }))
                }))
                .into_any_element(),
        )
    }

    fn render_body(
        &mut self,
        step: SetupStep,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = self.provider_name(cx);
        let muted = |text: SharedString| {
            Label::new(text)
                .size(LabelSize::Small)
                .color(Color::Muted)
                .into_any_element()
        };
        match step {
            SetupStep::AiDisabled => v_flex()
                .gap_2()
                .child(muted(
                    i18n::t!(
                        "AI features are off. Turn on \"Enable Agent\" on the AI page to use them."
                    )
                    .into(),
                ))
                .child(
                    h_flex().child(
                        Button::new(
                            "provider-setup-enable-agent",
                            i18n::t!("Go to \"Enable Agent\""),
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .tab_index(0isize)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.open_detailed_settings(ENABLE_AGENT_PATH, window, cx);
                        })),
                    ),
                )
                .into_any_element(),
            SetupStep::UnknownProvider => muted(
                i18n::t!("{name} is not available here. Its settings may have been removed.")
                    .into(),
            ),
            SetupStep::Consent => match missing_consent(&self.provider_id, cx) {
                Some(target) => v_flex()
                    .gap_2()
                    .child(self.render_consent(&target, cx))
                    .children(self.render_hosts(cx))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            SetupStep::ApiKey => v_flex()
                .gap_2()
                .child(self.render_api_key(cx))
                .children(self.render_hosts(cx))
                .into_any_element(),
            SetupStep::SignIn => {
                let view = self
                    .provider(cx)
                    .and_then(|provider| self.configuration_view(&provider, window, cx));
                let is_local = is_local_provider(self.provider_id.0.as_ref());
                v_flex()
                    .gap_2()
                    .when(is_local, |this| {
                        this.child(muted(
                            i18n::t!(
                                "Start {name} on this computer. If it has no model yet, download one first (for Ollama, for example: ollama pull qwen3:8b), then press Retry."
                            )
                            .into(),
                        ))
                    })
                    .children(view)
                    .children(self.render_hosts(cx))
                    .into_any_element()
            }
            SetupStep::NoModels => v_flex()
                .gap_2()
                .child(muted(if is_local_provider(self.provider_id.0.as_ref()) {
                    i18n::t!(
                        "{name} is running but has no model. Download one (for Ollama, for example: ollama pull qwen3:8b), then press Retry."
                    )
                    .into()
                } else {
                    i18n::t!(
                        "{name} is signed in, but no model could be listed. The list is fetched from {name}, so its hosts have to be in the allowlist; also check that your account or plan includes models. Then press Retry."
                    )
                    .into()
                }))
                .children(self.render_hosts(cx))
                .into_any_element(),
            SetupStep::Ready => muted(i18n::t!("{name} is ready. Opening a new thread…").into()),
        }
    }
}

impl EventEmitter<DismissEvent> for ProviderSetupModal {}

impl Focusable for ProviderSetupModal {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for ProviderSetupModal {}

impl Render for ProviderSetupModal {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let step = self.step(cx);
        let name = self.provider_name(cx);
        let body = self.render_body(step, window, cx);
        let local_choices = self.render_local_choices(cx);
        let can_retry = matches!(step, SetupStep::SignIn | SetupStep::NoModels);
        let saving = self.saving;

        v_flex()
            .id("provider-setup-modal")
            .key_context("ProviderSetupModal")
            .w(rems(34.))
            .elevation_3(cx)
            .overflow_hidden()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .child(
                Modal::new("provider-setup", None)
                    .header(
                        ModalHeader::new()
                            .headline(i18n::t!("Set Up {name}"))
                            .description(i18n::t!(
                                "Finish the steps below to start a thread with {name}."
                            ))
                            .show_dismiss_button(true),
                    )
                    .section(
                        Section::new().child(
                            v_flex()
                                .gap_2()
                                .children(local_choices)
                                .child(body)
                                .when_some(self.error.clone(), |this, error| {
                                    this.child(
                                        Label::new(error)
                                            .size(LabelSize::Small)
                                            .color(Color::Error),
                                    )
                                }),
                        ),
                    )
                    .footer(
                        ModalFooter::new()
                            .start_slot(
                                Button::new(
                                    "provider-setup-detailed-settings",
                                    i18n::t!("Open Detailed Settings"),
                                )
                                .label_size(LabelSize::Small)
                                .tab_index(0isize)
                                .on_click(cx.listener(
                                    |this, _, window, cx| {
                                        this.open_detailed_settings(LLM_PROVIDERS_PATH, window, cx);
                                    },
                                )),
                            )
                            .end_slot(
                                h_flex()
                                    .gap_1()
                                    .when(can_retry, |this| {
                                        this.child(
                                            Button::new("provider-setup-retry", i18n::t!("Retry"))
                                                .style(ButtonStyle::Outlined)
                                                .tab_index(0isize)
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.retry(window, cx);
                                                })),
                                        )
                                    })
                                    .when(step == SetupStep::ApiKey, |this| {
                                        this.child(
                                            Button::new(
                                                "provider-setup-save",
                                                i18n::t!("Save and Start"),
                                            )
                                            .style(ButtonStyle::Filled)
                                            .loading(saving)
                                            .disabled(saving)
                                            .tab_index(0isize)
                                            .on_click(
                                                cx.listener(|this, _, window, cx| {
                                                    this.confirm(&menu::Confirm, window, cx);
                                                }),
                                            ),
                                        )
                                    }),
                            ),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::pin::Pin;
    use std::rc::Rc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    use language_model::{
        ApiKeyConfiguration, AuthenticateError, LanguageModel, LanguageModelProviderName,
        LanguageModelProviderState,
    };

    use credentials_provider::CredentialsProvider;
    use fs::{FakeFs, Fs as _};
    use futures::FutureExt as _;
    use gpui::{AsyncApp, TestAppContext, UpdateGlobal as _, VisualTestContext};
    use language_models::provider::anthropic::AnthropicLanguageModelProvider;
    use project::Project;
    use serde_json::{Value, json};
    use workspace::MultiWorkspace;

    use super::*;

    const ANTHROPIC_API_URL: &str = "https://api.anthropic.com";

    /// The model list comes from Anthropic's `/v1/models`, which a test does
    /// not reach; a model of the user's own stands in for it.
    fn with_anthropic_model(mut settings: Value) -> Value {
        settings["language_models"] = json!({ "anthropic": { "available_models": [
            { "name": "claude-test", "max_tokens": 1000 }
        ] } });
        settings
    }

    /// A provider whose key store refuses the key, for what the window does
    /// when saving fails.
    struct RefusingProvider {
        state: Entity<RefusingState>,
    }

    struct RefusingState {
        refuse: Arc<AtomicBool>,
        has_key: bool,
    }

    impl LanguageModelProviderState for RefusingProvider {
        type ObservableEntity = RefusingState;

        fn observable_entity(&self) -> Option<Entity<RefusingState>> {
            Some(self.state.clone())
        }
    }

    impl language_model::LanguageModelClient for RefusingProvider {
        fn stream_completion(
            &self,
            _model: &LanguageModel,
            _request: language_model::LanguageModelRequest,
            _cx: &AsyncApp,
        ) -> futures::future::BoxFuture<
            'static,
            Result<
                language_model::LanguageModelCompletionStream,
                language_model::LanguageModelCompletionError,
            >,
        > {
            Box::pin(async { Err(anyhow::anyhow!("this test sends no requests").into()) })
        }
    }

    impl LanguageModelProvider for RefusingProvider {
        fn id(&self) -> LanguageModelProviderId {
            LanguageModelProviderId("refusing".into())
        }

        fn name(&self) -> LanguageModelProviderName {
            LanguageModelProviderName("Refusing".into())
        }

        fn default_model(&self, _: &App) -> Option<LanguageModel> {
            None
        }

        fn default_fast_model(&self, _: &App) -> Option<LanguageModel> {
            None
        }

        fn provided_models(&self, cx: &App) -> Vec<LanguageModel> {
            if self.state.read(cx).has_key {
                vec![LanguageModel::new(
                    language_model::LanguageModelId("refusing-model".into()),
                    language_model::LanguageModelName("Refusing".into()),
                    self.id(),
                    self.name(),
                    "refusing/refusing-model",
                    1000,
                )]
            } else {
                Vec::new()
            }
        }

        fn is_authenticated(&self, cx: &App) -> bool {
            self.state.read(cx).has_key
        }

        fn authenticate(&self, _: &mut App) -> Task<Result<(), AuthenticateError>> {
            Task::ready(Ok(()))
        }

        fn settings_view(&self, cx: &mut App) -> Option<ProviderSettingsView> {
            Some(ProviderSettingsView::ApiKey(ApiKeyConfiguration::new(
                self.state.read(cx).has_key,
                false,
                "REFUSING_API_KEY".into(),
                "https://example.com/keys".into(),
            )))
        }

        fn set_api_key(&self, key: Option<String>, cx: &mut App) -> Task<anyhow::Result<()>> {
            self.state.update(cx, |state, cx| {
                if state.refuse.load(Ordering::SeqCst) {
                    return Task::ready(Err(anyhow::anyhow!("the credential store is locked")));
                }
                state.has_key = key.is_some();
                cx.notify();
                Task::ready(Ok(()))
            })
        }
    }

    fn facts(needs_consent: bool, authenticated: bool, has_models: bool) -> ProviderFacts {
        ProviderFacts {
            needs_consent,
            authenticated,
            has_models,
            asks_for_api_key: true,
        }
    }

    #[test]
    fn each_step_comes_only_after_the_one_before_it() {
        assert_eq!(
            setup_step(false, Some(facts(false, true, true))),
            SetupStep::AiDisabled,
            "with AI off nothing is set up here, not even a ready provider"
        );
        assert_eq!(setup_step(true, None), SetupStep::UnknownProvider);
        assert_eq!(
            setup_step(true, Some(facts(true, true, true))),
            SetupStep::Consent,
            "a key from the environment does not skip the consent"
        );
        assert_eq!(
            setup_step(true, Some(facts(false, false, false))),
            SetupStep::ApiKey
        );
        assert_eq!(
            setup_step(
                true,
                Some(ProviderFacts {
                    asks_for_api_key: false,
                    ..facts(false, false, false)
                })
            ),
            SetupStep::SignIn
        );
        assert_eq!(
            setup_step(true, Some(facts(false, true, false))),
            SetupStep::NoModels
        );
        assert_eq!(
            setup_step(true, Some(facts(false, true, true))),
            SetupStep::Ready
        );
    }

    #[derive(Default)]
    struct FakeCredentials {
        keys: Mutex<HashMap<String, Vec<u8>>>,
        refuse_writes: AtomicBool,
    }

    impl FakeCredentials {
        fn key_for(&self, url: &str) -> Option<String> {
            self.keys
                .lock()
                .expect("credentials lock")
                .get(url)
                .map(|key| String::from_utf8_lossy(key).to_string())
        }
    }

    impl CredentialsProvider for FakeCredentials {
        fn read_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<Option<(String, Vec<u8>)>>> + 'a>> {
            let found = self
                .keys
                .lock()
                .expect("credentials lock")
                .get(url)
                .map(|key| ("Bearer".to_string(), key.clone()));
            async move { Ok(found) }.boxed_local()
        }

        fn write_credentials<'a>(
            &'a self,
            url: &'a str,
            _username: &'a str,
            password: &'a [u8],
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
            if self.refuse_writes.load(Ordering::SeqCst) {
                return async move { Err(anyhow::anyhow!("the credential store is locked")) }
                    .boxed_local();
            }
            self.keys
                .lock()
                .expect("credentials lock")
                .insert(url.to_string(), password.to_vec());
            async move { Ok(()) }.boxed_local()
        }

        fn delete_credentials<'a>(
            &'a self,
            url: &'a str,
            _cx: &'a AsyncApp,
        ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + 'a>> {
            self.keys.lock().expect("credentials lock").remove(url);
            async move { Ok(()) }.boxed_local()
        }
    }

    struct Harness {
        fs: Arc<FakeFs>,
        credentials: Arc<FakeCredentials>,
        workspace: Entity<Workspace>,
        threads: Rc<RefCell<Vec<String>>>,
    }

    async fn harness(
        cx: &mut TestAppContext,
        user_settings: Value,
    ) -> (Harness, VisualTestContext) {
        let fs = FakeFs::new(cx.executor());
        let credentials = Arc::new(FakeCredentials::default());
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            language_model::init(cx);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });
        fs.insert_tree("/project", json!({})).await;
        fs.create_dir(
            paths::settings_file()
                .parent()
                .expect("settings file has a parent directory"),
        )
        .await
        .expect("settings directory can be created");
        let text = serde_json::to_string_pretty(&user_settings).expect("settings serialize");
        fs.save(
            paths::settings_file(),
            &text.as_str().into(),
            language::LineEnding::Unix,
        )
        .await
        .expect("settings file can be written");
        cx.update(|cx| {
            SettingsStore::update_global(cx, |store, cx| {
                store
                    .set_user_settings(&text, cx)
                    .result()
                    .expect("the test settings parse");
            });
            let provider = AnthropicLanguageModelProvider::new(
                http_client::FakeHttpClient::with_404_response(),
                credentials.clone(),
                cx,
            );
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(Arc::new(provider), cx);
            });
            crate::init(cx);
        });

        let project = Project::test(fs.clone(), ["/project".as_ref()], cx).await;
        let window = cx.add_window(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = window
            .read_with(cx, |multi_workspace, _| multi_workspace.workspace().clone())
            .expect("the window is open");
        let mut visual = VisualTestContext::from_window(window.into(), cx);
        let threads = Rc::new(RefCell::new(Vec::new()));
        workspace.update(&mut visual, |workspace, _| {
            let threads = threads.clone();
            workspace.register_action(
                move |_, action: &zed_actions::agent::NewThreadWithLanguageModelProvider, _, _| {
                    threads.borrow_mut().push(action.provider.clone());
                },
            );
        });
        visual.run_until_parked();
        (
            Harness {
                fs,
                credentials,
                workspace,
                threads,
            },
            visual,
        )
    }

    impl Harness {
        fn open(&self, provider: &str, cx: &mut VisualTestContext) {
            self.workspace.update_in(cx, |workspace, window, cx| {
                open_provider_setup_modal(workspace, Some(provider.to_string()), window, cx);
            });
            cx.run_until_parked();
        }

        fn modal(&self, cx: &mut VisualTestContext) -> Option<Entity<ProviderSetupModal>> {
            self.workspace.read_with(cx, |workspace, cx| {
                workspace.active_modal::<ProviderSetupModal>(cx)
            })
        }

        fn step(&self, cx: &mut VisualTestContext) -> SetupStep {
            let modal = self.modal(cx).expect("the setup window is open");
            cx.update(|_, cx| {
                let provider_id = modal.read(cx).provider_id.clone();
                provider_step(&provider_id, cx)
            })
        }

        async fn settings_file(&self) -> String {
            self.fs
                .load(paths::settings_file())
                .await
                .expect("settings file can be read")
        }

        fn save_key(&self, key: &str, cx: &mut VisualTestContext) {
            let modal = self.modal(cx).expect("the setup window is open");
            let key = key.to_string();
            modal.update_in(cx, |modal, window, cx| modal.save_api_key(key, window, cx));
            cx.run_until_parked();
        }
    }

    #[gpui::test]
    async fn consent_then_key_then_a_new_thread(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(
            cx,
            with_anthropic_model(json!({ "agent": { "enabled": true } })),
        )
        .await;
        let cx = &mut cx;
        harness.open("anthropic", cx);
        assert_eq!(harness.step(cx), SetupStep::Consent);

        harness.save_key("sk-before-consent", cx);
        assert_eq!(
            harness.credentials.key_for(ANTHROPIC_API_URL),
            None,
            "no key is taken before the consent"
        );

        let modal = harness.modal(cx).expect("the setup window is open");
        modal.update(cx, |modal, cx| modal.acknowledge(true, cx));
        cx.run_until_parked();
        let written: Value =
            serde_json::from_str(&harness.settings_file().await).expect("settings are JSON");
        assert_eq!(
            written["acknowledged_external_ai_providers"],
            json!(["anthropic"]),
            "the consent goes where the LLM Providers page puts it"
        );
        assert_eq!(harness.step(cx), SetupStep::ApiKey);
        assert!(harness.threads.borrow().is_empty());

        harness.save_key("  sk-test-key  ", cx);
        assert_eq!(
            harness.credentials.key_for(ANTHROPIC_API_URL).as_deref(),
            Some("sk-test-key"),
            "the key goes to the credential store"
        );
        assert!(
            !harness.settings_file().await.contains("sk-test-key"),
            "and never into a settings file"
        );
        assert_eq!(*harness.threads.borrow(), ["anthropic"]);
        assert!(
            harness.modal(cx).is_none(),
            "the window closes once the thread is asked for"
        );
    }

    /// ADR-F88 through a real provider's key state: the credential store's
    /// refusal reaches the window instead of reading as a saved key.
    #[gpui::test]
    async fn a_key_the_credential_store_refuses_starts_no_thread(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(
            cx,
            with_anthropic_model(json!({
                "agent": { "enabled": true },
                "acknowledged_external_ai_providers": ["anthropic"]
            })),
        )
        .await;
        let cx = &mut cx;
        harness
            .credentials
            .refuse_writes
            .store(true, Ordering::SeqCst);
        harness.open("anthropic", cx);
        assert_eq!(harness.step(cx), SetupStep::ApiKey);

        harness.save_key("sk-test-key", cx);
        let modal = harness.modal(cx).expect("the window stays to try again");
        let error = modal
            .read_with(cx, |modal, _| modal.error.clone())
            .expect("the failure is shown");
        assert!(error.contains("locked"), "{error}");
        assert_eq!(
            harness.step(cx),
            SetupStep::ApiKey,
            "a key only in memory would be gone at the next start"
        );
        assert!(harness.threads.borrow().is_empty());
    }

    /// The agent panel starts a thread on a provider only once this crate's
    /// consent rule says so (`NewThreadWithLanguageModelProvider` can be
    /// bound to a key).
    #[gpui::test]
    async fn the_agent_panel_reads_the_consent_from_here(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(cx, json!({ "agent": { "enabled": true } })).await;
        let cx = &mut cx;
        let consented = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                agent_settings::ai_vendor::language_model_provider_is_consented("anthropic", cx)
            })
        };
        assert!(!consented(cx), "an external provider before the consent");
        harness.open("anthropic", cx);
        let modal = harness.modal(cx).expect("the setup window is open");
        modal.update(cx, |modal, cx| modal.acknowledge(true, cx));
        cx.run_until_parked();
        assert!(consented(cx));
    }

    #[gpui::test]
    async fn a_key_that_cannot_be_saved_says_so_and_stays(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(
            cx,
            json!({
                "agent": { "enabled": true },
                "acknowledged_external_ai_providers": ["refusing"]
            }),
        )
        .await;
        let cx = &mut cx;
        let refuse = Arc::new(AtomicBool::new(true));
        cx.update(|_, cx| {
            let state = cx.new(|_| RefusingState {
                refuse: refuse.clone(),
                has_key: false,
            });
            LanguageModelRegistry::global(cx).update(cx, |registry, cx| {
                registry.register_provider(Arc::new(RefusingProvider { state }), cx);
            });
        });
        harness.open("refusing", cx);
        assert_eq!(harness.step(cx), SetupStep::ApiKey);

        harness.save_key("   ", cx);
        let modal = harness.modal(cx).expect("the setup window is open");
        assert!(
            modal.read_with(cx, |modal, _| modal.error.is_some()),
            "an empty key is not saved"
        );

        harness.save_key("sk-test-key", cx);
        let error = modal
            .read_with(cx, |modal, _| modal.error.clone())
            .expect("the failure is shown");
        assert!(error.contains("locked"), "{error}");
        assert!(harness.threads.borrow().is_empty());
        assert!(harness.modal(cx).is_some(), "the window stays to try again");
        assert_eq!(harness.step(cx), SetupStep::ApiKey);
        assert!(!modal.read_with(cx, |modal, _| modal.saving));

        refuse.store(false, Ordering::SeqCst);
        harness.save_key("sk-test-key", cx);
        assert_eq!(*harness.threads.borrow(), ["refusing"]);
    }

    #[gpui::test]
    async fn with_ai_off_nothing_is_set_up(cx: &mut TestAppContext) {
        let (harness, mut cx) = harness(
            cx,
            with_anthropic_model(json!({ "agent": { "enabled": false } })),
        )
        .await;
        let cx = &mut cx;
        harness.open("anthropic", cx);
        assert_eq!(harness.step(cx), SetupStep::AiDisabled);

        harness.save_key("sk-test-key", cx);
        assert_eq!(harness.credentials.key_for(ANTHROPIC_API_URL), None);
        assert!(harness.threads.borrow().is_empty());
        let settings_file = harness.settings_file().await;
        assert!(
            !settings_file.contains("acknowledged_external_ai_providers")
                && !settings_file.contains("\"enabled\": true"),
            "neither AI nor the provider is turned on from here: {settings_file}"
        );
    }

    #[gpui::test]
    async fn a_provider_that_already_works_opens_the_thread_without_a_window(
        cx: &mut TestAppContext,
    ) {
        let (harness, mut cx) = harness(
            cx,
            with_anthropic_model(json!({
                "agent": { "enabled": true },
                "acknowledged_external_ai_providers": ["anthropic"]
            })),
        )
        .await;
        let cx = &mut cx;
        harness
            .credentials
            .keys
            .lock()
            .expect("credentials lock")
            .insert(ANTHROPIC_API_URL.to_string(), b"sk-existing".to_vec());
        let provider = cx.update(|_, cx| {
            LanguageModelRegistry::read_global(cx)
                .provider(&LanguageModelProviderId("anthropic".into()))
                .expect("registered")
        });
        cx.update(|_, cx| provider.authenticate(cx))
            .await
            .expect("the stored key loads");

        harness.open("anthropic", cx);
        assert!(harness.modal(cx).is_none());
        assert_eq!(*harness.threads.borrow(), ["anthropic"]);
    }
}
