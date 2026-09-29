use agent::{AgentTool, TerminalTool, ToolPermissionDecision};
use agent_settings::AgentSettings;
use gpui::{
    Focusable, HighlightStyle, ReadGlobal, ScrollHandle, StyledText, TextStyleRefinement, point,
    prelude::*,
};
use settings::{Settings as _, SettingsStore, ToolPermissionMode};
use shell_command_parser::extract_commands;
use std::sync::Arc;
use theme_settings::ThemeSettings;
use ui::{Banner, ContextMenu, Divider, PopoverMenu, Severity, Tooltip, prelude::*};
use util::ResultExt as _;
use util::shell::ShellKind;

use crate::{SettingsWindow, components::SettingsInputField};

const HARDCODED_RULES_DESCRIPTION: &str =
    i18n::msg!("`rm -rf` commands are always blocked when run on `$HOME`, `~`, `.`, `..`, or `/`");
const SETTINGS_DISCLAIMER: &str = i18n::msg!(
    "Note: custom tool permissions only apply to the Zed native agent and don’t extend to external agents connected through the Agent Client Protocol (ACP)."
);

/// Tools that support permission rules
const TOOLS: &[ToolInfo] = &[
    ToolInfo {
        id: "terminal",
        name: || i18n::tc!("agent tool", "Terminal"),
        description: i18n::msg!("Commands executed in the terminal"),
        regex_explanation: i18n::msg!(
            "Patterns are matched against each command in the input. Commands chained with &&, ||, ;, or pipes are split and checked individually."
        ),
    },
    ToolInfo {
        id: "edit_file",
        name: || i18n::tc!("agent tool", "Edit File"),
        description: i18n::msg!("File editing operations"),
        regex_explanation: i18n::msg!("Patterns are matched against the file path being edited."),
    },
    ToolInfo {
        id: "write_file",
        name: || i18n::tc!("agent tool", "Write File"),
        description: i18n::msg!("File creation and overwrite operations"),
        regex_explanation: i18n::msg!("Patterns are matched against the file path being written."),
    },
    ToolInfo {
        id: "delete_path",
        name: || i18n::tc!("agent tool", "Delete Path"),
        description: i18n::msg!("File and directory deletion"),
        regex_explanation: i18n::msg!("Patterns are matched against the path being deleted."),
    },
    ToolInfo {
        id: "copy_path",
        name: || i18n::tc!("agent tool", "Copy Path"),
        description: i18n::msg!("File and directory copying"),
        regex_explanation: i18n::msg!(
            "Patterns are matched independently against the source path and the destination path. Enter either path below to test."
        ),
    },
    ToolInfo {
        id: "move_path",
        name: || i18n::tc!("agent tool", "Move Path"),
        description: i18n::msg!("File and directory moves/renames"),
        regex_explanation: i18n::msg!(
            "Patterns are matched independently against the source path and the destination path. Enter either path below to test."
        ),
    },
    ToolInfo {
        id: "create_directory",
        name: || i18n::tc!("agent tool", "Create Directory"),
        description: i18n::msg!("Directory creation"),
        regex_explanation: i18n::msg!(
            "Patterns are matched against the directory path being created."
        ),
    },
    ToolInfo {
        id: "apply_code_action",
        name: || i18n::tc!("agent tool", "Apply Code Action"),
        description: i18n::msg!("Applying language server code actions"),
        regex_explanation: i18n::msg!(
            "Patterns are matched against the name of the code action. Its edits are written straight into the buffer without going through diff review."
        ),
    },
    ToolInfo {
        id: "rename_symbol",
        name: || i18n::tc!("agent tool", "Rename Symbol"),
        description: i18n::msg!("Project-wide symbol renames by the language server"),
        regex_explanation: i18n::msg!(
            "Patterns are matched against the symbol's name before the rename. The rename skips diff review, and every file that references the symbol is saved."
        ),
    },
    ToolInfo {
        id: "fetch",
        name: || i18n::tc!("agent tool", "Fetch"),
        description: i18n::msg!("HTTP requests to URLs"),
        regex_explanation: i18n::msg!("Patterns are matched against the URL being fetched."),
    },
    ToolInfo {
        id: "search_web",
        name: || i18n::tc!("agent tool", "Web Search"),
        description: i18n::msg!("Web search queries"),
        regex_explanation: i18n::msg!("Patterns are matched against the search query."),
    },
    ToolInfo {
        id: "skill",
        name: || i18n::tc!("agent tool", "Skill"),
        description: i18n::msg!("Loading agent skill instructions"),
        regex_explanation: i18n::msg!(
            "Patterns are matched against the absolute path to the skill's SKILL.md file."
        ),
    },
];

pub(crate) struct ToolInfo {
    id: &'static str,
    /// The tool's display name, translated when called.
    name: fn() -> &'static str,
    description: &'static str,
    regex_explanation: &'static str,
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

/// Finds the index of a tool in `TOOLS` by its ID. Panics (compile error in
/// const context) if the ID is not found, so every macro-generated render
/// function is validated at compile time.
const fn tool_index(id: &str) -> usize {
    let mut i = 0;
    while i < TOOLS.len() {
        if const_str_eq(TOOLS[i].id, id) {
            return i;
        }
        i += 1;
    }
    panic!("tool ID not found in TOOLS array")
}

/// Parses a string containing backtick-delimited code spans into a `StyledText`
/// with code background highlights applied to each span.
fn render_inline_code_markdown(text: &str, cx: &App) -> StyledText {
    let code_background = cx.theme().colors().surface_background;
    let mut plain = String::new();
    let mut highlights: Vec<(std::ops::Range<usize>, HighlightStyle)> = Vec::new();
    let mut in_code = false;
    let mut code_start = 0;

    for ch in text.chars() {
        if ch == '`' {
            if in_code {
                highlights.push((
                    code_start..plain.len(),
                    HighlightStyle {
                        background_color: Some(code_background),
                        ..Default::default()
                    },
                ));
            } else {
                code_start = plain.len();
            }
            in_code = !in_code;
        } else {
            plain.push(ch);
        }
    }

    StyledText::new(plain).with_highlights(highlights)
}

/// Renders the main tool permissions setup page showing a list of tools
pub(crate) fn render_tool_permissions_setup_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let tool_items: Vec<AnyElement> = TOOLS
        .iter()
        .enumerate()
        .map(|(i, tool)| render_tool_list_item(settings_window, tool, i, window, cx))
        .collect();

    let settings = AgentSettings::get_global(cx);
    let global_default = settings.tool_permissions.default;
    // A tool-specific "auto" runs the judge just as the global default does, so
    // the model row has to be reachable then too.
    let judge_model_is_used = global_default == ToolPermissionMode::Auto
        || settings
            .tool_permissions
            .tools
            .values()
            .any(|rules| rules.default == Some(ToolPermissionMode::Auto));

    let scroll_step = px(40.);

