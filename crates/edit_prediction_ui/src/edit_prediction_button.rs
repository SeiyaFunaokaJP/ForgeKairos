use anyhow::Result;
use client::{Client, UserStore, zed_urls};
use cloud_llm_client::UsageLimit;
use codestral::{self, CodestralEditPredictionDelegate};
use copilot::Status;
use edit_prediction::EditPredictionStore;
use edit_prediction_types::EditPredictionDelegateHandle;
use editor::{
    Editor, MultiBufferOffset, SelectionEffects, actions::ShowEditPrediction, scroll::Autoscroll,
};
use feature_flags::FeatureFlagAppExt;
use fs::Fs;
use gpui::{
    Action, Anchor, Animation, AnimationExt, App, AsyncWindowContext, Entity, FocusHandle,
    Focusable, IntoElement, ParentElement, Render, Subscription, TaskExt, WeakEntity, actions, div,
    ease_in_out, pulsating_between,
};
use indoc::indoc;
use language::{
    EditPredictionsMode, File, Language,
    language_settings::{
        AllLanguageSettings, EditPredictionProvider, LanguageSettings, all_language_settings,
    },
};
use project::{DisableAiSettings, Project};
use regex::Regex;
use settings::{Settings, SettingsStore, update_settings_file};
use std::{
    rc::Rc,
    sync::{Arc, LazyLock},
    time::Duration,
};
use ui::{
    Clickable, ContextMenu, ContextMenuEntry, DocumentationSide, IconButton, IconButtonShape,
    Indicator, PopoverMenu, PopoverMenuHandle, ProgressBar, Tooltip, prelude::*,
};
use util::ResultExt as _;

use workspace::{
    HideStatusItem, StatusItemView, Toast, Workspace, create_and_open_local_file, item::ItemHandle,
    notifications::NotificationId,
};
use zed_actions::{OpenBrowser, OpenSettingsAt};

use crate::{RatePredictions, rate_prediction_modal::PredictEditsRatePredictionsFeatureFlag};

actions!(
    edit_prediction,
    [
        /// Toggles the edit prediction menu.
        ToggleMenu
    ]
);

const COPILOT_SETTINGS_PATH: &str = "/settings/copilot";
const COPILOT_SETTINGS_URL: &str = concat!("https://github.com", "/settings/copilot");
const PRIVACY_DOCS: &str = "https://zed.dev/docs/ai/privacy-and-security";

struct CopilotErrorToast;

pub struct EditPredictionButton {
    editor_subscription: Option<(Subscription, usize)>,
    editor_enabled: Option<bool>,
    editor_show_predictions: bool,
    editor_focus_handle: Option<FocusHandle>,
    language: Option<Arc<Language>>,
    file: Option<Arc<dyn File>>,
    edit_prediction_provider: Option<Arc<dyn EditPredictionDelegateHandle>>,
    fs: Arc<dyn Fs>,
    user_store: Entity<UserStore>,
    popover_menu_handle: PopoverMenuHandle<ContextMenu>,
    project: WeakEntity<Project>,
}

impl Render for EditPredictionButton {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Return empty div if AI is disabled
        if DisableAiSettings::get_global(cx).disable_ai {
            return div().hidden();
        }

        let language_settings = all_language_settings(None, cx);

