use std::{collections::HashSet, path::Path, sync::Arc, time::Duration};

use editor::Editor;
use futures::AsyncReadExt as _;
use gpui::{AnyView, Entity, Focusable as _, FutureExt as _, ScrollHandle, Task, prelude::*};
use http_client::{AsyncBody, EgressDenied, HttpClient, Request, StatusCode};
use language_model::{
    ApiKeyConfiguration, CreateProviderSettingsView, IconOrSvg, InlineDescription,
    LanguageModelProvider, LanguageModelProviderId, LanguageModelRegistry, ProviderSettingsView,
};
use serde::Deserialize;

use settings::{
    AnthropicCompatibleAvailableModel, AnthropicCompatibleModelCapabilities,
    AnthropicCompatibleSettingsContent, OpenAiCompatibleAvailableModel,
    OpenAiCompatibleModelCapabilities, OpenAiCompatibleSettingsContent, OpenAiReasoningEffort,
};
use ui::{
    Banner, ButtonLink, Checkbox, ConfiguredApiCard, ContextMenu, Divider, DividerColor,
    DropdownMenu, DropdownStyle, IconPosition, PopoverMenu, SwitchField, ToggleState, Tooltip,
    prelude::*,
};
use util::ResultExt as _;

use crate::SettingsWindow;
use crate::components::SettingsInputField;
use crate::pages::external_ai_consent;

/// Provider id of the native Anthropic provider (`anthropic::ANTHROPIC_PROVIDER_ID`),
/// which is API-key only. Duplicated rather than imported for the same reason
/// `external_ai_consent` duplicates its endpoint table: one string is not worth
/// a dependency on the vendor crate.
const ANTHROPIC_PROVIDER_ID: &str = "anthropic";

/// The synthesized "Claude (サブスクリプション)" card.
///
/// Anthropic's consumer terms scope a Claude Pro / Max subscription to
/// Anthropic's own apps and Claude Code, so this fork deliberately does not
/// implement a subscription sign-in for the native provider (ADR-F20): there is
/// no `LanguageModelProvider` behind this card, it is drawn here so the answer
/// sits where the question is asked — in the provider list, next to Anthropic,
/// the way "ChatGPT Subscription" sits next to OpenAI. The sanctioned route is
/// the *official, unmodified* CLI: the buttons only run the vendor's own install
/// command and then start `claude` in the integrated terminal instead of a
/// separate command prompt. Nothing about the CLI is bundled, wrapped or
/// re-authenticated here.
const CLAUDE_SUBSCRIPTION_NAME: &str = i18n::msg!("Claude Subscription");

/// ADR-F53: the route is the official CLI running itself, so the card says
/// where it opens and that the sign-in happens on the CLI's own screen —
/// nothing here relays a subscription's credentials.
const CLAUDE_SUBSCRIPTION_DESCRIPTION: &str = i18n::msg!(
    "Opens the official Claude Code in a tab of the agent panel. You sign in on Claude Code's own screen. The CLI is not bundled; it is installed with the official install command. For pay-as-you-go billing with an API key, use Anthropic above."
);

/// The one line left on the Anthropic card: the same question is asked from
/// there, but the answer now has a card of its own right below it.
const CLAUDE_SUBSCRIPTION_NOTE: &str = i18n::msg!(
    "Claude Pro / Max subscriptions can't be used here (pay-as-you-go with an API key only). See \"Claude Subscription\" below."
);

const INSTALL_CLAUDE_CODE_LABEL: &str = i18n::msg!("Install Claude Code (Official Command)");

const LAUNCH_CLAUDE_CODE_LABEL: &str = i18n::msg!("Launch Claude Code in Terminal");

const LAUNCH_CLAUDE_CODE_OUTSIDE_SANDBOX_LABEL: &str = i18n::msg!("Launch Outside the Sandbox");

/// Levels accepted by the official CLI's `--effort` flag, verified locally with
/// `claude --help` ("Effort level for the current session (low, medium, high,
/// xhigh, max)"). These are passed to the CLI verbatim, so they are **not**
/// translated; only the "leave the flag off" entry gets a Japanese name.
const CLAUDE_CODE_EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

const CLAUDE_CODE_DEFAULT_EFFORT_LABEL: &str = i18n::msg!("Default");

/// Name of the official CLI, looked up on `PATH` (GLOSSARY §1: not translated).
const CLAUDE_CODE_PROGRAM: &str = "claude";

/// `claude --version` has to start Node and the CLI itself, so it is not
/// instant; it is also the kind of thing that can wedge. The card falls back to
/// "installed, version unknown" rather than waiting forever.
const CLAUDE_CODE_VERSION_TIMEOUT: Duration = Duration::from_secs(10);

pub(crate) fn render_llm_providers_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let providers = LanguageModelRegistry::read_global(cx).visible_providers();

    if settings_window.claude_cli.outcome.is_none() {
        start_claude_cli_probe(window, cx);
    }

    let provider_ids = providers
        .iter()
        .map(|provider| provider.id().0)
        .collect::<Vec<_>>();
    let claude_card_index = claude_subscription_card_index(&provider_ids);

    let mut sections = Vec::with_capacity(providers.len() + 1);
    for (index, provider) in providers.iter().enumerate() {
        if index == claude_card_index {
            let is_first = sections.is_empty();
            sections.push(render_claude_subscription_card(
                settings_window,
                is_first,
                window,
                cx,
            ));
        }
        let is_first = sections.is_empty();
        sections.push(render_provider_section(
            settings_window,
            provider,
            is_first,
            window,
            cx,
        ));
    }
    if claude_card_index >= providers.len() {
        let is_first = sections.is_empty();
        sections.push(render_claude_subscription_card(
            settings_window,
            is_first,
            window,
            cx,
        ));
    }

    v_flex()
        .id("llm-providers-page")
        .size_full()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .children(sections)
        .into_any_element()
}

/// Where the synthesized Claude card goes: directly after the native Anthropic
/// provider, which is the position "ChatGPT Subscription" holds relative to
/// OpenAI (the registry is keyed by provider id, so `openai-subscribed` sorts
/// right after `openai`). With no Anthropic card in the list — it can be hidden
/// by an extension — the card goes last rather than disappearing, since it is
/// the only route to a Claude subscription either way.
fn claude_subscription_card_index(provider_ids: &[SharedString]) -> usize {
    provider_ids
        .iter()
        .position(|id| id.as_ref() == ANTHROPIC_PROVIDER_ID)
        .map_or(provider_ids.len(), |index| index + 1)
}

fn render_claude_subscription_card(
    settings_window: &SettingsWindow,
    is_first: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    v_flex()
        .min_w_0()
        .map(|this| if is_first { this.pt_4() } else { this.pt_8() })
        .gap_1p5()
        .child(render_provider_header(
            i18n::tr(CLAUDE_SUBSCRIPTION_NAME).into(),
            IconOrSvg::Icon(IconName::AiClaude),
            cx,
        ))
        .child(
            v_flex()
                .pt_2p5()
                .w_full()
                .min_w_0()
                .gap_1p5()
                .child(
                    Label::new(i18n::tr(CLAUDE_SUBSCRIPTION_DESCRIPTION))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .child(render_claude_cli_status(&settings_window.claude_cli))
                .child(render_claude_code_buttons(settings_window, window, cx))
                .child(render_claude_code_outside_sandbox(cx)),
        )
        .into_any_element()
}

const CLAUDE_CODE_OUTSIDE_SANDBOX_LABEL: &str = i18n::msg!("Always Launch Outside the Sandbox");
const CLAUDE_CODE_OUTSIDE_SANDBOX_DESCRIPTION: &str = i18n::msg!(
    "Makes \"Launch Claude Code\" start outside the sandbox every time, without asking. Writes are no longer restricted, and network restrictions only apply to programs that read the proxy settings (on an SSH host, egress control does not apply at all). Sign-in is shared with your regular Claude Code. Saved only in your user settings."
);

/// ADR-F49 決定 19 (8c): `agent.claude_code_run_outside_sandbox`. The switch
/// shows what the launch acts on: the user's own settings file only.
fn render_claude_code_outside_sandbox(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    SwitchField::new(
        "claude-code-run-outside-sandbox",
        Some(i18n::tr(CLAUDE_CODE_OUTSIDE_SANDBOX_LABEL)),
        Some(i18n::tr(CLAUDE_CODE_OUTSIDE_SANDBOX_DESCRIPTION).into()),
        project::agent_sandbox::claude_code_runs_outside_sandbox_by_user_choice(cx),
        |state, _window, cx| {
            set_claude_code_run_outside_sandbox(*state == ToggleState::Selected, cx);
        },
    )
    .tab_index(0)
}

fn set_claude_code_run_outside_sandbox(enabled: bool, cx: &mut App) {
    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |settings, _| {
        settings
            .agent
            .get_or_insert_default()
            .claude_code_run_outside_sandbox = Some(enabled);
    });
}

