use codestral::{CODESTRAL_API_URL, codestral_api_key_state, codestral_api_url};
use edit_prediction::{
    ApiKeyState,
    mercury::{MERCURY_CREDENTIALS_URL, mercury_api_token},
    open_ai_compatible::{open_ai_compatible_api_token, open_ai_compatible_api_url},
};
use edit_prediction_ui::{get_available_providers, set_completion_provider};
use gpui::{App, Entity, ScrollHandle, TaskExt, prelude::*};
use language::language_settings::{AllLanguageSettings, EditPredictionProvider};

use settings::Settings as _;
use ui::{ButtonLink, ConfiguredApiCard, ContextMenu, DropdownMenu, DropdownStyle, prelude::*};
use workspace::AppState;

const OLLAMA_API_URL_PLACEHOLDER: &str = "http://localhost:11434";
const OLLAMA_MODEL_PLACEHOLDER: &str = "qwen2.5-coder:3b-base";

const OPEN_AI_COMPATIBLE_API_URL_PLACEHOLDER: &str = "http://localhost:8080/v1/completions";
const OPEN_AI_COMPATIBLE_MODEL_PLACEHOLDER: &str = "qwen2.5-coder:3b-base";

use crate::{
    SettingField, SettingItem, SettingsFieldMetadata, SettingsPageItem, SettingsWindow, USER,
    components::{SettingsInputField, SettingsSectionHeader},
    pages::external_ai_consent::{self, ConsentTarget},
    pages::llm_providers_page,
};

/// Endpoint the Copilot chat/completions API is reached at
/// (`DEFAULT_COPILOT_API_ENDPOINT` in `crates/copilot_chat/src/copilot_chat.rs`).
/// `github.com` is the device-code sign-in host
/// (`CopilotChatConfiguration::oauth_domain`).
const COPILOT_API_URL: &str = "https://api.githubcopilot.com";
/// `MERCURY_API_URL` / `FEEDBACK_API_URL` in `crates/edit_prediction/src/mercury.rs`.
const MERCURY_API_URL: &str = "https://api.inceptionlabs.ai/v1/edit/completions";

/// How `EditPredictionProvider::None` is named in the dropdown trigger and in
/// its menu entry, which spells out what picking it does.
const NONE_LABEL: &str = i18n::msg!("None");
const NONE_LABEL_WITH_HINT: &str = i18n::msg!("None (disable edit predictions)");

pub(crate) fn render_edit_prediction_setup_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    // Card order mirrors the dropdown order from `get_available_providers`:
    // self-hosted first, cloud services after.
    let providers = [
        Some(render_provider_dropdown_control(window, cx)),
        Some(
            render_api_key_provider(
                settings_window,
                IconName::AiOpenAiCompat,
                "OpenAI Compatible API",
                ApiKeyDocs::Custom {
                    message: i18n::t!("The API key sent as Authorization: Bearer {{key}}.").into(),
                },
                open_ai_compatible_api_token(cx),
                |cx| open_ai_compatible_api_url(cx),
                external_ai_consent::edit_prediction_target(
                    "open_ai_compatible_api",
                    "OpenAI Compatible API",
                    Some(&open_ai_compatible_api_url(cx)),
                    &[],
                    None,
                ),
                Some(
                    settings_window
                        .render_sub_page_items_section(
                            open_ai_compatible_settings().iter().enumerate(),
                            true,
                            window,
                            cx,
                        )
                        .into_any_element(),
                ),
                window,
                cx,
            )
            .into_any_element(),
        ),
        Some(render_ollama_provider(settings_window, window, cx).into_any_element()),
        render_github_copilot_provider(settings_window, window, cx)
            .map(IntoElement::into_any_element),
        Some(
            render_api_key_provider(
                settings_window,
                IconName::AiMistral,
                "Codestral",
                ApiKeyDocs::Link {
                    dashboard_url: "https://console.mistral.ai/codestral".into(),
                },
                codestral_api_key_state(cx),
                |cx| codestral_api_url(cx),
                external_ai_consent::edit_prediction_target(
                    "codestral",
                    "Codestral",
                    Some(&codestral_api_url(cx)),
                    &[],
                    None,
                ),
                Some(
                    settings_window
                        .render_sub_page_items_section(
                            codestral_settings().iter().enumerate(),
                            true,
                            window,
                            cx,
                        )
                        .into_any_element(),
                ),
                window,
                cx,
            )
            .into_any_element(),
        ),
        Some(
            render_api_key_provider(
                settings_window,
                IconName::Inception,
                "Mercury",
                ApiKeyDocs::Link {
                    dashboard_url: "https://platform.inceptionlabs.ai/dashboard/api-keys".into(),
                },
                mercury_api_token(cx),
                |_cx| MERCURY_CREDENTIALS_URL,
                external_ai_consent::edit_prediction_target(
                    "mercury",
                    "Mercury",
                    Some(MERCURY_API_URL),
                    &["api-feedback.inceptionlabs.ai"],
                    None,
                ),
                Some(
                    settings_window
                        .render_sub_page_items_section(
                            mercury_settings().iter().enumerate(),
                            true,
                            window,
                            cx,
                        )
                        .into_any_element(),
                ),
                window,
                cx,
            )
            .into_any_element(),
        ),
        Some(render_zed_provider(settings_window, window, cx).into_any_element()),
    ];

    div()
        .size_full()
        .child(
            v_flex()
                .id("ep-setup-page")
                .min_w_0()
                .size_full()
                .px_8()
                .pb_16()
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .children(providers.into_iter().flatten()),
        )
        .into_any_element()
}