    v_flex()
        .id("tool-permissions-page")
        .on_action({
            let scroll_handle = scroll_handle.clone();
            move |_: &menu::SelectNext, window, cx| {
                window.focus_next(cx);
                let current_offset = scroll_handle.offset();
                scroll_handle.set_offset(point(current_offset.x, current_offset.y - scroll_step));
            }
        })
        .on_action({
            let scroll_handle = scroll_handle.clone();
            move |_: &menu::SelectPrevious, window, cx| {
                window.focus_prev(cx);
                let current_offset = scroll_handle.offset();
                scroll_handle.set_offset(point(current_offset.x, current_offset.y + scroll_step));
            }
        })
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            Banner::new().child(
                Label::new(i18n::tr(SETTINGS_DISCLAIMER))
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .mt_0p5(),
            ),
        )
        .child(
            v_flex()
                .child(render_global_default_mode_section(global_default))
                .children(judge_model_is_used.then(|| render_permission_judge_model_section(cx)))
                .child(Divider::horizontal())
                .children(tool_items.into_iter().enumerate().flat_map(|(i, item)| {
                    let mut elements: Vec<AnyElement> = vec![item];
                    if i + 1 < TOOLS.len() {
                        elements.push(Divider::horizontal().into_any_element());
                    }
                    elements
                })),
        )
        .into_any_element()
}

fn render_tool_list_item(
    _settings_window: &SettingsWindow,
    tool: &'static ToolInfo,
    tool_index: usize,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let rules = get_tool_rules(tool.id, cx);
    let rule_count =
        rules.always_allow.len() + rules.always_deny.len() + rules.always_confirm.len();
    let invalid_count = rules.invalid_patterns.len();

    let rule_summary = if rule_count > 0 || invalid_count > 0 {
        let mut parts = Vec::new();
        if rule_count > 0 {
            parts.push(i18n::tn!(rule_count, "{count} rule", "{count} rules"));
        }
        if invalid_count > 0 {
            parts.push(i18n::t!("{} invalid", invalid_count));
        }
        Some(parts.join(", "))
    } else {
        None
    };

    let render_fn = get_tool_render_fn(tool.id);

    h_flex()
        .w_full()
        .min_w_0()
        .py_3()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .child(h_flex().gap_1().child(Label::new((tool.name)())).when_some(
                    rule_summary,
                    |this, summary| {
                        this.child(
                            Label::new(summary)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    },
                ))
                .child(
                    Label::new(i18n::tr(tool.description))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child({
            let tool_name = tool.name;
            Button::new(format!("configure-{}", tool.id), i18n::t!("Configure"))
                .tab_index(tool_index as isize)
                .style(ButtonStyle::OutlinedGhost)
                .size(ButtonSize::Medium)
                .end_icon(
                    Icon::new(IconName::ChevronRight)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.push_dynamic_sub_page(
                        tool_name(),
                        "Tool Permissions",
                        None,
                        true,
                        render_fn,
                        window,
                        cx,
                    );
                }))
        })
        .into_any_element()
}

fn get_tool_render_fn(
    tool_id: &str,
) -> fn(&SettingsWindow, &ScrollHandle, &mut Window, &mut Context<SettingsWindow>) -> AnyElement {
    match tool_id {
        "terminal" => render_terminal_tool_config,
        "edit_file" => render_edit_file_tool_config,
        "write_file" => render_write_file_tool_config,
        "delete_path" => render_delete_path_tool_config,
        "copy_path" => render_copy_path_tool_config,
        "move_path" => render_move_path_tool_config,
        "create_directory" => render_create_directory_tool_config,
        "fetch" => render_fetch_tool_config,
        "search_web" => render_web_search_tool_config,
        "skill" => render_skill_tool_config,
        "apply_code_action" => render_apply_code_action_tool_config,
        "rename_symbol" => render_rename_symbol_tool_config,
        _ => render_terminal_tool_config, // fallback
    }
}

/// Renders an individual tool's permission configuration page
pub(crate) fn render_tool_config_page(
    tool: &ToolInfo,
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let rules = get_tool_rules(tool.id, cx);
    let page_title = i18n::t!("{} Tool", (tool.name)());
    let scroll_step = px(80.);

    v_flex()
        .id(format!("tool-config-page-{}", tool.id))
        .on_action({
            let scroll_handle = scroll_handle.clone();
            move |_: &menu::SelectNext, window, cx| {
                window.focus_next(cx);
                let current_offset = scroll_handle.offset();
                scroll_handle.set_offset(point(current_offset.x, current_offset.y - scroll_step));
            }
        })
        .on_action({
            let scroll_handle = scroll_handle.clone();
            move |_: &menu::SelectPrevious, window, cx| {
                window.focus_prev(cx);
                let current_offset = scroll_handle.offset();
                scroll_handle.set_offset(point(current_offset.x, current_offset.y + scroll_step));
            }
        })
        .min_w_0()
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            v_flex()
                .min_w_0()
                .child(Label::new(page_title).size(LabelSize::Large))
                .child(
                    Label::new(i18n::tr(tool.regex_explanation))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .when(tool.id == TerminalTool::NAME, |this| {
            this.child(render_hardcoded_security_banner(cx))
        })
        .child(render_verification_section(tool.id, window, cx))
        .when_some(
            settings_window.regex_validation_error.clone(),
            |this, error| {
                this.child(
                    Banner::new()
                        .severity(Severity::Warning)
                        .child(Label::new(error).size(LabelSize::Small))
                        .action_slot(
                            Button::new("dismiss-regex-error", i18n::t!("Dismiss"))
                                .style(ButtonStyle::Tinted(ui::TintColor::Warning))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.regex_validation_error = None;
                                    cx.notify();
                                })),
                        ),
                )
            },
        )
        .child(
            v_flex()
                .mt_6()
                .min_w_0()
                .w_full()
                .gap_5()
                .child(render_default_mode_section(tool.id, rules.default, cx))
                .child(Divider::horizontal().color(ui::DividerColor::BorderFaded))
                .child(render_rule_section(
                    tool.id,
                    i18n::t!("Always Deny"),
                    i18n::t!("If any of these regexes match, the tool action will be denied."),
                    ToolPermissionMode::Deny,
                    &rules.always_deny,
                    cx,
                ))
                .child(Divider::horizontal().color(ui::DividerColor::BorderFaded))
                .child(render_rule_section(
                    tool.id,
                    i18n::t!("Always Allow"),
                    i18n::t!(
                        "If any of these regexes match, the action will be approved—unless an Always Confirm or Always Deny matches."
                    ),
                    ToolPermissionMode::Allow,
                    &rules.always_allow,
                    cx,
                ))
                .child(Divider::horizontal().color(ui::DividerColor::BorderFaded))
                .child(render_rule_section(
                    tool.id,
                    i18n::t!("Always Confirm"),
                    i18n::t!(
                        "If any of these regexes match, a confirmation will be shown unless an Always Deny regex matches."
                    ),
                    ToolPermissionMode::Confirm,
                    &rules.always_confirm,
                    cx,
                ))
                .when(!rules.invalid_patterns.is_empty(), |this| {
                    this.child(Divider::horizontal().color(ui::DividerColor::BorderFaded))
                        .child(render_invalid_patterns_section(
                            tool.id,
                            &rules.invalid_patterns,
                            cx,
                        ))
                }),
        )
        .into_any_element()
}