/// Whether the official `claude` CLI is on `PATH`, and what it reports as its
/// version.
#[derive(Default)]
pub(crate) struct ClaudeCliState {
    outcome: Option<ClaudeCliOutcome>,
    /// Held so the lookup is dropped with the settings window. Never reassigned
    /// from inside the task it holds, which would cancel that task mid-flight.
    _probe: Option<Task<()>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum ClaudeCliOutcome {
    Checking,
    Missing,
    Installed { version: Option<SharedString> },
}

/// Looks for `claude` once per settings window, when the page is first drawn.
///
/// Deferred rather than spawned straight from the render, so the "already
/// started" flag is set before the next frame can ask again.
fn start_claude_cli_probe(window: &mut Window, cx: &mut Context<SettingsWindow>) {
    cx.defer_in(window, |settings_window, _window, cx| {
        if settings_window.claude_cli.outcome.is_some() {
            return;
        }
        settings_window.claude_cli.outcome = Some(ClaudeCliOutcome::Checking);
        settings_window.claude_cli._probe = Some(cx.spawn(async move |settings_window, cx| {
            let executor = cx.background_executor().clone();
            let outcome = cx
                .background_spawn(async move { detect_claude_cli(&executor).await })
                .await;
            settings_window
                .update(cx, |settings_window, cx| {
                    settings_window.claude_cli.outcome = Some(outcome);
                    cx.notify();
                })
                .ok();
        }));
    });
}

async fn detect_claude_cli(executor: &gpui::BackgroundExecutor) -> ClaudeCliOutcome {
    let Ok(path) = which::which(CLAUDE_CODE_PROGRAM) else {
        return ClaudeCliOutcome::Missing;
    };
    ClaudeCliOutcome::Installed {
        version: claude_cli_version(&path, executor).await,
    }
}

async fn claude_cli_version(
    path: &Path,
    executor: &gpui::BackgroundExecutor,
) -> Option<SharedString> {
    let mut command = claude_cli_version_command(path)?;
    let output = command
        .output()
        .with_timeout(CLAUDE_CODE_VERSION_TIMEOUT, executor)
        .await
        .ok()?
        .log_err()?;
    if !output.status.success() {
        return None;
    }
    let reported = String::from_utf8_lossy(&output.stdout);
    let first_line = reported.lines().next()?.trim();
    (!first_line.is_empty()).then(|| SharedString::from(first_line.to_string()))
}

/// `claude` is usually an npm global shim, and on Windows that shim is a `.cmd`
/// batch file, which `CreateProcess` cannot start on its own. Running it through
/// `cmd /C` from its own directory keeps the argument free of spaces, which
/// would otherwise run into `cmd`'s quoting rules.
fn claude_cli_version_command(path: &Path) -> Option<util::command::Command> {
    let mut command = if cfg!(target_os = "windows")
        && path
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("cmd"))
    {
        let mut command = util::command::new_command("cmd");
        command
            .current_dir(path.parent()?)
            .arg("/C")
            .arg(path.file_name()?);
        command
    } else {
        util::command::new_command(path)
    };
    command.arg("--version");
    Some(command)
}

/// Says whether the CLI the buttons below drive is actually here. Always framed
/// as 「この PC」: the lookup is against this machine's `PATH`, while a remote
/// project's terminal would run whatever is on the remote host.
fn render_claude_cli_status(state: &ClaudeCliState) -> impl IntoElement {
    let (icon, color, text): (IconName, Color, SharedString) = match &state.outcome {
        None | Some(ClaudeCliOutcome::Checking) => (
            IconName::ArrowCircle,
            Color::Muted,
            i18n::t!("Checking PATH on this PC…").into(),
        ),
        Some(ClaudeCliOutcome::Missing) => (
            IconName::Info,
            Color::Muted,
            i18n::t!("Not installed on this PC (claude is not on PATH).").into(),
        ),
        Some(ClaudeCliOutcome::Installed { version }) => (
            IconName::Check,
            Color::Success,
            match version {
                Some(version) => i18n::t!("Installed on this PC ({version})").into(),
                None => i18n::t!("Installed on this PC.").into(),
            },
        ),
    };

    h_flex()
        .w_full()
        .min_w_0()
        .gap_1()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(Label::new(text).size(LabelSize::Small).color(color))
}

/// The two sanctioned routes to a Claude subscription, plus the CLI's own
/// `--effort` choice (ADR-F20):
///
/// 1. run the vendor's official install command, and
/// 2. start the resulting `claude` binary in the integrated terminal.
///
/// Both are disabled without a workspace to run the terminal in, exactly like
/// the "open network_allowlist.json" button.
fn render_claude_code_buttons(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let has_workspace = settings_window.original_window.is_some();
    let no_workspace_tooltip = i18n::t!("A project window is needed to run this in the terminal");

    h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .flex_wrap()
        .child(
            Button::new("claude-code-install", i18n::tr(INSTALL_CLAUDE_CODE_LABEL))
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Outlined)
                .disabled(!has_workspace)
                .start_icon(
                    Icon::new(IconName::Download)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(Tooltip::text(if has_workspace {
                    i18n::t!(
                        "Closes the settings window and runs the official install command `npm install -g @anthropic-ai/claude-code` in the integrated terminal. Requires Node.js (npm)."
                    )
                } else {
                    no_workspace_tooltip
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.install_claude_code(window, cx);
                })),
        )
        .child(
            Button::new("claude-code-launch", i18n::tr(LAUNCH_CLAUDE_CODE_LABEL))
                .label_size(LabelSize::Small)
                .style(ButtonStyle::Outlined)
                .disabled(!has_workspace)
                .start_icon(
                    Icon::new(IconName::Terminal)
                        .size(IconSize::XSmall)
                        .color(Color::Muted),
                )
                .tooltip(Tooltip::text(if has_workspace {
                    i18n::t!(
                        "Closes the settings window and launches Claude Code in a tab of the agent panel. For a local project on Windows it runs inside the sandbox (WSL); on a Linux host connected over SSH it runs inside that host's sandbox."
                    )
                } else {
                    no_workspace_tooltip
                }))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.launch_claude_code(window, cx);
                })),
        )
        .child(
            Button::new(
                "claude-code-launch-outside-sandbox",
                i18n::tr(LAUNCH_CLAUDE_CODE_OUTSIDE_SANDBOX_LABEL),
            )
            .label_size(LabelSize::Small)
            .style(ButtonStyle::Outlined)
            .disabled(!has_workspace)
            .tooltip(Tooltip::text(if has_workspace {
                i18n::t!(
                    "Closes the settings window and launches `claude` on this PC (on the SSH host, when connected over SSH) outside the sandbox. Asks for confirmation every time (unless \"Always Launch Outside the Sandbox\" below is on)."
                )
            } else {
                no_workspace_tooltip
            }))
            .on_click(cx.listener(|this, _, window, cx| {
                this.launch_claude_code_outside_sandbox(window, cx);
            })),
        )
        .child(render_claude_code_effort_selector(
            settings_window,
            window,
            cx,
        ))
}

/// Picks the value handed to the official CLI's `--effort` flag. Level names are
/// the CLI's own, so they stay untranslated (GLOSSARY §1); only "既定" (= omit
/// the flag) is ours.
fn render_claude_code_effort_selector(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let selected = settings_window.claude_code_effort.clone();
    let effort: &str = match &selected {
        Some(effort) => effort.as_ref(),
        None => i18n::tr(CLAUDE_CODE_DEFAULT_EFFORT_LABEL),
    };
    let label: SharedString = i18n::t!("Thinking Effort: {effort}").into();

    let settings_window = cx.weak_entity();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        let entries = std::iter::once((i18n::tr(CLAUDE_CODE_DEFAULT_EFFORT_LABEL), None)).chain(
            CLAUDE_CODE_EFFORT_LEVELS
                .into_iter()
                .map(|level| (level, Some(SharedString::new_static(level)))),
        );

        for (entry_label, value) in entries {
            let is_selected = value == selected;
            let settings_window = settings_window.clone();
            menu.push_item(
                ui::ContextMenuEntry::new(entry_label)
                    .toggleable(IconPosition::End, is_selected)
                    .handler(move |_window, cx| {
                        let value = value.clone();
                        settings_window
                            .update(cx, |this, cx| {
                                this.claude_code_effort = value;
                                cx.notify();
                            })
                            .ok();
                    }),
            );
        }
        menu
    });

    DropdownMenu::new("claude-code-effort", label, menu)
        .style(DropdownStyle::Outlined)
        .trigger_size(ButtonSize::Compact)
        .aria_label(i18n::t!("Claude Code Thinking Effort (--effort)"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompatibleProviderKind {
    OpenAi,
    Anthropic,
}

impl CompatibleProviderKind {
    fn label(self) -> &'static str {
        match self {
            Self::OpenAi => "OpenAI",
            Self::Anthropic => "Anthropic",
        }
    }

    fn default_api_url(self) -> &'static str {
        match self {
            Self::OpenAi => "https://api.openai.com/v1",
            Self::Anthropic => "https://api.anthropic.com",
        }
    }
}