/// The endpoint the Copilot edit prediction provider signs in against: the
/// enterprise URI when one is configured, and Copilot's own otherwise.
fn copilot_edit_prediction_api_url(cx: &App) -> String {
    AllLanguageSettings::get_global(cx)
        .edit_predictions
        .copilot
        .enterprise_uri
        .clone()
        .filter(|uri| !uri.trim().is_empty())
        .unwrap_or_else(|| COPILOT_API_URL.to_string())
}

/// Copilot's consent target, which exists even for a loopback `enterprise_uri`:
/// the language server reaches GitHub on its own, outside the egress allowlist,
/// and `copilot` does not start it without this consent.
fn copilot_consent_target(note: Option<&'static str>, cx: &App) -> Option<ConsentTarget> {
    const HOSTS: &[&str] = &["github.com", "api.github.com"];
    external_ai_consent::edit_prediction_target(
        "copilot",
        "GitHub Copilot",
        Some(&copilot_edit_prediction_api_url(cx)),
        HOSTS,
        note,
    )
    .or_else(|| {
        external_ai_consent::edit_prediction_target("copilot", "GitHub Copilot", None, HOSTS, note)
    })
}

/// Every edit prediction provider's consent target, built exactly as the cards
/// above build them.
///
/// `external_ai_consent` needs this to answer "is this host still needed by
/// something else the user has consented to?" when a consent is withdrawn: the
/// `edit_prediction:` keys stored in `acknowledged_external_ai_providers` carry
/// no endpoint of their own, and the settings each of these reads live in
/// crates `external_ai_consent` does not depend on. `Zed` is deliberately absent
/// — it shares the `zed.dev` key with the LLM provider, which that lookup
/// resolves on its own.
pub(crate) fn edit_prediction_consent_targets(cx: &App) -> Vec<ConsentTarget> {
    [
        external_ai_consent::edit_prediction_target(
            "open_ai_compatible_api",
            "OpenAI Compatible API",
            Some(&open_ai_compatible_api_url(cx)),
            &[],
            None,
        ),
        external_ai_consent::edit_prediction_target(
            "ollama",
            "Ollama",
            Some(&ollama_edit_prediction_api_url(cx)),
            &[],
            None,
        ),
        copilot_consent_target(None, cx),
        external_ai_consent::edit_prediction_target(
            "codestral",
            "Codestral",
            Some(&codestral_api_url(cx)),
            &[],
            None,
        ),
        external_ai_consent::edit_prediction_target(
            "mercury",
            "Mercury",
            Some(MERCURY_API_URL),
            &["api-feedback.inceptionlabs.ai"],
            None,
        ),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// `Zed` is the one edit prediction provider whose consent key is shared with an
/// agent (LLM) provider, so both surfaces have to say so.
const ZED_CONSENT_SHARED_NOTE: &str = i18n::msg!(
    "This consent is shared with the \"Zed\" LLM provider on the AI page. Withdrawing it applies to both edit predictions and the agent."
);

/// The `zed.dev` consent target: the same one the "LLM プロバイダー" sub-page
/// builds, so the hosts and the key never drift between the two surfaces.
fn zed_consent_target(cx: &App) -> Option<ConsentTarget> {
    let mut target = external_ai_consent::llm_provider_target(
        settings::ZED_CLOUD_AI_CONSENT_KEY,
        "Zed".into(),
        cx,
    )?;
    target.note = Some(ZED_CONSENT_SHARED_NOTE);
    Some(target)
}

/// The interactive "使用中のプロバイダー" control at the top of this page: a
/// dropdown over every provider `get_available_providers` reports as usable,
/// plus the "なし" entry that turns edit prediction off. It is the only UI
/// writer of `edit_predictions.provider`.
///
/// Every entry it lists is already past its ADR-F10 gate — a provider is listed
/// only once its credentials, endpoint or consent have been set up on one of the
/// cards below — so picking one applies it straight away.
fn render_provider_dropdown_control(
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let current_provider = AllLanguageSettings::get_global(cx)
        .edit_predictions
        .provider;
    // `EditPredictionProvider::None` has no `display_name`; it is the "なし"
    // entry of this dropdown, so the trigger has to name it the same way.
    let trigger_label: SharedString = current_provider
        .display_name()
        .unwrap_or_else(|| i18n::tr(NONE_LABEL))
        .into();

    let menu = ContextMenu::build(window, cx, move |mut menu, _, cx| {
        let available_providers = get_available_providers(cx);
        let fs = <dyn fs::Fs>::global(cx);

        for provider in available_providers {
            let Some(name) = provider.display_name() else {
                continue;
            };
            let is_current = provider == current_provider;

            menu = menu.toggleable_entry(name, is_current, IconPosition::Start, None, {
                let fs = fs.clone();
                move |_, cx| {
                    set_completion_provider(fs.clone(), cx, provider);
                }
            });
        }

        // `get_available_providers` only reports providers that can be turned
        // on, so turning edit prediction off has to be offered here.
        menu.separator().toggleable_entry(
            i18n::tr(NONE_LABEL_WITH_HINT),
            current_provider == EditPredictionProvider::None,
            IconPosition::Start,
            None,
            move |_, cx| {
                set_completion_provider(fs.clone(), cx, EditPredictionProvider::None);
            },
        )
    });

    v_flex()
        .id("provider-selector")
        .min_w_0()
        .gap_1p5()
        .child(SettingsSectionHeader::new(i18n::t!("Active Provider")).no_padding(true))
        .child(
            h_flex()
                .pt_2p5()
                .w_full()
                .min_w_0()
                .justify_between()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .max_w_1_2()
                        .child(Label::new(i18n::t!("Provider")))
                        .child(
                            Label::new(i18n::t!(
                                "Select which provider to use for edit predictions."
                            ))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                )
                .child(
                    DropdownMenu::new("provider-dropdown", trigger_label, menu)
                        .tab_index(0)
                        .style(DropdownStyle::Outlined),
                ),
        )
        .child(
            // `get_available_providers` only lists providers that are past
            // their own gate, so a provider that has not been set up is simply
            // absent from the dropdown; say where to set one up.
            Label::new(i18n::t!(
                "OpenAI Compatible API, Ollama, GitHub Copilot, Codestral, and Mercury become selectable once their credentials or endpoint URL are set up in their sections below. Zed becomes selectable once you give consent in the \"Zed\" section below."
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .into_any_element()
}

enum ApiKeyDocs {
    Link { dashboard_url: SharedString },
    Custom { message: SharedString },
}

fn render_api_key_provider(
    settings_window: &SettingsWindow,
    icon: IconName,
    title: &'static str,
    docs: ApiKeyDocs,
    api_key_state: Entity<ApiKeyState>,
    current_url: fn(&mut App) -> SharedString,
    consent: Option<ConsentTarget>,
    additional_fields: Option<AnyElement>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let weak_page = cx.weak_entity();
    let credentials_provider = zed_credentials_provider::global(cx);
    _ = window.use_keyed_state(current_url(cx), cx, |_, cx| {
        let task = api_key_state.update(cx, |key_state, cx| {
            key_state.load_if_needed(
                current_url(cx),
                |state| state,
                credentials_provider.clone(),
                cx,
            )
        });
        cx.spawn(async move |_, cx| {
            task.await.ok();
            weak_page
                .update(cx, |_, cx| {
                    cx.notify();
                })
                .ok();
        })
    });

    let (has_key, env_var_name, is_from_env_var) = api_key_state.read_with(cx, |state, _| {
        (
            state.has_key(),
            Some(state.env_var_name().clone()),
            state.is_from_env_var(),
        )
    });

    let write_key = move |api_key: Option<String>, cx: &mut App| {
        let credentials_provider = zed_credentials_provider::global(cx);
        api_key_state
            .update(cx, |key_state, cx| {
                let url = current_url(cx);
                key_state.store(
                    url,
                    api_key,
                    |key_state| key_state,
                    credentials_provider,
                    cx,
                )
            })
            .detach_and_log_err(cx);
    };

    let base_container = v_flex().id(title).min_w_0().pt_8().gap_1p5();

    let header = SettingsSectionHeader::new(title)
        .icon(icon)
        .no_padding(true);

    let description = match docs {
        ApiKeyDocs::Custom { message } => div().min_w_0().w_full().child(
            Label::new(message)
                .size(LabelSize::Small)
                .color(Color::Muted),
        ),
        ApiKeyDocs::Link { dashboard_url } => {
            let (before_link, after_link) = llm_providers_page::split_around_link(&i18n::t!(
                "Visit the {link} to generate an API key.",
                link = llm_providers_page::LINK_SLOT
            ));
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
                        i18n::t!("{provider_name} dashboard", provider_name = title),
                        dashboard_url,
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
                })
        }
    };

    let configured_card_label = if is_from_env_var {
        i18n::t!("API Key Set in Environment Variable")
    } else {
        i18n::t!("API Key Configured")
    };

    // While consent for an external endpoint is missing, the credential UI is
    // replaced by the warning (ADR-F10). `additional_fields` stays visible: the
    // API URL lives there, and the user has to be able to point it back at a
    // local server to get out of the gate.
    let needs_consent = consent
        .as_ref()
        .is_some_and(|target| !external_ai_consent::is_acknowledged(&target.key, cx));
    let base_container =
        base_container
            .child(header)
            .when_some(consent.as_ref(), |this, target| {
                this.child(if needs_consent {
                    external_ai_consent::render_consent_gate(settings_window, target, cx)
                } else {
                    external_ai_consent::render_consent_summary(settings_window, target, cx)
                })
            });

    let container = if needs_consent {
        base_container
    } else if has_key {
        base_container.child(
            ConfiguredApiCard::new(format!("{title}-reset-key"), configured_card_label)
                .button_label(i18n::t!("Reset Key"))
                .button_tab_index(0)
                .disabled(is_from_env_var)
                .when_some(env_var_name, |this, env_var_name| {
                    this.when(is_from_env_var, |this| {
                        this.tooltip_label(i18n::t!(
                            "To reset your API key, unset the {} environment variable.",
                            env_var_name
                        ))
                    })
                })
                .on_click(move |_, _, cx| {
                    write_key(None, cx);
                }),
        )
    } else {
        base_container.child(
            h_flex()
                .pt_2p5()
                .w_full()
                .min_w_0()
                .justify_between()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .max_w_1_2()
                        .gap_0p5()
                        .child(Label::new(i18n::t!("API Key")))
                        .child(description)
                        .when_some(env_var_name, |this, env_var_name| {
                            this.child({
                                let label = i18n::t!(
                                    "Or set the {} env var and restart Zed.",
                                    env_var_name.as_ref()
                                );
                                Label::new(label).size(LabelSize::Small).color(Color::Muted)
                            })
                        }),
                )
                .child(
                    SettingsInputField::new(format!("{}-api-key-input", title))
                        .tab_index(0)
                        .with_placeholder("xxxxxxxxxxxxxxxxxxxx")
                        .aria_label(i18n::t!("{} API Key", title))
                        .on_confirm(move |api_key, _window, cx| {
                            write_key(api_key.filter(|key| !key.is_empty()), cx);
                        }),
                ),
        )
    };

    container.when_some(additional_fields, |this, additional_fields| {
        this.child(
            div()
                .map(|this| if has_key { this.mt_1() } else { this.mt_4() })
                .px_neg_8()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .child(additional_fields),
        )
    })
}

fn render_ollama_provider(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let ollama_settings = ollama_settings();
    let additional_fields = settings_window
        .render_sub_page_items_section(ollama_settings.iter().enumerate(), true, window, cx)
        .into_any_element();

    // Ollama has no credential to withhold, so the warning is shown alongside
    // its fields rather than in place of them. It only appears at all if the
    // user pointed `edit_predictions.ollama.api_url` at a remote server.
    let consent = external_ai_consent::edit_prediction_target(
        "ollama",
        "Ollama",
        Some(&ollama_edit_prediction_api_url(cx)),
        &[],
        None,
    );

    v_flex()
        .id("ollama")
        .min_w_0()
        .pt_8()
        .gap_1p5()
        .child(
            SettingsSectionHeader::new("Ollama")
                .icon(IconName::AiOllama)
                .no_padding(true),
        )
        .when_some(consent.as_ref(), |this, target| {
            this.child(if external_ai_consent::is_acknowledged(&target.key, cx) {
                external_ai_consent::render_consent_summary(settings_window, target, cx)
            } else {
                external_ai_consent::render_consent_gate(settings_window, target, cx)
            })
        })
        .child(div().px_neg_8().child(additional_fields))
}

/// The endpoint the Ollama edit prediction provider actually uses: the
/// `edit_predictions.ollama.api_url` setting, falling back to Ollama's own
/// default (`ollama::OLLAMA_API_URL`).
fn ollama_edit_prediction_api_url(cx: &App) -> String {
    AllLanguageSettings::get_global(cx)
        .edit_predictions
        .ollama
        .as_ref()
        .map(|settings| settings.api_url.to_string())
        .filter(|api_url| !api_url.trim().is_empty())
        .unwrap_or_else(|| OLLAMA_API_URL_PLACEHOLDER.to_string())
}

fn ollama_settings() -> Box<[SettingsPageItem]> {
    Box::new([
        SettingsPageItem::SettingItem(SettingItem {
            title: "API URL",
            description: "The base URL of your Ollama server.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .ollama
                        .as_ref()?
                        .api_url
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .api_url = value;
                },
                json_path: Some("edit_predictions.ollama.api_url"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some(OLLAMA_API_URL_PLACEHOLDER),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Model",
            description: "The Ollama model to use for edit predictions.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .ollama
                        .as_ref()?
                        .model
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .model = value;
                },
                json_path: Some("edit_predictions.ollama.model"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some(OLLAMA_MODEL_PLACEHOLDER),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Prompt Format",
            description: "The prompt format to use when requesting predictions. Set to Infer to have the format inferred based on the model name.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .ollama
                        .as_ref()?
                        .prompt_format
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .prompt_format = value;
                },
                json_path: Some("edit_predictions.ollama.prompt_format"),
            }),
            files: USER,
            metadata: None,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Max Output Tokens",
            description: "The maximum number of tokens to generate.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .ollama
                        .as_ref()?
                        .max_output_tokens
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .max_output_tokens = value;
                },
                json_path: Some("edit_predictions.ollama.max_output_tokens"),
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Prediction Debounce",
            description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .ollama
                        .as_ref()?
                        .prediction_debounce
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .ollama
                        .get_or_insert_default()
                        .prediction_debounce = value;
                },
                json_path: Some("edit_predictions.ollama.prediction_debounce"),
            }),
            metadata: None,
            files: USER,
        }),
    ])
}

