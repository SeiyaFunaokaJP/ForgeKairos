use std::sync::Arc;

use collections::HashMap;
use context_server::ContextServerId;
use editor::Editor;
use extension_host::ExtensionStore;
use gpui::{
    Action as _, Entity, EntityId, Focusable as _, ScrollHandle, Subscription, Task, WeakEntity,
    prelude::*,
};
use project::context_server_store::{
    ContextServerConfiguration, ContextServerStatus, ContextServerStore, ServerStatusChangedEvent,
    wait_for_context_server,
};
use project::project_settings::ContextServerSettings;
use settings::{ContextServerCommand, ContextServerSettingsContent, OAuthClientSettings};
use ui::{
    AiSettingItem, AiSettingItemSource, AiSettingItemStatus, ContextMenu, Divider, PopoverMenu,
    Switch, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;

use zed_actions::ExtensionCategoryFilter;

use crate::{PROJECT, SettingField, SettingItem, SettingsPageItem, SettingsWindow, USER};

/// Live state the MCP servers page keeps while it is open.
#[derive(Default)]
pub(crate) struct McpPageState {
    /// Subscription to the active project's [`ContextServerStore`], tagged with
    /// the store it watches. Without it the status indicators would only change
    /// when the page happened to be re-rendered for some other reason.
    store_subscription: Option<(EntityId, Subscription)>,
    connection_tests: HashMap<ContextServerId, McpConnectionTest>,
    /// The server whose client secret is being entered, and the input for it.
    client_secret_input: Option<(ContextServerId, Entity<Editor>)>,
}

struct McpConnectionTest {
    outcome: McpConnectionTestOutcome,
    /// Held so the wait is cancelled when the page state is dropped. Never
    /// replaced from inside the task it holds, which would drop that task while
    /// it is still running.
    _task: Option<Task<()>>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
enum McpConnectionTestOutcome {
    Running,
    Succeeded,
    /// The server settled in a state that needs something from the user; the
    /// row's own status details say what.
    NeedsAction(SharedString),
    Failed(SharedString),
}

impl McpPageState {
    fn report(&mut self, id: ContextServerId, outcome: McpConnectionTestOutcome) {
        self.connection_tests.insert(
            id,
            McpConnectionTest {
                outcome,
                _task: None,
            },
        );
    }
}

/// Called when the MCP servers sub-page opens: drops feedback left over from the
/// previous visit and points the status subscription at the current project.
pub(crate) fn open_mcp_servers_page(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.mcp_page.connection_tests.clear();
    settings_window.mcp_page.client_secret_input = None;
    subscribe_to_context_server_store(settings_window, cx);
}

/// The settings window is re-pointed at whichever workspace opened it, so the
/// project — and with it the server store — can change under an already-open
/// page. Does nothing until the page has been opened at least once.
pub(crate) fn retarget_mcp_server_subscription(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    if settings_window.mcp_page.store_subscription.is_some() {
        subscribe_to_context_server_store(settings_window, cx);
    }
}

fn subscribe_to_context_server_store(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        settings_window.mcp_page = McpPageState::default();
        return;
    };

    if settings_window
        .mcp_page
        .store_subscription
        .as_ref()
        .is_some_and(|(subscribed_to, _)| *subscribed_to == store.entity_id())
    {
        return;
    }

    // Feedback is keyed by server id, which means nothing once the page is
    // looking at a different project's servers.
    settings_window.mcp_page.connection_tests.clear();
    settings_window.mcp_page.client_secret_input = None;

    let subscription = cx.subscribe(
        &store,
        |this, _store, event: &ServerStatusChangedEvent, cx| {
            let secret_prompt_is_stale =
                this.mcp_page
                    .client_secret_input
                    .as_ref()
                    .is_some_and(|(id, _)| {
                        id == &event.server_id
                            && !matches!(
                                event.status,
                                ContextServerStatus::ClientSecretRequired { .. }
                            )
                    });
            if secret_prompt_is_stale {
                this.mcp_page.client_secret_input = None;
            }
            cx.notify();
        },
    );

    settings_window.mcp_page.store_subscription = Some((store.entity_id(), subscription));
}

pub(crate) fn render_mcp_servers_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let context_server_store = get_context_server_store(settings_window, cx);

    let server_list = if let Some(store) = context_server_store.as_ref() {
        let server_ids = store.read(cx).server_ids().to_vec();

        if server_ids.is_empty() {
            render_empty_state(cx)
        } else {
            render_server_list(settings_window, &server_ids, store, cx)
        }
    } else {
        render_no_project_state(cx)
    };

    let timeout_setting = render_context_server_timeout(settings_window, window, cx);

    v_flex()
        .id("mcp-servers-page")
        .track_scroll(scroll_handle)
        .size_full()
        .pt_2p5()
        .pb_16()
        .overflow_y_scroll()
        .child(
            v_flex()
                .w_full()
                .px_8()
                .gap_2()
                .child(
                    v_flex().child(Label::new("構成済みのサーバー")).child(
                        Label::new("直接、または拡張機能を通じて接続されたサーバーを管理します。")
                            .size(LabelSize::Small)
                            .color(Color::Muted),
                    ),
                )
                .child(server_list)
                .child(Divider::horizontal()),
        )
        .child(timeout_setting)
        .into_any_element()
}

fn render_context_server_timeout(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let item = SettingsPageItem::SettingItem(SettingItem {
        title: "MCP サーバーのタイムアウト",
        description: "MCP サーバーのツール呼び出しの既定のタイムアウト (秒) です。",
        field: Box::new(SettingField {
            organization_override: None,
            json_path: Some("context_server_timeout"),
            pick: |settings_content| settings_content.project.context_server_timeout.as_ref(),
            write: |settings_content, value, _| {
                settings_content.project.context_server_timeout = value;
            },
        }),
        metadata: None,
        files: USER | PROJECT,
    });

    item.render(settings_window, 0, false, false, window, cx)
}

fn get_context_server_store(
    settings_window: &SettingsWindow,
    cx: &App,
) -> Option<Entity<ContextServerStore>> {
    let project = settings_window.active_project(cx)?;
    Some(project.read(cx).context_server_store())
}

fn render_empty_state(cx: &App) -> AnyElement {
    h_flex()
        .p_4()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .rounded_sm()
        .child(
            Label::new(
                "MCP サーバーはまだ追加されていません。「サーバーを追加」から始めてください。",
            )
            .color(Color::Muted)
            .size(LabelSize::Small),
        )
        .into_any_element()
}

fn render_no_project_state(cx: &App) -> AnyElement {
    h_flex()
        .p_4()
        .justify_center()
        .border_1()
        .border_dashed()
        .border_color(cx.theme().colors().border.opacity(0.6))
        .rounded_sm()
        .child(
            Label::new("アクティブなプロジェクトがありません。MCP サーバーを管理するにはワークスペースを開いてください。")
                .color(Color::Muted)
                .size(LabelSize::Small),
        )
        .into_any_element()
}

fn render_server_list(
    settings_window: &SettingsWindow,
    server_ids: &[ContextServerId],
    store: &Entity<ContextServerStore>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    v_flex()
        .w_full()
        .gap_1()
        .children(itertools::intersperse_with(
            server_ids.iter().map(|server_id| {
                render_context_server(settings_window, server_id, store, cx).into_any_element()
            }),
            || Divider::horizontal_dashed().into_any_element(),
        ))
        .into_any_element()
}

