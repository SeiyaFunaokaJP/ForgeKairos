use context_server::ContextServerId;
use gpui::{ScrollHandle, SharedString, prelude::*};
use serde::Deserialize;
use ui::{Tooltip, prelude::*};

use super::{
    McpServerForm, McpServerFormValues, McpTransport, display_command_line, existing_server_ids,
    push_mcp_server_form, render_form_error,
};
use crate::SettingsWindow;
use crate::pages::network_allowlist;

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct McpServerTemplate {
    /// The server name the form starts with.
    pub(super) name: String,
    pub(super) title: String,
    pub(super) description: String,
    #[serde(flatten)]
    pub(super) definition: TemplateDefinition,
    /// What has to be installed on this machine for the command to run.
    #[serde(default)]
    pub(super) requires: Option<String>,
    /// Hosts the server cannot start without (the package registry an `npx`
    /// or `uvx` command installs from, or an HTTP server's own host), all of
    /// which go through the egress allowlist.
    #[serde(default)]
    pub(super) hosts: Vec<String>,
    #[serde(default)]
    pub(super) note: Option<String>,
}

/// Tagged rather than told apart by its fields, which is what makes the
/// settings' own `context_servers` entries depend on the order of their arms.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "transport", rename_all = "snake_case")]
pub(super) enum TemplateDefinition {
    Stdio {
        command: String,
        #[serde(default)]
        args: Vec<TemplateArgument>,
        #[serde(default)]
        env: Vec<TemplateInput>,
    },
    Http {
        url: String,
        #[serde(default)]
        headers: Vec<TemplateInput>,
    },
}

#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(untagged)]
pub(super) enum TemplateArgument {
    Fixed(String),
    /// Left blank for the user, who is told what goes there.
    Input {
        prompt: String,
    },
}

/// An environment variable or header whose value only the user can supply.
#[derive(Deserialize, Clone, Debug, PartialEq, Eq)]
pub(super) struct TemplateInput {
    pub(super) name: String,
    pub(super) prompt: String,
}

pub(super) fn parse_templates(text: &str) -> Result<Vec<McpServerTemplate>, SharedString> {
    serde_json::from_str(text).map_err(|error| {
        SharedString::from(format!("テンプレートの一覧を読み込めませんでした: {error}"))
    })
}

impl McpServerTemplate {
    fn transport(&self) -> McpTransport {
        match self.definition {
            TemplateDefinition::Stdio { .. } => McpTransport::Stdio,
            TemplateDefinition::Http { .. } => McpTransport::Http,
        }
    }

    fn summary(&self) -> String {
        match &self.definition {
            TemplateDefinition::Stdio { command, args, .. } => {
                let args = args
                    .iter()
                    .map(|argument| match argument {
                        TemplateArgument::Fixed(argument) => argument.clone(),
                        TemplateArgument::Input { prompt } => format!("<{prompt}>"),
                    })
                    .collect::<Vec<_>>();
                display_command_line(command, &args)
            }
            TemplateDefinition::Http { url, .. } => url.clone(),
        }
    }

    fn inputs(&self) -> &[TemplateInput] {
        match &self.definition {
            TemplateDefinition::Stdio { env, .. } => env,
            TemplateDefinition::Http { headers, .. } => headers,
        }
    }
}

impl McpServerFormValues {
    /// Everything the template knows, with the blanks the user has to fill
    /// marked so that saving refuses to write them empty.
    pub(super) fn from_template(template: &McpServerTemplate) -> Self {
        let mut values = Self::empty(template.transport());
        values.name = template.name.clone();
        match &template.definition {
            TemplateDefinition::Stdio { command, args, env } => {
                values.command = command.clone();
                for argument in args {
                    match argument {
                        TemplateArgument::Fixed(argument) => {
                            values.args.push(argument.clone());
                            values.prompts.args.push(None);
                        }
                        TemplateArgument::Input { prompt } => {
                            values.args.push(String::new());
                            values.prompts.args.push(Some(prompt.clone()));
                        }
                    }
                }
                values.env = template_inputs(env, &mut values.prompts.env);
            }
            TemplateDefinition::Http { url, headers } => {
                values.url = url.clone();
                values.headers = template_inputs(headers, &mut values.prompts.headers);
            }
        }
        values
    }
}