fn open_ai_compatible_settings() -> Box<[SettingsPageItem]> {
    Box::new([
        SettingsPageItem::SettingItem(SettingItem {
            title: "API URL",
            description: "The URL of your OpenAI-compatible server's completions API.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .open_ai_compatible_api
                        .as_ref()?
                        .api_url
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .open_ai_compatible_api
                        .get_or_insert_default()
                        .api_url = value;
                },
                json_path: Some("edit_predictions.open_ai_compatible_api.api_url"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some(OPEN_AI_COMPATIBLE_API_URL_PLACEHOLDER),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Model",
            description: "The model string to pass to the OpenAI-compatible server.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .open_ai_compatible_api
                        .as_ref()?
                        .model
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .open_ai_compatible_api
                        .get_or_insert_default()
                        .model = value;
                },
                json_path: Some("edit_predictions.open_ai_compatible_api.model"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some(OPEN_AI_COMPATIBLE_MODEL_PLACEHOLDER),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Prompt Format",
            description: "The prompt format to use when requesting predictions. Set to Infer to have the format inferred based on the model name.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .open_ai_compatible_api
                        .as_ref()?
                        .prompt_format
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .open_ai_compatible_api
                        .get_or_insert_default()
                        .prompt_format = value;
                },
                json_path: Some("edit_predictions.open_ai_compatible_api.prompt_format"),
            }),
            files: USER,
            metadata: None,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Max Output Tokens",
            description: "The maximum number of tokens to generate.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .open_ai_compatible_api
                        .as_ref()?
                        .max_output_tokens
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .open_ai_compatible_api
                        .get_or_insert_default()
                        .max_output_tokens = value;
                },
                json_path: Some("edit_predictions.open_ai_compatible_api.max_output_tokens"),
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Prediction Debounce",
            description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .open_ai_compatible_api
                        .as_ref()?
                        .prediction_debounce
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .open_ai_compatible_api
                        .get_or_insert_default()
                        .prediction_debounce = value;
                },
                json_path: Some("edit_predictions.open_ai_compatible_api.prediction_debounce"),
            }),
            metadata: None,
            files: USER,
        }),
    ])
}

