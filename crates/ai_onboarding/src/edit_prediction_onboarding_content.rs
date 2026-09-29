use std::sync::Arc;

use client::{Client, UserStore};
use cloud_api_types::Plan;
use gpui::{Action, Entity, IntoElement, ParentElement};
use ui::prelude::*;

use crate::ZedAiOnboarding;

pub struct EditPredictionOnboarding {
    user_store: Entity<UserStore>,
    client: Arc<Client>,
    copilot_is_configured: bool,
    continue_with_zed_ai: Arc<dyn Fn(&mut Window, &mut App)>,
    continue_with_copilot: Arc<dyn Fn(&mut Window, &mut App)>,
}

impl EditPredictionOnboarding {
    pub fn new(
        user_store: Entity<UserStore>,
        client: Arc<Client>,
        copilot_is_configured: bool,
        continue_with_zed_ai: Arc<dyn Fn(&mut Window, &mut App)>,
        continue_with_copilot: Arc<dyn Fn(&mut Window, &mut App)>,
        _cx: &mut Context<Self>,
    ) -> Self {
        Self {
            user_store,
            copilot_is_configured,
            client,
            continue_with_zed_ai,
            continue_with_copilot,
        }
    }
}

impl Render for EditPredictionOnboarding {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let is_free_plan = self
            .user_store
            .read(cx)
            .plan()
            .is_some_and(|plan| plan == Plan::ZedFree);

        // zed-jp-enhance (`docs/jp-enhance/DESIGN.md` ADR-F10): GitHub Copilot is an
        // external AI service, and this button would otherwise kick off its
        // sign-in flow directly, bypassing the consent gate that
        // `settings_ui::pages::edit_prediction_provider_setup` enforces for the
        // same provider. `copilot_is_configured` only reflects whether the
        // shared Copilot OAuth token exists, not whether this surface's warning
        // was acknowledged, so it cannot substitute for the check.
        let copilot_needs_consent = !settings::external_ai_provider_is_acknowledged(
            settings::EDIT_PREDICTION_COPILOT_CONSENT_KEY,
            cx,
        );

        let github_copilot = if copilot_needs_consent {
            v_flex()
                .gap_1()
                .child(
                    Label::new(
                        i18n::t!(
                            "GitHub Copilot can also be used as an edit prediction provider, but it is an external AI service. Enable edit predictions under Configure Providers on the AI page and review the model-training risk warning."
                        ),
                    )
                    .color(Color::Muted),
                )
                .child(
                    Button::new("configure-copilot", i18n::t!("Open Configure Providers"))
                        .full_width()
                        .style(ButtonStyle::Outlined)
                        .on_click(|_, window, cx| {
                            window.dispatch_action(
                                zed_actions::OpenSettingsAt {
                                    path: "edit_predictions.providers".to_string(),
                                    target: None,
                                }
                                .boxed_clone(),
                                cx,
                            );
                        }),
                )
        } else {
            v_flex()
                .gap_1()
                .child(Label::new(if self.copilot_is_configured {
                    i18n::t!(
                        "Alternatively, you can continue to use GitHub Copilot as that's already set up."
                    )
                } else {
                    i18n::t!(
                        "Alternatively, you can use GitHub Copilot as your edit prediction provider."
                    )
                }))
                .child(
                    Button::new(
                        "configure-copilot",
                        if self.copilot_is_configured {
                            i18n::t!("Use Copilot")
                        } else {
                            i18n::t!("Configure Copilot")
                        },
                    )
                    .full_width()
                    .style(ButtonStyle::Outlined)
                    .on_click({
                        let callback = self.continue_with_copilot.clone();
                        move |_, window, cx| callback(window, cx)
                    }),
                )
        };

        v_flex()
            .gap_2()
            .child(ZedAiOnboarding::new(
                self.client.clone(),
                &self.user_store,
                self.continue_with_zed_ai.clone(),
                cx,
            ))
            .when(is_free_plan, |this| {
                this.child(ui::Divider::horizontal()).child(github_copilot)
            })
    }
}