fn render_hardcoded_rules(smaller_font_size: bool, cx: &App) -> AnyElement {
    div()
        .map(|this| {
            if smaller_font_size {
                this.text_xs()
            } else {
                this.text_sm()
            }
        })
        .text_color(cx.theme().colors().text_muted)
        .child(render_inline_code_markdown(
            i18n::tr(HARDCODED_RULES_DESCRIPTION),
            cx,
        ))
        .into_any_element()
}

fn render_hardcoded_security_banner(cx: &mut Context<SettingsWindow>) -> AnyElement {
    div()
        .mt_3()
        .child(Banner::new().child(render_hardcoded_rules(false, cx)))
        .into_any_element()
}

fn render_verification_section(
    tool_id: &'static str,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let input_id = format!("{}-verification-input", tool_id);

    let editor = window.use_keyed_state(input_id, cx, |window, cx| {
        let mut editor = editor::Editor::single_line(window, cx);
        editor.set_placeholder_text(
            i18n::t!("Enter a tool input to test your rules…"),
            window,
            cx,
        );

        let global_settings = ThemeSettings::get_global(cx);
        editor.set_text_style_refinement(TextStyleRefinement {
            font_family: Some(global_settings.buffer_font.family.clone()),
            font_size: Some(rems(0.75).into()),
            ..Default::default()
        });

        editor
    });

    cx.observe(&editor, |_, _, cx| cx.notify()).detach();

    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);

    let current_text = editor.read(cx).text(cx);
    // ADR-F46: with the sandbox switched off, terminal commands run with Zed's
    // own permissions and are never auto-approved.
    let outside_sandbox = tool_id == TerminalTool::NAME
        && AgentSettings::get_global(cx)
            .sandbox_permissions
            .allow_unsandboxed;
    let (decision, matched_patterns) = if current_text.is_empty() {
        (None, Vec::new())
    } else {
        let matches = find_matched_patterns(tool_id, &current_text, cx);
        let decision = evaluate_test_input(tool_id, &current_text, outside_sandbox, cx);
        (Some(decision), matches)
    };

    let default_mode = get_tool_rules(tool_id, cx).default;
    let is_hardcoded_denial = decision.as_ref().is_some_and(is_hardcoded_denial);
    let denial_reason = match &decision {
        Some(ToolPermissionDecision::Deny(reason))
            if !reason.is_empty() && !is_hardcoded_denial =>
        {
            Some(reason.clone())
        }
        _ => None,
    };
    let (authoritative_mode, patterns_agree) = match &decision {
        Some(decision) => {
            let authoritative = decision_to_mode(decision);
            let implied = match implied_mode_from_patterns(&matched_patterns, default_mode) {
                // Outside the sandbox an allow rule only lets the command be
                // asked for instead of refused; it never approves it, and the
                // AI judge never gets to either.
                ToolPermissionMode::Allow | ToolPermissionMode::Auto if outside_sandbox => {
                    ToolPermissionMode::Confirm
                }
                implied => implied,
            };
            let agrees = authoritative == implied;
            if !agrees {
                log::error!(
                    "Tool permission verdict disagreement for '{}': \
                     engine={}, pattern_preview={}. \
                     Showing authoritative verdict only.",
                    tool_id,
                    mode_display_label(authoritative),
                    mode_display_label(implied),
                );
            }
            (Some(authoritative), agrees)
        }
        None => (None, true),
    };

    let color = cx.theme().colors();
    let border_focused = color.border_focused;

    v_flex()
        .mt_3()
        .min_w_0()
        .gap_2()
        .child(
            v_flex()
                .p_2p5()
                .gap_1p5()
                .bg(color.surface_background.opacity(0.15))
                .border_1()
                .border_dashed()
                .border_color(color.border_variant)
                .rounded_sm()
                .child(
                    Label::new(i18n::t!("Test Your Rules"))
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .child(
                    // The frame is rendered unfocused and the focused border comes
                    // from the element's focus style, so no `Window` is needed here.
                    ui::input_frame(false, cx)
                        .w_full()
                        .track_focus(&focus_handle)
                        .focus(move |style| style.border_color(border_focused))
                        .child(editor),
                )
                .when_some(authoritative_mode, |this, mode| {
                    this.when(patterns_agree, |this| {
                        if matched_patterns.is_empty() {
                            this.child(
                                Label::new(i18n::t!("No regex matches, using the default action."))
                                    .size(LabelSize::Small)
                                    .color(Color::Muted),
                            )
                        } else {
                            this.child(render_matched_patterns(&matched_patterns, cx))
                        }
                    })
                    .when(!patterns_agree, |this| {
                        if is_hardcoded_denial {
                            this.child(render_hardcoded_rules(true, cx))
                        } else if let Some(reason) = &denial_reason {
                            this.child(
                                Label::new(i18n::t!("Denied: {}", reason))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Warning),
                            )
                        } else {
                            this.child(
                                Label::new(
                                    i18n::t!(
                                        "Pattern preview differs from engine — showing authoritative result."
                                    ),
                                )
                                .size(LabelSize::XSmall)
                                .color(Color::Warning),
                            )
                        }
                    })
                    .when(is_hardcoded_denial && patterns_agree, |this| {
                        this.child(render_hardcoded_rules(true, cx))
                    })
                    .child(render_verdict_label(mode))
                    .when(outside_sandbox, |this| {
                        this.child(
                            Label::new(
                                i18n::t!(
                                    "The sandbox is disabled, so terminal commands are confirmed every time, even with \"Allow\"."
                                ),
                            )
                            .size(LabelSize::XSmall)
                            .color(Color::Muted),
                        )
                    })
                    .when_some(
                        denial_reason.filter(|_| patterns_agree && !is_hardcoded_denial),
                        |this, reason| {
                            this.child(
                                Label::new(i18n::t!("Reason: {}", reason))
                                    .size(LabelSize::XSmall)
                                    .color(Color::Error),
                            )
                        },
                    )
                }),
        )
        .into_any_element()
}

#[derive(Clone, Debug)]
struct MatchedPattern {
    pattern: String,
    rule_type: ToolPermissionMode,
    is_overridden: bool,
}