fn render_context_server(
    settings_window: &SettingsWindow,
    context_server_id: &ContextServerId,
    store: &Entity<ContextServerStore>,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let server_status = store
        .read(cx)
        .status_for_server(context_server_id)
        .unwrap_or(ContextServerStatus::Stopped);
    let server_configuration = store.read(cx).configuration_for_server(context_server_id);

    let is_enabled = store.read(cx).is_server_enabled(context_server_id, cx);
    let is_transitioning = matches!(
        server_status,
        ContextServerStatus::Starting | ContextServerStatus::Authenticating
    );
    let item_id = SharedString::from(context_server_id.0.to_string());

    // Determine the source from the configured settings rather than the runtime
    // configuration: a custom (Stdio/HTTP) server that is disabled or not yet
    // started has no runtime configuration, and must not be mistaken for an
    // extension-provided server.
    let provided_by_extension = store.read(cx).is_extension_provided(context_server_id, cx);
    let display_name = if provided_by_extension {
        resolve_extension_display_name(context_server_id, cx).unwrap_or_else(|| item_id.clone())
    } else {
        item_id.clone()
    };

    let source = if provided_by_extension {
        AiSettingItemSource::Extension
    } else {
        AiSettingItemSource::Custom
    };

    let status = map_server_status(&server_status);

    let should_show_logout = server_configuration.as_ref().is_some_and(|config| {
        matches!(config.as_ref(), ContextServerConfiguration::Http { .. })
            && !config.has_static_auth_header()
    });

    let server_settings = store
        .read(cx)
        .settings_for_server(context_server_id)
        .cloned();

    // ContextServerRegistry is per-project (not a global), so the tool count is
    // not available here; the detail label reports egress control instead.
    let proxy_status = proxy_status_for_server(
        server_settings.as_ref(),
        server_configuration.as_deref(),
        store.read(cx).is_remote_project(),
        http_client::child_process_proxy().is_some(),
    );

    let test = settings_window
        .mcp_page
        .connection_tests
        .get(context_server_id);
    let is_testing = matches!(
        test.map(|test| &test.outcome),
        Some(McpConnectionTestOutcome::Running)
    );

    let test_button = render_connection_test_button(context_server_id, is_testing, cx);
    let configure_button = (!provided_by_extension).then(|| {
        render_configure_button(
            context_server_id,
            cx.entity().downgrade(),
            server_settings.clone(),
        )
    });
    let uninstall_button = render_uninstall_button(context_server_id, provided_by_extension);

    // Build toggle switch
    let toggle_switch = render_toggle_switch(
        context_server_id,
        store,
        is_enabled,
        is_transitioning,
        configured_remote(server_settings.as_ref(), server_configuration.as_deref()),
    );

    // Surface invalid settings (which prevent the server from starting at all)
    // ahead of runtime status feedback, so the misconfiguration is visible.
    let status_details = match settings_validation_error(server_settings.as_ref()) {
        Some(error) => Some(render_form_error(error).into_any_element()),
        None => render_status_details(
            settings_window,
            &server_status,
            context_server_id,
            store,
            should_show_logout,
            cx,
        ),
    };
    let test_feedback = test.map(|test| render_connection_test_feedback(&test.outcome));
    let details = (test_feedback.is_some() || status_details.is_some()).then(|| {
        v_flex()
            .w_full()
            .min_w_0()
            .children(test_feedback)
            .children(status_details)
            .into_any_element()
    });

    AiSettingItem::new(item_id, display_name, status, source)
        .action(test_button)
        .when_some(configure_button, |this, button| this.action(button))
        .action(uninstall_button)
        .action(toggle_switch)
        .when_some(proxy_status.detail(), |this, (label, tooltip)| {
            this.detail_label(label).detail_label_tooltip(tooltip)
        })
        .when_some(details, |this, details| this.details(details))
}

fn map_server_status(status: &ContextServerStatus) -> AiSettingItemStatus {
    match status {
        ContextServerStatus::Starting => AiSettingItemStatus::Starting,
        ContextServerStatus::Running => AiSettingItemStatus::Running,
        ContextServerStatus::Stopped => AiSettingItemStatus::Stopped,
        ContextServerStatus::Error(_) => AiSettingItemStatus::Error,
        ContextServerStatus::AuthRequired => AiSettingItemStatus::AuthRequired,
        ContextServerStatus::ClientSecretRequired { .. } => {
            AiSettingItemStatus::ClientSecretRequired
        }
        ContextServerStatus::Authenticating => AiSettingItemStatus::Authenticating,
    }
}

fn resolve_extension_display_name(id: &ContextServerId, cx: &App) -> Option<SharedString> {
    ExtensionStore::global(cx)
        .read(cx)
        .installed_extensions()
        .iter()
        .find(|(_, entry)| entry.manifest.context_servers.contains_key(&id.0))
        .map(|(_, entry)| {
            let name = entry.manifest.name.as_str();
            let stripped = name
                .strip_suffix(" MCP Server")
                .or_else(|| name.strip_suffix(" MCP"))
                .or_else(|| name.strip_suffix(" Context Server"))
                .unwrap_or(name);
            SharedString::from(stripped.to_string())
        })
}

fn render_configure_button(
    context_server_id: &ContextServerId,
    settings_window: WeakEntity<SettingsWindow>,
    server_settings: Option<ContextServerSettings>,
) -> impl IntoElement {
    let context_server_id = context_server_id.clone();

    IconButton::new(
        format!("mcp-configure-btn-{}", context_server_id.0),
        IconName::Settings,
    )
    .icon_size(IconSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text("MCP サーバーを設定"))
    .on_click(move |_event, window, cx| {
        let transport = match &server_settings {
            Some(ContextServerSettings::Http { .. }) => McpTransport::Http,
            _ => McpTransport::Stdio,
        };
        let existing = server_settings
            .clone()
            .map(|settings| (context_server_id.clone(), settings));
        settings_window
            .update(cx, |this, cx| {
                open_mcp_server_form(this, transport, existing, window, cx);
            })
            .log_err();
    })
}

/// Rendered as a labelled button rather than a bare icon: the row's other
/// controls are all unlabelled icons, and a fourth glyph among them was reported
/// as invisible in practice — the only thing naming it was its hover tooltip.
fn render_connection_test_button(
    context_server_id: &ContextServerId,
    is_testing: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let context_server_id = context_server_id.clone();

    Button::new(
        SharedString::from(format!("mcp-test-btn-{}", context_server_id.0)),
        if is_testing {
            "テスト中…"
        } else {
            "接続テスト"
        },
    )
    .style(ButtonStyle::Outlined)
    .label_size(LabelSize::Small)
    .start_icon(
        Icon::new(IconName::RotateCw)
            .size(IconSize::Small)
            .color(Color::Muted),
    )
    .tab_index(0isize)
    .disabled(is_testing)
    .tooltip(Tooltip::text(
        "サーバーを再接続して、接続できるかどうかを確認します。",
    ))
    .on_click(cx.listener(move |this, _event, _window, cx| {
        start_connection_test(this, context_server_id.clone(), cx);
    }))
    .into_any_element()
}

fn render_connection_test_feedback(outcome: &McpConnectionTestOutcome) -> AnyElement {
    let (icon, color, message): (IconName, Color, SharedString) = match outcome {
        McpConnectionTestOutcome::Running => (
            IconName::RotateCw,
            Color::Muted,
            "接続をテストしています…".into(),
        ),
        McpConnectionTestOutcome::Succeeded => (
            IconName::Check,
            Color::Success,
            "接続テスト: 接続できました。".into(),
        ),
        McpConnectionTestOutcome::NeedsAction(message) => (
            IconName::Info,
            Color::Warning,
            format!("接続テスト: {message}").into(),
        ),
        McpConnectionTestOutcome::Failed(message) => (
            IconName::XCircle,
            Color::Error,
            format!("接続テスト: {message}").into(),
        ),
    };

    h_flex()
        .py_1()
        .min_w_0()
        .w_full()
        .gap_2()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(
            div()
                .min_w_0()
                .flex_1()
                // Coloured like its icon so the outcome reads without hovering.
                .child(Label::new(message).color(color).size(LabelSize::Small)),
        )
        .into_any_element()
}

/// Restarts a server and reports where it ended up, so the user can tell a
/// working configuration from a broken one without leaving the page.
fn start_connection_test(
    settings_window: &mut SettingsWindow,
    context_server_id: ContextServerId,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        return;
    };

    if !store.read(cx).is_server_enabled(&context_server_id, cx) {
        settings_window.mcp_page.report(
            context_server_id,
            McpConnectionTestOutcome::Failed(
                "このサーバーは無効です。有効にしてから接続をテストしてください。".into(),
            ),
        );
        cx.notify();
        return;
    }

    // `start_server` needs the store's own handle for the server, which only
    // exists once the store has built it. A server that is enabled but absent
    // was never successfully created (a missing extension, or settings that
    // could not be resolved), and there is nothing here to restart.
    let Some(server) = store.read(cx).get_server(&context_server_id) else {
        settings_window.mcp_page.report(
            context_server_id,
            McpConnectionTestOutcome::Failed(
                "このサーバーはまだ作成されていません。設定と拡張機能を確認してください。".into(),
            ),
        );
        cx.notify();
        return;
    };

    // Stop before the wait is set up: the wait treats `Stopped` as a failure and
    // must not see the one this restart causes.
    store.update(cx, |store, cx| {
        store.stop_server(&context_server_id, cx).log_err();
    });

    let wait = wait_for_context_server(&store, context_server_id.clone(), cx);
    let task = cx.spawn({
        let context_server_id = context_server_id.clone();
        async move |this, cx| {
            let outcome = match wait.await {
                Ok(ContextServerStatus::Running) => McpConnectionTestOutcome::Succeeded,
                Ok(ContextServerStatus::AuthRequired) => {
                    McpConnectionTestOutcome::NeedsAction("接続するには認証が必要です。".into())
                }
                Ok(ContextServerStatus::ClientSecretRequired { .. }) => {
                    McpConnectionTestOutcome::NeedsAction(
                        "接続するにはクライアントシークレットが必要です。".into(),
                    )
                }
                Ok(_) => McpConnectionTestOutcome::Failed(
                    "サーバーが接続できる状態になりませんでした。".into(),
                ),
                Err(error) => McpConnectionTestOutcome::Failed(error.to_string().into()),
            };
            this.update(cx, |this, cx| {
                // Only the outcome is replaced: this task is stored in the very
                // entry being updated, and replacing the entry would drop it.
                if let Some(test) = this.mcp_page.connection_tests.get_mut(&context_server_id) {
                    test.outcome = outcome;
                }
                cx.notify();
            })
            .log_err();
        }
    });

    settings_window.mcp_page.connection_tests.insert(
        context_server_id,
        McpConnectionTest {
            outcome: McpConnectionTestOutcome::Running,
            _task: Some(task),
        },
    );

    store.update(cx, |store, cx| store.start_server(server, cx));
    cx.notify();
}

