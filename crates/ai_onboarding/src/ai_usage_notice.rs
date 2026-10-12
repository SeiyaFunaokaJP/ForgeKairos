use std::sync::Arc;

use gpui::{Action, AnyElement, IntoElement, ParentElement};
use ui::{Tooltip, prelude::*};

/// What to know before using AI in this fork (ADR-F91): where the data goes
/// (ADR-F10), what the network allowlist stops (ADR-F03, ADR-F38) and what the
/// sandbox confines (ADR-F46, ADR-F54, ADR-F58). It replaces upstream's
/// "Welcome to Zed AI" card, which only spoke for one of the AI services this
/// fork can use.
#[derive(IntoElement, RegisterComponent)]
pub struct AiUsageNotice {
    compact: bool,
    dismiss: Option<Arc<dyn Fn(&mut Window, &mut App)>>,
}

struct Point {
    icon: IconName,
    title: SharedString,
    detail: SharedString,
}

impl AiUsageNotice {
    /// The full card, as the agent panel and the edit prediction onboarding
    /// show it before any external AI has been agreed to.
    pub fn new() -> Self {
        Self {
            compact: false,
            dismiss: None,
        }
    }

    /// One line per point, for the onboarding page's AI section.
    pub fn compact() -> Self {
        Self {
            compact: true,
            dismiss: None,
        }
    }

    pub fn on_dismiss(mut self, dismiss: Arc<dyn Fn(&mut Window, &mut App)>) -> Self {
        self.dismiss = Some(dismiss);
        self
    }

    fn points(compact: bool) -> [Point; 3] {
        [
            Point {
                icon: IconName::FileLock,
                title: i18n::t!("Your data").into(),
                detail: if compact {
                    i18n::t!(
                        "External AI receives the code, files, and conversations you give it and may store them or train on them. Local models on this machine send nothing out."
                    )
                    .into()
                } else {
                    i18n::t!(
                        "External AI (cloud APIs and external agents) receives the code, file contents, and conversations you give it, and depending on the provider's terms may store them or use them to train models. Local models served on this machine (loopback) send nothing out. Before using external AI for work, check your organization's policy."
                    )
                    .into()
                },
            },
            Point {
                icon: IconName::Public,
                title: i18n::t!("Network").into(),
                detail: if compact {
                    i18n::t!(
                        "Connections made by AI are denied unless their host is on the network allowlist, and adding a host asks you first."
                    )
                    .into()
                } else {
                    i18n::t!(
                        "Connections made by AI (model providers, agent tools, MCP servers, and external agents) are denied by default unless their host is on the network allowlist, and adding a host asks for your confirmation. The list is under Settings → Network → Network Allowlist. The IDE's own tooling, such as language servers and extensions, is not affected."
                    )
                    .into()
                },
            },
            Point {
                icon: IconName::Terminal,
                title: i18n::t!("Sandbox").into(),
                detail: if compact {
                    i18n::t!(
                        "Agent commands run inside an OS sandbox. Touching files outside the project, opening new network access, or running outside the sandbox asks you first."
                    )
                    .into()
                } else {
                    i18n::t!(
                        "Commands the agent runs, and external agents themselves, run inside an OS sandbox (Seatbelt on macOS, bubblewrap on Linux, WSL on Windows). File operations outside the project, new network access, and running outside the sandbox ask for your confirmation. Running outside the sandbox is not allowed by default; you can change this in the sandbox settings."
                    )
                    .into()
                },
            },
        ]
    }

    fn render_point(point: Point, compact: bool) -> impl IntoElement {
        h_flex()
            .items_start()
            .gap_2()
            .child(
                div().pt_0p5().child(
                    Icon::new(point.icon)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
            )
            .child(
                v_flex()
                    .min_w_0()
                    .flex_1()
                    .child(Label::new(point.title).size(if compact {
                        LabelSize::Small
                    } else {
                        LabelSize::Default
                    }))
                    .child(
                        Label::new(point.detail)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
            )
    }

    fn render_buttons(compact: bool) -> impl IntoElement {
        let style = if compact {
            ButtonStyle::OutlinedGhost
        } else {
            ButtonStyle::Outlined
        };
        h_flex()
            .gap_2()
            .flex_wrap()
            .child(
                Button::new("ai-usage-notice-ai-settings", i18n::t!("Open AI Settings"))
                    .style(style)
                    .when(!compact, |button| button.full_width())
                    .on_click(|_, window, cx| {
                        window.dispatch_action(zed_actions::agent::OpenSettings.boxed_clone(), cx);
                    }),
            )
            .child(
                Button::new(
                    "ai-usage-notice-sandbox-settings",
                    i18n::t!("Open Sandbox Settings"),
                )
                .style(style)
                .when(!compact, |button| button.full_width())
                .on_click(|_, window, cx| {
                    window.dispatch_action(
                        Box::new(zed_actions::OpenSettingsAt {
                            path: zed_actions::AGENT_SANDBOX_SETTINGS_PATH.to_string(),
                            target: None,
                        }),
                        cx,
                    );
                }),
            )
    }
}

impl RenderOnce for AiUsageNotice {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let compact = self.compact;
        let points = Self::points(compact)
            .into_iter()
            .map(|point| Self::render_point(point, compact));

        v_flex()
            .w_full()
            .relative()
            .gap_2()
            .map(|this| {
                if compact {
                    this.child(Label::new(i18n::t!("Before You Use AI")))
                } else {
                    this.child(Headline::new(i18n::t!("Before You Use AI")))
                }
            })
            .children(points)
            .child(Self::render_buttons(compact))
            .when_some(self.dismiss, |this, dismiss| {
                this.child(
                    h_flex().absolute().top_0().right_0().child(
                        IconButton::new("dismiss-ai-usage-notice", IconName::Close)
                            .icon_size(IconSize::Small)
                            .tooltip(Tooltip::text(i18n::t!("Dismiss")))
                            .on_click(move |_, window, cx| dismiss(window, cx)),
                    ),
                )
            })
    }
}

impl Component for AiUsageNotice {
    fn scope() -> ComponentScope {
        ComponentScope::Onboarding
    }

    fn description() -> &'static str {
        "What to know before using AI: where the data goes, the network allowlist, and the sandbox."
    }

    fn preview(_window: &mut Window, _cx: &mut App) -> AnyElement {
        v_flex()
            .gap_6()
            .max_w(px(640.))
            .child(single_example(
                "Full",
                AiUsageNotice::new().into_any_element(),
            ))
            .child(single_example(
                "Compact",
                AiUsageNotice::compact().into_any_element(),
            ))
            .into_any_element()
    }
}