fn find_matched_patterns(tool_id: &str, input: &str, cx: &App) -> Vec<MatchedPattern> {
    let settings = AgentSettings::get_global(cx);
    let rules = match settings.tool_permissions.tools.get(tool_id) {
        Some(rules) => rules,
        None => return Vec::new(),
    };

    let mut matched = Vec::new();

    // For terminal commands, parse chained commands (&&, ||, ;) so the preview
    // matches the real permission engine's behavior.
    // When parsing fails (extract_commands returns None), the real engine
    // ignores always_allow rules, so we track parse success to mirror that.
    let (inputs_to_check, allow_enabled) = if tool_id == TerminalTool::NAME {
        match extract_commands(input) {
            Some(cmds) => (cmds, true),
            None => (vec![input.to_string()], false),
        }
    } else {
        (vec![input.to_string()], true)
    };

    let mut has_deny_match = false;
    let mut has_confirm_match = false;

    for rule in &rules.always_deny {
        if inputs_to_check.iter().any(|cmd| rule.is_match(cmd)) {
            has_deny_match = true;
            matched.push(MatchedPattern {
                pattern: rule.pattern.clone(),
                rule_type: ToolPermissionMode::Deny,
                is_overridden: false,
            });
        }
    }

    for rule in &rules.always_confirm {
        if inputs_to_check.iter().any(|cmd| rule.is_match(cmd)) {
            has_confirm_match = true;
            matched.push(MatchedPattern {
                pattern: rule.pattern.clone(),
                rule_type: ToolPermissionMode::Confirm,
                is_overridden: has_deny_match,
            });
        }
    }

    // The real engine requires ALL commands to match at least one allow
    // pattern for the overall verdict to be Allow. Compute that first,
    // then show individual patterns with correct override status.
    let all_commands_matched_allow = !inputs_to_check.is_empty()
        && inputs_to_check
            .iter()
            .all(|cmd| rules.always_allow.iter().any(|rule| rule.is_match(cmd)));

    for rule in &rules.always_allow {
        if inputs_to_check.iter().any(|cmd| rule.is_match(cmd)) {
            matched.push(MatchedPattern {
                pattern: rule.pattern.clone(),
                rule_type: ToolPermissionMode::Allow,
                is_overridden: !allow_enabled
                    || has_deny_match
                    || has_confirm_match
                    || !all_commands_matched_allow,
            });
        }
    }

    matched
}

fn render_matched_patterns(patterns: &[MatchedPattern], cx: &App) -> AnyElement {
    v_flex()
        .gap_1()
        .children(patterns.iter().map(|pattern| {
            let (type_label, color) = match pattern.rule_type {
                ToolPermissionMode::Deny => (i18n::t!("Always Deny"), Color::Error),
                ToolPermissionMode::Confirm => (i18n::t!("Always Confirm"), Color::Warning),
                ToolPermissionMode::Allow => (i18n::t!("Always Allow"), Color::Success),
                // Only the three pattern lists produce matches; "AI が判定" is a
                // default, never a pattern.
                ToolPermissionMode::Auto => {
                    (mode_display_label(ToolPermissionMode::Auto), Color::Info)
                }
            };

            let type_color = if pattern.is_overridden {
                Color::Muted
            } else {
                color
            };

            h_flex()
                .gap_1()
                .child(
                    Label::new(pattern.pattern.clone())
                        .size(LabelSize::Small)
                        .color(Color::Muted)
                        .buffer_font(cx)
                        .when(pattern.is_overridden, |this| this.strikethrough()),
                )
                .child(
                    Icon::new(IconName::Dash)
                        .size(IconSize::Small)
                        .color(Color::Custom(cx.theme().colors().icon_muted.opacity(0.4))),
                )
                .child(
                    Label::new(type_label)
                        .size(LabelSize::XSmall)
                        .color(type_color)
                        .when(pattern.is_overridden, |this| {
                            this.strikethrough().alpha(0.5)
                        }),
                )
        }))
        .into_any_element()
}

fn evaluate_test_input(
    tool_id: &str,
    input: &str,
    outside_sandbox: bool,
    cx: &App,
) -> ToolPermissionDecision {
    let settings = AgentSettings::get_global(cx);

    // A terminal command always goes through `decide_terminal_permission`, so
    // the preview shows what actually happens at either execution location
    // (in particular, a sandboxed command's shell substitution is not refused
    // outright).
    if tool_id == TerminalTool::NAME {
        return agent::decide_terminal_permission(
            &[input.to_string()],
            &settings.tool_permissions,
            ShellKind::system(),
            if outside_sandbox {
                agent::TerminalExecution::Unsandboxed
            } else {
                agent::TerminalExecution::Sandboxed
            },
        );
    }

    // ShellKind is only used for terminal tool's hardcoded security rules;
    // for other tools, the check returns None immediately.
    ToolPermissionDecision::from_input(
        tool_id,
        &[input.to_string()],
        &settings.tool_permissions,
        ShellKind::system(),
    )
}

/// The built-in rules deny with one fixed message; any other denial comes from
/// the user's own patterns and shows its reason.
fn is_hardcoded_denial(decision: &ToolPermissionDecision) -> bool {
    matches!(
        decision,
        ToolPermissionDecision::Deny(reason)
            if reason.contains("built-in security rule")
    )
}

fn decision_to_mode(decision: &ToolPermissionDecision) -> ToolPermissionMode {
    match decision {
        ToolPermissionDecision::Allow => ToolPermissionMode::Allow,
        ToolPermissionDecision::Deny(_) => ToolPermissionMode::Deny,
        ToolPermissionDecision::Confirm => ToolPermissionMode::Confirm,
        ToolPermissionDecision::Judge => ToolPermissionMode::Auto,
    }
}

fn implied_mode_from_patterns(
    patterns: &[MatchedPattern],
    default_mode: ToolPermissionMode,
) -> ToolPermissionMode {
    let has_active_deny = patterns
        .iter()
        .any(|p| matches!(p.rule_type, ToolPermissionMode::Deny) && !p.is_overridden);
    let has_active_confirm = patterns
        .iter()
        .any(|p| matches!(p.rule_type, ToolPermissionMode::Confirm) && !p.is_overridden);
    let has_active_allow = patterns
        .iter()
        .any(|p| matches!(p.rule_type, ToolPermissionMode::Allow) && !p.is_overridden);

    if has_active_deny {
        ToolPermissionMode::Deny
    } else if has_active_confirm {
        ToolPermissionMode::Confirm
    } else if has_active_allow {
        ToolPermissionMode::Allow
    } else {
        default_mode
    }
}

fn mode_display_label(mode: ToolPermissionMode) -> &'static str {
    match mode {
        ToolPermissionMode::Allow => i18n::tc!("tool permission mode", "Allow"),
        ToolPermissionMode::Deny => i18n::tc!("tool permission mode", "Deny"),
        ToolPermissionMode::Confirm => i18n::tc!("tool permission mode", "Confirm"),
        ToolPermissionMode::Auto => i18n::tc!("tool permission mode", "AI Judge"),
    }
}