pub(crate) fn render_add_llm_provider_popover(
    settings_window: &SettingsWindow,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let focus_handle = settings_window
        .llm_provider_add_focus_handle
        .clone()
        .tab_index(0)
        .tab_stop(true);

    let settings_window = cx.entity().downgrade();

    PopoverMenu::new("add-llm-provider-popover")
        .trigger(
            Button::new("add-llm-provider", i18n::t!("Add Provider"))
                .style(ButtonStyle::Outlined)
                .track_focus(&focus_handle)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                ),
        )
        .anchor(gpui::Anchor::TopRight)
        .offset(gpui::Point {
            x: px(0.0),
            y: px(2.0),
        })
        .menu(move |window, cx| {
            let settings_window = settings_window.clone();
            Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                menu.header(i18n::t!("Compatible APIs"))
                    .entry("OpenAI", None, {
                        let settings_window = settings_window.clone();
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_llm_provider_form(
                                        this,
                                        CompatibleProviderKind::OpenAi,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
                    .entry("Anthropic", None, {
                        let settings_window = settings_window;
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_llm_provider_form(
                                        this,
                                        CompatibleProviderKind::Anthropic,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
            }))
        })
}

fn render_provider_section(
    settings_window: &SettingsWindow,
    provider: &Arc<dyn LanguageModelProvider>,
    is_first: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let provider_name = provider.name().0;

    // External providers are gated on an explicit opt-in (ADR-F10). The gate
    // goes here rather than around the API key field alone so that sign-in and
    // sub-page based providers are covered too, and so that
    // `settings_view` — which builds the provider's configuration view — is not
    // even called while consent is missing.
    let consent =
        external_ai_consent::llm_provider_target(provider_id.0.as_ref(), provider_name.clone(), cx);

    let body = match &consent {
        Some(target) if !external_ai_consent::is_acknowledged(&target.key, cx) => {
            external_ai_consent::render_consent_gate(settings_window, target, cx)
        }
        _ => {
            let body = match provider.settings_view(cx) {
                Some(ProviderSettingsView::ApiKey(config)) => {
                    render_api_key_providers_item(provider, provider_name.clone(), config, cx)
                }
                Some(ProviderSettingsView::Inline(settings)) => {
                    let view = get_or_create_configuration_view(
                        settings_window,
                        &provider_id,
                        settings.create_view,
                        window,
                        cx,
                    );
                    render_inline_body(
                        provider_name.clone(),
                        settings.title,
                        settings.description,
                        view,
                    )
                }
                Some(ProviderSettingsView::SubPage(settings)) => {
                    render_subpage_item(provider, settings.description, cx)
                }
                None => div().into_any_element(),
            };
            match &consent {
                Some(target) => v_flex()
                    .child(external_ai_consent::render_consent_summary(
                        settings_window,
                        target,
                        cx,
                    ))
                    .child(body)
                    .into_any_element(),
                None => body,
            }
        }
    };

    v_flex()
        .min_w_0()
        .map(|s| if is_first { s.pt_4() } else { s.pt_8() })
        .gap_1p5()
        .child(render_provider_header(provider_name, provider.icon(), cx))
        .child(body)
        .when(provider_id.0.as_ref() == ANTHROPIC_PROVIDER_ID, |this| {
            this.child(
                div().pt_2p5().child(
                    Label::new(i18n::tr(CLAUDE_SUBSCRIPTION_NOTE))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
            )
        })
        .into_any_element()
}

/// An icon + name header with a faded divider, mirroring `SettingsSectionHeader`
/// but able to render providers' external SVG icons.
fn render_provider_header(
    provider_name: SharedString,
    icon: IconOrSvg,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let icon = match icon {
        IconOrSvg::Svg(path) => Icon::from_external_svg(path),
        IconOrSvg::Icon(name) => Icon::new(name),
    }
    .color(Color::Muted);

    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            h_flex().gap_1p5().child(icon).child(
                Label::new(provider_name)
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            ),
        )
        .child(Divider::horizontal().color(DividerColor::BorderFaded))
}

fn render_api_key_providers_item(
    provider: &Arc<dyn LanguageModelProvider>,
    provider_name: SharedString,
    config: ApiKeyConfiguration,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let has_key = config.has_key;
    let is_from_env_var = config.is_from_env_var;
    let env_var_name = config.env_var_name;
    let api_key_url = config.api_key_url;

    if has_key {
        let configured_label = if is_from_env_var {
            i18n::t!("API Key Set in Environment Variable")
        } else {
            i18n::t!("API Key Configured")
        };
        let button_id = format!("reset-api-key-{}", provider_id.0);

        let card = ConfiguredApiCard::new(button_id, configured_label)
            .button_label(i18n::t!("Reset Key"))
            .button_tab_index(0)
            .disabled(is_from_env_var)
            .when(is_from_env_var, |this| {
                this.tooltip_label(i18n::t!(
                    "To reset your API key, unset the {env_var_name} environment variable."
                ))
            })
            .on_click({
                let provider = provider.clone();
                move |_, _, cx| {
                    provider.set_api_key(None, cx).detach_and_log_err(cx);
                }
            })
            .into_any_element();

        return v_flex().gap_2().child(card).into_any_element();
    }

    let input_id = format!("{}-api-key-input", provider_id.0);
    let aria_label = i18n::t!("{provider_name} API Key");
    let (before_link, after_link) = split_around_link(&i18n::t!(
        "Visit the {link} to generate an API key.",
        link = LINK_SLOT
    ));

    v_flex()
        .gap_2()
        .child(
            h_flex()
                .pt_2p5()
                .w_full()
                .min_w_0()
                .gap_4()
                .justify_between()
                .child(
                    v_flex()
                        .w_full()
                        .min_w_0()
                        .max_w_1_2()
                        .gap_0p5()
                        .child(Label::new(i18n::t!("API Key")))
                        .child(
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
                                        api_key_url,
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
                        .child(
                            Label::new(i18n::t!(
                                "Or set the {env_var_name} env var and restart Zed for it to take effect."
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        ),
                )
                .child(
                    SettingsInputField::new(input_id)
                        .tab_index(0)
                        .with_placeholder("xxxxxxxxxxxxxxxxxxxx")
                        .aria_label(aria_label)
                        .on_confirm({
                            let provider = provider.clone();
                            move |api_key, _window, cx| {
                                if let Some(key) = api_key.filter(|key| !key.is_empty()) {
                                    provider.set_api_key(Some(key), cx).detach_and_log_err(cx);
                                }
                            }
                        }),
                ),
        )
        .into_any_element()
}

/// Stands in for a link inside a translated sentence, so that each language can
/// put the link where its word order needs it.
pub(super) const LINK_SLOT: &str = "\u{1}";

pub(super) fn split_around_link(sentence: &str) -> (SharedString, SharedString) {
    match sentence.split_once(LINK_SLOT) {
        Some((before, after)) => (
            before.trim().to_string().into(),
            after.trim().to_string().into(),
        ),
        None => (sentence.trim().to_string().into(), SharedString::default()),
    }
}

fn render_inline_body(
    provider_name: SharedString,
    title: Option<SharedString>,
    description: Option<InlineDescription>,
    view: impl IntoElement,
) -> AnyElement {
    let view = view.into_any_element();

    if title.is_none() && description.is_none() {
        return v_flex()
            .pt_1()
            .w_full()
            .min_w_0()
            .child(view)
            .into_any_element();
    }

    h_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .max_w_1_2()
                .debug_selector(|| "inline-provider-description".into())
                .when_some(title, |this, title| this.child(Label::new(title)))
                .when_some(description, |this, description| {
                    this.child(render_inline_description(provider_name, description))
                }),
        )
        .child(
            h_flex()
                .min_w_0()
                .max_w_1_2()
                .flex_1()
                .justify_end()
                .child(view),
        )
        .into_any_element()
}

fn render_subpage_item(
    provider: &Arc<dyn LanguageModelProvider>,
    description: Option<InlineDescription>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let provider_id = provider.id();
    let provider_name = provider.name().0;

    h_flex()
        .pt_2p5()
        .w_full()
        .min_w_0()
        .gap_4()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .max_w_1_2()
                .gap_0p5()
                .child(Label::new(i18n::t!("Configure Provider")))
                .when_some(description, |this, description| {
                    this.child(render_inline_description(provider_name, description))
                }),
        )
        .child(
            Button::new(
                format!("configure-{}", provider_id.0),
                i18n::t!("Configure"),
            )
            .style(ButtonStyle::OutlinedGhost)
            .size(ButtonSize::Medium)
            .end_icon(
                Icon::new(IconName::ChevronRight)
                    .size(IconSize::Small)
                    .color(Color::Muted),
            )
            .tab_index(0isize)
            .on_click(cx.listener(move |this, _, window, cx| {
                open_provider_configuration(this, provider_id.clone(), window, cx);
            })),
        )
        .into_any_element()
}

fn render_inline_description(
    provider_name: SharedString,
    description: InlineDescription,
) -> AnyElement {
    match description {
        InlineDescription::ApiKeyUrl(url) => {
            let (before_link, after_link) = split_around_link(&i18n::t!(
                "To find an API key, visit the {link}",
                link = LINK_SLOT
            ));
            h_flex()
                .gap_0p5()
                .when(!before_link.is_empty(), |this| {
                    this.child(
                        Label::new(before_link)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .child(
                    ButtonLink::new(i18n::t!("{provider_name} dashboard."), url)
                        .label_size(LabelSize::Small),
                )
                .when(!after_link.is_empty(), |this| {
                    this.child(
                        Label::new(after_link)
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    )
                })
                .into_any_element()
        }
        InlineDescription::Text(text) => Label::new(text)
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    }
}

fn open_provider_configuration(
    settings_window: &mut SettingsWindow,
    provider_id: LanguageModelProviderId,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let title = LanguageModelRegistry::read_global(cx)
        .provider(&provider_id)
        .map(|provider| provider.name().0)
        .unwrap_or_else(|| provider_id.0.clone());

    settings_window.configuring_provider = Some(provider_id);

    settings_window.push_dynamic_sub_page(
        title,
        "Agent Configuration",
        Some("llm_providers"),
        true,
        render_provider_config_sub_page,
        window,
        cx,
    );
}

fn render_provider_config_sub_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(provider_id) = settings_window.configuring_provider.clone() else {
        return div().into_any_element();
    };
    let Some(provider) = LanguageModelRegistry::read_global(cx).provider(&provider_id) else {
        return div().into_any_element();
    };

    let Some(create_view) =
        provider
            .settings_view(cx)
            .and_then(|settings_view| match settings_view {
                ProviderSettingsView::Inline(settings) => Some(settings.create_view),
                ProviderSettingsView::SubPage(settings) => Some(settings.create_view),
                ProviderSettingsView::ApiKey(_) => None,
            })
    else {
        return div().into_any_element();
    };
    let view =
        get_or_create_configuration_view(settings_window, &provider_id, create_view, window, cx);

    v_flex()
        .id("provider-config-sub-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(view)
        .into_any_element()
}

fn get_or_create_configuration_view(
    settings_window: &SettingsWindow,
    provider_id: &LanguageModelProviderId,
    create_view: CreateProviderSettingsView,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyView {
    if let Some(view) = settings_window
        .provider_configuration_views
        .get(provider_id)
    {
        return view.clone();
    }

    let view = create_view(window, cx);

    // Store the view for future renders by deferring a mutation
    let provider_id = provider_id.clone();
    let view_clone = view.clone();
    cx.defer_in(window, move |this, _window, _cx| {
        this.provider_configuration_views
            .insert(provider_id, view_clone);
    });

    view
}

pub(crate) struct LlmProviderForm {
    kind: CompatibleProviderKind,
    provider_name: Entity<Editor>,
    api_url: Entity<Editor>,
    api_key: Entity<Editor>,
    /// The endpoint takes no credential at all, so the API key field is ignored
    /// and validation stops requiring one.
    no_api_key: bool,
    models: Vec<ModelInput>,
    error: Option<SharedString>,
    /// Result of the last "モデル一覧を取得" run, shown next to the model rows.
    models_fetch: Option<ModelsFetchOutcome>,
    /// Held so that leaving the form cancels a fetch still in flight.
    models_fetch_task: Option<Task<()>>,
    /// Ticked by the user in the form's warning block. Only persisted to
    /// `acknowledged_external_ai_providers` once the provider is actually
    /// saved, since the consent is keyed by the provider name.
    consent_acknowledged: bool,
    /// Redraws the form as the endpoint is typed, so that the warning appears
    /// and disappears with the URL. Editors notify their own view on edit, which
    /// would otherwise not reach the settings window.
    _api_url_subscription: gpui::Subscription,
}

impl LlmProviderForm {
    fn new(
        kind: CompatibleProviderKind,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let provider_name = new_input(kind.label(), None, false, window, cx);
        let api_url = new_input(kind.default_api_url(), None, false, window, cx);
        let api_url_subscription = cx.observe(&api_url, |_, _, cx| cx.notify());
        Self {
            kind,
            provider_name,
            api_url,
            api_key: new_input(
                "000000000000000000000000000000000000000000000000",
                None,
                true,
                window,
                cx,
            ),
            no_api_key: false,
            models: vec![ModelInput::new(0, window, cx)],
            error: None,
            models_fetch: None,
            models_fetch_task: None,
            consent_acknowledged: false,
            _api_url_subscription: api_url_subscription,
        }
    }
}

/// What the last model-listing fetch produced, rendered inline in the model
/// section rather than in the form footer so it sits next to the rows it filled.
enum ModelsFetchOutcome {
    Added(usize),
    NoneNew,
    Error(SharedString),
}

struct ModelInput {
    name: Entity<Editor>,
    max_completion_tokens: Entity<Editor>,
    max_output_tokens: Entity<Editor>,
    max_tokens: Entity<Editor>,
    reasoning_effort: OpenAiReasoningEffort,
    supports_tools: ToggleState,
    supports_images: ToggleState,
    supports_parallel_tool_calls: ToggleState,
    supports_prompt_cache_key: ToggleState,
    supports_chat_completions: ToggleState,
    supports_thinking: ToggleState,
    interleaved_reasoning: ToggleState,
    max_tokens_parameter: ToggleState,
}

impl ModelInput {
    fn new(_index: usize, window: &mut Window, cx: &mut Context<SettingsWindow>) -> Self {
        let OpenAiCompatibleModelCapabilities {
            tools,
            images,
            parallel_tool_calls,
            prompt_cache_key,
            chat_completions,
            interleaved_reasoning,
            max_tokens_parameter,
        } = OpenAiCompatibleModelCapabilities::default();

        Self {
            name: new_input(
                i18n::t!("e.g. gpt-5, claude-opus-4, gemini-2.5-pro"),
                None,
                false,
                window,
                cx,
            ),
            max_completion_tokens: new_input("200000", Some("200000"), false, window, cx),
            max_output_tokens: new_input(
                i18n::t!("Max Output Tokens"),
                Some("32000"),
                false,
                window,
                cx,
            ),
            max_tokens: new_input(i18n::t!("Max Tokens"), Some("200000"), false, window, cx),
            reasoning_effort: OpenAiReasoningEffort::Medium,
            supports_tools: tools.into(),
            supports_images: images.into(),
            supports_parallel_tool_calls: parallel_tool_calls.into(),
            supports_prompt_cache_key: prompt_cache_key.into(),
            supports_chat_completions: chat_completions.into(),
            supports_thinking: ToggleState::Unselected,
            interleaved_reasoning: interleaved_reasoning.into(),
            max_tokens_parameter: max_tokens_parameter.into(),
        }
    }
}

fn new_input(
    placeholder: &str,
    initial: Option<&str>,
    masked: bool,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    let placeholder = placeholder.to_string();
    let initial = initial.map(str::to_string);
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder.as_str(), window, cx);
        editor.set_masked(masked, cx);
        if let Some(text) = initial {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

fn open_llm_provider_form(
    settings_window: &mut SettingsWindow,
    kind: CompatibleProviderKind,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.llm_provider_form = Some(LlmProviderForm::new(kind, window, cx));
    settings_window.push_dynamic_sub_page(
        i18n::t!("Add {}-Compatible Provider", kind.label()),
        "Agent Configuration",
        Some("llm_providers"),
        true,
        render_llm_provider_form_page,
        window,
        cx,
    );
}

fn render_llm_provider_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window.llm_provider_form.as_ref() else {
        return div().into_any_element();
    };

    // The provider does not exist yet, so the warning follows whatever endpoint
    // is currently typed: pointing it at loopback makes it disappear (ADR-F10).
    let consent = external_ai_consent::form_target(
        &form.provider_name.read(cx).text(cx),
        &form.api_url.read(cx).text(cx),
        form.kind.label(),
    );
    let consent_acknowledged = form.consent_acknowledged;

    v_flex()
        .size_full()
        .child(
            v_flex()
                .id("llm-provider-form-page")
                .track_scroll(scroll_handle)
                .pt_2p5()
                .px_8()
                .pb_16()
                .gap_4()
                .overflow_y_scroll()
                .child(Label::new(match form.kind {
                    CompatibleProviderKind::OpenAi => {
                        i18n::t!("This provider will use an OpenAI-compatible API.")
                    }
                    CompatibleProviderKind::Anthropic => {
                        i18n::t!("This provider will use an Anthropic Messages-compatible API.")
                    }
                }))
                .when_some(consent.as_ref(), |this, target| {
                    this.child(render_form_consent(
                        settings_window,
                        target,
                        consent_acknowledged,
                        cx,
                    ))
                })
                .child(Divider::horizontal().flex_shrink_0())
                .child(render_form_field(
                    i18n::t!("Provider Name"),
                    i18n::t!("A unique name used to identify this provider."),
                    &form.provider_name,
                    cx,
                ))
                .child(render_form_field(
                    i18n::t!("API URL"),
                    i18n::t!("The base URL for the compatible API."),
                    &form.api_url,
                    cx,
                ))
                .child(render_api_key_field(form, cx))
                .child(render_models_section(form, window, cx)),
        )
        .child(
            v_flex()
                .px_8()
                .py_2p5()
                .gap_1()
                .border_t_1()
                .border_color(cx.theme().colors().border_variant)
                .when_some(form.error.clone(), |this, error| {
                    this.child(render_form_error(error))
                })
                .child(render_form_actions(cx)),
        )
        .into_any_element()
}

/// The data-retention warning shown while the form's endpoint is remote. Unlike
/// the per-provider gate this does not hide the fields — the API URL is one of
/// them, and the user has to be able to point it at a local server.
fn render_form_consent(
    settings_window: &SettingsWindow,
    target: &external_ai_consent::ConsentTarget,
    acknowledged: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    Banner::new().severity(Severity::Warning).child(
        v_flex()
            .w_full()
            .min_w_0()
            .gap_2()
            .child(external_ai_consent::render_warning_content(
                settings_window,
                target,
                cx,
            ))
            .child(Divider::horizontal())
            .child(
                Checkbox::new("llm-provider-form-consent", acknowledged.into())
                    .label(i18n::t!(
                        "I understand the risks above and want to add this provider"
                    ))
                    .label_size(LabelSize::Small)
                    .on_click(cx.listener(|this, checked: &ToggleState, _, cx| {
                        if let Some(form) = this.llm_provider_form.as_mut() {
                            form.consent_acknowledged = checked.selected();
                        }
                        cx.notify();
                    })),
            ),
    )
}

fn render_form_field(
    title: &'static str,
    description: &'static str,
    editor: &Entity<Editor>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let border_focused = cx.theme().colors().border_focused;
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    v_flex()
        .w_full()
        .gap_1p5()
        .child(
            v_flex()
                .gap_0p5()
                .child(
                    h_flex().gap_0p5().child(Label::new(title)).child(
                        Label::new("*")
                            .size(LabelSize::Small)
                            .color(Color::Error)
                            .mb_2(),
                    ),
                )
                .child(
                    Label::new(description)
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(
            // The frame is rendered unfocused and the focused border comes from
            // the element's focus style, so no `Window` is needed here.
            ui::input_frame(false, cx)
                .w_full()
                .min_w_64()
                .track_focus(&focus_handle)
                .focus(|style| style.border_color(border_focused))
                .child(editor.clone()),
        )
        .into_any_element()
}

fn render_api_key_field(
    form: &LlmProviderForm,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let no_api_key = form.no_api_key;

    v_flex()
        .w_full()
        .gap_1p5()
        .map(|this| {
            if no_api_key {
                this.child(
                    v_flex().gap_0p5().child(Label::new(i18n::t!("API Key"))).child(
                        Label::new(i18n::t!(
                            "No API key is sent, because this connects to a server that needs no authentication."
                        ))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                    ),
                )
            } else {
                this.child(render_form_field(
                    i18n::t!("API Key"),
                    i18n::t!("Stored in the system keychain, not in settings.json."),
                    &form.api_key,
                    cx,
                ))
            }
        })
        .child(
            Checkbox::new("llm-provider-form-no-api-key", no_api_key.into())
                .label(i18n::t!("Connect without an API key (server needs no authentication)"))
                .label_size(LabelSize::Small)
                .on_click(cx.listener(|this, checked: &ToggleState, _, cx| {
                    if let Some(form) = this.llm_provider_form.as_mut() {
                        form.no_api_key = checked.selected();
                    }
                    cx.notify();
                })),
        )
}

fn render_models_section(
    form: &LlmProviderForm,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let is_fetching = form.models_fetch_task.is_some();

    v_flex()
        .mt_1()
        .gap_2()
        .child(
            h_flex()
                .justify_between()
                .child(Label::new(i18n::t!("Models")))
                .child(
                    h_flex()
                        .gap_1()
                        .child(
                            Button::new(
                                "fetch-models",
                                if is_fetching {
                                    i18n::t!("Fetching…")
                                } else {
                                    i18n::t!("Fetch Models")
                                },
                            )
                            .start_icon(
                                Icon::new(IconName::CloudDownload)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .label_size(LabelSize::Small)
                            .disabled(is_fetching)
                            .on_click(cx.listener(
                                |this, _, window, cx| {
                                    fetch_llm_provider_models(this, window, cx);
                                },
                            )),
                        )
                        .child(
                            Button::new("add-model", i18n::t!("Add Model"))
                                .start_icon(
                                    Icon::new(IconName::Plus)
                                        .size(IconSize::XSmall)
                                        .color(Color::Muted),
                                )
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(|this, _, window, cx| {
                                    if let Some(form) = this.llm_provider_form.as_mut() {
                                        let index = form.models.len();
                                        form.models.push(ModelInput::new(index, window, cx));
                                    }
                                    cx.notify();
                                })),
                        ),
                ),
        )
        .when_some(form.models_fetch.as_ref(), |this, outcome| {
            this.child(render_models_fetch_outcome(outcome))
        })
        .children(form.models.iter().enumerate().map(|(index, model)| {
            render_model(form.kind, model, index, form.models.len(), window, cx)
        }))
}

fn render_models_fetch_outcome(outcome: &ModelsFetchOutcome) -> AnyElement {
    match outcome {
        ModelsFetchOutcome::Error(message) => render_form_error(message.clone()).into_any_element(),
        ModelsFetchOutcome::Added(count) => h_flex()
            .w_full()
            .gap_2()
            .child(
                Icon::new(IconName::Check)
                    .size(IconSize::Small)
                    .color(Color::Success),
            )
            .child(
                Label::new(i18n::tn!(
                    *count,
                    "Added {count} model",
                    "Added {count} models"
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .into_any_element(),
        ModelsFetchOutcome::NoneNew => Label::new(i18n::t!("No new models were found to add"))
            .size(LabelSize::Small)
            .color(Color::Muted)
            .into_any_element(),
    }
}

fn render_model(
    kind: CompatibleProviderKind,
    model: &ModelInput,
    index: usize,
    model_count: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    v_flex()
        .p_2()
        .gap_2()
        .rounded_sm()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .bg(cx.theme().colors().element_active.opacity(0.15))
        .child(render_form_field(
            i18n::t!("Model Name"),
            i18n::t!("The model's name in the provider's API."),
            &model.name,
            cx,
        ))
        .when(matches!(kind, CompatibleProviderKind::OpenAi), |this| {
            this.child(render_form_field(
                i18n::t!("Max Completion Tokens"),
                i18n::t!("Maximum completion tokens for OpenAI-compatible requests."),
                &model.max_completion_tokens,
                cx,
            ))
        })
        .child(render_form_field(
            i18n::t!("Max Output Tokens"),
            i18n::t!("The maximum number of tokens the model can output."),
            &model.max_output_tokens,
            cx,
        ))
        .child(render_form_field(
            i18n::t!("Max Tokens"),
            i18n::t!("The model context window size."),
            &model.max_tokens,
            cx,
        ))
        .child(render_model_capabilities(kind, model, index, window, cx))
        .when(model_count > 1, |this| {
            this.child(
                Button::new(("remove-model", index), i18n::t!("Remove Model"))
                    .start_icon(
                        Icon::new(IconName::Trash)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .label_size(LabelSize::Small)
                    .style(ButtonStyle::Outlined)
                    .full_width()
                    .on_click(cx.listener(move |this, _, _window, cx| {
                        if let Some(form) = this.llm_provider_form.as_mut()
                            && index < form.models.len()
                        {
                            form.models.remove(index);
                        }
                        cx.notify();
                    })),
            )
        })
        .into_any_element()
}

fn render_model_capabilities(
    kind: CompatibleProviderKind,
    model: &ModelInput,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    v_flex()
        .gap_1()
        .child(render_capability_checkbox(
            "supports-tools",
            index,
            i18n::t!("Supports tools"),
            model.supports_tools,
            |model, state| model.supports_tools = state,
            cx,
        ))
        .child(render_capability_checkbox(
            "supports-images",
            index,
            i18n::t!("Supports images"),
            model.supports_images,
            |model, state| model.supports_images = state,
            cx,
        ))
        .when(matches!(kind, CompatibleProviderKind::OpenAi), |this| {
            this.child(render_capability_checkbox(
                "supports-parallel-tool-calls",
                index,
                i18n::t!("Supports parallel_tool_calls"),
                model.supports_parallel_tool_calls,
                |model, state| model.supports_parallel_tool_calls = state,
                cx,
            ))
            .child(render_capability_checkbox(
                "supports-prompt-cache-key",
                index,
                i18n::t!("Supports prompt_cache_key"),
                model.supports_prompt_cache_key,
                |model, state| model.supports_prompt_cache_key = state,
                cx,
            ))
            .child(render_capability_checkbox(
                "supports-chat-completions",
                index,
                i18n::t!("Supports /chat/completions"),
                model.supports_chat_completions,
                |model, state| model.supports_chat_completions = state,
                cx,
            ))
            .when(model.supports_chat_completions.selected(), |this| {
                this.child(render_capability_checkbox(
                    "max-tokens-parameter",
                    index,
                    i18n::t!("Uses max_tokens for output limit"),
                    model.max_tokens_parameter,
                    |model, state| model.max_tokens_parameter = state,
                    cx,
                ))
            })
            .child(render_capability_checkbox(
                "supports-thinking",
                index,
                i18n::t!("Supports thinking"),
                model.supports_thinking,
                |model, state| model.supports_thinking = state,
                cx,
            ))
            .when(model.supports_thinking.selected(), |this| {
                this.child(render_reasoning_effort_selector(
                    model.reasoning_effort,
                    index,
                    window,
                    cx,
                ))
                .when(model.supports_chat_completions.selected(), |this| {
                    this.child(render_capability_checkbox(
                        "interleaved-reasoning",
                        index,
                        i18n::t!("Preserves thinking in chat history"),
                        model.interleaved_reasoning,
                        |model, state| model.interleaved_reasoning = state,
                        cx,
                    ))
                })
            })
        })
}

fn render_capability_checkbox(
    id: &'static str,
    index: usize,
    label: &'static str,
    state: ToggleState,
    update: fn(&mut ModelInput, ToggleState),
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    Checkbox::new((id, index), state)
        .label(label)
        .on_click(cx.listener(move |this, checked, _window, cx| {
            if let Some(form) = this.llm_provider_form.as_mut()
                && let Some(model) = form.models.get_mut(index)
            {
                update(model, *checked);
            }
            cx.notify();
        }))
}

fn render_reasoning_effort_selector(
    selected: OpenAiReasoningEffort,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let settings_window = cx.weak_entity();
    let menu = ContextMenu::build(window, cx, move |mut menu, _window, _cx| {
        for effort in OpenAiReasoningEffort::OPENAI_COMPATIBLE_SELECTABLE {
            let is_selected = effort == selected;
            let settings_window = settings_window.clone();
            menu.push_item(
                ui::ContextMenuEntry::new(effort.label())
                    .toggleable(IconPosition::End, is_selected)
                    .handler(move |_window, cx| {
                        settings_window
                            .update(cx, |this, cx| {
                                if let Some(form) = this.llm_provider_form.as_mut()
                                    && let Some(model) = form.models.get_mut(index)
                                {
                                    model.reasoning_effort = effort;
                                }
                                cx.notify();
                            })
                            .ok();
                    }),
            );
        }
        menu
    });

    v_flex()
        .gap_1()
        .child(Label::new(i18n::t!("Default reasoning effort")).size(LabelSize::Small))
        .child(
            DropdownMenu::new(
                ElementId::Name(format!("reasoning-effort-selector-{index}").into()),
                selected.label(),
                menu,
            )
            .style(DropdownStyle::Outlined)
            .trigger_size(ButtonSize::Compact)
            .full_width(true)
            .aria_label(i18n::t!("Default reasoning effort")),
        )
}

/// Header value every Anthropic Messages API request carries; mirrored from
/// `anthropic::list_models` so a compatible server sees the same version it
/// would see for a completion.
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// Characters of an error response body quoted back to the user. Enough to
/// carry a server's own JSON error message without pasting a whole HTML error
/// page into the form.
const MAX_QUOTED_ERROR_BODY: usize = 300;

#[derive(Deserialize)]
struct ModelListResponse {
    data: Vec<ModelListEntry>,
}

#[derive(Deserialize)]
struct ModelListEntry {
    id: String,
}

enum ModelsFetchError {
    BuildRequest(String),
    /// Refused by the fork's egress allowlist rather than by the network.
    EgressDenied(String),
    Connection(String),
    Unauthorized(u16),
    Http(u16, String),
    ReadBody(String),
    Parse(String),
}

impl ModelsFetchError {
    fn message(&self) -> SharedString {
        match self {
            Self::BuildRequest(error) => {
                i18n::t!("Couldn't create the request: {error}").into()
            }
            Self::EgressDenied(host) => i18n::t!(
                "The network allowlist blocked the connection to {host}. Add this host under \"Network Allowlist\" on the \"Network\" page."
            )
            .into(),
            Self::Connection(error) => i18n::t!("Couldn't connect to the server: {error}").into(),
            Self::Unauthorized(status) => i18n::t!(
                "Authentication failed ({status}). Check the API key. If the server needs no authentication, check \"Connect without an API key\"."
            )
            .into(),
            Self::Http(status, body) if body.is_empty() => {
                i18n::t!("The server returned HTTP {status}.").into()
            }
            Self::Http(status, body) => {
                i18n::t!("The server returned HTTP {status}: {body}").into()
            }
            Self::ReadBody(error) => {
                i18n::t!("Failed to read the response body: {error}").into()
            }
            Self::Parse(error) => {
                i18n::t!("Couldn't parse the model list response: {error}").into()
            }
        }
    }
}

/// The listing endpoint, derived from the base URL exactly the way the provider
/// crates derive the completion endpoint: `open_ai::stream_completion` appends
/// straight onto the configured base (which already carries `/v1` when the
/// server wants it), while `anthropic::list_models` appends the version segment
/// itself. Deriving it any other way — trimming a trailing slash, say — would
/// let the fetch succeed against a URL that later fails to complete.
fn models_endpoint(kind: CompatibleProviderKind, api_url: &str) -> String {
    match kind {
        CompatibleProviderKind::OpenAi => format!("{api_url}/models"),
        CompatibleProviderKind::Anthropic => format!("{api_url}/v1/models?limit=1000"),
    }
}

async fn fetch_model_ids(
    http_client: Arc<dyn HttpClient>,
    kind: CompatibleProviderKind,
    api_url: String,
    api_key: Option<String>,
) -> Result<Vec<String>, ModelsFetchError> {
    let mut builder =
        Request::get(models_endpoint(kind, &api_url)).header("Accept", "application/json");
    match kind {
        CompatibleProviderKind::OpenAi => {
            if let Some(api_key) = &api_key {
                builder = builder.header("Authorization", format!("Bearer {api_key}"));
            }
        }
        CompatibleProviderKind::Anthropic => {
            builder = builder.header("Anthropic-Version", ANTHROPIC_VERSION);
            if let Some(api_key) = &api_key {
                builder = builder.header("X-Api-Key", api_key.as_str());
            }
        }
    }
    let request = builder
        .body(AsyncBody::default())
        .map_err(|error| ModelsFetchError::BuildRequest(error.to_string()))?;

    let mut response = http_client.send(request).await.map_err(|error| {
        match error.downcast_ref::<EgressDenied>() {
            Some(denied) => ModelsFetchError::EgressDenied(denied.host.clone()),
            None => ModelsFetchError::Connection(error.to_string()),
        }
    })?;

    let status = response.status();
    let mut body = String::new();
    response
        .body_mut()
        .read_to_string(&mut body)
        .await
        .map_err(|error| ModelsFetchError::ReadBody(error.to_string()))?;

    if !status.is_success() {
        return Err(
            if matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN) {
                ModelsFetchError::Unauthorized(status.as_u16())
            } else {
                ModelsFetchError::Http(status.as_u16(), quoted_error_body(&body))
            },
        );
    }

    parse_model_ids(&body)
}

fn quoted_error_body(body: &str) -> String {
    let body = body.trim();
    match body.char_indices().nth(MAX_QUOTED_ERROR_BODY) {
        Some((index, _)) => format!("{}…", &body[..index]),
        None => body.to_string(),
    }
}

fn parse_model_ids(body: &str) -> Result<Vec<String>, ModelsFetchError> {
    let response: ModelListResponse =
        serde_json::from_str(body).map_err(|error| ModelsFetchError::Parse(error.to_string()))?;
    Ok(response
        .data
        .into_iter()
        .map(|entry| entry.id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect())
}

fn fetch_llm_provider_models(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window.llm_provider_form.as_ref() else {
        return;
    };
    if form.models_fetch_task.is_some() {
        return;
    }

    let kind = form.kind;
    let api_url = form.api_url.read(cx).text(cx).trim().to_string();
    let api_key = form.api_key.read(cx).text(cx).trim().to_string();
    let api_key = (!form.no_api_key && !api_key.is_empty()).then_some(api_key);

    if api_url.is_empty() {
        if let Some(form) = settings_window.llm_provider_form.as_mut() {
            form.models_fetch = Some(ModelsFetchOutcome::Error(
                i18n::t!("API URL cannot be empty").into(),
            ));
        }
        cx.notify();
        return;
    }

    let fetch = cx.background_spawn(fetch_model_ids(cx.http_client(), kind, api_url, api_key));
    let task = cx.spawn_in(window, async move |this, cx| {
        let result = fetch.await;
        this.update_in(cx, |this, window, cx| {
            let Some(form) = this.llm_provider_form.as_mut() else {
                return;
            };
            let outcome = match result {
                Ok(model_ids) => match apply_fetched_models(form, model_ids, window, cx) {
                    0 => ModelsFetchOutcome::NoneNew,
                    added => ModelsFetchOutcome::Added(added),
                },
                Err(error) => ModelsFetchOutcome::Error(error.message()),
            };
            form.models_fetch_task = None;
            form.models_fetch = Some(outcome);
            cx.notify();
        })
        .log_err();
    });

    if let Some(form) = settings_window.llm_provider_form.as_mut() {
        form.models_fetch = None;
        form.models_fetch_task = Some(task);
    }
    cx.notify();
}

/// Writes the fetched ids into the form, reusing rows the user left blank
/// before appending new ones and skipping ids that are already listed, so a
/// fetch never overwrites what was typed by hand. Returns how many rows it
/// filled.
fn apply_fetched_models(
    form: &mut LlmProviderForm,
    model_ids: Vec<String>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> usize {
    let mut listed = form
        .models
        .iter()
        .map(|model| model.name.read(cx).text(cx).trim().to_string())
        .filter(|name| !name.is_empty())
        .collect::<HashSet<_>>();

    let mut added = 0;
    for model_id in model_ids {
        if !listed.insert(model_id.clone()) {
            continue;
        }

        let blank_row = form
            .models
            .iter()
            .position(|model| model.name.read(cx).text(cx).trim().is_empty());
        let index = match blank_row {
            Some(index) => index,
            None => {
                let index = form.models.len();
                form.models.push(ModelInput::new(index, window, cx));
                index
            }
        };
        if let Some(model) = form.models.get(index) {
            model.name.update(cx, |editor, cx| {
                editor.set_text(model_id.as_str(), window, cx);
            });
        }
        added += 1;
    }
    added
}

fn render_form_error(error: SharedString) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .child(
            Icon::new(IconName::XCircle)
                .size(IconSize::Small)
                .color(Color::Error),
        )
        .child(Label::new(error).size(LabelSize::Small).color(Color::Error))
}

fn render_form_actions(cx: &mut Context<SettingsWindow>) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_1()
        .justify_end()
        .child(
            Button::new("llm-provider-form-cancel", i18n::t!("Cancel")).on_click(cx.listener(
                |this, _, window, cx| {
                    this.llm_provider_form = None;
                    this.pop_sub_page(window, cx);
                },
            )),
        )
        .child(
            Button::new("llm-provider-form-save", i18n::t!("Save Provider"))
                .style(ButtonStyle::Filled)
                .on_click(cx.listener(|this, _, window, cx| {
                    save_llm_provider_form(this, window, cx);
                })),
        )
}

struct LlmProviderFormValues {
    kind: CompatibleProviderKind,
    provider_name: String,
    api_url: String,
    api_key: String,
    no_api_key: bool,
    models: Vec<ModelValues>,
    consent_acknowledged: bool,
}

struct ModelValues {
    name: String,
    max_completion_tokens: String,
    max_output_tokens: String,
    max_tokens: String,
    reasoning_effort: OpenAiReasoningEffort,
    supports_tools: bool,
    supports_images: bool,
    supports_parallel_tool_calls: bool,
    supports_prompt_cache_key: bool,
    supports_chat_completions: bool,
    supports_thinking: bool,
    interleaved_reasoning: bool,
    max_tokens_parameter: bool,
}

enum ParsedModels {
    OpenAi(Vec<OpenAiCompatibleAvailableModel>),
    Anthropic(Vec<AnthropicCompatibleAvailableModel>),
}

fn save_llm_provider_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let values = {
        let Some(form) = settings_window.llm_provider_form.as_ref() else {
            return;
        };
        LlmProviderFormValues {
            kind: form.kind,
            provider_name: form.provider_name.read(cx).text(cx),
            api_url: form.api_url.read(cx).text(cx),
            api_key: form.api_key.read(cx).text(cx),
            no_api_key: form.no_api_key,
            models: form
                .models
                .iter()
                .map(|model| ModelValues {
                    name: model.name.read(cx).text(cx),
                    max_completion_tokens: model.max_completion_tokens.read(cx).text(cx),
                    max_output_tokens: model.max_output_tokens.read(cx).text(cx),
                    max_tokens: model.max_tokens.read(cx).text(cx),
                    reasoning_effort: model.reasoning_effort,
                    supports_tools: model.supports_tools.selected(),
                    supports_images: model.supports_images.selected(),
                    supports_parallel_tool_calls: model.supports_parallel_tool_calls.selected(),
                    supports_prompt_cache_key: model.supports_prompt_cache_key.selected(),
                    supports_chat_completions: model.supports_chat_completions.selected(),
                    supports_thinking: model.supports_thinking.selected(),
                    interleaved_reasoning: model.interleaved_reasoning.selected(),
                    max_tokens_parameter: model.max_tokens_parameter.selected(),
                })
                .collect(),
            consent_acknowledged: form.consent_acknowledged,
        }
    };

    let ValidatedLlmProviderForm {
        provider_name,
        api_url,
        api_key,
        no_api_key,
        models,
        needs_consent,
    } = match validate_llm_provider_form(&values, cx) {
        Ok(value) => value,
        Err(error) => {
            if let Some(form) = settings_window.llm_provider_form.as_mut() {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
    };

    let fs = <dyn fs::Fs>::global(cx);
    cx.spawn_in(window, async move |this, cx| {
        let result = async {
            let provider_id = LanguageModelProviderId(provider_name.clone().into());
            let settings_update = cx.update(|_window, cx| {
                settings::update_settings_file_with_completion(fs, cx, move |settings, _cx| {
                    let language_models = settings.language_models.get_or_insert_default();
                    match models {
                        ParsedModels::OpenAi(available_models) => {
                            language_models
                                .openai_compatible
                                .get_or_insert_default()
                                .insert(
                                    Arc::from(provider_name.as_str()),
                                    OpenAiCompatibleSettingsContent {
                                        api_url: api_url.clone(),
                                        available_models,
                                        custom_headers: None,
                                        no_api_key: no_api_key.then_some(true),
                                    },
                                );
                        }
                        ParsedModels::Anthropic(available_models) => {
                            language_models
                                .anthropic_compatible
                                .get_or_insert_default()
                                .insert(
                                    Arc::from(provider_name.as_str()),
                                    AnthropicCompatibleSettingsContent {
                                        api_url: api_url.clone(),
                                        available_models,
                                        custom_headers: None,
                                        no_api_key: no_api_key.then_some(true),
                                    },
                                );
                        }
                    }
                    if needs_consent {
                        // Same write as the provider itself, so a saved external
                        // provider is never left in the gated state (ADR-F10).
                        external_ai_consent::acknowledge_in(settings, &provider_name);
                    }
                })
            })?;

            settings_update
                .await
                .map_err(|_| anyhow::anyhow!("Settings update was canceled"))??;

            // A key-less provider stores nothing in the keychain; its
            // `is_authenticated` comes from the settings entry alone.
            if let Some(api_key) = api_key {
                let set_api_key = cx.update(|_window, cx| {
                    let provider = LanguageModelRegistry::read_global(cx)
                        .provider(&provider_id)
                        .ok_or_else(|| anyhow::anyhow!("Provider was not registered"))?;
                    anyhow::Ok(provider.set_api_key(Some(api_key), cx))
                })??;
                set_api_key.await?;
            }

            cx.update(|window, cx| {
                this.update(cx, |this, cx| {
                    this.provider_configuration_views.remove(&provider_id);
                    this.llm_provider_form = None;
                    this.pop_sub_page(window, cx);
                })
            })??;

            anyhow::Ok(())
        }
        .await;

        if let Err(error) = result {
            this.update(cx, |this, cx| {
                if let Some(form) = this.llm_provider_form.as_mut() {
                    form.error = Some(error.to_string().into());
                }
                cx.notify();
            })?;
        }

        anyhow::Ok(())
    })
    .detach_and_log_err(cx);
}

struct ValidatedLlmProviderForm {
    provider_name: String,
    api_url: String,
    /// `None` when the provider is configured key-less: nothing is written to
    /// the system keychain in that case.
    api_key: Option<String>,
    no_api_key: bool,
    models: ParsedModels,
    /// The endpoint is remote, so the saved provider has to be recorded as
    /// acknowledged or it would come back gated.
    needs_consent: bool,
}

fn validate_llm_provider_form(
    values: &LlmProviderFormValues,
    cx: &App,
) -> Result<ValidatedLlmProviderForm, SharedString> {
    let provider_name = values.provider_name.clone();
    if provider_name.is_empty() {
        return Err(i18n::t!("Provider Name cannot be empty").into());
    }

    if LanguageModelRegistry::read_global(cx)
        .providers()
        .iter()
        .any(|provider| {
            provider.id().0.as_ref() == provider_name.as_str()
                || provider.name().0.as_ref() == provider_name.as_str()
        })
    {
        return Err(i18n::t!("Provider Name is already taken by another provider").into());
    }

    let api_url = values.api_url.clone();
    if api_url.is_empty() {
        return Err(i18n::t!("API URL cannot be empty").into());
    }

    let needs_consent =
        external_ai_consent::form_target(&provider_name, &api_url, values.kind.label()).is_some();
    if needs_consent && !values.consent_acknowledged {
        return Err(
            i18n::t!(
                "This provider connects to an external service. Review the warning about training risks and check the box"
            )
            .into(),
        );
    }

    let api_key = validate_api_key(&values.api_key, values.no_api_key)?;

    let models = match values.kind {
        CompatibleProviderKind::OpenAi => ParsedModels::OpenAi(
            values
                .models
                .iter()
                .map(parse_open_ai_model)
                .collect::<Result<Vec<_>, _>>()?,
        ),
        CompatibleProviderKind::Anthropic => ParsedModels::Anthropic(
            values
                .models
                .iter()
                .map(parse_anthropic_model)
                .collect::<Result<Vec<_>, _>>()?,
        ),
    };

    let mut model_names = HashSet::new();
    let model_names_are_unique = match &models {
        ParsedModels::OpenAi(models) => models
            .iter()
            .all(|model| model_names.insert(model.name.clone())),
        ParsedModels::Anthropic(models) => models
            .iter()
            .all(|model| model_names.insert(model.name.clone())),
    };
    if !model_names_are_unique {
        return Err(i18n::t!("Model Names must be unique").into());
    }

    Ok(ValidatedLlmProviderForm {
        provider_name,
        api_url,
        api_key,
        no_api_key: values.no_api_key,
        models,
        needs_consent,
    })
}

/// The key is dropped entirely when the provider is key-less, so a value typed
/// before the checkbox was ticked never reaches the system keychain.
fn validate_api_key(api_key: &str, no_api_key: bool) -> Result<Option<String>, SharedString> {
    if no_api_key {
        return Ok(None);
    }
    if api_key.is_empty() {
        return Err(i18n::t!("API Key cannot be empty").into());
    }
    Ok(Some(api_key.to_string()))
}

fn parse_model_name(model: &ModelValues) -> Result<String, SharedString> {
    if model.name.is_empty() {
        return Err(i18n::t!("Model Name cannot be empty").into());
    }
    Ok(model.name.clone())
}

fn parse_open_ai_model(
    model: &ModelValues,
) -> Result<OpenAiCompatibleAvailableModel, SharedString> {
    Ok(OpenAiCompatibleAvailableModel {
        name: parse_model_name(model)?,
        display_name: None,
        max_completion_tokens: Some(parse_u64_field(
            &model.max_completion_tokens,
            i18n::t!("Max Completion Tokens"),
        )?),
        max_output_tokens: Some(parse_u64_field(
            &model.max_output_tokens,
            i18n::t!("Max Output Tokens"),
        )?),
        max_tokens: parse_u64_field(&model.max_tokens, i18n::t!("Max Tokens"))?,
        reasoning_effort: model.supports_thinking.then_some(model.reasoning_effort),
        capabilities: OpenAiCompatibleModelCapabilities {
            tools: model.supports_tools,
            images: model.supports_images,
            parallel_tool_calls: model.supports_parallel_tool_calls,
            prompt_cache_key: model.supports_prompt_cache_key,
            chat_completions: model.supports_chat_completions,
            interleaved_reasoning: model.supports_thinking
                && model.supports_chat_completions
                && model.interleaved_reasoning,
            max_tokens_parameter: model.supports_chat_completions && model.max_tokens_parameter,
        },
    })
}

fn parse_anthropic_model(
    model: &ModelValues,
) -> Result<AnthropicCompatibleAvailableModel, SharedString> {
    Ok(AnthropicCompatibleAvailableModel {
        name: parse_model_name(model)?,
        display_name: None,
        max_tokens: parse_u64_field(&model.max_tokens, i18n::t!("Max Tokens"))?,
        tool_override: None,
        max_output_tokens: Some(parse_u64_field(
            &model.max_output_tokens,
            i18n::t!("Max Output Tokens"),
        )?),
        default_temperature: None,
        extra_beta_headers: Vec::new(),
        mode: None,
        capabilities: AnthropicCompatibleModelCapabilities {
            tools: model.supports_tools,
            images: model.supports_images,
            prompt_caching: false,
        },
    })
}

fn parse_u64_field(value: &str, name: &str) -> Result<u64, SharedString> {
    value
        .parse::<u64>()
        .map_err(|_| i18n::t!("{name} must be a number").into())
}

#[cfg(test)]
mod jp_enhance_tests {
    use super::*;

    /// ADR-F49 決定 19 (8c): Claude Code の「常にサンドボックスの外で起動」は
    /// `agent.claude_code_run_outside_sandbox` に書かれ、読み戻せる。
    #[gpui::test]
    async fn the_claude_code_switch_writes_a_setting_that_reads_back(
        cx: &mut gpui::TestAppContext,
    ) {
        use fs::Fs as _;

        let fs = fs::FakeFs::new(cx.executor());
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            <dyn fs::Fs>::set_global(fs.clone(), cx);
        });
        fs.create_dir(
            paths::settings_file()
                .parent()
                .expect("settings file has a parent directory"),
        )
        .await
        .expect("settings directory can be created");
        fs.save(
            paths::settings_file(),
            &"{}".into(),
            language::LineEnding::Unix,
        )
        .await
        .expect("settings file can be written");

        for enabled in [true, false] {
            cx.update(|cx| set_claude_code_run_outside_sandbox(enabled, cx));
            cx.run_until_parked();
            let text = fs
                .load(paths::settings_file())
                .await
                .expect("settings file can be read");
            let content: serde_json::Value =
                serde_json::from_str(&text).expect("settings file is JSON");
            assert_eq!(
                content
                    .get("agent")
                    .and_then(|agent| agent.get("claude_code_run_outside_sandbox"))
                    .and_then(serde_json::Value::as_bool),
                Some(enabled),
                "{text}"
            );
        }
    }

    #[test]
    fn the_claude_subscription_card_sits_where_the_chatgpt_one_does() {
        // The registry is keyed by provider id, so this is the order the page
        // renders: `openai-subscribed` right after `openai`.
        let providers = ["zed.dev", "anthropic", "copilot_chat", "openai"]
            .into_iter()
            .map(SharedString::new_static)
            .collect::<Vec<_>>();
        let index = claude_subscription_card_index(&providers);
        assert_eq!(
            providers.get(index - 1).map(|id| id.to_string()),
            Some("anthropic".to_string())
        );
        assert_eq!(index, 2, "the card belongs directly after Anthropic");

        // Anthropic hidden by an extension: the card is the only route to a
        // Claude subscription, so it must still be drawn.
        let without_anthropic = ["zed.dev", "openai"]
            .into_iter()
            .map(SharedString::new_static)
            .collect::<Vec<_>>();
        assert_eq!(
            claude_subscription_card_index(&without_anthropic),
            without_anthropic.len()
        );
        assert_eq!(claude_subscription_card_index(&[]), 0);
    }

    #[test]
    fn an_empty_api_key_is_only_accepted_when_the_provider_is_key_less() {
        assert!(validate_api_key("", false).is_err());
        assert_eq!(
            validate_api_key("sk-secret", false),
            Ok(Some("sk-secret".to_string()))
        );
        // The typed value is deliberately discarded rather than stored.
        assert_eq!(validate_api_key("", true), Ok(None));
        assert_eq!(validate_api_key("sk-secret", true), Ok(None));
    }

    #[test]
    fn the_models_endpoint_is_appended_the_way_completions_are() {
        assert_eq!(
            models_endpoint(CompatibleProviderKind::OpenAi, "https://api.openai.com/v1"),
            "https://api.openai.com/v1/models"
        );
        assert_eq!(
            models_endpoint(CompatibleProviderKind::OpenAi, "http://llm.corp:8000/v1"),
            "http://llm.corp:8000/v1/models"
        );
        assert_eq!(
            models_endpoint(
                CompatibleProviderKind::Anthropic,
                "https://api.anthropic.com"
            ),
            "https://api.anthropic.com/v1/models?limit=1000"
        );
    }

    #[test]
    fn model_ids_are_read_from_the_data_array_of_either_api() {
        let open_ai = r#"{
            "object": "list",
            "data": [
                { "id": "gpt-5", "object": "model", "owned_by": "openai" },
                { "id": " qwen3-32b ", "object": "model" },
                { "id": "  ", "object": "model" }
            ]
        }"#;
        assert_eq!(
            parse_model_ids(open_ai).ok(),
            Some(vec!["gpt-5".to_string(), "qwen3-32b".to_string()])
        );

        let anthropic = r#"{
            "data": [
                { "type": "model", "id": "claude-opus-4-1", "display_name": "Claude Opus 4.1" }
            ],
            "has_more": false
        }"#;
        assert_eq!(
            parse_model_ids(anthropic).ok(),
            Some(vec!["claude-opus-4-1".to_string()])
        );

        assert!(parse_model_ids("not json").is_err());
        assert!(parse_model_ids(r#"{"models": []}"#).is_err());
    }

    #[test]
    fn a_quoted_error_body_is_truncated_on_a_character_boundary() {
        assert_eq!(quoted_error_body("  boom  "), "boom");
        let long = "あ".repeat(MAX_QUOTED_ERROR_BODY + 10);
        let quoted = quoted_error_body(&long);
        assert_eq!(quoted.chars().count(), MAX_QUOTED_ERROR_BODY + 1);
        assert!(quoted.ends_with('…'));
    }

    #[test]
    fn key_less_is_off_unless_the_settings_entry_says_otherwise() {
        let without = serde_json::from_str::<OpenAiCompatibleSettingsContent>(
            r#"{ "api_url": "http://llm.corp:8000/v1", "available_models": [] }"#,
        )
        .expect("settings entry should parse");
        assert_eq!(without.no_api_key, None);

        let with = serde_json::from_str::<OpenAiCompatibleSettingsContent>(
            r#"{ "api_url": "http://llm.corp:8000/v1", "available_models": [], "no_api_key": true }"#,
        )
        .expect("settings entry should parse");
        assert_eq!(with.no_api_key, Some(true));

        // A key-less provider is the only one whose saved entry carries the
        // flag at all, so existing configurations round-trip unchanged.
        let serialized = serde_json::to_value(&without).expect("settings entry should serialize");
        assert_eq!(serialized.get("no_api_key"), None);
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, VisualTestContext, size};
    use language_models::provider::cloud;
    use settings::SettingsStore;

    use super::*;

    struct YoungAccountProviderRow;

    impl Render for YoungAccountProviderRow {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div().size_full().p_4().child(
                div()
                    .w_full()
                    .debug_selector(|| "provider-row".into())
                    .child(render_inline_body(
                        "Zed".into(),
                        Some("Subscribed to Business".into()),
                        Some(InlineDescription::Text(
                            "You have access to Zed's hosted models through your organization."
                                .into(),
                        )),
                        cloud::test_support::young_account_configuration(),
                    )),
            )
        }
    }

    #[gpui::test]
    fn young_account_configuration_stays_within_provider_row(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        for width in [500., 600., 800.] {
            let window = cx.open_window(size(px(width), px(400.)), |_window, _cx| {
                YoungAccountProviderRow
            });
            cx.run_until_parked();

            let mut visual_context = VisualTestContext::from_window(window.into(), cx);
            let provider_row_bounds = visual_context
                .debug_bounds("provider-row")
                .expect("provider row should be rendered");
            let description_bounds = visual_context
                .debug_bounds("inline-provider-description")
                .expect("provider description should be rendered");
            let configuration_bounds = visual_context
                .debug_bounds("zed-ai-configuration")
                .expect("Zed AI configuration should be rendered");

            assert!(
                configuration_bounds.right() <= provider_row_bounds.right(),
                "young account configuration extends past the provider row at {width}px"
            );
            assert!(
                configuration_bounds.size.height >= px(80.),
                "young account warning does not wrap at {width}px"
            );
            assert!(
                description_bounds.size.width >= px(width / 3.),
                "provider description collapsed at {width}px"
            );
        }
    }
}