fn render_uninstall_button(
    context_server_id: &ContextServerId,
    provided_by_extension: bool,
) -> impl IntoElement {
    let context_server_id = context_server_id.clone();

    IconButton::new(
        format!("mcp-uninstall-btn-{}", context_server_id.0),
        IconName::Trash,
    )
    .icon_size(IconSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text("MCP サーバーをアンインストール"))
    .on_click(move |_event, _window, cx| {
        uninstall_server(&context_server_id, provided_by_extension, cx);
    })
}

fn render_toggle_switch(
    context_server_id: &ContextServerId,
    store: &Entity<ContextServerStore>,
    is_enabled: bool,
    is_transitioning: bool,
    configured_remote: bool,
) -> impl IntoElement {
    let context_server_id = context_server_id.clone();
    let store = store.clone();

    Switch::new(
        SharedString::from(format!("mcp-toggle-{}", context_server_id.0)),
        if is_enabled {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        },
    )
    .disabled(is_transitioning)
    .tab_index(0isize)
    .on_click({
        move |state, _window, cx| {
            let is_enabled = match state {
                ToggleState::Unselected | ToggleState::Indeterminate => {
                    store.update(cx, |this, cx| {
                        this.stop_server(&context_server_id, cx).log_err();
                    });
                    false
                }
                ToggleState::Selected => {
                    store.update(cx, |this, cx| {
                        if let Some(server) = this.get_server(&context_server_id) {
                            this.start_server(server, cx);
                        }
                    });
                    true
                }
            };

            let fs = <dyn fs::Fs>::global(cx);
            settings::update_settings_file(fs, cx, {
                let context_server_id = context_server_id.clone();
                move |settings, _| {
                    settings
                        .project
                        .context_servers
                        .entry(context_server_id.0.clone())
                        // Materializing an entry for a server that had none must
                        // not silently move it back to this machine: carry over
                        // the execution host it is running with.
                        .or_insert_with(|| ContextServerSettingsContent::Extension {
                            enabled: is_enabled,
                            remote: configured_remote,
                            settings: serde_json::json!({}),
                        })
                        .set_enabled(is_enabled);
                }
            });
        }
    })
}

fn render_status_details(
    settings_window: &SettingsWindow,
    server_status: &ContextServerStatus,
    context_server_id: &ContextServerId,
    store: &Entity<ContextServerStore>,
    should_show_logout: bool,
    cx: &mut Context<SettingsWindow>,
) -> Option<AnyElement> {
    let feedback_base = || h_flex().py_1().min_w_0().w_full().gap_1().justify_between();

    match server_status {
        ContextServerStatus::Error(error) => {
            let store = store.clone();
            let context_server_id = context_server_id.clone();
            Some(
                feedback_base()
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_2()
                            .child(
                                Icon::new(IconName::XCircle)
                                    .size(IconSize::XSmall)
                                    .color(Color::Error),
                            )
                            .child(
                                div().min_w_0().flex_1().child(
                                    Label::new(error.to_string())
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                ),
                            ),
                    )
                    .when(should_show_logout, |this| {
                        this.child(
                            Button::new(
                                SharedString::from(format!("error-logout-{}", context_server_id.0)),
                                "ログアウト",
                            )
                            .style(ButtonStyle::Outlined)
                            .label_size(LabelSize::Small)
                            .on_click({
                                let store = store.clone();
                                let context_server_id = context_server_id.clone();
                                move |_event, _window, cx| {
                                    store.update(cx, |s, cx| {
                                        s.logout_server(&context_server_id, cx).log_err();
                                    });
                                }
                            }),
                        )
                    })
                    .into_any_element(),
            )
        }
        ContextServerStatus::AuthRequired => {
            let store = store.clone();
            let context_server_id = context_server_id.clone();
            Some(
                feedback_base()
                    .child(
                        h_flex()
                            .min_w_0()
                            .w_full()
                            .gap_2()
                            .child(
                                Icon::new(IconName::Info)
                                    .size(IconSize::XSmall)
                                    .color(Color::Muted),
                            )
                            .child(
                                Label::new("このサーバーに接続するには認証してください")
                                    .color(Color::Muted)
                                    .size(LabelSize::Small),
                            ),
                    )
                    .child(
                        Button::new(
                            SharedString::from(format!(
                                "authenticate-server-{}",
                                context_server_id.0
                            )),
                            "認証",
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click({
                            move |_event, _window, cx| {
                                store.update(cx, |s, cx| {
                                    s.authenticate_server(&context_server_id, cx).log_err();
                                });
                            }
                        }),
                    )
                    .into_any_element(),
            )
        }
        ContextServerStatus::ClientSecretRequired { error } => Some(render_client_secret_details(
            settings_window,
            context_server_id,
            error.as_deref(),
            cx,
        )),
        ContextServerStatus::Authenticating => Some(
            h_flex()
                .mt_1()
                .min_w_0()
                .w_full()
                .gap_2()
                .child(div().size_3().flex_shrink_0())
                .child(
                    Label::new("認証中…")
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                )
                .into_any_element(),
        ),
        ContextServerStatus::Running if should_show_logout => {
            let store = store.clone();
            let context_server_id = context_server_id.clone();
            Some(
                h_flex()
                    .py_1()
                    .w_full()
                    .justify_end()
                    .child(
                        Button::new(
                            SharedString::from(format!("running-logout-{}", context_server_id.0)),
                            "ログアウト",
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(move |_event, _window, cx| {
                            store.update(cx, |s, cx| {
                                s.logout_server(&context_server_id, cx).log_err();
                            });
                        }),
                    )
                    .into_any_element(),
            )
        }
        _ => None,
    }
}

/// The `ClientSecretRequired` state is a dead end without somewhere to type the
/// secret: the OAuth flow cannot continue until one is supplied.
fn render_client_secret_details(
    settings_window: &SettingsWindow,
    context_server_id: &ContextServerId,
    error: Option<&str>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let input = settings_window
        .mcp_page
        .client_secret_input
        .as_ref()
        .filter(|(id, _)| id == context_server_id)
        .map(|(_, editor)| editor.clone());

    v_flex()
        .py_1()
        .min_w_0()
        .w_full()
        .gap_1()
        .child(
            h_flex()
                .min_w_0()
                .w_full()
                .gap_1()
                .justify_between()
                .child(
                    h_flex()
                        .min_w_0()
                        .w_full()
                        .gap_2()
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .child(
                            div().min_w_0().flex_1().child(
                                Label::new(
                                    "このサーバーに接続するにはクライアントシークレットが必要です",
                                )
                                .color(Color::Muted)
                                .size(LabelSize::Small),
                            ),
                        ),
                )
                .when(input.is_none(), |this| {
                    let context_server_id = context_server_id.clone();
                    this.child(
                        Button::new(
                            SharedString::from(format!(
                                "client-secret-open-{}",
                                context_server_id.0
                            )),
                            "シークレットを入力",
                        )
                        .style(ButtonStyle::Outlined)
                        .label_size(LabelSize::Small)
                        .on_click(cx.listener(
                            move |this, _event, window, cx| {
                                let editor = new_secret_input(window, cx);
                                editor.focus_handle(cx).focus(window, cx);
                                this.mcp_page.client_secret_input =
                                    Some((context_server_id.clone(), editor));
                                cx.notify();
                            },
                        )),
                    )
                }),
        )
        .when_some(error, |this, error| {
            this.child(
                Label::new(error.to_string())
                    .color(Color::Error)
                    .size(LabelSize::Small),
            )
        })
        .when_some(input, |this, editor| {
            let submit_id = context_server_id.clone();
            this.child(
                v_flex()
                    .w_full()
                    .gap_1()
                    .child(input_box(&editor, cx))
                    .child(
                        Label::new("パブリッククライアントの場合は空欄のまま送信してください。")
                            .color(Color::Muted)
                            .size(LabelSize::Small),
                    )
                    .child(
                        h_flex()
                            .w_full()
                            .gap_1()
                            .justify_end()
                            .child(
                                Button::new(
                                    SharedString::from(format!(
                                        "client-secret-cancel-{}",
                                        submit_id.0
                                    )),
                                    "キャンセル",
                                )
                                .style(ButtonStyle::Subtle)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(
                                    |this, _event, _window, cx| {
                                        this.mcp_page.client_secret_input = None;
                                        cx.notify();
                                    },
                                )),
                            )
                            .child(
                                Button::new(
                                    SharedString::from(format!(
                                        "client-secret-submit-{}",
                                        submit_id.0
                                    )),
                                    "送信",
                                )
                                .style(ButtonStyle::Filled)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(
                                    move |this, _event, _window, cx| {
                                        submit_client_secret(this, &submit_id, cx);
                                    },
                                )),
                            ),
                    ),
            )
        })
        .into_any_element()
}