fn verdict_color(mode: ToolPermissionMode) -> Color {
    match mode {
        ToolPermissionMode::Allow => Color::Success,
        ToolPermissionMode::Deny => Color::Error,
        ToolPermissionMode::Confirm => Color::Warning,
        // The judge still reviews the action, so it is neither an approval nor
        // a refusal.
        ToolPermissionMode::Auto => Color::Info,
    }
}

fn render_verdict_label(mode: ToolPermissionMode) -> AnyElement {
    h_flex()
        .gap_1()
        .child(
            Label::new(i18n::t!("Result:"))
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            Label::new(mode_display_label(mode))
                .size(LabelSize::Small)
                .color(verdict_color(mode)),
        )
        .into_any_element()
}

fn render_invalid_patterns_section(
    tool_id: &'static str,
    invalid_patterns: &[InvalidPatternView],
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let section_id = format!("{}-invalid-patterns-section", tool_id);
    let theme_colors = cx.theme().colors();

    v_flex()
        .id(section_id)
        .child(
            h_flex()
                .gap_1()
                .child(
                    Icon::new(IconName::Warning)
                        .size(IconSize::Small)
                        .color(Color::Error),
                )
                .child(Label::new(i18n::t!("Invalid Patterns")).color(Color::Error)),
        )
        .child(
            Label::new(i18n::t!(
                "These patterns failed to compile as regular expressions. \
                 The tool will be blocked until they are fixed or removed."
            ))
            .size(LabelSize::Small)
            .color(Color::Muted),
        )
        .child(
            v_flex()
                .mt_2()
                .w_full()
                .gap_1p5()
                .children(invalid_patterns.iter().map(|invalid| {
                    let rule_type_label = match invalid.rule_type.as_str() {
                        "always_allow" => i18n::t!("Always Allow"),
                        "always_deny" => i18n::t!("Always Deny"),
                        "always_confirm" => i18n::t!("Always Confirm"),
                        other => other,
                    };

                    let pattern_for_delete = invalid.pattern.clone();
                    let rule_type = match invalid.rule_type.as_str() {
                        "always_allow" => ToolPermissionMode::Allow,
                        "always_deny" => ToolPermissionMode::Deny,
                        _ => ToolPermissionMode::Confirm,
                    };
                    let tool_id_for_delete = tool_id.to_string();
                    let delete_id =
                        format!("{}-invalid-delete-{}", tool_id, invalid.pattern.clone());

                    v_flex()
                        .p_2()
                        .rounded_md()
                        .border_1()
                        .border_color(theme_colors.border_variant)
                        .bg(theme_colors.surface_background.opacity(0.15))
                        .gap_1()
                        .child(
                            h_flex()
                                .justify_between()
                                .child(
                                    h_flex()
                                        .gap_1p5()
                                        .min_w_0()
                                        .child(
                                            Label::new(invalid.pattern.clone())
                                                .size(LabelSize::Small)
                                                .color(Color::Error)
                                                .buffer_font(cx),
                                        )
                                        .child(
                                            Label::new(format!("({})", rule_type_label))
                                                .size(LabelSize::XSmall)
                                                .color(Color::Muted),
                                        ),
                                )
                                .child(
                                    IconButton::new(delete_id, IconName::Trash)
                                        .icon_size(IconSize::Small)
                                        .icon_color(Color::Muted)
                                        .tooltip(Tooltip::text(i18n::t!("Delete Invalid Pattern")))
                                        .on_click(cx.listener(move |_, _, _, cx| {
                                            delete_pattern(
                                                &tool_id_for_delete,
                                                rule_type,
                                                &pattern_for_delete,
                                                cx,
                                            );
                                        })),
                                ),
                        )
                        .child(
                            Label::new(i18n::t!("Error: {}", invalid.error))
                                .size(LabelSize::XSmall)
                                .color(Color::Muted),
                        )
                })),
        )
        .into_any_element()
}

fn render_rule_section(
    tool_id: &'static str,
    title: &'static str,
    description: &'static str,
    rule_type: ToolPermissionMode,
    patterns: &[String],
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let section_id = format!("{}-{:?}-section", tool_id, rule_type);

    let user_patterns: Vec<_> = patterns.iter().enumerate().collect();

    v_flex()
        .id(section_id)
        .child(Label::new(title))
        .child(
            Label::new(description)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            v_flex()
                .mt_2()
                .w_full()
                .gap_1p5()
                .when(patterns.is_empty(), |this| {
                    this.child(render_pattern_empty_state(cx))
                })
                .when(!user_patterns.is_empty(), |this| {
                    this.child(v_flex().gap_1p5().children(user_patterns.iter().map(
                        |(index, pattern)| {
                            render_user_pattern_row(
                                tool_id,
                                rule_type,
                                *index,
                                (*pattern).clone(),
                                cx,
                            )
                        },
                    )))
                })
                .child(render_add_pattern_input(tool_id, rule_type, cx)),
        )
        .into_any_element()
}

fn render_pattern_empty_state(cx: &mut Context<SettingsWindow>) -> AnyElement {
    h_flex()
        .p_2()
        .rounded_md()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border_variant)
        .child(
            Label::new(i18n::t!("No patterns configured"))
                .size(LabelSize::Small)
                .color(Color::Disabled),
        )
        .into_any_element()
}

fn render_user_pattern_row(
    tool_id: &'static str,
    rule_type: ToolPermissionMode,
    index: usize,
    pattern: String,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let pattern_for_delete = pattern.clone();
    let pattern_for_update = pattern.clone();
    let tool_id_for_delete = tool_id.to_string();
    let tool_id_for_update = tool_id.to_string();
    let input_id = format!("{}-{:?}-pattern-{}", tool_id, rule_type, index);
    let delete_id = format!("{}-{:?}-delete-{}", tool_id, rule_type, index);
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new(input_id)
        .with_initial_text(pattern)
        .tab_index(0)
        .with_buffer_font()
        .color(Color::Default)
        .action_slot(
            IconButton::new(delete_id, IconName::Trash)
                .icon_size(IconSize::Small)
                .icon_color(Color::Muted)
                .tooltip(Tooltip::text(i18n::t!("Delete Pattern")))
                .on_click(cx.listener(move |_, _, _, cx| {
                    delete_pattern(&tool_id_for_delete, rule_type, &pattern_for_delete, cx);
                })),
        )
        .on_confirm(move |new_pattern, _window, cx| {
            if let Some(new_pattern) = new_pattern {
                let new_pattern = new_pattern.trim().to_string();
                if !new_pattern.is_empty() && new_pattern != pattern_for_update {
                    let updated = update_pattern(
                        &tool_id_for_update,
                        rule_type,
                        &pattern_for_update,
                        new_pattern.clone(),
                        cx,
                    );

                    let validation_error = if !updated {
                        Some(
                            i18n::t!("A pattern with that name already exists in this rule list.")
                                .to_string(),
                        )
                    } else {
                        match regex::Regex::new(&new_pattern) {
                            Err(err) => Some(i18n::t!(
                                "Invalid regex: {err}. Pattern saved but will block this tool until fixed or removed."
                            )),
                            Ok(_) => None,
                        }
                    };
                    settings_window
                        .update(cx, |this, cx| {
                            this.regex_validation_error = validation_error;
                            cx.notify();
                        })
                        .log_err();
                }
            }
        })
        .into_any_element()
}