fn codestral_settings() -> Box<[SettingsPageItem]> {
    Box::new([
        SettingsPageItem::SettingItem(SettingItem {
            title: "API URL",
            description: "The API URL to use for Codestral.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .codestral
                        .as_ref()?
                        .api_url
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .codestral
                        .get_or_insert_default()
                        .api_url = value;
                },
                json_path: Some("edit_predictions.codestral.api_url"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some(CODESTRAL_API_URL),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Max Tokens",
            description: "The maximum number of tokens to generate.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .codestral
                        .as_ref()?
                        .max_tokens
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .codestral
                        .get_or_insert_default()
                        .max_tokens = value;
                },
                json_path: Some("edit_predictions.codestral.max_tokens"),
            }),
            metadata: None,
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Model",
            description: "The Codestral model id to use.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .codestral
                        .as_ref()?
                        .model
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .codestral
                        .get_or_insert_default()
                        .model = value;
                },
                json_path: Some("edit_predictions.codestral.model"),
            }),
            metadata: Some(Box::new(SettingsFieldMetadata {
                placeholder: Some("codestral-latest"),
                ..Default::default()
            })),
            files: USER,
        }),
        SettingsPageItem::SettingItem(SettingItem {
            title: "Prediction Debounce",
            description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
            field: Box::new(SettingField {
                organization_override: None,
                pick: |settings| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .as_ref()?
                        .codestral
                        .as_ref()?
                        .prediction_debounce
                        .as_ref()
                },
                write: |settings, value, _app: &App| {
                    settings
                        .project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_default()
                        .codestral
                        .get_or_insert_default()
                        .prediction_debounce = value;
                },
                json_path: Some("edit_predictions.codestral.prediction_debounce"),
            }),
            metadata: None,
            files: USER,
        }),
    ])
}