        match language_settings.edit_predictions.provider {
            EditPredictionProvider::Copilot => {
                let Some(copilot) = EditPredictionStore::try_global(cx)
                    .and_then(|store| store.read(cx).copilot_for_project(&self.project.upgrade()?))
                else {
                    return div().hidden();
                };
                let status = copilot.read(cx).status();

                let enabled = self.editor_enabled.unwrap_or(false);

                let icon = match status {
                    Status::Error(_) => IconName::CopilotError,
                    Status::Authorized => {
                        if enabled {
                            IconName::Copilot
                        } else {
                            IconName::CopilotDisabled
                        }
                    }
                    _ => IconName::CopilotInit,
                };

                if let Status::Error(e) = status {
                    return div().child(
                        IconButton::new("copilot-error", icon)
                            .icon_size(IconSize::Small)
                            .tab_index(0isize)
                            .aria_label("GitHub Copilot")
                            .on_click(cx.listener(move |_, _, window, cx| {
                                if let Some(workspace) = Workspace::for_window(window, cx) {
                                    workspace.update(cx, |workspace, cx| {
                                        let copilot = copilot.clone();
                                        workspace.show_toast(
                                            Toast::new(
                                                NotificationId::unique::<CopilotErrorToast>(),
                                                format!(
                                                    "Copilot Edit Predictions を起動できません: {}",
                                                    e
                                                ),
                                            )
                                            .on_click(
                                                "Copilot Edit Predictions を再インストール",
                                                move |window, cx| {
                                                    copilot_ui::reinstall_and_sign_in(
                                                        copilot.clone(),
                                                        window,
                                                        cx,
                                                    )
                                                },
                                            ),
                                            cx,
                                        );
                                    });
                                }
                            }))
                            .tooltip(|_window, cx| {
                                Tooltip::for_action(
                                    "GitHub Copilot Edit Predictions",
                                    &ToggleMenu,
                                    cx,
                                )
                            }),
                    );
                }
                let this = cx.weak_entity();
                let project = self.project.clone();
                let file = self.file.clone();
                let language = self.language.clone();
                div().child(
                    PopoverMenu::new("copilot")
                        .on_open({
                            let file = file.clone();
                            let language = language;
                            let project = project.clone();
                            Rc::new(move |_window, cx| {
                                emit_edit_prediction_menu_opened(
                                    "copilot", &file, &language, &project, cx,
                                );
                            })
                        })
                        .menu(move |window, cx| {
                            let current_status = EditPredictionStore::try_global(cx)
                                .and_then(|store| {
                                    store.read(cx).copilot_for_project(&project.upgrade()?)
                                })?
                                .read(cx)
                                .status();
                            match current_status {
                                Status::Authorized => this.update(cx, |this, cx| {
                                    this.build_copilot_context_menu(window, cx)
                                }),
                                _ => this.update(cx, |this, cx| {
                                    this.build_copilot_start_menu(window, cx)
                                }),
                            }
                            .ok()
                        })
                        .anchor(Anchor::BottomRight)
                        .trigger_with_tooltip(
                            IconButton::new("copilot-icon", icon)
                                .tab_index(0isize)
                                .aria_label("GitHub Copilot"),
                            |_window, cx| {
                                Tooltip::for_action(
                                    "GitHub Copilot Edit Predictions",
                                    &ToggleMenu,
                                    cx,
                                )
                            },
                        )
                        .with_handle(self.popover_menu_handle.clone()),
                )
            }
            EditPredictionProvider::Codestral => {
                let enabled = self.editor_enabled.unwrap_or(true);
                let has_api_key = codestral::codestral_api_key(cx).is_some();
                let this = cx.weak_entity();
                let file = self.file.clone();
                let language = self.language.clone();
                let project = self.project.clone();

                let tooltip_meta = if has_api_key {
                    "Codestral を使用"
                } else {
                    "Codestral の API キーがありません"
                };

                div().child(
                    PopoverMenu::new("codestral")
                        .on_open({
                            let file = file.clone();
                            let language = language;
                            let project = project;
                            Rc::new(move |_window, cx| {
                                emit_edit_prediction_menu_opened(
                                    "codestral",
                                    &file,
                                    &language,
                                    &project,
                                    cx,
                                );
                            })
                        })
                        .menu(move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.build_codestral_context_menu(window, cx)
                            })
                            .ok()
                        })
                        .anchor(Anchor::BottomRight)
                        .trigger_with_tooltip(
                            IconButton::new("codestral-icon", IconName::AiMistral)
                                .shape(IconButtonShape::Square)
                                .tab_index(0isize)
                                .aria_label("編集予測")
                                .when(!has_api_key, |this| {
                                    this.indicator(Indicator::dot().color(Color::Error))
                                        .indicator_border_color(Some(
                                            cx.theme().colors().status_bar_background,
                                        ))
                                })
                                .when(has_api_key && !enabled, |this| {
                                    this.indicator(Indicator::dot().color(Color::Ignored))
                                        .indicator_border_color(Some(
                                            cx.theme().colors().status_bar_background,
                                        ))
                                }),
                            move |_window, cx| {
                                Tooltip::with_meta("編集予測", Some(&ToggleMenu), tooltip_meta, cx)
                            },
                        )
                        .with_handle(self.popover_menu_handle.clone()),
                )
            }
            EditPredictionProvider::OpenAiCompatibleApi => {
                let enabled = self.editor_enabled.unwrap_or(true);
                let this = cx.weak_entity();

                div().child(
                    PopoverMenu::new("openai-compatible-api")
                        .menu(move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.build_edit_prediction_context_menu(
                                    EditPredictionProvider::OpenAiCompatibleApi,
                                    window,
                                    cx,
                                )
                            })
                            .ok()
                        })
                        .anchor(Anchor::BottomRight)
                        .trigger(
                            IconButton::new("openai-compatible-api-icon", IconName::AiOpenAiCompat)
                                .shape(IconButtonShape::Square)
                                .tab_index(0isize)
                                .aria_label("編集予測")
                                .when(!enabled, |this| {
                                    this.indicator(Indicator::dot().color(Color::Ignored))
                                        .indicator_border_color(Some(
                                            cx.theme().colors().status_bar_background,
                                        ))
                                }),
                        )
                        .with_handle(self.popover_menu_handle.clone()),
                )
            }
            EditPredictionProvider::Ollama => {
                let enabled = self.editor_enabled.unwrap_or(true);
                let this = cx.weak_entity();

                div().child(
                    PopoverMenu::new("ollama")
                        .menu(move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.build_edit_prediction_context_menu(
                                    EditPredictionProvider::Ollama,
                                    window,
                                    cx,
                                )
                            })
                            .ok()
                        })
                        .anchor(Anchor::BottomRight)
                        .trigger_with_tooltip(
                            IconButton::new("ollama-icon", IconName::AiOllama)
                                .shape(IconButtonShape::Square)
                                .tab_index(0isize)
                                .aria_label("編集予測")
                                .when(!enabled, |this| {
                                    this.indicator(Indicator::dot().color(Color::Ignored))
                                        .indicator_border_color(Some(
                                            cx.theme().colors().status_bar_background,
                                        ))
                                }),
                            move |_window, cx| {
                                let settings = all_language_settings(None, cx);
                                let tooltip_meta = match settings.edit_predictions.ollama.as_ref() {
                                    Some(settings) if !settings.model.trim().is_empty() => {
                                        format!("Ollama を使用 ({})", settings.model)
                                    }
                                    _ => {
                                        "Ollama のモデルが未設定です。使用する前にモデルを設定してください。"
                                            .to_string()
                                    }
                                };

                                Tooltip::with_meta(
                                    "編集予測",
                                    Some(&ToggleMenu),
                                    tooltip_meta,
                                    cx,
                                )
                            },
                        )
                        .with_handle(self.popover_menu_handle.clone()),
                )
            }
            provider @ (EditPredictionProvider::Zed | EditPredictionProvider::Mercury) => {
                let enabled = self.editor_enabled.unwrap_or(true);
                let file = self.file.clone();
                let language = self.language.clone();
                let project = self.project.clone();
                let provider_name: &'static str = match provider {
                    EditPredictionProvider::Zed => "zed",
                    _ => "unknown",
                };
                let icons = self
                    .edit_prediction_provider
                    .as_ref()
                    .map(|p| p.icons(cx))
                    .unwrap_or_else(|| {
                        edit_prediction_types::EditPredictionIconSet::new(IconName::ZedPredict)
                    });

                let ep_icon;
                let tooltip_meta;
                let mut missing_token = false;

                match provider {
                    EditPredictionProvider::Mercury => {
                        ep_icon = if enabled { icons.base } else { icons.disabled };
                        let mercury_has_error =
                            edit_prediction::EditPredictionStore::try_global(cx).is_some_and(
                                |ep_store| ep_store.read(cx).mercury_has_payment_required_error(),
                            );
                        missing_token = edit_prediction::EditPredictionStore::try_global(cx)
                            .is_some_and(|ep_store| !ep_store.read(cx).has_mercury_api_token(cx));
                        tooltip_meta = if missing_token {
                            "Mercury の API キーがありません"
                        } else if mercury_has_error {
                            "Mercury の無料枠の上限に達しました"
                        } else {
                            "Mercury を使用"
                        };
                    }
                    _ => {
                        ep_icon = if enabled { icons.base } else { icons.disabled };
                        tooltip_meta = "Zeta を使用"
                    }
                };

                if edit_prediction::should_show_upsell_modal(cx) {
                    let tooltip_meta = if self.user_store.read(cx).current_user().is_some() {
                        "プランを選択"
                    } else {
                        "プロバイダーを設定"
                    };

                    return div().child(
                        IconButton::new("zed-predict-pending-button", ep_icon)
                            .shape(IconButtonShape::Square)
                            .tab_index(0isize)
                            .aria_label("編集予測")
                            .indicator(Indicator::dot().color(Color::Muted))
                            .indicator_border_color(Some(cx.theme().colors().status_bar_background))
                            .tooltip(move |_window, cx| {
                                Tooltip::with_meta("編集予測", None, tooltip_meta, cx)
                            })
                            .on_click(cx.listener(move |_, _, window, cx| {
                                telemetry::event!(
                                    "Pending ToS Clicked",
                                    source = "Edit Prediction Status Button"
                                );
                                window.dispatch_action(
                                    zed_actions::OpenZedPredictOnboarding.boxed_clone(),
                                    cx,
                                );
                            })),
                    );
                }

                let mut over_limit = false;

                if let Some(usage) = self
                    .edit_prediction_provider
                    .as_ref()
                    .and_then(|provider| provider.usage(cx))
                {
                    over_limit = usage.over_limit()
                }

                let show_editor_predictions = self.editor_show_predictions;
                let user = self.user_store.read(cx).current_user();

                let mercury_has_error = matches!(provider, EditPredictionProvider::Mercury)
                    && edit_prediction::EditPredictionStore::try_global(cx).is_some_and(
                        |ep_store| ep_store.read(cx).mercury_has_payment_required_error(),
                    );

                let indicator_color = if missing_token || mercury_has_error {
                    Some(Color::Error)
                } else if enabled && (!show_editor_predictions || over_limit) {
                    Some(if over_limit {
                        Color::Error
                    } else {
                        Color::Muted
                    })
                } else {
                    None
                };

                let zed_cloud_needs_sign_in =
                    matches!(provider, EditPredictionProvider::Zed) && user.is_none();
                let provider_unavailable =
                    missing_token || mercury_has_error || zed_cloud_needs_sign_in;

                let icon_button = IconButton::new("zed-predict-pending-button", ep_icon)
                    .shape(IconButtonShape::Square)
                    .tab_index(0isize)
                    .aria_label("編集予測")
                    .when_some(indicator_color, |this, color| {
                        this.indicator(Indicator::dot().color(color))
                            .indicator_border_color(Some(cx.theme().colors().status_bar_background))
                    })
                    .when(!self.popover_menu_handle.is_deployed(), |element| {
                        element.tooltip(move |_window, cx| {
                            let description = if !enabled {
                                "このファイルでは無効"
                            } else if zed_cloud_needs_sign_in {
                                "サインインまたはプロバイダーを設定"
                            } else if provider_unavailable || show_editor_predictions {
                                tooltip_meta
                            } else {
                                "使用するには有効化"
                            };

                            Tooltip::with_meta("編集予測", Some(&ToggleMenu), description, cx)
                        })
                    });

                let this = cx.weak_entity();

                let mut popover_menu = PopoverMenu::new("edit-prediction")
                    .on_open({
                        let file = file.clone();
                        let language = language;
                        let project = project;
                        Rc::new(move |_window, cx| {
                            emit_edit_prediction_menu_opened(
                                provider_name,
                                &file,
                                &language,
                                &project,
                                cx,
                            );
                        })
                    })
                    .map(|popover_menu| {
                        let this = this.clone();
                        popover_menu.menu(move |window, cx| {
                            this.update(cx, |this, cx| {
                                this.build_edit_prediction_context_menu(provider, window, cx)
                            })
                            .ok()
                        })
                    })
                    .anchor(Anchor::BottomRight)
                    .with_handle(self.popover_menu_handle.clone());

                let is_refreshing = self
                    .edit_prediction_provider
                    .as_ref()
                    .is_some_and(|provider| provider.is_refreshing(cx));

                if is_refreshing {
                    popover_menu = popover_menu.trigger(
                        icon_button.with_animation(
                            "pulsating-label",
                            Animation::new(Duration::from_secs(2))
                                .repeat()
                                .with_easing(pulsating_between(0.2, 1.0)),
                            |icon_button, delta| icon_button.alpha(delta),
                        ),
                    );
                } else {
                    popover_menu = popover_menu.trigger(icon_button);
                }

                div().child(popover_menu.into_any_element())
            }

            EditPredictionProvider::None => div().hidden(),
        }
    }
}