fn new_secret_input(window: &mut Window, cx: &mut Context<SettingsWindow>) -> Entity<Editor> {
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(
            "クライアントシークレット (パブリッククライアントの場合は空欄)",
            window,
            cx,
        );
        editor.set_masked(true, cx);
        editor
    })
}

fn submit_client_secret(
    settings_window: &mut SettingsWindow,
    context_server_id: &ContextServerId,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        return;
    };
    let Some(secret) = settings_window
        .mcp_page
        .client_secret_input
        .as_ref()
        .filter(|(id, _)| id == context_server_id)
        .map(|(_, editor)| editor.read(cx).text(cx))
    else {
        return;
    };

    settings_window.mcp_page.client_secret_input = None;
    settings_window
        .mcp_page
        .connection_tests
        .remove(context_server_id);

    if let Err(error) = store.update(cx, |store, cx| {
        store.submit_client_secret(context_server_id, secret, cx)
    }) {
        settings_window.mcp_page.report(
            context_server_id.clone(),
            McpConnectionTestOutcome::Failed(
                format!("クライアントシークレットを送信できませんでした: {error}").into(),
            ),
        );
    }
    cx.notify();
}

pub(crate) fn render_add_server_popover(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let original_window = settings_window.original_window;
    // Stable handle so the button keeps focus state across renders and can show a
    // focus ring even when the page is opened (and the button auto-focused) via a
    // mouse click, where `focus_visible` styling is suppressed.
    let focus_handle = settings_window
        .mcp_add_server_focus_handle
        .clone()
        .tab_index(0)
        .tab_stop(true);
    let is_focused = focus_handle.is_focused(window);
    let border_color = if is_focused {
        cx.theme().colors().border_focused
    } else {
        gpui::transparent_black()
    };
    let settings_window = cx.entity().downgrade();

    let popover = PopoverMenu::new("add-mcp-server-popover")
        .trigger(
            Button::new("add-mcp-server", "サーバーを追加")
                .style(ButtonStyle::Outlined)
                .track_focus(&focus_handle)
                .start_icon(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .label_size(LabelSize::Small),
        )
        .anchor(gpui::Anchor::TopRight)
        .menu({
            move |window, cx| {
                let settings_window = settings_window.clone();
                Some(ContextMenu::build(window, cx, move |menu, _window, _cx| {
                    menu.entry("ローカルサーバーを追加", None, {
                        let settings_window = settings_window.clone();
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_mcp_server_form(
                                        this,
                                        McpTransport::Stdio,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
                    .entry("リモートサーバーを追加", None, {
                        let settings_window = settings_window.clone();
                        move |window, cx| {
                            settings_window
                                .update(cx, |this, cx| {
                                    open_mcp_server_form(
                                        this,
                                        McpTransport::Http,
                                        None,
                                        window,
                                        cx,
                                    );
                                })
                                .log_err();
                        }
                    })
                    .separator()
                    .entry("拡張機能からインストール", None, {
                        move |_window, cx| {
                            if let Some(original_window) = original_window.as_ref() {
                                cx.activate(true);
                                original_window
                                    .update(cx, |_, window, cx| {
                                        window.activate_window();
                                        window.dispatch_action(
                                            zed_actions::Extensions {
                                                category_filter: Some(
                                                    ExtensionCategoryFilter::ContextServers,
                                                ),
                                                id: None,
                                            }
                                            .boxed_clone(),
                                            cx,
                                        );
                                    })
                                    .log_err();
                            }
                        }
                    })
                }))
            }
        });

    div()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .child(popover)
}

fn uninstall_server(
    context_server_id: &ContextServerId,
    provided_by_extension: bool,
    cx: &mut App,
) {
    if provided_by_extension {
        if let Some((ext_id, manifest)) =
            resolve_extension_for_context_server(context_server_id, cx)
        {
            if extension_only_provides_context_server(&manifest) {
                ExtensionStore::global(cx)
                    .update(cx, |store, cx| store.uninstall_extension(ext_id, cx))
                    .detach_and_log_err(cx);
            }
        }
    }

    let fs = <dyn fs::Fs>::global(cx);
    let context_server_id = context_server_id.clone();
    settings::update_settings_file(fs, cx, move |settings, _| {
        settings
            .project
            .context_servers
            .remove(&context_server_id.0);
    });
}

fn resolve_extension_for_context_server(
    id: &ContextServerId,
    cx: &App,
) -> Option<(Arc<str>, Arc<extension::ExtensionManifest>)> {
    ExtensionStore::global(cx)
        .read(cx)
        .installed_extensions()
        .iter()
        .find(|(_, entry)| entry.manifest.context_servers.contains_key(&id.0))
        .map(|(id, entry)| (id.clone(), entry.manifest.clone()))
}

fn extension_only_provides_context_server(manifest: &extension::ExtensionManifest) -> bool {
    manifest.context_servers.len() == 1
        && manifest.themes.is_empty()
        && manifest.icon_themes.is_empty()
        && manifest.languages.is_empty()
        && manifest.grammars.is_empty()
        && manifest.language_servers.is_empty()
        && manifest.slash_commands.is_empty()
        && manifest.snippets.is_none()
        && manifest.debug_locators.is_empty()
}

// === Custom (Stdio/HTTP) MCP server add/edit form ===

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum McpTransport {
    /// Local server launched via stdin/stdout.
    Stdio,
    /// Remote server connected over HTTP.
    Http,
}

#[derive(Clone, Copy)]
enum McpKvKind {
    Env,
    Header,
}

impl McpKvKind {
    fn rows_mut(self, form: &mut McpServerForm) -> &mut Vec<KeyValueRow> {
        match self {
            McpKvKind::Env => &mut form.env,
            McpKvKind::Header => &mut form.headers,
        }
    }

    fn remove_id(self) -> &'static str {
        match self {
            McpKvKind::Env => "mcp-env-remove",
            McpKvKind::Header => "mcp-header-remove",
        }
    }

    fn add_id(self) -> &'static str {
        match self {
            McpKvKind::Env => "mcp-env-add",
            McpKvKind::Header => "mcp-header-add",
        }
    }
}

struct KeyValueRow {
    key: Entity<Editor>,
    value: Entity<Editor>,
}

/// Editor-backed state for the custom MCP server add/edit form.
pub(crate) struct McpServerForm {
    transport: McpTransport,
    /// `Some` when editing an existing server (used to remove the old entry on rename).
    original_id: Option<ContextServerId>,
    name: Entity<Editor>,
    command: Entity<Editor>,
    args: Entity<Editor>,
    url: Entity<Editor>,
    timeout: Entity<Editor>,
    oauth_client_id: Entity<Editor>,
    env: Vec<KeyValueRow>,
    headers: Vec<KeyValueRow>,
    /// Run the server on the SSH host instead of this machine. Only meaningful
    /// for a stdio server in a remote project, but round-tripped in every case
    /// so saving the form never drops a hand-written value.
    remote: bool,
    error: Option<SharedString>,
}

impl McpServerForm {
    fn new(
        transport: McpTransport,
        existing: Option<(ContextServerId, ContextServerSettings)>,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        let original_id = existing.as_ref().map(|(id, _)| id.clone());
        let settings = existing.map(|(_, settings)| settings);
        let name_initial = original_id.as_ref().map(|id| id.0.to_string());

        let mut command_initial = None;
        let mut args_initial = None;
        let mut url_initial = None;
        let mut timeout_initial = None;
        let mut oauth_initial = None;
        let mut env = Vec::new();
        let mut headers = Vec::new();
        // Not shown outside a remote project, but read back regardless so saving
        // the form does not silently discard a hand-written execution host.
        let remote = configured_remote(settings.as_ref(), None);

        // Pre-fill from the raw settings so invalid values (e.g. a malformed URL
        // the user typed directly into settings.json) still load into the form
        // for correction, rather than being dropped during resolution.
        if let Some(settings) = settings.as_ref() {
            match settings {
                ContextServerSettings::Stdio { command, .. } => {
                    command_initial = Some(command.path.to_string_lossy().to_string());
                    if !command.args.is_empty() {
                        args_initial = Some(command.args.join(" "));
                    }
                    timeout_initial = command.timeout.map(|timeout| timeout.to_string());
                    if let Some(env_map) = &command.env {
                        for (key, value) in sorted_pairs(env_map) {
                            env.push(new_kv_row(Some(&key), Some(&value), window, cx));
                        }
                    }
                }
                ContextServerSettings::Http {
                    url,
                    headers: header_map,
                    timeout,
                    oauth,
                    ..
                } => {
                    url_initial = Some(url.clone());
                    timeout_initial = timeout.map(|timeout| timeout.to_string());
                    for (key, value) in sorted_pairs(header_map) {
                        headers.push(new_kv_row(Some(&key), Some(&value), window, cx));
                    }
                    oauth_initial = oauth.as_ref().map(|oauth| oauth.client_id.clone());
                }
                ContextServerSettings::Extension { .. } => {}
            }
        }

        Self {
            transport,
            original_id,
            name: new_input("my-mcp-server", name_initial.as_deref(), window, cx),
            command: new_input("/path/to/server", command_initial.as_deref(), window, cx),
            args: new_input("--flag value", args_initial.as_deref(), window, cx),
            url: new_input(
                "https://example.com/mcp",
                url_initial.as_deref(),
                window,
                cx,
            ),
            timeout: new_input("60", timeout_initial.as_deref(), window, cx),
            oauth_client_id: new_input(
                "OAuth クライアント ID (省略可)",
                oauth_initial.as_deref(),
                window,
                cx,
            ),
            env,
            headers,
            remote,
            error: None,
        }
    }
}

fn sorted_pairs(map: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = map
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    pairs.sort_by(|a, b| a.0.cmp(&b.0));
    pairs
}

fn new_input(
    placeholder: &str,
    initial: Option<&str>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    let placeholder = placeholder.to_string();
    let initial = initial.map(|text| text.to_string());
    cx.new(|cx| {
        let mut editor = Editor::single_line(window, cx);
        editor.set_placeholder_text(placeholder.as_str(), window, cx);
        if let Some(text) = initial {
            editor.set_text(text, window, cx);
        }
        editor
    })
}

fn new_kv_row(
    key: Option<&str>,
    value: Option<&str>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> KeyValueRow {
    KeyValueRow {
        key: new_input("キー", key, window, cx),
        value: new_input("値", value, window, cx),
    }
}

/// Creates the form state and pushes the form sub-page onto the stack.
pub(crate) fn open_mcp_server_form(
    settings_window: &mut SettingsWindow,
    transport: McpTransport,
    existing: Option<(ContextServerId, ContextServerSettings)>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let is_edit = existing.is_some();
    settings_window.mcp_server_form = Some(McpServerForm::new(transport, existing, window, cx));

    let title = if is_edit {
        "MCP サーバーの設定"
    } else {
        match transport {
            McpTransport::Stdio => "ローカル MCP サーバーの追加",
            McpTransport::Http => "リモート MCP サーバーの追加",
        }
    };

    settings_window.push_dynamic_sub_page(
        title,
        "エージェントの設定",
        Some("context_servers"),
        false,
        render_mcp_server_form_page,
        window,
        cx,
    );
}

fn render_mcp_server_form_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(form) = settings_window.mcp_server_form.as_ref() else {
        return div().into_any_element();
    };
    let transport = form.transport;
    let error = form.error.clone();
    let remote = form.remote;
    // Outside a remote project the setting has no effect (the server always runs
    // here), so offering the choice would only invite a wrong expectation.
    let show_execution_host = get_context_server_store(settings_window, cx)
        .is_some_and(|store| store.read(cx).is_remote_project());

    let fields = v_flex()
        .w_full()
        .gap_4()
        .child(render_form_field(
            settings_window,
            "サーバー名",
            "必須。この MCP サーバーを識別する一意の名前です。",
            &form.name,
            cx,
        ))
        .map(|this| match transport {
            McpTransport::Stdio => this
                .child(render_form_field(
                    settings_window,
                    "コマンド",
                    "必須。サーバーを起動する実行ファイルのパスです。",
                    &form.command,
                    cx,
                ))
                .child(render_form_field(
                    settings_window,
                    "引数",
                    "コマンドに渡す引数です。半角スペースで区切ります。",
                    &form.args,
                    cx,
                ))
                .child(render_kv_section(
                    settings_window,
                    "環境変数",
                    "サーバーのプロセスに渡す環境変数です。",
                    &form.env,
                    McpKvKind::Env,
                    cx,
                ))
                .child(render_form_field(
                    settings_window,
                    "タイムアウト (秒)",
                    "サーバーの応答をタイムアウトまでどれだけ待つかを設定します。",
                    &form.timeout,
                    cx,
                ))
                .when(show_execution_host, |this| {
                    this.child(render_execution_host_field(settings_window, remote, cx))
                }),
            McpTransport::Http => this
                .child(render_form_field(
                    settings_window,
                    "URL",
                    "必須。リモート MCP サーバーのベース URL です。",
                    &form.url,
                    cx,
                ))
                .child(render_kv_section(
                    settings_window,
                    "ヘッダー",
                    "サーバーへの各リクエストに付与する HTTP ヘッダーです。",
                    &form.headers,
                    McpKvKind::Header,
                    cx,
                ))
                .child(render_form_field(
                    settings_window,
                    "タイムアウト (秒)",
                    "サーバーの応答をタイムアウトまでどれだけ待つかを設定します。",
                    &form.timeout,
                    cx,
                ))
                .child(render_form_field(
                    settings_window,
                    "OAuth クライアント ID",
                    "サーバーの認証に使う OAuth クライアント ID です (省略可)。",
                    &form.oauth_client_id,
                    cx,
                )),
        })
        .when_some(error, |this, error| this.child(render_form_error(error)))
        .child(render_form_actions(cx));

    v_flex()
        .id("mcp-server-form-page")
        .size_full()
        .pt_2p5()
        .px_8()
        .pb_16()
        .track_scroll(scroll_handle)
        .overflow_y_scroll()
        .child(fields)
        .into_any_element()
}

fn input_box(editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let colors = cx.theme().colors();
    // All form inputs share tab index 0, so tab order follows render (insertion)
    // order. Tracking the editor's focus handle makes the field a tab stop and
    // routes keyboard focus into the editor when tabbed to.
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    h_flex()
        .min_w_64()
        .py_1()
        .px_2()
        .h_8()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(colors.border_focused))
        .child(editor.clone())
}

fn render_form_field(
    settings_window: &SettingsWindow,
    title: &'static str,
    description: &'static str,
    editor: &Entity<Editor>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let control = input_box(editor, cx).into_any_element();
    crate::render_settings_item_layout(
        settings_window,
        title,
        description,
        control,
        None,
        None,
        None,
        false,
        cx,
    )
    .into_any_element()
}

/// The "run it here / run it over there" choice for a stdio server, shown only
/// in a remote project. Deliberately worded as an *execution host* rather than
/// local/remote *server*, which this page already uses for stdio versus HTTP.
fn render_execution_host_field(
    settings_window: &SettingsWindow,
    remote: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let control = Switch::new(
        "mcp-form-execution-host",
        if remote {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        },
    )
    .label(if remote {
        "SSH 接続先で実行"
    } else {
        "この PC で実行"
    })
    .tab_index(0isize)
    .on_click(cx.listener(|this, state, _window, cx| {
        if let Some(form) = this.mcp_server_form.as_mut() {
            form.remote = *state == ToggleState::Selected;
        }
        cx.notify();
    }))
    .into_any_element();

    crate::render_settings_item_layout(
        settings_window,
        "実行ホスト",
        "SSH 接続先で実行する場合、コマンドは接続先で解決されます (拡張機能提供の場合は接続先にも同じ拡張機能が必要です)。この PC で実行する場合、サーバーが読み書きするファイルはこの PC のものになります。",
        control,
        None,
        None,
        None,
        false,
        cx,
    )
    .into_any_element()
}

fn render_kv_section(
    settings_window: &SettingsWindow,
    title: &'static str,
    description: &'static str,
    rows: &[KeyValueRow],
    kind: McpKvKind,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let control = v_flex()
        .min_w_64()
        .gap_2()
        .children(rows.iter().enumerate().map(|(ix, row)| {
            v_flex()
                .gap_1()
                .child(
                    h_flex()
                        .gap_1()
                        .items_center()
                        .child(input_box(&row.key, cx))
                        .child(
                            IconButton::new((kind.remove_id(), ix), IconName::Close)
                                .icon_size(IconSize::Small)
                                .icon_color(Color::Muted)
                                .tooltip(Tooltip::text("削除"))
                                .on_click(cx.listener(move |this, _, _window, cx| {
                                    if let Some(form) = this.mcp_server_form.as_mut() {
                                        let rows = kind.rows_mut(form);
                                        if ix < rows.len() {
                                            rows.remove(ix);
                                        }
                                    }
                                    cx.notify();
                                })),
                        ),
                )
                .child(input_box(&row.value, cx))
        }))
        .child(
            Button::new(kind.add_id(), "追加")
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    let row = new_kv_row(None, None, window, cx);
                    if let Some(form) = this.mcp_server_form.as_mut() {
                        kind.rows_mut(form).push(row);
                    }
                    cx.notify();
                })),
        )
        .into_any_element();

    crate::render_settings_item_layout(
        settings_window,
        title,
        description,
        control,
        None,
        None,
        None,
        false,
        cx,
    )
    .into_any_element()
}