fn mercury_settings() -> Box<[SettingsPageItem]> {
    Box::new([SettingsPageItem::SettingItem(SettingItem {
        title: "Prediction Debounce",
        description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
        field: Box::new(SettingField {
            organization_override: None,
            pick: |settings| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .as_ref()?
                    .mercury
                    .as_ref()?
                    .prediction_debounce
                    .as_ref()
            },
            write: |settings, value, _app: &App| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .get_or_insert_default()
                    .mercury
                    .get_or_insert_default()
                    .prediction_debounce = value;
            },
            json_path: Some("edit_predictions.mercury.prediction_debounce"),
        }),
        metadata: None,
        files: USER,
    })])
}

fn zed_settings() -> Box<[SettingsPageItem]> {
    Box::new([SettingsPageItem::SettingItem(SettingItem {
        title: "Prediction Debounce",
        description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
        field: Box::new(SettingField {
            organization_override: None,
            pick: |settings| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .as_ref()?
                    .zed
                    .as_ref()?
                    .prediction_debounce
                    .as_ref()
            },
            write: |settings, value, _app: &App| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .get_or_insert_default()
                    .zed
                    .get_or_insert_default()
                    .prediction_debounce = value;
            },
            json_path: Some("edit_predictions.zed.prediction_debounce"),
        }),
        metadata: None,
        files: USER,
    })])
}