impl EditPredictionButton {
    pub fn new(
        fs: Arc<dyn Fs>,
        user_store: Entity<UserStore>,
        popover_menu_handle: PopoverMenuHandle<ContextMenu>,
        project: Entity<Project>,
        cx: &mut Context<Self>,
    ) -> Self {
        let copilot = EditPredictionStore::try_global(cx).and_then(|store| {
            store.update(cx, |this, cx| this.start_copilot_for_project(&project, cx))
        });
        if let Some(copilot) = copilot {
            cx.observe(&copilot, |_, _, cx| cx.notify()).detach()
        }

        cx.observe_global::<SettingsStore>(move |_, cx| cx.notify())
            .detach();

        cx.observe_global::<EditPredictionStore>(move |_, cx| cx.notify())
            .detach();

        let mercury_api_token_task = edit_prediction::mercury::load_mercury_api_token(cx);
        let open_ai_compatible_api_token_task =
            edit_prediction::open_ai_compatible::load_open_ai_compatible_api_token(cx);

        cx.spawn(async move |this, cx| {
            _ = futures::join!(mercury_api_token_task, open_ai_compatible_api_token_task);
            this.update(cx, |_, cx| {
                cx.notify();
            })
            .ok();
        })
        .detach();

        CodestralEditPredictionDelegate::ensure_api_key_loaded(cx);

        Self {
            editor_subscription: None,
            editor_enabled: None,
            editor_show_predictions: true,
            editor_focus_handle: None,
            language: None,
            file: None,
            edit_prediction_provider: None,
            user_store,
            popover_menu_handle,
            project: project.downgrade(),
            fs,
        }
    }

    fn add_provider_switching_section(
        &self,
        mut menu: ContextMenu,
        current_provider: EditPredictionProvider,
        cx: &mut App,
    ) -> ContextMenu {
        let organization_configuration = self
            .user_store
            .read(cx)
            .current_organization_configuration();

        let is_zed_provider_disabled = organization_configuration
            .is_some_and(|configuration| !configuration.edit_prediction.is_enabled);

        let available_providers = get_available_providers(cx);

        let providers: Vec<_> = available_providers
            .into_iter()
            .filter(|p| *p != EditPredictionProvider::None)
            .collect();

        if !providers.is_empty() {
            menu = menu.separator().header("プロバイダー");

            for provider in providers {
                let Some(name) = provider.display_name() else {
                    continue;
                };
                let is_current = provider == current_provider;
                let is_disabled_zed_provider =
                    provider == EditPredictionProvider::Zed && is_zed_provider_disabled;
                let fs = self.fs.clone();

                menu = menu.item(
                    ContextMenuEntry::new(name)
                        .toggleable(IconPosition::Start, is_current && !is_disabled_zed_provider)
                        .disabled(is_disabled_zed_provider)
                        .when(is_disabled_zed_provider, |item| {
                            item.documentation_aside(DocumentationSide::Left, move |_cx| {
                                Label::new("この組織では編集予測が無効になっています。")
                                    .into_any_element()
                            })
                        })
                        .handler(move |_window, cx| {
                            set_completion_provider(fs.clone(), cx, provider);
                        }),
                )
            }
        }

        menu
    }

    fn add_configure_providers_item(&self, menu: ContextMenu) -> ContextMenu {
        menu.separator().item(
            ContextMenuEntry::new("プロバイダーを設定")
                .icon(IconName::Settings)
                .icon_position(IconPosition::Start)
                .icon_color(Color::Muted)
                .handler(move |window, cx| {
                    telemetry::event!(
                        "Edit Prediction Menu Action",
                        action = "configure_providers",
                    );
                    window.dispatch_action(
                        OpenSettingsAt {
                            path: "edit_predictions.providers".to_string(),
                            target: None,
                        }
                        .boxed_clone(),
                        cx,
                    );
                }),
        )
    }