fn render_form_error(error: SharedString) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
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
        .gap_2()
        .justify_end()
        .pt_2()
        .child(
            Button::new("mcp-form-cancel", "キャンセル")
                .style(ButtonStyle::Subtle)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.mcp_server_form = None;
                    this.pop_sub_page(window, cx);
                })),
        )
        .child(
            Button::new("mcp-form-save", "保存")
                .style(ButtonStyle::Filled)
                .on_click(cx.listener(|this, _, window, cx| {
                    save_mcp_server_form(this, window, cx);
                })),
        )
}

fn save_mcp_server_form(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let built = {
        let Some(form) = settings_window.mcp_server_form.as_ref() else {
            return;
        };
        build_settings_from_form(form, cx)
    };

    let (id, original_id, content) = match built {
        Ok(value) => value,
        Err(error) => {
            if let Some(form) = settings_window.mcp_server_form.as_mut() {
                form.error = Some(error);
            }
            cx.notify();
            return;
        }
    };

    // Reject names that would collide with a *different* existing server. This
    // covers both adding a new server and renaming an existing one (where the new
    // name must not clobber another server's configuration).
    let collides_with_other_server =
        get_context_server_store(settings_window, cx).is_some_and(|store| {
            name_collides_with_other_server(&id, original_id.as_ref(), store.read(cx).server_ids())
        });
    if collides_with_other_server {
        if let Some(form) = settings_window.mcp_server_form.as_mut() {
            form.error = Some(format!("「{}」という名前のサーバーは既に存在します。", id.0).into());
        }
        cx.notify();
        return;
    }

    let fs = <dyn fs::Fs>::global(cx);
    settings::update_settings_file(fs, cx, move |settings, _| {
        if let Some(original_id) = &original_id
            && original_id.0 != id.0
        {
            settings.project.context_servers.remove(&original_id.0);
        }
        settings
            .project
            .context_servers
            .insert(id.0.clone(), content);
    });

    settings_window.mcp_server_form = None;
    settings_window.pop_sub_page(window, cx);
}