fn render_add_pattern_input(
    tool_id: &'static str,
    rule_type: ToolPermissionMode,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let tool_id_owned = tool_id.to_string();
    let input_id = format!("{}-{:?}-new-pattern", tool_id, rule_type);
    let settings_window = cx.entity().downgrade();

    SettingsInputField::new(input_id)
        .with_placeholder(i18n::t!("Add regex pattern…"))
        .tab_index(0)
        .with_buffer_font()
        .display_clear_button()
        .display_confirm_button()
        .clear_on_confirm()
        .on_confirm(move |pattern, _window, cx| {
            if let Some(pattern) = pattern {
                let trimmed = pattern.trim().to_string();
                if !trimmed.is_empty() {
                    save_pattern(&tool_id_owned, rule_type, trimmed.clone(), cx);

                    let validation_error = match regex::Regex::new(&trimmed) {
                        Err(err) => Some(i18n::t!(
                            "Invalid regex: {err}. Pattern saved but will block this tool until fixed or removed."
                        )),
                        Ok(_) => None,
                    };
                    settings_window
                        .update(cx, |this, cx| {
                            this.regex_validation_error = validation_error;
                            cx.notify();
                        })
                        .log_err();
                }
            }
        })
        .into_any_element()
}

/// The model that judges tool actions while the default is "AI が判定"
/// (ADR-F52), with the list of models the user has already authenticated. The
/// description spells out that the tool input leaves the machine for whatever
/// provider that model belongs to.
fn render_permission_judge_model_section(cx: &mut Context<SettingsWindow>) -> AnyElement {
    let configured = AgentSettings::get_global(cx).permission_judge_model.clone();
    let model_label: SharedString = match &configured {
        Some(selection) => format!("{} / {}", selection.provider.0, selection.model).into(),
        None => i18n::t!("Default (Fast Model)").into(),
    };
    let available_models = language_model::LanguageModelRegistry::read_global(cx)
        .available_models(cx)
        .map(|model| {
            (
                model.provider_id().0.to_string(),
                model.id().0.to_string(),
                format!("{} / {}", model.provider_name().0, model.name().0),
            )
        })
        .collect::<Vec<_>>();

    h_flex()
        .my_4()
        .min_w_0()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .child(Label::new(i18n::t!("Judge Model")))
                .child(
                    Label::new(i18n::t!(
                        "A small, fast model is enough (for example, an 8B-class model on Ollama). When none is set, the fast model is used, then the default model. To judge an action, the tool name, command, and paths are sent to that model's provider, so a local model is recommended. If no answer comes back within 10 seconds, no model is available, or the answer can't be read, a confirmation is shown."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .child(
            PopoverMenu::new("permission-judge-model")
                .trigger(
                    Button::new("permission-judge-model-trigger", model_label)
                        .tab_index(0_isize)
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Medium)
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::Small)),
                )
                .menu(move |window, cx| {
                    let available_models = available_models.clone();
                    Some(ContextMenu::build(window, cx, move |mut menu, _, _| {
                        menu = menu.entry(i18n::t!("Default (Fast Model)"), None, move |_, cx| {
                            set_permission_judge_model(None, cx);
                        });
                        if available_models.is_empty() {
                            menu = menu.label(i18n::t!("No authenticated models"));
                        }
                        for (provider, model, label) in available_models {
                            menu = menu.entry(label, None, move |_, cx| {
                                set_permission_judge_model(
                                    Some((provider.clone(), model.clone())),
                                    cx,
                                );
                            });
                        }
                        menu
                    }))
                })
                .anchor(gpui::Anchor::TopRight),
        )
        .into_any_element()
}

fn set_permission_judge_model(selection: Option<(String, String)>, cx: &mut App) {
    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        let agent = settings.agent.get_or_insert_default();
        match &selection {
            Some((provider, model)) => {
                agent.set_permission_judge_model(provider.clone(), model.clone());
            }
            None => agent.permission_judge_model = None,
        }
    });
}

fn render_global_default_mode_section(current_mode: ToolPermissionMode) -> AnyElement {
    let mode_label = mode_display_label(current_mode);

    h_flex()
        .my_4()
        .min_w_0()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .child(Label::new(i18n::t!("Default Permission")))
                .child(
                    Label::new(i18n::t!(
                        "Controls the default behavior for all tool actions. Per-tool rules and patterns can override this. \"Allow\" is the same setting as \"Auto-Approve (In Sandbox)\" in the agent panel. \"AI Judge\" runs automatically only the actions that a separate model judges not dangerous, and asks for confirmation for everything else. In either case, the built-in security rules, your deny and confirm patterns, the sandbox, and the confirmation for writes to settings files all still apply. File operations outside the project (worktree) are always confirmed, whatever this is set to, unless you wrote an \"Always Allow\" pattern for that path yourself. Terminal commands that run outside the sandbox (when the sandbox is disabled, or in a project without one) are confirmed every time, regardless of \"Allow\", \"AI Judge\", or \"Always Allow\". Which tools are available is chosen by the profile, not by this setting."
                    ))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                ),
        )
        .child(
            PopoverMenu::new("global-default-mode")
                .trigger(
                    Button::new("global-mode-trigger", mode_label)
                        .tab_index(0_isize)
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Medium)
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::Small)),
                )
                .menu(move |window, cx| {
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        menu.entry(mode_display_label(ToolPermissionMode::Confirm), None, move |_, cx| {
                            set_global_default_permission(ToolPermissionMode::Confirm, cx);
                        })
                        .entry(mode_display_label(ToolPermissionMode::Auto), None, move |_, cx| {
                            set_global_default_permission(ToolPermissionMode::Auto, cx);
                        })
                        .entry(mode_display_label(ToolPermissionMode::Allow), None, move |_, cx| {
                            set_global_default_permission(ToolPermissionMode::Allow, cx);
                        })
                        .entry(mode_display_label(ToolPermissionMode::Deny), None, move |_, cx| {
                            set_global_default_permission(ToolPermissionMode::Deny, cx);
                        })
                    }))
                })
                .anchor(gpui::Anchor::TopRight),
        )
        .into_any_element()
}