    pub fn build_copilot_start_menu(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let fs = self.fs.clone();
        let project = self.project.clone();
        ContextMenu::build(window, cx, |menu, _, cx| {
            let menu = menu
                .entry(
                    "Copilot Edit Predictions にサインイン",
                    None,
                    move |window, cx| {
                        telemetry::event!(
                            "Edit Prediction Menu Action",
                            action = "sign_in",
                            provider = "copilot",
                        );
                        // zed-jp-enhance (`docs/jp-enhance/DESIGN.md` ADR-F10): this
                        // menu is a second, unguarded path to the same sign-in
                        // that `settings_ui::pages::edit_prediction_provider_setup`
                        // gates on consent. Redirect to the "AI" page's
                        // "プロバイダーの設定" sub-page instead of starting the
                        // OAuth flow directly — that page holds both the
                        // provider picker and Copilot's consent gate.
                        if !settings::external_ai_provider_is_acknowledged(
                            settings::EDIT_PREDICTION_COPILOT_CONSENT_KEY,
                            cx,
                        ) {
                            window.dispatch_action(
                                OpenSettingsAt {
                                    path: "edit_predictions.providers".to_string(),
                                    target: None,
                                }
                                .boxed_clone(),
                                cx,
                            );
                            return;
                        }
                        if let Some(copilot) =
                            EditPredictionStore::try_global(cx).and_then(|store| {
                                store.update(cx, |this, cx| {
                                    this.start_copilot_for_project(&project.upgrade()?, cx)
                                })
                            })
                        {
                            copilot_ui::initiate_sign_in(copilot, window, cx);
                        }
                    },
                )
                .entry("Copilot Edit Predictions を無効化", None, {
                    let fs = fs.clone();
                    move |_window, cx| {
                        telemetry::event!(
                            "Edit Prediction Menu Action",
                            action = "disable_provider",
                            provider = "copilot",
                        );
                        hide_copilot(fs.clone(), cx)
                    }
                });

            let menu =
                self.add_provider_switching_section(menu, EditPredictionProvider::Copilot, cx);
            let menu = self.add_configure_providers_item(menu);
            menu
        })
    }

    pub fn build_language_settings_menu(
        &self,
        mut menu: ContextMenu,
        window: &Window,
        cx: &mut App,
    ) -> ContextMenu {
        let fs = self.fs.clone();
        let line_height = window.line_height();

        menu = menu.header("編集予測を表示する対象");

        let language_state = self.language.as_ref().map(|language| {
            (
                language.clone(),
                LanguageSettings::resolve(None, Some(&language.name()), cx).show_edit_predictions,
            )
        });

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            let entry = ContextMenuEntry::new("このバッファー")
                .toggleable(IconPosition::Start, self.editor_show_predictions)
                .action(Box::new(editor::actions::ToggleEditPrediction))
                .handler(move |window, cx| {
                    editor_focus_handle.dispatch_action(
                        &editor::actions::ToggleEditPrediction,
                        window,
                        cx,
                    );
                });

            match language_state.clone() {
                Some((language, false)) => {
                    menu = menu.item(entry.disabled(true).documentation_aside(
                        DocumentationSide::Left,
                        move |_cx| {
                            Label::new(format!(
                                "{} では編集予測が無効になっています",
                                language.name()
                            ))
                            .into_any_element()
                        },
                    ));
                }
                Some(_) | None => menu = menu.item(entry),
            }
        }

        if let Some((language, language_enabled)) = language_state {
            let fs = fs.clone();
            let language_name = language.name();

            menu = menu.toggleable_entry(
                language_name.clone(),
                language_enabled,
                IconPosition::Start,
                None,
                move |_, cx| {
                    telemetry::event!(
                        "Edit Prediction Setting Changed",
                        setting = "language",
                        language = language_name.to_string(),
                        enabled = !language_enabled,
                    );
                    toggle_show_edit_predictions_for_language(language.clone(), fs.clone(), cx)
                },
            );
        }

        let settings = AllLanguageSettings::get_global(cx);

        let globally_enabled = settings.show_edit_predictions(None, cx);
        let entry = ContextMenuEntry::new("すべてのファイル")
            .toggleable(IconPosition::Start, globally_enabled)
            .action(workspace::ToggleEditPrediction.boxed_clone())
            .handler(|window, cx| {
                window.dispatch_action(workspace::ToggleEditPrediction.boxed_clone(), cx)
            });
        menu = menu.item(entry);

        let provider = settings.edit_predictions.provider;
        let current_mode = settings.edit_predictions_mode();
        let subtle_mode = matches!(current_mode, EditPredictionsMode::Subtle);
        let eager_mode = matches!(current_mode, EditPredictionsMode::Eager);

        menu = menu
                .separator()
                .header("表示モード")
                .item(
                    ContextMenuEntry::new("Eager")
                        .toggleable(IconPosition::Start, eager_mode)
                        .documentation_aside(DocumentationSide::Left, move |_| {
                            Label::new("言語サーバーの補完候補がないときに、予測をインラインで表示します。").into_any_element()
                        })
                        .handler({
                            let fs = fs.clone();
                            move |_, cx| {
                                telemetry::event!(
                                    "Edit Prediction Setting Changed",
                                    setting = "mode",
                                    value = "eager",
                                );
                                toggle_edit_prediction_mode(fs.clone(), EditPredictionsMode::Eager, cx)
                            }
                        }),
                )
                .item(
                    ContextMenuEntry::new("Subtle")
                        .toggleable(IconPosition::Start, subtle_mode)
                        .documentation_aside(DocumentationSide::Left, move |_| {
                            Label::new(i18n::t!(
                                "Display predictions inline only when holding a modifier key ({} by default).",
                                ui::alt_key_name!()
                            ))
                            .into_any_element()
                        })
                        .handler({
                            let fs = fs.clone();
                            move |_, cx| {
                                telemetry::event!(
                                    "Edit Prediction Setting Changed",
                                    setting = "mode",
                                    value = "subtle",
                                );
                                toggle_edit_prediction_mode(fs.clone(), EditPredictionsMode::Subtle, cx)
                            }
                        }),
                );

        menu = menu.separator().header("プライバシー");

        if matches!(provider, EditPredictionProvider::Zed) {
            if let Some(provider) = &self.edit_prediction_provider {
                let data_collection = provider.data_collection_state(cx);

                if data_collection.is_supported() {
                    let provider = provider.clone();
                    let enabled = data_collection.is_enabled();
                    let is_open_source = data_collection.is_project_open_source();
                    let is_collecting = data_collection.is_enabled();
                    let (icon_name, icon_color) = if is_open_source && is_collecting {
                        (IconName::Check, Color::Success)
                    } else {
                        (IconName::Check, Color::Accent)
                    };

                    menu = menu.item(
                        ContextMenuEntry::new("学習データの収集")
                            .toggleable(IconPosition::Start, data_collection.is_enabled())
                            .icon(icon_name)
                            .icon_color(icon_color)
                            .disabled(!provider.can_toggle_data_collection(cx))
                            .documentation_aside(DocumentationSide::Left, move |cx| {
                                let (msg, label_color, icon_name, icon_color) = match (is_open_source, is_collecting) {
                                    (true, true) => (
                                        "プロジェクトはオープンソースと判定され、データを共有しています。",
                                        Color::Default,
                                        IconName::Check,
                                        Color::Success,
                                    ),
                                    (true, false) => (
                                        "プロジェクトはオープンソースと判定されましたが、データは共有していません。",
                                        Color::Muted,
                                        IconName::Close,
                                        Color::Muted,
                                    ),
                                    (false, true) => (
                                        "プロジェクトはオープンソースと判定されていません。データは収集されません。",
                                        Color::Muted,
                                        IconName::Close,
                                        Color::Muted,
                                    ),
                                    (false, false) => (
                                        "プロジェクトはオープンソースと判定されておらず、設定も無効です。",
                                        Color::Muted,
                                        IconName::Close,
                                        Color::Muted,
                                    ),
                                };
                                v_flex()
                                    .gap_2()
                                    .child(
                                        Label::new(indoc!{
                                            "オープンソースリポジトリのデータを共有して、オープンデータセットモデルの改善にご協力ください。\
                                            この設定を有効にするには、リポジトリ内のライセンスファイルを Zed が検出できる必要があります。\
                                            機微なデータやシークレットを含むファイルは既定で除外されます。"
                                        })
                                    )
                                    .child(
                                        h_flex()
                                            .items_start()
                                            .pt_2()
                                            .pr_1()
                                            .flex_1()
                                            .gap_1p5()
                                            .border_t_1()
                                            .border_color(cx.theme().colors().border_variant)
                                            .child(h_flex().flex_shrink_0().h(line_height).child(Icon::new(icon_name).size(IconSize::XSmall).color(icon_color)))
                                            .child(div().child(msg).w_full().text_sm().text_color(label_color.color(cx)))
                                    )
                                    .into_any_element()
                            })
                            .handler(move |_, cx| {
                                provider.toggle_data_collection(cx);

                                if !enabled {
                                    telemetry::event!(
                                        "Data Collection Enabled",
                                        source = "Edit Prediction Status Menu"
                                    );
                                } else {
                                    telemetry::event!(
                                        "Data Collection Disabled",
                                        source = "Edit Prediction Status Menu"
                                    );
                                }
                            })
                    );

                    if is_collecting && !is_open_source {
                        menu = menu.item(
                            ContextMenuEntry::new("データは収集されません。")
                                .disabled(true)
                                .icon(IconName::Close)
                                .icon_color(Color::Error)
                                .icon_size(IconSize::Small),
                        );
                    }
                }
            }
        }