fn template_inputs(
    inputs: &[TemplateInput],
    prompts: &mut Vec<Option<String>>,
) -> Vec<(String, String)> {
    inputs
        .iter()
        .map(|input| {
            prompts.push(Some(input.prompt.clone()));
            (input.name.clone(), String::new())
        })
        .collect()
}

pub(super) struct McpTemplatesState {
    templates: Result<Vec<McpServerTemplate>, SharedString>,
}

pub(super) fn open_mcp_templates_page(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.mcp_page.templates = Some(McpTemplatesState {
        templates: parse_templates(&settings::mcp_server_templates_content()),
    });
    settings_window.push_dynamic_sub_page(
        "テンプレートから MCP サーバーを追加",
        "エージェントの設定",
        Some("context_servers"),
        false,
        render_mcp_templates_page,
        window,
        cx,
    );
}

fn open_template_form(
    settings_window: &mut SettingsWindow,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(template) = settings_window
        .mcp_page
        .templates
        .as_ref()
        .and_then(|state| state.templates.as_ref().ok())
        .and_then(|templates| templates.get(index))
        .cloned()
    else {
        return;
    };
    let values = McpServerFormValues::from_template(&template);
    let form = McpServerForm::from_values(
        values,
        Some(
            format!(
                "テンプレート「{}」を読み込みました。「入力してください」の欄を埋めてから保存してください。",
                template.title
            )
            .into(),
        ),
        window,
        cx,
    );
    push_mcp_server_form(settings_window, form, window, cx);
}