/// Plain (editor-free) snapshot of the form's contents, so the validation /
/// build logic can be exercised without a GPUI context.
struct McpServerFormValues {
    transport: McpTransport,
    original_id: Option<ContextServerId>,
    name: String,
    command: String,
    args: String,
    url: String,
    timeout: String,
    oauth_client_id: String,
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    remote: bool,
}

fn build_settings_from_form(
    form: &McpServerForm,
    cx: &App,
) -> Result<
    (
        ContextServerId,
        Option<ContextServerId>,
        ContextServerSettingsContent,
    ),
    SharedString,
> {
    let values = McpServerFormValues {
        transport: form.transport,
        original_id: form.original_id.clone(),
        name: form.name.read(cx).text(cx),
        command: form.command.read(cx).text(cx),
        args: form.args.read(cx).text(cx),
        url: form.url.read(cx).text(cx),
        timeout: form.timeout.read(cx).text(cx),
        oauth_client_id: form.oauth_client_id.read(cx).text(cx),
        env: read_kv(&form.env, cx),
        headers: read_kv(&form.headers, cx),
        remote: form.remote,
    };
    build_settings_from_values(&values)
}

fn read_kv(rows: &[KeyValueRow], cx: &App) -> Vec<(String, String)> {
    rows.iter()
        .map(|row| (row.key.read(cx).text(cx), row.value.read(cx).text(cx)))
        .collect()
}

fn build_settings_from_values(
    values: &McpServerFormValues,
) -> Result<
    (
        ContextServerId,
        Option<ContextServerId>,
        ContextServerSettingsContent,
    ),
    SharedString,
> {
    let name = values.name.trim().to_string();
    if name.is_empty() {
        return Err("サーバー名を入力してください。".into());
    }

    let timeout = parse_timeout(&values.timeout)?;

    let content = match values.transport {
        McpTransport::Stdio => {
            let command = values.command.trim().to_string();
            if command.is_empty() {
                return Err("コマンドを入力してください。".into());
            }
            let args = values
                .args
                .split_whitespace()
                .map(|arg| arg.to_string())
                .collect::<Vec<_>>();
            let env = collect_kv(&values.env, "環境変数")?;
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: values.remote,
                command: ContextServerCommand {
                    path: command.into(),
                    args,
                    env: (!env.is_empty()).then_some(env),
                    timeout,
                },
            }
        }
        McpTransport::Http => {
            let url = values.url.trim().to_string();
            if url.is_empty() {
                return Err("URL を入力してください。".into());
            }
            // Validate the URL on save (a deliberate action) rather than on every
            // render, so a clearly invalid URL is reported to the user instead of
            // being silently written and failing later when the server starts.
            if let Err(error) = url::Url::parse(&url) {
                return Err(format!("URL が正しくありません: {error}").into());
            }
            let headers = collect_kv(&values.headers, "ヘッダー")?;
            let oauth_client_id = values.oauth_client_id.trim().to_string();
            let oauth = (!oauth_client_id.is_empty()).then(|| OAuthClientSettings {
                client_id: oauth_client_id,
                client_secret: None,
            });
            ContextServerSettingsContent::Http {
                enabled: true,
                url,
                headers,
                timeout,
                oauth,
            }
        }
    };

    Ok((
        ContextServerId(name.into()),
        values.original_id.clone(),
        content,
    ))
}

/// Returns a human-readable error when a server's configured settings are
/// invalid in a way that prevents it from starting (currently: an HTTP server
/// whose URL cannot be parsed). Used to surface misconfiguration in the list.
fn settings_validation_error(settings: Option<&ContextServerSettings>) -> Option<SharedString> {
    match settings? {
        ContextServerSettings::Http { url, .. } if url::Url::parse(url).is_err() => {
            Some("設定内の URL が正しくありません。".into())
        }
        _ => None,
    }
}

/// Whether the fork's loopback egress proxy (ADR-F13 / F2-7) reaches a server's
/// traffic.
///
/// Recomputed rather than read back: [`ContextServerStore`] injects the proxy
/// into a copy of the command it spawns, so neither the settings nor the stored
/// runtime configuration record that it happened. The answer is therefore only
/// as current as the page's last render, which is fine — the proxy endpoint does
/// not change without a settings edit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum McpProxyStatus {
    Proxied,
    /// Child-process proxying is off, so this server's traffic never reaches the
    /// allowlist.
    ProxyDisabled,
    /// The server's own environment names a proxy, which the injection
    /// deliberately leaves alone.
    EnvOverride,
    /// Nothing to report: an HTTP server (whose requests go through the app's
    /// own allowlisted HTTP client) or a server that runs on the remote host.
    NotApplicable,
}