        menu = menu
            .item(
                ContextMenuEntry::new("除外ファイルを設定")
                    .icon(IconName::Lock)
                    .icon_color(Color::Muted)
                    .documentation_aside(DocumentationSide::Left, |_| {
                        Label::new(indoc! {"
                        Zed が編集を予測しない機微なパスを追加するために、設定を開きます。"})
                        .into_any_element()
                    })
                    .handler(move |window, cx| {
                        telemetry::event!(
                            "Edit Prediction Menu Action",
                            action = "configure_excluded_files",
                        );
                        if let Some(workspace) = Workspace::for_window(window, cx) {
                            let workspace = workspace.downgrade();
                            window
                                .spawn(cx, async |cx| {
                                    open_disabled_globs_setting_in_editor(workspace, cx).await
                                })
                                .detach_and_log_err(cx);
                        }
                    }),
            )
            .item(
                ContextMenuEntry::new("ドキュメントを表示")
                    .icon(IconName::FileGeneric)
                    .icon_color(Color::Muted)
                    .handler(move |_, cx| {
                        telemetry::event!("Edit Prediction Menu Action", action = "view_docs",);
                        cx.open_url(PRIVACY_DOCS);
                    }),
            );

        if !self.editor_enabled.unwrap_or(true) {
            let icons = self
                .edit_prediction_provider
                .as_ref()
                .map(|p| p.icons(cx))
                .unwrap_or_else(|| {
                    edit_prediction_types::EditPredictionIconSet::new(IconName::ZedPredict)
                });
            menu = menu.item(
                ContextMenuEntry::new("このファイルは除外されています。")
                    .disabled(true)
                    .icon(icons.disabled)
                    .icon_size(IconSize::Small),
            );
        }

        if let Some(editor_focus_handle) = self.editor_focus_handle.clone() {
            menu = menu
                .separator()
                .header("アクション")
                .entry(
                    "カーソル位置の編集を予測",
                    Some(Box::new(ShowEditPrediction)),
                    {
                        let editor_focus_handle = editor_focus_handle.clone();
                        move |window, cx| {
                            telemetry::event!(
                                "Edit Prediction Menu Action",
                                action = "predict_at_cursor",
                            );
                            editor_focus_handle.dispatch_action(&ShowEditPrediction, window, cx);
                        }
                    },
                )
                .context(editor_focus_handle)
                .when(
                    cx.has_flag::<PredictEditsRatePredictionsFeatureFlag>(),
                    |this| this.action("予測を評価", RatePredictions.boxed_clone()),
                );
        }

        menu
    }

    fn build_copilot_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        let all_language_settings = all_language_settings(None, cx);
        let next_edit_suggestions = all_language_settings
            .edit_predictions
            .copilot
            .enable_next_edit_suggestions
            .unwrap_or(true);
        let copilot_config = copilot_chat::CopilotChatConfiguration {
            enterprise_uri: all_language_settings
                .edit_predictions
                .copilot
                .enterprise_uri
                .clone(),
        };
        let settings_url = copilot_settings_url(copilot_config.enterprise_uri.as_deref());

        ContextMenu::build(window, cx, |menu, window, cx| {
            let menu = self.build_language_settings_menu(menu, window, cx);
            let menu =
                self.add_provider_switching_section(menu, EditPredictionProvider::Copilot, cx);

            let menu = self.add_configure_providers_item(menu);
            let menu = menu
                .separator()
                .item(
                    ContextMenuEntry::new("Copilot: 次の編集候補")
                        .toggleable(IconPosition::Start, next_edit_suggestions)
                        .handler({
                            let fs = self.fs.clone();
                            move |_, cx| {
                                update_settings_file(fs.clone(), cx, move |settings, _| {
                                    settings
                                        .project
                                        .all_languages
                                        .edit_predictions
                                        .get_or_insert_default()
                                        .copilot
                                        .get_or_insert_default()
                                        .enable_next_edit_suggestions =
                                        Some(!next_edit_suggestions);
                                });
                            }
                        }),
                )
                .separator()
                .link(
                    "Copilot の設定を開く",
                    OpenBrowser { url: settings_url }.boxed_clone(),
                )
                .entry("サインアウト", None, |window, cx| {
                    if let Some(auth) = copilot::GlobalCopilotAuth::try_global(cx) {
                        copilot_ui::initiate_sign_out(auth.0.clone(), window, cx);
                    }
                });
            menu
        })
    }