fn render_mcp_templates_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(state) = settings_window.mcp_page.templates.as_ref() else {
        return div().into_any_element();
    };
    let body = match &state.templates {
        Err(error) => render_form_error(error.clone()).into_any_element(),
        Ok(templates) => {
            let existing_ids = existing_server_ids(settings_window, cx);
            v_flex()
                .w_full()
                .gap_2()
                .children(templates.iter().enumerate().map(|(index, template)| {
                    render_template_card(settings_window, index, template, &existing_ids, cx)
                }))
                .into_any_element()
        }
    };

    v_flex()
        .id("mcp-templates-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .gap_4()
                .child(
                    Label::new(
                        "よく使う MCP サーバーの設定をフォームに読み込みます。「保存」を押すまで設定には書き込まれません。",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(body),
        )
        .into_any_element()
}

fn render_template_card(
    settings_window: &SettingsWindow,
    index: usize,
    template: &McpServerTemplate,
    existing_ids: &[ContextServerId],
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let border_color = cx.theme().colors().border_variant;
    let name_exists = existing_ids
        .iter()
        .any(|id| id.0.as_ref() == template.name.as_str());
    let kind = match template.transport() {
        McpTransport::Stdio => "ローカル (コマンド)",
        McpTransport::Http => "リモート (HTTP)",
    };
    let inputs = template
        .inputs()
        .iter()
        .map(|input| input.name.as_str())
        .collect::<Vec<_>>();
    let inputs_label = (!inputs.is_empty()).then(|| {
        let what = match template.transport() {
            McpTransport::Stdio => "環境変数",
            McpTransport::Http => "ヘッダー",
        };
        format!("入力が必要な{what}: {}", inputs.join(", "))
    });

    v_flex()
        .w_full()
        .min_w_0()
        .p_2()
        .gap_1()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .child(
            h_flex()
                .w_full()
                .min_w_0()
                .gap_2()
                .justify_between()
                .child(
                    h_flex()
                        .min_w_0()
                        .gap_2()
                        .child(Label::new(template.title.clone()))
                        .child(Label::new(kind).size(LabelSize::Small).color(Color::Muted))
                        .when(name_exists, |this| {
                            this.child(
                                Label::new("同名のサーバーがあります")
                                    .size(LabelSize::Small)
                                    .color(Color::Warning),
                            )
                        }),
                )
                .child(
                    Button::new(("mcp-template-use", index), "このテンプレートを使う")
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            open_template_form(this, index, window, cx);
                        })),
                ),
        )
        .child(
            Label::new(template.description.clone())
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .child(
            div().min_w_0().child(
                Label::new(template.summary())
                    .size(LabelSize::Small)
                    .color(Color::Muted)
                    .buffer_font(cx),
            ),
        )
        .when_some(template.requires.clone(), |this, requires| {
            this.child(
                Label::new(format!("必要なもの: {requires}"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
        })
        .when_some(inputs_label, |this, label| {
            this.child(Label::new(label).size(LabelSize::Small).color(Color::Muted))
        })
        .child(render_template_hosts(settings_window, index, template, cx))
        .when_some(template.note.clone(), |this, note| {
            this.child(
                Label::new(note)
                    .size(LabelSize::Small)
                    .color(Color::Warning),
            )
        })
        .into_any_element()
}

/// The template's hosts as the allowlist sees them, with the same button the
/// external-AI consent blocks use to add the missing ones: without them the
/// package install (or the HTTP server) is refused, which is only the
/// default, not a failure.
fn render_template_hosts(
    settings_window: &SettingsWindow,
    index: usize,
    template: &McpServerTemplate,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    if template.hosts.is_empty() {
        return div().into_any_element();
    }
    let allowlist = &settings_window.network_allowlist;
    let is_loaded = allowlist.is_loaded();
    let missing = template
        .hosts
        .iter()
        .filter(|host| !allowlist.allows(host))
        .map(|host| SharedString::from(host.clone()))
        .collect::<Vec<_>>();
    let host_labels = template
        .hosts
        .iter()
        .map(|host| {
            let state = if !is_loaded {
                ""
            } else if allowlist.allows(host) {
                " (登録済み)"
            } else {
                " (未登録)"
            };
            h_flex()
                .gap_0p5()
                .child(
                    Label::new(host.clone())
                        .size(LabelSize::Small)
                        .buffer_font(cx),
                )
                .child(Label::new(state).size(LabelSize::Small).color(Color::Muted))
                .into_any_element()
        })
        .collect::<Vec<_>>();

    v_flex()
        .gap_0p5()
        .child(
            h_flex()
                .flex_wrap()
                .gap_1p5()
                .child(
                    Label::new("許可が必要なホスト:")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
                .children(host_labels)
                .child(
                    Button::new(("mcp-template-allowlist", index), "許可リストに追加")
                        .label_size(LabelSize::Small)
                        .style(ButtonStyle::Outlined)
                        .disabled(!is_loaded || missing.is_empty())
                        .start_icon(
                            Icon::new(IconName::Plus)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(
                            "network_allowlist.json に追加します。追加しない場合、このサーバーの取得や接続は拒否されたままになります",
                        ))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            network_allowlist::add_allowed_hosts(this, &missing, cx);
                        })),
                ),
        )
        .when_some(allowlist.consent_error(), |this, error| {
            this.child(render_form_error(error))
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::super::build_settings_from_values;
    use super::*;
    use settings::{ContextServerCommand, ContextServerSettingsContent};

    fn shipped_templates() -> Vec<McpServerTemplate> {
        parse_templates(&settings::mcp_server_templates_content())
            .expect("the shipped templates should parse")
    }

    #[test]
    fn the_shipped_templates_parse_and_have_unique_names() {
        let templates = shipped_templates();
        assert!(templates.len() >= 5, "{templates:?}");
        let mut names = templates
            .iter()
            .map(|template| template.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), templates.len());
        for template in &templates {
            assert!(
                !template.hosts.is_empty(),
                "{}: every template reaches the network through the allowlist",
                template.name
            );
            assert!(
                template
                    .hosts
                    .iter()
                    .all(|host| http_client::RequestHost::parse(host).is_some()),
                "{}: {:?}",
                template.name,
                template.hosts
            );
        }
    }

    /// Saved untouched, a template must not produce a server that runs with
    /// its blanks empty; once they are filled, it saves like a typed one.
    #[test]
    fn every_template_asks_for_its_blanks_before_it_saves() {
        for template in shipped_templates() {
            let mut values = McpServerFormValues::from_template(&template);
            let has_blanks = values.prompts.args.iter().any(Option::is_some)
                || values.prompts.env.iter().any(Option::is_some)
                || values.prompts.headers.iter().any(Option::is_some);
            if has_blanks {
                let error = build_settings_from_values(&values)
                    .expect_err(&format!("{}: blanks must be filled", template.name));
                assert!(error.contains("入力してください"), "{error}");
            }

            for (argument, prompt) in values.args.iter_mut().zip(&values.prompts.args) {
                if prompt.is_some() {
                    *argument = "/work".into();
                }
            }
            for (_, value) in values.env.iter_mut().chain(values.headers.iter_mut()) {
                if value.is_empty() {
                    *value = "filled".into();
                }
            }
            let (id, original_id, _) = build_settings_from_values(&values)
                .unwrap_or_else(|error| panic!("{}: {error}", template.name));
            assert_eq!(id.0.as_ref(), template.name);
            assert_eq!(original_id, None);
        }
    }

    #[test]
    fn a_template_fills_the_form_with_its_command_and_blanks() {
        let template: McpServerTemplate = serde_json::from_str(
            r#"{
                "name": "fs",
                "title": "File system",
                "description": "d",
                "transport": "stdio",
                "command": "npx",
                "args": ["-y", "@scope/server", { "prompt": "folder" }],
                "env": [{ "name": "TOKEN", "prompt": "token" }],
                "hosts": ["registry.npmjs.org"]
            }"#,
        )
        .expect("valid template");
        let mut values = McpServerFormValues::from_template(&template);
        assert_eq!(values.transport, McpTransport::Stdio);
        assert_eq!(values.name, "fs");
        assert_eq!(values.command, "npx");
        assert_eq!(values.args, vec!["-y", "@scope/server", ""]);
        assert_eq!(
            values.prompts.args,
            vec![None, None, Some("folder".to_string())]
        );
        assert_eq!(values.env, vec![("TOKEN".to_string(), String::new())]);
        assert_eq!(values.prompts.env, vec![Some("token".to_string())]);

        assert_eq!(
            build_settings_from_values(&values).map(|_| ()),
            Err(SharedString::from("「folder」を入力してください。"))
        );
        values.args[2] = "C:\\work".into();
        assert_eq!(
            build_settings_from_values(&values).map(|_| ()),
            Err(SharedString::from(
                "環境変数「TOKEN」の値を入力してください。"
            ))
        );
        values.env[0].1 = "secret".into();
        let (_, _, content) = build_settings_from_values(&values).expect("filled");
        assert_eq!(
            content,
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "npx".into(),
                    args: vec!["-y".into(), "@scope/server".into(), "C:\\work".into()],
                    env: Some(
                        [("TOKEN".to_string(), "secret".to_string())]
                            .into_iter()
                            .collect()
                    ),
                    timeout: None,
                },
            }
        );
    }

    #[test]
    fn an_http_template_asks_for_its_header() {
        let template: McpServerTemplate = serde_json::from_str(
            r#"{
                "name": "gh",
                "title": "GitHub",
                "description": "d",
                "transport": "http",
                "url": "https://example.com/mcp/",
                "headers": [{ "name": "Authorization", "prompt": "Bearer <token>" }]
            }"#,
        )
        .expect("valid template");
        let mut values = McpServerFormValues::from_template(&template);
        assert_eq!(values.transport, McpTransport::Http);
        assert_eq!(values.url, "https://example.com/mcp/");
        assert_eq!(
            build_settings_from_values(&values).map(|_| ()),
            Err(SharedString::from(
                "ヘッダー「Authorization」の値を入力してください。"
            ))
        );
        values.headers[0].1 = "Bearer x".into();
        assert!(build_settings_from_values(&values).is_ok());
    }

    #[test]
    fn a_template_without_a_transport_is_rejected() {
        assert!(
            parse_templates(
                r#"[{ "name": "x", "title": "x", "description": "x", "command": "x" }]"#
            )
            .is_err()
        );
    }
}