fn copilot_settings() -> Box<[SettingsPageItem]> {
    Box::new([SettingsPageItem::SettingItem(SettingItem {
        title: "Prediction Debounce",
        description: "Delay in milliseconds before automatically requesting a prediction after typing stops. Set to 0 to request predictions immediately.",
        field: Box::new(SettingField {
            organization_override: None,
            pick: |settings| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .as_ref()?
                    .copilot
                    .as_ref()?
                    .prediction_debounce
                    .as_ref()
            },
            write: |settings, value, _app: &App| {
                settings
                    .project
                    .all_languages
                    .edit_predictions
                    .get_or_insert_default()
                    .copilot
                    .get_or_insert_default()
                    .prediction_debounce = value;
            },
            json_path: Some("edit_predictions.copilot.prediction_debounce"),
        }),
        metadata: None,
        files: USER,
    })])
}

/// The card for `Zed`, last in the list so that it matches the dropdown order.
///
/// `Zed` is gated exactly like the other providers: while its shared `zed.dev`
/// consent is missing the card shows the same `render_consent_gate` warning the
/// other cards show, and `get_available_providers` keeps `Zed` out of the
/// dropdown until it is ticked. Where the other cards then reveal a credential
/// field, this one has nothing to enter — so it shows what there is to manage
/// instead: the acknowledged state, the hosts it needs in the egress allowlist,
/// and a way to withdraw the consent.
fn render_zed_provider(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let target = zed_consent_target(cx);
    let zed_settings = zed_settings();
    let additional_fields = settings_window
        .render_sub_page_items_section(zed_settings.iter().enumerate(), true, window, cx)
        .into_any_element();
    let acknowledged = external_ai_consent::is_acknowledged(settings::ZED_CLOUD_AI_CONSENT_KEY, cx);

    v_flex()
        .id("zed")
        .min_w_0()
        .pt_8()
        .gap_1p5()
        .child(
            SettingsSectionHeader::new("Zed")
                .icon(IconName::AiZed)
                .no_padding(true),
        )
        .child(
            Label::new(
                i18n::t!("Zed's cloud edit predictions (Zeta). The code you are editing and the code around it are sent to cloud.zed.dev."),
            )
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .when_some(target.as_ref(), |this, target| {
            if !acknowledged {
                // The gate already carries the hosts and the shared-key note via
                // `render_warning_content`.
                return this.child(external_ai_consent::render_consent_gate(
                    settings_window,
                    target,
                    cx,
                ));
            }
            this.child(
                v_flex()
                    .pt_2p5()
                    .w_full()
                    .min_w_0()
                    .gap_1()
                    .child(external_ai_consent::render_acknowledged_row(target, cx))
                    .child(
                        Label::new(
                            i18n::t!("Withdrawing consent does not stop Zed while it is the selected provider (the warning is shown again and Zed disappears from the \"Provider\" list above). To stop it, choose \"None\" under \"Provider\" above."),
                        )
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    )
                    // Unlike `render_consent_summary`, the hosts stay visible
                    // once they are registered: this is the only place on this
                    // page where the egress allowlist for `zed.dev` can be
                    // inspected.
                    .child(external_ai_consent::render_hosts(
                        settings_window,
                        target,
                        cx,
                    ))
                    .when_some(target.note, |this, note| {
                        this.child(
                            Label::new(i18n::tr(note))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
        })
        // The debounce only delays requests Zed makes itself; it sends nothing, so
        // it stays reachable while the consent is missing, like Ollama's fields.
        .child(div().px_neg_8().child(additional_fields))
}

fn render_github_copilot_provider(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Option<impl IntoElement> {
    let consent = copilot_consent_target(
        Some(i18n::msg!(
            "Edit prediction traffic is sent by the Copilot language server (a separate process), so it is not covered by the egress allowlist. Registering the hosts only affects signing in from Zed itself."
        )),
        cx,
    );
    let needs_consent = consent
        .as_ref()
        .is_some_and(|target| !external_ai_consent::is_acknowledged(&target.key, cx));

    // The sign-in view is only built once consent has been given.
    let configuration_view = (!needs_consent).then(|| {
        window.use_state(cx, |_, cx| {
            copilot_ui::ConfigurationView::new(
                move |cx| {
                    let app_state = AppState::global(cx);
                    copilot::GlobalCopilotAuth::try_get_or_init(app_state, cx)
                        .is_some_and(|copilot| copilot.0.read(cx).is_authenticated())
                },
                copilot_ui::ConfigurationMode::EditPrediction,
                cx,
            )
        })
    });

    let additional_fields = settings_window
        .render_sub_page_items_section(copilot_settings().iter().enumerate(), true, window, cx)
        .into_any_element();

    Some(
        v_flex()
            .id("github-copilot")
            .min_w_0()
            .pt_8()
            .gap_1p5()
            .child(
                SettingsSectionHeader::new("GitHub Copilot")
                    .icon(IconName::Copilot)
                    .no_padding(true),
            )
            .when_some(consent.as_ref(), |this, target| {
                this.child(if needs_consent {
                    external_ai_consent::render_consent_gate(settings_window, target, cx)
                } else {
                    external_ai_consent::render_consent_summary(settings_window, target, cx)
                })
            })
            .children(configuration_view)
            .child(div().px_neg_8().child(additional_fields)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The "Zed" card's consent block and the provider dropdown have to act on
    /// the same key: ticking the card's checkbox is what makes
    /// `edit_prediction_ui::get_available_providers` start listing `Zed`, and
    /// the card's revoke button is what removes it again.
    #[gpui::test]
    fn the_zed_card_gates_on_the_shared_zed_cloud_key(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let target = zed_consent_target(cx)
                .expect("Zed Cloud has no configurable endpoint and is always external");
            assert_eq!(
                target.key,
                SharedString::from(settings::ZED_CLOUD_AI_CONSENT_KEY)
            );
            assert_eq!(
                target.note,
                Some(ZED_CONSENT_SHARED_NOTE),
                "the card has to say that the key is shared with the LLM provider"
            );
            assert!(target.hosts.contains(&SharedString::from("cloud.zed.dev")));
        });
    }

    /// `copilot` does not start its language server without this consent, whatever
    /// `enterprise_uri` says, so the card has to ask for it even for a loopback one.
    #[gpui::test]
    fn the_copilot_card_asks_for_the_key_copilot_checks(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
        });
        for enterprise_uri in [
            None,
            Some("http://127.0.0.1:8443"),
            Some("https://ghe.corp.example"),
        ] {
            cx.update(|cx| {
                cx.update_global::<settings::SettingsStore, _>(|store, cx| {
                    store.update_user_settings(cx, |settings| {
                        settings
                            .project
                            .all_languages
                            .edit_predictions
                            .get_or_insert_default()
                            .copilot
                            .get_or_insert_default()
                            .enterprise_uri = enterprise_uri.map(str::to_string);
                    });
                });
                let target = copilot_consent_target(None, cx)
                    .unwrap_or_else(|| panic!("no consent asked for {enterprise_uri:?}"));
                assert_eq!(
                    target.key,
                    SharedString::from(settings::EDIT_PREDICTION_COPILOT_CONSENT_KEY),
                    "{enterprise_uri:?}"
                );
                assert!(
                    target.hosts.contains(&SharedString::from("github.com")),
                    "{enterprise_uri:?}"
                );
                assert!(
                    edit_prediction_consent_targets(cx)
                        .iter()
                        .any(|target| target.key.as_ref()
                            == settings::EDIT_PREDICTION_COPILOT_CONSENT_KEY),
                    "{enterprise_uri:?}"
                );
            });
        }
    }
}