impl McpProxyStatus {
    /// The row's detail label and the explanation shown on hovering it.
    fn detail(self) -> Option<(SharedString, SharedString)> {
        match self {
            McpProxyStatus::Proxied => Some((
                "プロキシ経由".into(),
                "このサーバーの子プロセスはループバックのプロキシを経由するため、外部通信はネットワーク許可リストで統制されます。"
                    .into(),
            )),
            McpProxyStatus::ProxyDisabled => Some((
                "統制外".into(),
                "子プロセスへのプロキシ適用が無効なため、このサーバーの外部通信はネットワーク許可リストの統制外です。"
                    .into(),
            )),
            McpProxyStatus::EnvOverride => Some((
                "統制外 (独自プロキシ)".into(),
                "このサーバーの環境変数がプロキシを指定しているため、ループバックのプロキシは適用されません。外部通信はネットワーク許可リストの統制外です。"
                    .into(),
            )),
            McpProxyStatus::NotApplicable => None,
        }
    }
}

fn proxy_status_for_server(
    settings: Option<&ContextServerSettings>,
    configuration: Option<&ContextServerConfiguration>,
    is_remote_project: bool,
    proxy_running: bool,
) -> McpProxyStatus {
    let launch = child_process_launch(settings, configuration);

    // The execution host is a per-server choice: a `remote: false` server in a
    // remote project still spawns here, and is proxied like any local one.
    if !launch.spawns_child_process || (is_remote_project && launch.on_remote_host) {
        return McpProxyStatus::NotApplicable;
    }
    if !proxy_running {
        return McpProxyStatus::ProxyDisabled;
    }
    if launch
        .env
        .is_some_and(http_client::env_overrides_child_process_proxy)
    {
        return McpProxyStatus::EnvOverride;
    }
    McpProxyStatus::Proxied
}

struct ChildProcessLaunch<'a> {
    spawns_child_process: bool,
    /// Configured to run on the SSH host rather than this machine. Only takes
    /// effect in a remote project.
    on_remote_host: bool,
    env: Option<&'a HashMap<String, String>>,
}

/// Whether a server is launched as a child process and, if so, where and with
/// what environment. The runtime configuration wins where it exists: an
/// extension-provided server's command comes from the extension, not settings.
fn child_process_launch<'a>(
    settings: Option<&'a ContextServerSettings>,
    configuration: Option<&'a ContextServerConfiguration>,
) -> ChildProcessLaunch<'a> {
    if let Some(configuration) = configuration {
        return match configuration {
            ContextServerConfiguration::Http { .. } => ChildProcessLaunch {
                spawns_child_process: false,
                on_remote_host: false,
                env: None,
            },
            _ => ChildProcessLaunch {
                spawns_child_process: true,
                on_remote_host: configuration.remote(),
                env: configuration
                    .command()
                    .and_then(|command| command.env.as_ref()),
            },
        };
    }

    match settings {
        Some(ContextServerSettings::Http { .. }) => ChildProcessLaunch {
            spawns_child_process: false,
            on_remote_host: false,
            env: None,
        },
        Some(ContextServerSettings::Stdio {
            command, remote, ..
        }) => ChildProcessLaunch {
            spawns_child_process: true,
            on_remote_host: *remote,
            env: command.env.as_ref(),
        },
        // An extension server that has not started yet will spawn a child
        // process, but its command — and so its environment — is not known here.
        Some(ContextServerSettings::Extension { remote, .. }) => ChildProcessLaunch {
            spawns_child_process: true,
            on_remote_host: *remote,
            env: None,
        },
        None => ChildProcessLaunch {
            spawns_child_process: true,
            on_remote_host: false,
            env: None,
        },
    }
}

/// The execution host a server is configured with, for carrying through an edit
/// that does not itself touch it.
fn configured_remote(
    settings: Option<&ContextServerSettings>,
    configuration: Option<&ContextServerConfiguration>,
) -> bool {
    child_process_launch(settings, configuration).on_remote_host
}

/// Returns whether saving under `id` would overwrite a *different* existing
/// server. Editing a server in place (`id == original_id`) is allowed.
fn name_collides_with_other_server(
    id: &ContextServerId,
    original_id: Option<&ContextServerId>,
    existing_ids: &[ContextServerId],
) -> bool {
    original_id.is_none_or(|original| original.0 != id.0)
        && existing_ids.iter().any(|existing| existing.0 == id.0)
}

fn parse_timeout(text: &str) -> Result<Option<u64>, SharedString> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(None);
    }
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| "タイムアウトには正の整数の秒数を指定してください。".into())
}