    fn build_codestral_context_menu(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, |menu, window, cx| {
            let menu = self.build_language_settings_menu(menu, window, cx);
            let menu =
                self.add_provider_switching_section(menu, EditPredictionProvider::Codestral, cx);

            let menu = self.add_configure_providers_item(menu);
            menu
        })
    }

    fn build_edit_prediction_context_menu(
        &self,
        provider: EditPredictionProvider,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<ContextMenu> {
        ContextMenu::build(window, cx, |mut menu, window, cx| {
            let user = self.user_store.read(cx).current_user();

            let needs_sign_in = user.is_none()
                && matches!(
                    provider,
                    EditPredictionProvider::None | EditPredictionProvider::Zed
                );

            if needs_sign_in {
                menu = menu
                    .custom_row(move |_window, cx| {
                        let description = indoc! {
                            "オープンソース・オープンデータのモデル Zeta により、\
                            キー入力のたびに採用される候補を無料で 2,000 件利用できます"
                        };

                        v_flex()
                            .max_w_64()
                            .h(rems_from_px(148_f32))
                            .child(render_zeta_tab_animation(cx))
                            .child(Label::new("編集予測"))
                            .child(
                                Label::new(description)
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            )
                            .into_any_element()
                    })
                    .separator()
                    .entry(
                        "サインインして使い始める",
                        None,
                        |window, cx| {
                            telemetry::event!(
                                "Edit Prediction Menu Action",
                                action = "sign_in",
                                provider = "zed",
                            );
                            // zed-jp-enhance (`docs/jp-enhance/DESIGN.md` ADR-F10): Zed
                            // Cloud is an external AI service. Redirect to the
                            // "AI" page's "プロバイダーの設定" sub-page instead
                            // of signing in directly — the same destination the
                            // Copilot entry above uses. Its "Zed" card holds the
                            // consent block for the `zed.dev` key (shared with
                            // the chat provider of the same id), which is what
                            // makes `Zed` selectable at all.

                            if !settings::external_ai_provider_is_acknowledged(
                                settings::ZED_CLOUD_AI_CONSENT_KEY,
                                cx,
                            ) {
                                window.dispatch_action(
                                    OpenSettingsAt {
                                        path: "edit_predictions.providers".to_string(),
                                        target: None,
                                    }
                                    .boxed_clone(),
                                    cx,
                                );
                                return;
                            }
                            let client = Client::global(cx);
                            window
                                .spawn(cx, async move |cx| {
                                    client
                                        .sign_in_with_optional_connect(true, &cx)
                                        .await
                                        .log_err();
                                })
                                .detach();
                        },
                    )
                    .link_with_handler(
                        "詳細情報",
                        OpenBrowser {
                            url: zed_urls::edit_prediction_docs(cx).into(),
                        }
                        .boxed_clone(),
                        |_window, _cx| {
                            telemetry::event!(
                                "Edit Prediction Menu Action",
                                action = "view_docs",
                                source = "upsell",
                            );
                        },
                    )
                    .separator();
            } else {
                let mercury_payment_required = matches!(provider, EditPredictionProvider::Mercury)
                    && edit_prediction::EditPredictionStore::try_global(cx).is_some_and(
                        |ep_store| ep_store.read(cx).mercury_has_payment_required_error(),
                    );

                if mercury_payment_required {
                    menu = menu
                        .header("Mercury")
                        .item(ContextMenuEntry::new("無料枠の上限に達しました").disabled(true))
                        .item(
                            ContextMenuEntry::new(
                                "サービスを継続して利用するには有料プランにアップグレードしてください",
                            )
                            .disabled(true),
                        )
                        .separator();
                }

                if let Some(usage) = self
                    .edit_prediction_provider
                    .as_ref()
                    .and_then(|provider| provider.usage(cx))
                {
                    menu = menu.header("使用状況");
                    menu = menu
                        .custom_entry(
                            move |_window, cx| {
                                let used_percentage = match usage.limit {
                                    UsageLimit::Limited(limit) => {
                                        Some((usage.amount as f32 / limit as f32) * 100.)
                                    }
                                    UsageLimit::Unlimited => None,
                                };

                                h_flex()
                                    .flex_1()
                                    .gap_1p5()
                                    .children(used_percentage.map(|percent| {
                                        ProgressBar::new("usage", percent, 100., cx)
                                    }))
                                    .child(
                                        Label::new(match usage.limit {
                                            UsageLimit::Limited(limit) => {
                                                format!("{} / {limit}", usage.amount)
                                            }
                                            UsageLimit::Unlimited => {
                                                format!("{} / ∞", usage.amount)
                                            }
                                        })
                                        .size(LabelSize::Small)
                                        .color(Color::Muted),
                                    )
                                    .into_any_element()
                            },
                            move |_, cx| cx.open_url(&zed_urls::account_url(cx)),
                        )
                        .when(usage.over_limit(), |menu| -> ContextMenu {
                            menu.entry(
                                "上限を引き上げるにはサブスクリプションを購入",
                                None,
                                |_window, cx| {
                                    telemetry::event!(
                                        "Edit Prediction Menu Action",
                                        action = "upsell_clicked",
                                        reason = "usage_limit",
                                    );
                                    cx.open_url(&zed_urls::account_url(cx))
                                },
                            )
                        })
                        .separator();
                } else if self.user_store.read(cx).account_too_young() {
                    menu = menu
                        .custom_entry(
                            |_window, _cx| {
                                Label::new("GitHub アカウントの作成から 30 日が経過していません。")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning)
                                    .into_any_element()
                            },
                            |_window, cx| cx.open_url(&zed_urls::account_url(cx)),
                        )
                        .entry(
                            "Zed Pro にアップグレードするか、お問い合わせください。",
                            None,
                            |_window, cx| {
                                telemetry::event!(
                                    "Edit Prediction Menu Action",
                                    action = "upsell_clicked",
                                    reason = "account_age",
                                );
                                cx.open_url(&zed_urls::account_url(cx))
                            },
                        )
                        .separator();
                } else if self.user_store.read(cx).has_overdue_invoices() {
                    menu = menu
                        .custom_entry(
                            |_window, _cx| {
                                Label::new("未払いの請求書があります")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning)
                                    .into_any_element()
                            },
                            |_window, cx| {
                                cx.open_url(&zed_urls::account_url(cx))
                            },
                        )
                        .entry(
                            "この機能を継続して利用するには、支払い状況を確認するか billing-support@zed.dev までご連絡ください。",
                            None,
                            |_window, cx| {
                                cx.open_url(&zed_urls::account_url(cx))
                            },
                        )
                        .separator();
                }
            }

            if !needs_sign_in {
                menu = self.build_language_settings_menu(menu, window, cx);
            }
            menu = self.add_provider_switching_section(menu, provider, cx);

            if cx.is_staff() {
                if let Some(store) = EditPredictionStore::try_global(cx) {
                    store.update(cx, |store, cx| {
                        store.refresh_available_experiments(cx);
                    });
                    let store = store.read(cx);
                    let experiments = store.available_experiments().to_vec();
                    let preferred = store.preferred_experiment().map(|s| s.to_owned());
                    let active = store.active_experiment().map(|s| s.to_owned());

                    let preferred_for_submenu = preferred.clone();
                    menu = menu
                        .separator()
                        .submenu("Experiment", move |menu, _window, _cx| {
                            let mut menu = menu.toggleable_entry(
                                "Default",
                                preferred_for_submenu.is_none(),
                                IconPosition::Start,
                                None,
                                {
                                    move |_window, cx| {
                                        if let Some(store) = EditPredictionStore::try_global(cx) {
                                            store.update(cx, |store, _cx| {
                                                store.set_preferred_experiment(None);
                                            });
                                        }
                                    }
                                },
                            );
                            for experiment in &experiments {
                                let is_selected = active.as_deref() == Some(experiment.as_str())
                                    || preferred.as_deref() == Some(experiment.as_str());
                                let experiment_name = experiment.clone();
                                menu = menu.toggleable_entry(
                                    experiment.clone(),
                                    is_selected,
                                    IconPosition::Start,
                                    None,
                                    move |_window, cx| {
                                        if let Some(store) = EditPredictionStore::try_global(cx) {
                                            store.update(cx, |store, _cx| {
                                                store.set_preferred_experiment(Some(
                                                    experiment_name.clone(),
                                                ));
                                            });
                                        }
                                    },
                                );
                            }
                            menu
                        });
                }
            }

            let menu = self.add_configure_providers_item(menu);
            menu
        })
    }

    pub fn update_enabled(&mut self, editor: Entity<Editor>, cx: &mut Context<Self>) {
        let editor = editor.read(cx);
        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let suggestion_anchor = editor.selections.newest_anchor().start;
        let language = snapshot.language_at(suggestion_anchor);
        let file = snapshot.file_at(suggestion_anchor).cloned();
        self.editor_enabled = {
            let file = file.as_ref();
            Some(
                file.map(|file| {
                    all_language_settings(Some(file), cx)
                        .edit_predictions_enabled_for_file(file, cx)
                })
                .unwrap_or(true),
            )
        };
        self.editor_show_predictions = editor.edit_predictions_enabled();
        self.edit_prediction_provider = editor.edit_prediction_provider();
        self.language = language.cloned();
        self.file = file;
        self.editor_focus_handle = Some(editor.focus_handle(cx));

        cx.notify();
    }
}