fn render_default_mode_section(
    tool_id: &'static str,
    current_mode: ToolPermissionMode,
    _cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let mode_label = mode_display_label(current_mode);

    let tool_id_owned = tool_id.to_string();

    h_flex()
        .min_w_0()
        .justify_between()
        .child(
            v_flex()
                .w_full()
                .min_w_0()
                .child(Label::new(i18n::t!("Default Action")))
                .child(
                    Label::new(i18n::t!("Action to take when no patterns match."))
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                ),
        )
        .child(
            PopoverMenu::new(format!("default-mode-{}", tool_id))
                .trigger(
                    Button::new(format!("mode-trigger-{}", tool_id), mode_label)
                        .tab_index(0_isize)
                        .style(ButtonStyle::Outlined)
                        .size(ButtonSize::Medium)
                        .end_icon(Icon::new(IconName::ChevronDown).size(IconSize::Small)),
                )
                .menu(move |window, cx| {
                    let tool_id = tool_id_owned.clone();
                    Some(ContextMenu::build(window, cx, move |menu, _, _| {
                        let tool_id_confirm = tool_id.clone();
                        let tool_id_auto = tool_id.clone();
                        let tool_id_allow = tool_id.clone();
                        let tool_id_deny = tool_id;

                        menu.entry(
                            mode_display_label(ToolPermissionMode::Confirm),
                            None,
                            move |_, cx| {
                                set_default_mode(&tool_id_confirm, ToolPermissionMode::Confirm, cx);
                            },
                        )
                        .entry(
                            mode_display_label(ToolPermissionMode::Auto),
                            None,
                            move |_, cx| {
                                set_default_mode(&tool_id_auto, ToolPermissionMode::Auto, cx);
                            },
                        )
                        .entry(
                            mode_display_label(ToolPermissionMode::Allow),
                            None,
                            move |_, cx| {
                                set_default_mode(&tool_id_allow, ToolPermissionMode::Allow, cx);
                            },
                        )
                        .entry(
                            mode_display_label(ToolPermissionMode::Deny),
                            None,
                            move |_, cx| {
                                set_default_mode(&tool_id_deny, ToolPermissionMode::Deny, cx);
                            },
                        )
                    }))
                })
                .anchor(gpui::Anchor::TopRight),
        )
        .into_any_element()
}

struct InvalidPatternView {
    pattern: String,
    rule_type: String,
    error: String,
}

struct ToolRulesView {
    default: ToolPermissionMode,
    always_allow: Vec<String>,
    always_deny: Vec<String>,
    always_confirm: Vec<String>,
    invalid_patterns: Vec<InvalidPatternView>,
}

fn get_tool_rules(tool_name: &str, cx: &App) -> ToolRulesView {
    let settings = AgentSettings::get_global(cx);

    let tool_rules = settings.tool_permissions.tools.get(tool_name);

    match tool_rules {
        Some(rules) => ToolRulesView {
            default: rules.default.unwrap_or(settings.tool_permissions.default),
            always_allow: rules
                .always_allow
                .iter()
                .map(|r| r.pattern.clone())
                .collect(),
            always_deny: rules
                .always_deny
                .iter()
                .map(|r| r.pattern.clone())
                .collect(),
            always_confirm: rules
                .always_confirm
                .iter()
                .map(|r| r.pattern.clone())
                .collect(),
            invalid_patterns: rules
                .invalid_patterns
                .iter()
                .map(|p| InvalidPatternView {
                    pattern: p.pattern.clone(),
                    rule_type: p.rule_type.clone(),
                    error: p.error.clone(),
                })
                .collect(),
        },
        None => ToolRulesView {
            default: settings.tool_permissions.default,
            always_allow: Vec::new(),
            always_deny: Vec::new(),
            always_confirm: Vec::new(),
            invalid_patterns: Vec::new(),
        },
    }
}

fn save_pattern(tool_name: &str, rule_type: ToolPermissionMode, pattern: String, cx: &mut App) {
    let tool_name = tool_name.to_string();

    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        let tool_permissions = settings
            .agent
            .get_or_insert_default()
            .tool_permissions
            .get_or_insert_default();
        let tool_rules = tool_permissions
            .tools
            .entry(Arc::from(tool_name.as_str()))
            .or_default();

        let rule = settings::ToolRegexRule {
            pattern,
            case_sensitive: None,
        };

        let rules_list = match rule_type {
            ToolPermissionMode::Allow => tool_rules.always_allow.get_or_insert_default(),
            ToolPermissionMode::Deny => tool_rules.always_deny.get_or_insert_default(),
            ToolPermissionMode::Confirm => tool_rules.always_confirm.get_or_insert_default(),
            // "AI が判定" has no pattern list of its own; no section offers it.
            ToolPermissionMode::Auto => return,
        };

        if !rules_list.0.iter().any(|r| r.pattern == rule.pattern) {
            rules_list.0.push(rule);
        }
    });
}

fn update_pattern(
    tool_name: &str,
    rule_type: ToolPermissionMode,
    old_pattern: &str,
    new_pattern: String,
    cx: &mut App,
) -> bool {
    let settings = AgentSettings::get_global(cx);
    if let Some(tool_rules) = settings.tool_permissions.tools.get(tool_name) {
        let patterns = match rule_type {
            ToolPermissionMode::Allow => &tool_rules.always_allow,
            ToolPermissionMode::Deny => &tool_rules.always_deny,
            ToolPermissionMode::Confirm => &tool_rules.always_confirm,
            ToolPermissionMode::Auto => return false,
        };
        if patterns.iter().any(|r| r.pattern == new_pattern) {
            return false;
        }
    }

    let tool_name = tool_name.to_string();
    let old_pattern = old_pattern.to_string();

    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        let tool_permissions = settings
            .agent
            .get_or_insert_default()
            .tool_permissions
            .get_or_insert_default();

        if let Some(tool_rules) = tool_permissions.tools.get_mut(tool_name.as_str()) {
            let rules_list = match rule_type {
                ToolPermissionMode::Allow => &mut tool_rules.always_allow,
                ToolPermissionMode::Deny => &mut tool_rules.always_deny,
                ToolPermissionMode::Confirm => &mut tool_rules.always_confirm,
                ToolPermissionMode::Auto => return,
            };

            if let Some(list) = rules_list {
                let already_exists = list.0.iter().any(|r| r.pattern == new_pattern);
                if !already_exists {
                    if let Some(rule) = list.0.iter_mut().find(|r| r.pattern == old_pattern) {
                        rule.pattern = new_pattern;
                    }
                }
            }
        }
    });

    true
}

fn delete_pattern(tool_name: &str, rule_type: ToolPermissionMode, pattern: &str, cx: &mut App) {
    let tool_name = tool_name.to_string();
    let pattern = pattern.to_string();

    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        let tool_permissions = settings
            .agent
            .get_or_insert_default()
            .tool_permissions
            .get_or_insert_default();

        if let Some(tool_rules) = tool_permissions.tools.get_mut(tool_name.as_str()) {
            let rules_list = match rule_type {
                ToolPermissionMode::Allow => &mut tool_rules.always_allow,
                ToolPermissionMode::Deny => &mut tool_rules.always_deny,
                ToolPermissionMode::Confirm => &mut tool_rules.always_confirm,
                ToolPermissionMode::Auto => return,
            };

            if let Some(list) = rules_list {
                list.0.retain(|r| r.pattern != pattern);
            }
        }
    });
}