fn collect_kv(
    rows: &[(String, String)],
    label: &str,
) -> Result<HashMap<String, String>, SharedString> {
    let mut map = HashMap::default();
    for (key, value) in rows {
        let key = key.trim().to_string();
        if key.is_empty() {
            continue;
        }
        if map.contains_key(&key) {
            return Err(format!("{label}「{key}」が重複しています。").into());
        }
        map.insert(key, value.clone());
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(transport: McpTransport) -> McpServerFormValues {
        McpServerFormValues {
            transport,
            original_id: None,
            name: "my-server".into(),
            command: String::new(),
            args: String::new(),
            url: String::new(),
            timeout: String::new(),
            oauth_client_id: String::new(),
            env: Vec::new(),
            headers: Vec::new(),
            remote: false,
        }
    }

    fn id(name: &str) -> ContextServerId {
        ContextServerId(name.into())
    }

    #[test]
    fn parse_timeout_handles_empty_and_invalid() {
        assert_eq!(parse_timeout(""), Ok(None));
        assert_eq!(parse_timeout("   "), Ok(None));
        assert_eq!(parse_timeout("60"), Ok(Some(60)));
        assert_eq!(parse_timeout("  90  "), Ok(Some(90)));
        assert!(parse_timeout("abc").is_err());
        assert!(parse_timeout("-5").is_err());
        assert!(parse_timeout("1.5").is_err());
    }

    #[test]
    fn requires_server_name() {
        let mut values = values(McpTransport::Stdio);
        values.name = "   ".into();
        values.command = "/bin/server".into();
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "サーバー名を入力してください。"
        );
    }

    #[test]
    fn requires_command_for_local_server() {
        let values = values(McpTransport::Stdio);
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "コマンドを入力してください。"
        );
    }

    #[test]
    fn requires_url_for_remote_server() {
        let values = values(McpTransport::Http);
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "URL を入力してください。"
        );
    }

    #[test]
    fn rejects_invalid_url() {
        let mut values = values(McpTransport::Http);
        values.url = "not a url".into();
        let error = build_settings_from_values(&values).unwrap_err();
        assert!(
            error.starts_with("URL が正しくありません"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn rejects_invalid_timeout() {
        let mut values = values(McpTransport::Stdio);
        values.command = "/bin/server".into();
        values.timeout = "soon".into();
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "タイムアウトには正の整数の秒数を指定してください。"
        );
    }

    #[test]
    fn rejects_duplicate_environment_variables() {
        let mut values = values(McpTransport::Stdio);
        values.command = "/bin/server".into();
        values.env = vec![("FOO".into(), "1".into()), ("FOO".into(), "2".into())];
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "環境変数「FOO」が重複しています。"
        );
    }

    #[test]
    fn rejects_duplicate_headers() {
        let mut values = values(McpTransport::Http);
        values.url = "https://example.com/mcp".into();
        values.headers = vec![
            ("Authorization".into(), "a".into()),
            ("Authorization".into(), "b".into()),
        ];
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "ヘッダー「Authorization」が重複しています。"
        );
    }

    #[test]
    fn builds_local_server() {
        let mut values = values(McpTransport::Stdio);
        values.name = "  local  ".into();
        values.command = "/usr/bin/server".into();
        values.args = "--flag  value".into();
        values.timeout = "30".into();
        // Empty values are kept, but rows with a blank key are ignored.
        values.env = vec![
            ("KEY".into(), "VALUE".into()),
            ("EMPTY".into(), String::new()),
            ("   ".into(), "ignored".into()),
        ];

        let (id, original_id, content) = build_settings_from_values(&values).unwrap();
        assert_eq!(id.0.as_ref(), "local");
        assert_eq!(original_id, None);

        let expected_env = HashMap::from_iter([
            ("KEY".to_string(), "VALUE".to_string()),
            ("EMPTY".to_string(), String::new()),
        ]);
        assert_eq!(
            content,
            ContextServerSettingsContent::Stdio {
                enabled: true,
                remote: false,
                command: ContextServerCommand {
                    path: "/usr/bin/server".into(),
                    args: vec!["--flag".into(), "value".into()],
                    env: Some(expected_env),
                    timeout: Some(30),
                },
            }
        );
    }

    #[test]
    fn builds_a_local_server_that_runs_on_the_ssh_host() {
        let mut values = values(McpTransport::Stdio);
        values.command = "/usr/bin/server".into();
        values.remote = true;

        let (_, _, content) = build_settings_from_values(&values).unwrap();
        assert!(matches!(
            content,
            ContextServerSettingsContent::Stdio { remote: true, .. }
        ));
    }

    /// Regression: the form used to have no execution host of its own, so
    /// saving an unrelated edit rewrote a hand-written `remote: true` to false.
    #[test]
    fn saving_the_form_keeps_the_configured_execution_host() {
        for remote in [false, true] {
            let existing = stdio_settings_on_host(None, remote);
            // What `McpServerForm::new` pre-fills the form with.
            let mut values = values(McpTransport::Stdio);
            values.command = "/bin/server".into();
            values.remote = configured_remote(Some(&existing), None);

            let (_, _, content) = build_settings_from_values(&values).unwrap();
            let ContextServerSettingsContent::Stdio {
                remote: saved_remote,
                ..
            } = content
            else {
                panic!("expected a stdio server");
            };
            assert_eq!(saved_remote, remote);
        }
    }

    #[test]
    fn reads_the_execution_host_of_an_extension_server() {
        let extension = |remote| ContextServerSettings::Extension {
            enabled: true,
            remote,
            settings: serde_json::json!({}),
        };
        assert!(configured_remote(Some(&extension(true)), None));
        assert!(!configured_remote(Some(&extension(false)), None));
        // The runtime configuration wins where there is one.
        assert!(!configured_remote(
            Some(&extension(true)),
            Some(&extension_configuration(None))
        ));
        // An HTTP server has no execution host to preserve.
        assert!(!configured_remote(Some(&http_settings()), None));
        assert!(!configured_remote(None, None));
    }

    #[test]
    fn builds_remote_server() {
        let mut values = values(McpTransport::Http);
        values.name = "remote".into();
        values.url = "https://example.com/mcp".into();
        values.oauth_client_id = "client-123".into();
        values.headers = vec![("Authorization".into(), "Bearer token".into())];

        let (id, _, content) = build_settings_from_values(&values).unwrap();
        assert_eq!(id.0.as_ref(), "remote");

        let expected_headers =
            HashMap::from_iter([("Authorization".to_string(), "Bearer token".to_string())]);
        assert_eq!(
            content,
            ContextServerSettingsContent::Http {
                enabled: true,
                url: "https://example.com/mcp".into(),
                headers: expected_headers,
                timeout: None,
                oauth: Some(OAuthClientSettings {
                    client_id: "client-123".into(),
                    client_secret: None,
                }),
            }
        );
    }

    #[test]
    fn flags_invalid_url_in_settings() {
        let http = |url: &str| ContextServerSettings::Http {
            enabled: true,
            url: url.into(),
            headers: HashMap::default(),
            timeout: None,
            oauth: None,
        };
        assert_eq!(
            settings_validation_error(Some(&http("not a url")))
                .unwrap()
                .as_ref(),
            "設定内の URL が正しくありません。"
        );
        assert!(settings_validation_error(Some(&http("https://example.com/mcp"))).is_none());
        assert!(settings_validation_error(None).is_none());
    }

    fn stdio_settings(env: Option<HashMap<String, String>>) -> ContextServerSettings {
        stdio_settings_on_host(env, false)
    }

    fn stdio_settings_on_host(
        env: Option<HashMap<String, String>>,
        remote: bool,
    ) -> ContextServerSettings {
        ContextServerSettings::Stdio {
            enabled: true,
            remote,
            command: ContextServerCommand {
                path: "/bin/server".into(),
                args: Vec::new(),
                env,
                timeout: None,
            },
        }
    }

    fn http_settings() -> ContextServerSettings {
        ContextServerSettings::Http {
            enabled: true,
            url: "https://example.com/mcp".into(),
            headers: HashMap::default(),
            timeout: None,
            oauth: None,
        }
    }

    fn extension_configuration(env: Option<HashMap<String, String>>) -> ContextServerConfiguration {
        ContextServerConfiguration::Extension {
            command: ContextServerCommand {
                path: "/bin/extension-server".into(),
                args: Vec::new(),
                env,
                timeout: None,
            },
            settings: serde_json::json!({}),
            remote: false,
        }
    }

    #[test]
    fn proxy_applies_to_local_child_processes_only() {
        // A local stdio server with the proxy running is the case the injection
        // actually covers.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(None)), None, false, true),
            McpProxyStatus::Proxied
        );
        // Turning child-process proxying off leaves it outside the allowlist.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(None)), None, false, false),
            McpProxyStatus::ProxyDisabled
        );
        // A server that runs on the SSH host spawns over there, where this
        // proxy is not reachable.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings_on_host(None, true)), None, true, true),
            McpProxyStatus::NotApplicable
        );
        // But a remote project is not itself the criterion: a server left on
        // this machine is spawned — and proxied — here.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(None)), None, true, true),
            McpProxyStatus::Proxied
        );
        // Outside a remote project `remote` has no effect, so the server runs
        // here regardless.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings_on_host(None, true)), None, false, true),
            McpProxyStatus::Proxied
        );
        // HTTP servers make their requests through the app's own client.
        assert_eq!(
            proxy_status_for_server(Some(&http_settings()), None, false, true),
            McpProxyStatus::NotApplicable
        );
    }

    #[test]
    fn proxy_reports_an_environment_that_names_its_own_proxy() {
        let env = HashMap::from_iter([("HTTPS_PROXY".to_string(), "http://corp:3128".to_string())]);
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(Some(env.clone()))), None, false, true),
            McpProxyStatus::EnvOverride
        );
        // With no proxy running there is nothing to be overridden in the first
        // place; the traffic is uncontrolled either way.
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(Some(env))), None, false, false),
            McpProxyStatus::ProxyDisabled
        );

        let unrelated = HashMap::from_iter([("TOKEN".to_string(), "secret".to_string())]);
        assert_eq!(
            proxy_status_for_server(Some(&stdio_settings(Some(unrelated))), None, false, true),
            McpProxyStatus::Proxied
        );
    }

    #[test]
    fn proxy_reads_the_environment_of_an_extension_server_from_its_runtime_configuration() {
        // An extension server has no command in settings, so before it starts
        // the environment is unknown and the proxy is assumed to apply.
        let settings = ContextServerSettings::Extension {
            enabled: true,
            remote: false,
            settings: serde_json::json!({}),
        };
        assert_eq!(
            proxy_status_for_server(Some(&settings), None, false, true),
            McpProxyStatus::Proxied
        );

        let env = HashMap::from_iter([("http_proxy".to_string(), "http://corp:3128".to_string())]);
        assert_eq!(
            proxy_status_for_server(
                Some(&settings),
                Some(&extension_configuration(Some(env))),
                false,
                true
            ),
            McpProxyStatus::EnvOverride
        );
        assert_eq!(
            proxy_status_for_server(
                Some(&settings),
                Some(&extension_configuration(None)),
                false,
                true
            ),
            McpProxyStatus::Proxied
        );
    }

    #[test]
    fn proxy_detail_label_is_only_shown_when_it_says_something() {
        assert!(McpProxyStatus::NotApplicable.detail().is_none());
        for status in [
            McpProxyStatus::Proxied,
            McpProxyStatus::ProxyDisabled,
            McpProxyStatus::EnvOverride,
        ] {
            let (label, tooltip) = status
                .detail()
                .unwrap_or_else(|| panic!("{status:?} should have a detail label"));
            assert!(!label.is_empty());
            assert!(!tooltip.is_empty());
        }
    }

    #[test]
    fn name_collision_covers_new_and_rename() {
        let existing = vec![id("foo"), id("bar")];

        // New server taking an existing name collides.
        assert!(name_collides_with_other_server(&id("foo"), None, &existing));
        // New server with a free name is fine.
        assert!(!name_collides_with_other_server(
            &id("baz"),
            None,
            &existing
        ));
        // Editing a server in place is allowed even though the name "exists".
        assert!(!name_collides_with_other_server(
            &id("foo"),
            Some(&id("foo")),
            &existing
        ));
        // Renaming onto a different server's name collides.
        assert!(name_collides_with_other_server(
            &id("bar"),
            Some(&id("foo")),
            &existing
        ));
        // Renaming to a free name is fine.
        assert!(!name_collides_with_other_server(
            &id("baz"),
            Some(&id("foo")),
            &existing
        ));
    }
}