impl StatusItemView for EditPredictionButton {
    fn set_active_pane_item(
        &mut self,
        item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = item.and_then(|item| item.act_as::<Editor>(cx)) {
            self.editor_subscription = Some((
                cx.observe(&editor, Self::update_enabled),
                editor.entity_id().as_u64() as usize,
            ));
            self.update_enabled(editor, cx);
        } else {
            self.language = None;
            self.editor_subscription = None;
            self.editor_enabled = None;
        }
        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // This button is already gated on having a non-disabled edit
        // prediction provider, which the user manages through provider/AI
        // settings.
        None
    }
}

async fn open_disabled_globs_setting_in_editor(
    workspace: WeakEntity<Workspace>,
    cx: &mut AsyncWindowContext,
) -> Result<()> {
    let settings_editor = workspace
        .update_in(cx, |_, window, cx| {
            create_and_open_local_file(paths::settings_file(), window, cx, || {
                settings::initial_user_settings_content().as_ref().into()
            })
        })?
        .await?
        .downcast::<Editor>()
        .unwrap();

    settings_editor
        .downgrade()
        .update_in(cx, |item, window, cx| {
            let text = item.buffer().read(cx).snapshot(cx).text();

            let settings = cx.global::<SettingsStore>();

            // Ensure that we always have "edit_predictions { "disabled_globs": [] }"
            let Some(edits) = settings
                .edits_for_update(&text, |file| {
                    file.project
                        .all_languages
                        .edit_predictions
                        .get_or_insert_with(Default::default)
                        .disabled_globs
                        .get_or_insert_with(Vec::new);
                })
                .log_err()
            else {
                return;
            };

            if !edits.is_empty() {
                item.edit(
                    edits
                        .into_iter()
                        .map(|(r, s)| (MultiBufferOffset(r.start)..MultiBufferOffset(r.end), s)),
                    cx,
                );
            }

            let text = item.buffer().read(cx).snapshot(cx).text();

            static DISABLED_GLOBS_REGEX: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r#""disabled_globs":\s*\[\s*(?P<content>(?:.|\n)*?)\s*\]"#).unwrap()
            });
            // Only capture [...]
            let range = DISABLED_GLOBS_REGEX.captures(&text).and_then(|captures| {
                captures
                    .name("content")
                    .map(|inner_match| inner_match.start()..inner_match.end())
            });
            if let Some(range) = range {
                let range = MultiBufferOffset(range.start)..MultiBufferOffset(range.end);
                item.change_selections(
                    SelectionEffects::scroll(Autoscroll::newest()),
                    window,
                    cx,
                    |selections| {
                        selections.select_ranges(vec![range]);
                    },
                );
            }
        })?;

    anyhow::Ok(())
}

pub fn set_completion_provider(fs: Arc<dyn Fs>, cx: &mut App, provider: EditPredictionProvider) {
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .all_languages
            .edit_predictions
            .get_or_insert_default()
            .provider = Some(provider);
    });
}

/// The providers offered in every provider picker, in display order.
///
/// zed-jp-enhance (`docs/jp-enhance/DESIGN.md` ADR-F10): self-hosted providers come
/// first and `Zed` comes last. An OpenAI-compatible endpoint pointed at a local
/// server is what this fork expects people to use, and `Zed` — which needs no
/// local setup and sends code to a cloud service — should not be what the picker
/// offers first.
///
/// Every entry has passed its own gate: the others need credentials or an
/// endpoint, and `Zed`, which has neither, needs its external-AI consent, which
/// is granted on the "Zed" card of the "プロバイダーの設定" sub-page. So a
/// picker can apply any provider it lists without a further prompt.
pub fn get_available_providers(cx: &mut App) -> Vec<EditPredictionProvider> {
    let mut providers = self_hosted_providers(cx);

    let app_state = workspace::AppState::global(cx);
    if copilot::GlobalCopilotAuth::try_get_or_init(app_state, cx)
        .is_some_and(|copilot| copilot.0.read(cx).is_authenticated())
    {
        providers.push(EditPredictionProvider::Copilot);
    };

    if codestral::codestral_api_key(cx).is_some() {
        providers.push(EditPredictionProvider::Codestral);
    }

    if edit_prediction::mercury::mercury_api_token(cx)
        .read(cx)
        .has_key()
    {
        providers.push(EditPredictionProvider::Mercury);
    }

    if settings::external_ai_provider_is_acknowledged(settings::ZED_CLOUD_AI_CONSENT_KEY, cx) {
        providers.push(EditPredictionProvider::Zed);
    }

    providers
}

/// The self-hosted providers, which have no credentials to gate them.
fn self_hosted_providers(cx: &App) -> Vec<EditPredictionProvider> {
    let mut providers = Vec::new();

    if all_language_settings(None, cx)
        .edit_predictions
        .open_ai_compatible_api
        .as_ref()
        .is_some_and(|settings| {
            endpoint_is_local_or_acknowledged("open_ai_compatible_api", &settings.api_url, cx)
        })
    {
        providers.push(EditPredictionProvider::OpenAiCompatibleApi);
    }

    // The fork's default settings name an Ollama model, so these settings are always present;
    // only the endpoint decides whether the provider has passed its gate.
    if all_language_settings(None, cx)
        .edit_predictions
        .ollama
        .as_ref()
        .is_some_and(|settings| endpoint_is_local_or_acknowledged("ollama", &settings.api_url, cx))
    {
        providers.push(EditPredictionProvider::Ollama);
    }

    providers
}

/// A self-hosted provider needs no consent while its endpoint is on this machine; pointed anywhere
/// else it needs the consent its card on the "プロバイダーの設定" sub-page asks for (ADR-F10),
/// under the same key.
fn endpoint_is_local_or_acknowledged(provider: &str, api_url: &str, cx: &App) -> bool {
    let is_loopback = http_client::Url::parse(api_url.trim())
        .ok()
        .and_then(|url| url.host_str().and_then(http_client::RequestHost::parse))
        .is_some_and(|host| host.is_loopback());
    is_loopback
        || settings::external_ai_provider_is_acknowledged(
            &format!("edit_prediction:{provider}"),
            cx,
        )
}

fn toggle_show_edit_predictions_for_language(
    language: Arc<Language>,
    fs: Arc<dyn Fs>,
    cx: &mut App,
) {
    let show_edit_predictions =
        all_language_settings(None, cx).show_edit_predictions(Some(&language), cx);
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .all_languages
            .languages
            .0
            .entry(language.name().0.to_string())
            .or_default()
            .show_edit_predictions = Some(!show_edit_predictions);
    });
}

fn hide_copilot(fs: Arc<dyn Fs>, cx: &mut App) {
    update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .all_languages
            .edit_predictions
            .get_or_insert(Default::default())
            .provider = Some(EditPredictionProvider::None);
    });
}