fn set_global_default_permission(mode: ToolPermissionMode, cx: &mut App) {
    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        settings
            .agent
            .get_or_insert_default()
            .tool_permissions
            .get_or_insert_default()
            .default = Some(mode);
    });
}

fn set_default_mode(tool_name: &str, mode: ToolPermissionMode, cx: &mut App) {
    let tool_name = tool_name.to_string();

    SettingsStore::global(cx).update_settings_file(<dyn fs::Fs>::global(cx), move |settings, _| {
        let tool_permissions = settings
            .agent
            .get_or_insert_default()
            .tool_permissions
            .get_or_insert_default();
        let tool_rules = tool_permissions
            .tools
            .entry(Arc::from(tool_name.as_str()))
            .or_default();
        tool_rules.default = Some(mode);
    });
}

macro_rules! tool_config_page_fn {
    ($fn_name:ident, $tool_id:literal) => {
        pub fn $fn_name(
            settings_window: &SettingsWindow,
            scroll_handle: &ScrollHandle,
            window: &mut Window,
            cx: &mut Context<SettingsWindow>,
        ) -> AnyElement {
            const INDEX: usize = tool_index($tool_id);
            render_tool_config_page(&TOOLS[INDEX], settings_window, scroll_handle, window, cx)
        }
    };
}

tool_config_page_fn!(render_terminal_tool_config, "terminal");
tool_config_page_fn!(render_edit_file_tool_config, "edit_file");
tool_config_page_fn!(render_write_file_tool_config, "write_file");
tool_config_page_fn!(render_delete_path_tool_config, "delete_path");
tool_config_page_fn!(render_copy_path_tool_config, "copy_path");
tool_config_page_fn!(render_move_path_tool_config, "move_path");
tool_config_page_fn!(render_create_directory_tool_config, "create_directory");
tool_config_page_fn!(render_fetch_tool_config, "fetch");
tool_config_page_fn!(render_web_search_tool_config, "search_web");
tool_config_page_fn!(render_skill_tool_config, "skill");
tool_config_page_fn!(render_apply_code_action_tool_config, "apply_code_action");
tool_config_page_fn!(render_rename_symbol_tool_config, "rename_symbol");

#[cfg(test)]
mod tests {
    use super::*;

    /// The preview swaps the reason for the list of built-in rules only when the
    /// agent's own engine denied the input with them.
    #[test]
    fn test_hardcoded_denial_is_recognized_from_the_engine() {
        let decision = agent::decide_terminal_permission(
            &["rm -rf /".to_string()],
            &agent_settings::ToolPermissions::default(),
            ShellKind::Posix,
            agent::TerminalExecution::Sandboxed,
        );
        assert!(
            is_hardcoded_denial(&decision),
            "`rm -rf /` should be denied by the built-in rules: {decision:?}"
        );
        assert!(!is_hardcoded_denial(&ToolPermissionDecision::Deny(
            "matched an always_deny pattern".to_string()
        )));
        assert!(!is_hardcoded_denial(&ToolPermissionDecision::Allow));
    }

    /// ADR-F52: the preview on this page reports the judged default as its own
    /// verdict rather than pretending the action is allowed or refused.
    #[test]
    fn test_judged_decision_is_shown_as_its_own_verdict() {
        assert_eq!(
            decision_to_mode(&ToolPermissionDecision::Judge),
            ToolPermissionMode::Auto
        );
        assert_eq!(mode_display_label(ToolPermissionMode::Auto), "AI Judge");
        // Neither a success nor an error colour: the judge still reviews it.
        assert_eq!(verdict_color(ToolPermissionMode::Auto), Color::Info);
    }

    /// A pattern preview never claims a judged default approved the command:
    /// the three pattern lists are the only rule types a match can have.
    #[test]
    fn test_judged_default_is_not_a_pattern_rule_type() {
        assert_eq!(
            implied_mode_from_patterns(&[], ToolPermissionMode::Auto),
            ToolPermissionMode::Auto,
            "with no matching pattern the default decides"
        );
        assert_eq!(
            implied_mode_from_patterns(
                &[MatchedPattern {
                    pattern: "^cargo".to_string(),
                    rule_type: ToolPermissionMode::Deny,
                    is_overridden: false,
                }],
                ToolPermissionMode::Auto,
            ),
            ToolPermissionMode::Deny,
            "a deny pattern outranks the judge"
        );
    }

    #[test]
    fn test_all_tools_are_in_tool_info_or_excluded() {
        // Tools that intentionally don't appear in the permissions UI.
        // If you add a new tool and this test fails, either:
        //   1. Add a ToolInfo entry to TOOLS (if the tool has permission checks), or
        //   2. Add it to this list with a comment explaining why it's excluded.
        const EXCLUDED_TOOLS: &[&str] = &[
            // Interactive prompt: always asks the user via prompt_for_decision,
            // so it has no settings-driven permission rules.
            "ask_user",
            // Read-only / low-risk tools that don't call decide_permission_from_settings
            "diagnostics",
            "find_path",
            "find_references",
            "get_code_actions",
            "git_diff",
            "go_to_definition",
            "grep",
            "list_agents_and_models",
            "list_directory",
            "open",
            "read_file",
            "repo_map",
            "thinking",
            // streaming_edit_file uses "edit_file" for permission lookups,
            // so its rules are configured under the edit_file entry.
            "streaming_edit_file",
            // Sibling/subagent thread creation delegates permission checks to
            // tool calls inside the spawned thread, not the spawning itself.
            "create_thread",
            "spawn_agent",
            // IDE assist (ADR-F56): language queries only read, and the display
            // tools only show the agent's text in the editor, so neither asks.
            "hover",
            "search_symbols",
            "highlight_code",
            "annotate_code",
            "code_tour",
            "clear_annotations",
        ];

        let tool_info_ids: Vec<&str> = TOOLS.iter().map(|t| t.id).collect();

        for tool_name in agent::ALL_TOOL_NAMES {
            if EXCLUDED_TOOLS.contains(tool_name) {
                assert!(
                    !tool_info_ids.contains(tool_name),
                    "Tool '{}' is in both EXCLUDED_TOOLS and TOOLS — pick one.",
                    tool_name,
                );
                continue;
            }
            assert!(
                tool_info_ids.contains(tool_name),
                "Tool '{}' is in ALL_TOOL_NAMES but has no entry in TOOLS and \
                 is not in EXCLUDED_TOOLS. Either add a ToolInfo entry (if the \
                 tool has permission checks) or add it to EXCLUDED_TOOLS with \
                 a comment explaining why.",
                tool_name,
            );
        }

        for tool_id in &tool_info_ids {
            assert!(
                agent::ALL_TOOL_NAMES.contains(tool_id),
                "TOOLS contains '{}' but it is not in ALL_TOOL_NAMES. \
                 Is this a valid built-in tool?",
                tool_id,
            );
        }
    }
}