fn toggle_edit_prediction_mode(fs: Arc<dyn Fs>, mode: EditPredictionsMode, cx: &mut App) {
    let settings = AllLanguageSettings::get_global(cx);
    let current_mode = settings.edit_predictions_mode();

    if current_mode != mode {
        update_settings_file(fs, cx, move |settings, _cx| {
            if let Some(edit_predictions) = settings.project.all_languages.edit_predictions.as_mut()
            {
                edit_predictions.mode = Some(mode);
            } else {
                settings.project.all_languages.edit_predictions =
                    Some(settings::EditPredictionSettingsContent {
                        mode: Some(mode),
                        ..Default::default()
                    });
            }
        });
    }
}

fn render_zeta_tab_animation(cx: &App) -> impl IntoElement {
    let tab = |n: u64, inverted: bool| {
        let text_color = cx.theme().colors().text;

        h_flex().child(
            h_flex()
                .text_size(TextSize::XSmall.rems(cx))
                .text_color(text_color)
                .child("tab")
                .with_animation(
                    ElementId::Integer(n),
                    Animation::new(Duration::from_secs(3)).repeat(),
                    move |tab, delta| {
                        let n_f32 = n as f32;

                        let offset = if inverted {
                            0.2 * (4.0 - n_f32)
                        } else {
                            0.2 * n_f32
                        };

                        let phase = (delta - offset + 1.0) % 1.0;
                        let pulse = if phase < 0.6 {
                            let t = phase / 0.6;
                            1.0 - (0.5 - t).abs() * 2.0
                        } else {
                            0.0
                        };

                        let eased = ease_in_out(pulse);
                        let opacity = 0.1 + 0.5 * eased;

                        tab.text_color(text_color.opacity(opacity))
                    },
                ),
        )
    };

    let tab_sequence = |inverted: bool| {
        h_flex()
            .gap_1()
            .child(tab(0, inverted))
            .child(tab(1, inverted))
            .child(tab(2, inverted))
            .child(tab(3, inverted))
            .child(tab(4, inverted))
    };

    h_flex()
        .my_1p5()
        .p_4()
        .justify_center()
        .gap_2()
        .rounded_xs()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border)
        .bg(gpui::pattern_slash(
            cx.theme().colors().border.opacity(0.5),
            1.,
            8.,
        ))
        .child(tab_sequence(true))
        .child(Icon::new(IconName::ZedPredict))
        .child(tab_sequence(false))
}

fn emit_edit_prediction_menu_opened(
    provider: &str,
    file: &Option<Arc<dyn File>>,
    language: &Option<Arc<Language>>,
    project: &WeakEntity<Project>,
    cx: &App,
) {
    let language_name = language.as_ref().map(|l| l.name());
    let edit_predictions_enabled_for_language =
        LanguageSettings::resolve(None, language_name.as_ref(), cx).show_edit_predictions;
    let file_extension = file
        .as_ref()
        .and_then(|f| {
            std::path::Path::new(f.file_name(cx))
                .extension()
                .and_then(|e| e.to_str())
        })
        .map(|s| s.to_string());
    let is_via_ssh = project
        .upgrade()
        .map(|p| p.read(cx).is_via_remote_server())
        .unwrap_or(false);
    telemetry::event!(
        "Toolbar Menu Opened",
        name = "Edit Predictions",
        provider,
        file_extension,
        edit_predictions_enabled_for_language,
        is_via_ssh,
    );
}

fn copilot_settings_url(enterprise_uri: Option<&str>) -> Arc<str> {
    match enterprise_uri {
        Some(uri) => format!("{}{}", uri.trim_end_matches('/'), COPILOT_SETTINGS_PATH).into(),
        None => COPILOT_SETTINGS_URL.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn test_copilot_settings_url_with_enterprise_uri(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        cx.update_global(|settings_store: &mut SettingsStore, cx| {
            settings_store
                .set_user_settings(
                    r#"{"edit_predictions":{"copilot":{"enterprise_uri":"https://my-company.ghe.com"}}}"#,
                    cx,
                )
                .unwrap();
        });

        let url = cx.update(|cx| {
            let all_language_settings = all_language_settings(None, cx);
            copilot_settings_url(
                all_language_settings
                    .edit_predictions
                    .copilot
                    .enterprise_uri
                    .as_deref(),
            )
        });

        assert_eq!(url.as_ref(), "https://my-company.ghe.com/settings/copilot");
    }

    #[gpui::test]
    async fn test_copilot_settings_url_with_enterprise_uri_trailing_slash(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        cx.update_global(|settings_store: &mut SettingsStore, cx| {
            settings_store
                .set_user_settings(
                    r#"{"edit_predictions":{"copilot":{"enterprise_uri":"https://my-company.ghe.com/"}}}"#,
                    cx,
                )
                .unwrap();
        });

        let url = cx.update(|cx| {
            let all_language_settings = all_language_settings(None, cx);
            copilot_settings_url(
                all_language_settings
                    .edit_predictions
                    .copilot
                    .enterprise_uri
                    .as_deref(),
            )
        });

        assert_eq!(url.as_ref(), "https://my-company.ghe.com/settings/copilot");
    }

    #[gpui::test]
    async fn test_copilot_settings_url_without_enterprise_uri(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });

        let url = cx.update(|cx| {
            let all_language_settings = all_language_settings(None, cx);
            copilot_settings_url(
                all_language_settings
                    .edit_predictions
                    .copilot
                    .enterprise_uri
                    .as_deref(),
            )
        });

        assert_eq!(url.as_ref(), "https://github.com/settings/copilot");
    }

    #[gpui::test]
    async fn test_a_remote_self_hosted_endpoint_is_listed_only_after_consent(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
        let set_user_settings = |json: &str, cx: &mut TestAppContext| {
            cx.update_global(|settings_store: &mut SettingsStore, cx| {
                settings_store.set_user_settings(json, cx).unwrap();
            });
        };

        set_user_settings(
            r#"{"edit_predictions":{"ollama":{"model":"m","api_url":"http://localhost:11434"}}}"#,
            cx,
        );
        assert_eq!(
            cx.update(|cx| self_hosted_providers(cx)),
            vec![EditPredictionProvider::Ollama]
        );

        set_user_settings(
            r#"{
                "edit_predictions": {
                    "ollama": { "model": "m", "api_url": "https://llm.example.com" },
                    "open_ai_compatible_api": { "model": "m", "api_url": "https://llm.example.com/v1" }
                }
            }"#,
            cx,
        );
        assert_eq!(cx.update(|cx| self_hosted_providers(cx)), vec![]);

        set_user_settings(
            r#"{
                "acknowledged_external_ai_providers": ["edit_prediction:ollama", "edit_prediction:open_ai_compatible_api"],
                "edit_predictions": {
                    "ollama": { "model": "m", "api_url": "https://llm.example.com" },
                    "open_ai_compatible_api": { "model": "m", "api_url": "https://llm.example.com/v1" }
                }
            }"#,
            cx,
        );
        assert_eq!(
            cx.update(|cx| self_hosted_providers(cx)),
            vec![
                EditPredictionProvider::OpenAiCompatibleApi,
                EditPredictionProvider::Ollama
            ]
        );

        // Consent to the chat provider of the same name is not consent to edit prediction.
        set_user_settings(
            r#"{
                "acknowledged_external_ai_providers": ["ollama"],
                "edit_predictions": { "ollama": { "model": "m", "api_url": "https://llm.example.com" } }
            }"#,
            cx,
        );
        assert_eq!(cx.update(|cx| self_hosted_providers(cx)), vec![]);
    }
}
