mod launch_approval_prompts;
mod mcp_json_import;
mod mcp_server_templates;

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;

use collections::HashMap;
use context_server::ContextServerId;
use editor::Editor;
use extension_host::ExtensionStore;
use gpui::{
    Action as _, Entity, EntityId, Focusable as _, ScrollHandle, Subscription, Task, WeakEntity,
    prelude::*,
};
use project::ProjectItem as _;
use project::WorktreeId;
use project::context_server_store::{
    ContextServerConfiguration, ContextServerSource, ContextServerStatus, ContextServerStore,
    ServerStatusChangedEvent, wait_for_context_server,
};
use project::project_launch_approvals::{
    CHANGED_SINCE_SHOWN_MESSAGE, LaunchApprovalDetails, LaunchApprovalKey, LaunchApprovalState,
    LaunchDefinitionFile, LaunchHost, LaunchTarget, ProjectLaunchApprovals,
    ProjectLaunchApprovalsEvent, escape_for_display,
};
use project::project_settings::ContextServerSettings;
use settings::{ContextServerCommand, ContextServerSettingsContent, OAuthClientSettings};
use ui::{
    AiSettingItem, AiSettingItemSource, AiSettingItemStatus, ContextMenu, Divider, PopoverMenu,
    Switch, ToggleState, Tooltip, prelude::*,
};
use util::ResultExt as _;
use workspace::Workspace;

use zed_actions::ExtensionCategoryFilter;

use crate::{PROJECT, SettingField, SettingItem, SettingsPageItem, SettingsWindow, USER};
use mcp_json_import::{ImportedDefinition, ImportedServer, parse_mcp_json_snippet};
use mcp_server_templates::{McpTemplatesState, open_mcp_templates_page};

pub(crate) use launch_approval_prompts::{
    LaunchApprovalsPageState, init as init_launch_approval_prompts, open_launch_approvals_page,
    render_launch_approvals_page,
};

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
    /// Asked of each running server directly: the agent's tool registry lives
    /// per thread, not anywhere this window can reach.
    tool_lists: HashMap<ContextServerId, McpToolList>,
    /// Kept here rather than on the window because the paste page is only ever
    /// pushed on top of this one, and is gone whenever this state is reset.
    paste: Option<McpPasteState>,
    /// Like `paste`, for the page listing the shipped templates.
    templates: Option<McpTemplatesState>,
    /// The row a pending-launch notification sent the user to look at.
    focused_server: Option<ContextServerId>,
    /// Why storing a decision about a project-defined server failed.
    approval_errors: HashMap<ContextServerId, SharedString>,
    /// Decisions are stored by the approval service, whose events are not
    /// always mirrored by a status change in the server store.
    approvals_subscription: Option<Subscription>,
    /// Whether the page is on screen, so that re-pointing the window at
    /// another workspace re-subscribes even after a spell with no project.
    page_open: bool,
    /// The workspace a pending-launch notification came from: the page shows
    /// that project's servers even while another workspace is active.
    target_workspace: Option<WeakEntity<Workspace>>,
}

struct McpToolList {
    state: McpToolListState,
    expanded: bool,
    /// Never replaced from inside the task it holds (see [`McpConnectionTest`]).
    _task: Option<Task<()>>,
}

enum McpToolListState {
    Loading,
    Loaded(Vec<McpToolSummary>),
    /// The server does not offer tools at all.
    Unsupported,
    Failed(SharedString),
}

struct McpToolSummary {
    name: SharedString,
    /// First line only: tool descriptions are often whole paragraphs.
    description: Option<SharedString>,
}

struct McpPasteState {
    editor: Entity<Editor>,
    outcome: Option<McpPasteOutcome>,
}

enum McpPasteOutcome {
    Failed(SharedString),
    /// More than one server: each can be opened in the form, or all added at
    /// once after a confirmation.
    Several {
        servers: Vec<ImportedServer>,
        confirming_add_all: bool,
        error: Option<SharedString>,
    },
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
    settings_window.mcp_page.page_open = true;
    settings_window.mcp_page.connection_tests.clear();
    settings_window.mcp_page.client_secret_input = None;
    subscribe_to_context_server_store(settings_window, cx);
    if settings_window.mcp_page.approvals_subscription.is_none() {
        let approvals = ProjectLaunchApprovals::global(cx);
        settings_window.mcp_page.approvals_subscription = Some(
            cx.subscribe(&approvals, |_, _, _: &ProjectLaunchApprovalsEvent, cx| {
                cx.notify()
            }),
        );
    }
    refresh_all_tool_lists(settings_window, cx);
}

/// The settings window is re-pointed at whichever workspace opened it, so the
/// project — and with it the server store — can change under an already-open
/// page. Does nothing until the page has been opened at least once.
pub(crate) fn retarget_mcp_server_subscription(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    if settings_window.mcp_page.page_open {
        // Whoever reopened the window decides which project the page shows.
        settings_window.mcp_page.target_workspace = None;
        subscribe_to_context_server_store(settings_window, cx);
        refresh_all_tool_lists(settings_window, cx);
    }
}

fn refresh_all_tool_lists(settings_window: &mut SettingsWindow, cx: &mut Context<SettingsWindow>) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        settings_window.mcp_page.tool_lists.clear();
        return;
    };
    let running_ids = store
        .read(cx)
        .running_servers()
        .iter()
        .map(|server| server.id())
        .collect::<Vec<_>>();
    settings_window
        .mcp_page
        .tool_lists
        .retain(|id, _| running_ids.contains(id));
    for id in running_ids {
        refresh_tool_list(settings_window, &store, id, cx);
    }
}

fn refresh_tool_list(
    settings_window: &mut SettingsWindow,
    store: &Entity<ContextServerStore>,
    context_server_id: ContextServerId,
    cx: &mut Context<SettingsWindow>,
) {
    let client = store
        .read(cx)
        .get_running_server(&context_server_id)
        .and_then(|server| server.client());
    let Some(client) = client else {
        settings_window
            .mcp_page
            .tool_lists
            .remove(&context_server_id);
        return;
    };
    let expanded = settings_window
        .mcp_page
        .tool_lists
        .get(&context_server_id)
        .is_some_and(|list| list.expanded);

    if !client.capable(context_server::protocol::ServerCapability::Tools) {
        settings_window.mcp_page.tool_lists.insert(
            context_server_id,
            McpToolList {
                state: McpToolListState::Unsupported,
                expanded,
                _task: None,
            },
        );
        return;
    }

    let task = cx.spawn({
        let context_server_id = context_server_id.clone();
        async move |this, cx| {
            let state = match client
                .request::<context_server::types::requests::ListTools>(())
                .await
            {
                Ok(response) => {
                    let mut tools = response
                        .tools
                        .into_iter()
                        .map(|tool| McpToolSummary {
                            name: tool.name.into(),
                            description: tool.description.as_deref().and_then(|description| {
                                description
                                    .lines()
                                    .map(str::trim)
                                    .find(|line| !line.is_empty())
                                    .map(|line| SharedString::from(line.to_string()))
                            }),
                        })
                        .collect::<Vec<_>>();
                    tools.sort_by(|a, b| a.name.cmp(&b.name));
                    McpToolListState::Loaded(tools)
                }
                Err(error) => {
                    McpToolListState::Failed(server_error_text(&error.to_string()).into())
                }
            };
            this.update(cx, |this, cx| {
                if let Some(list) = this.mcp_page.tool_lists.get_mut(&context_server_id) {
                    list.state = state;
                }
                cx.notify();
            })
            .log_err();
        }
    });

    settings_window.mcp_page.tool_lists.insert(
        context_server_id,
        McpToolList {
            state: McpToolListState::Loading,
            expanded,
            _task: Some(task),
        },
    );
}

fn subscribe_to_context_server_store(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        // Only what belonged to the previous project goes: the paste page and
        // the approval subscription never depended on one.
        settings_window.mcp_page.store_subscription = None;
        settings_window.mcp_page.connection_tests.clear();
        settings_window.mcp_page.client_secret_input = None;
        settings_window.mcp_page.tool_lists.clear();
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
    settings_window.mcp_page.tool_lists.clear();

    let subscription = cx.subscribe(
        &store,
        |this, store, event: &ServerStatusChangedEvent, cx| {
            if matches!(event.status, ContextServerStatus::Running) {
                refresh_tool_list(this, &store, event.server_id.clone(), cx);
            } else {
                this.mcp_page.tool_lists.remove(&event.server_id);
            }
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

/// The workspace whose servers the page shows: the one a pending-launch
/// notification came from, or else the one the settings window was opened
/// from.
fn target_workspace(settings_window: &SettingsWindow, cx: &App) -> Option<Entity<Workspace>> {
    if let Some(workspace) = settings_window
        .mcp_page
        .target_workspace
        .as_ref()
        .and_then(WeakEntity::upgrade)
    {
        return Some(workspace);
    }
    let original_window = settings_window.original_window.as_ref()?;
    Some(original_window.read(cx).ok()?.workspace().clone())
}

fn get_context_server_store(
    settings_window: &SettingsWindow,
    cx: &App,
) -> Option<Entity<ContextServerStore>> {
    let workspace = target_workspace(settings_window, cx)?;
    Some(workspace.read(cx).project().read(cx).context_server_store())
}

/// Names already taken. A project's store sees the merged settings, but with
/// no project open the user's own file is still there to be overwritten.
fn existing_server_ids(settings_window: &SettingsWindow, cx: &App) -> Vec<ContextServerId> {
    let mut ids = cx
        .global::<settings::SettingsStore>()
        .raw_user_settings()
        .map(|settings| {
            settings
                .content
                .project
                .context_servers
                .keys()
                .map(|name| ContextServerId(name.clone()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if let Some(store) = get_context_server_store(settings_window, cx) {
        ids.extend(store.read(cx).server_ids().iter().cloned());
    }
    ids
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
    let focused_server = settings_window.mcp_page.focused_server.clone();
    let border_focused = cx.theme().colors().border_focused;
    v_flex()
        .w_full()
        .gap_1()
        .children(itertools::intersperse_with(
            server_ids.iter().map(|server_id| {
                let row =
                    render_context_server(settings_window, server_id, store, cx).into_any_element();
                if focused_server.as_ref() == Some(server_id) {
                    div()
                        .px_2()
                        .rounded_md()
                        .border_1()
                        .border_color(border_focused)
                        .child(row)
                        .into_any_element()
                } else {
                    row
                }
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

    let server_source = store.read(cx).server_source(context_server_id, cx);
    let launch = store.read(cx).launch_approval_details(context_server_id);
    let approval = launch.as_ref().map(|details| details.state);
    let project_definition = match server_source {
        Some(ContextServerSource::Project {
            worktree_id,
            settings_file,
            host,
        }) => Some((settings_file, host, worktree_id)),
        _ => None,
    };
    // A `remote: true` extension server whose command the SSH host resolved
    // needs the same approval as a project's definition without being one.
    let needs_approval = project_definition.is_some() || launch.is_some();
    let decision_blocked = launch
        .as_ref()
        .is_some_and(|details| details.store_error.is_some());

    let source = if project_definition.is_some() {
        AiSettingItemSource::Project
    } else if provided_by_extension {
        AiSettingItemSource::Extension
    } else {
        AiSettingItemSource::Custom
    };

    let status = row_status(&server_status, approval);

    let should_show_logout = server_configuration.as_ref().is_some_and(|config| {
        matches!(config.as_ref(), ContextServerConfiguration::Http { .. })
            && !config.has_static_auth_header()
    });

    let server_settings = store
        .read(cx)
        .settings_for_server(context_server_id)
        .cloned();

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

    let waits_for_approval = needs_approval && approval != Some(LaunchApprovalState::Approved);
    let test_button =
        render_connection_test_button(context_server_id, is_testing, waits_for_approval, cx);
    // A project's definition is changed where it lives: the edit form would
    // save the command into the user's settings, where it runs unapproved.
    let edit_button = project_definition
        .as_ref()
        .and_then(|(settings_file, host, worktree_id)| {
            let LaunchDefinitionFile::Worktree(_) = settings_file else {
                return None;
            };
            project_file_entry(*worktree_id, context_server_id, cx)
                .and_then(|entry| form_transport(&entry))?;
            Some(render_edit_project_server_button(
                context_server_id,
                *worktree_id,
                launch_approval_prompts::definition_file_label(settings_file, host).into(),
                cx,
            ))
        });
    let (configure_button, remove_button, toggle_switch) = match &project_definition {
        Some((settings_file, _, _)) => (
            // A command the SSH host resolved has no file behind it.
            (!matches!(settings_file, LaunchDefinitionFile::ResolvedOnRemoteHost)).then(|| {
                render_open_definition_button(context_server_id, settings_file.clone(), cx)
            }),
            launch
                .as_ref()
                .filter(|details| details.state == LaunchApprovalState::Approved)
                .map(|details| render_revoke_button(context_server_id, details.key.clone(), cx)),
            render_project_toggle(
                context_server_id,
                approval,
                launch.as_ref().map(|details| details.key.clone()),
                server_settings
                    .as_ref()
                    .is_some_and(ContextServerSettings::enabled),
                decision_blocked,
                is_transitioning,
                cx,
            ),
        ),
        None => (
            (!provided_by_extension).then(|| {
                render_configure_button(
                    context_server_id,
                    cx.entity().downgrade(),
                    server_settings.clone(),
                )
                .into_any_element()
            }),
            Some(
                render_uninstall_button(context_server_id, provided_by_extension)
                    .into_any_element(),
            ),
            render_toggle_switch(
                context_server_id,
                store,
                is_enabled,
                is_transitioning || waits_for_approval,
                configured_remote(server_settings.as_ref(), server_configuration.as_deref()),
            )
            .into_any_element(),
        ),
    };

    let approval_block = needs_approval.then(|| {
        render_approval_block(
            settings_window,
            context_server_id,
            launch.as_ref(),
            project_definition
                .as_ref()
                .map(|(settings_file, host, _)| (settings_file, host)),
            server_settings.as_ref(),
            cx,
        )
    });

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
    let tool_list = matches!(server_status, ContextServerStatus::Running)
        .then(|| settings_window.mcp_page.tool_lists.get(context_server_id))
        .flatten()
        .and_then(|list| render_tool_list(context_server_id, list, cx));
    let details = (approval_block.is_some()
        || test_feedback.is_some()
        || status_details.is_some()
        || tool_list.is_some())
    .then(|| {
        v_flex()
            .w_full()
            .min_w_0()
            .children(approval_block)
            .children(test_feedback)
            .children(status_details)
            .children(tool_list)
            .into_any_element()
    });

    AiSettingItem::new(item_id, display_name, status, source)
        .when(project_definition.is_some(), |this| {
            this.badge("プロジェクト")
        })
        .action(test_button)
        .when_some(edit_button, |this, button| this.action(button))
        .when_some(configure_button, |this, button| this.action(button))
        .when_some(remove_button, |this, button| this.action(button))
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
        ContextServerStatus::ApprovalRequired => AiSettingItemStatus::ApprovalRequired,
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
    waits_for_approval: bool,
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
    .disabled(is_testing || waits_for_approval)
    .tooltip(Tooltip::text(if waits_for_approval {
        "許可するまで接続テストはできません。"
    } else {
        "サーバーを再接続して、接続できるかどうかを確認します。"
    }))
    .on_click(cx.listener(move |this, _event, _window, cx| {
        start_connection_test(this, context_server_id.clone(), cx);
    }))
    .into_any_element()
}

/// `ApprovalRequired` covers both a project definition nobody has decided on
/// yet and one the user said no to; only the first is waiting for anything.
fn row_status(
    status: &ContextServerStatus,
    approval: Option<LaunchApprovalState>,
) -> AiSettingItemStatus {
    match (status, approval) {
        (ContextServerStatus::ApprovalRequired, Some(LaunchApprovalState::Denied)) => {
            AiSettingItemStatus::Stopped
        }
        _ => map_server_status(status),
    }
}

fn render_open_definition_button(
    context_server_id: &ContextServerId,
    settings_file: LaunchDefinitionFile,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    IconButton::new(
        format!("mcp-open-definition-{}", context_server_id.0),
        IconName::Settings,
    )
    .icon_size(IconSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text(
        "このサーバーを定義しているプロジェクトの設定ファイルを開く",
    ))
    .on_click(cx.listener(move |this, _event, _window, cx| {
        open_project_definition(this, &settings_file, cx);
    }))
    .into_any_element()
}

/// The entry for `id` as the worktree's own `.zed/settings.json` has it, which
/// is what an edit of a project's server starts from and writes back to.
fn project_file_entry(
    worktree_id: WorktreeId,
    id: &ContextServerId,
    cx: &App,
) -> Option<ContextServerSettingsContent> {
    cx.global::<settings::SettingsStore>()
        .get_content_for_file(settings::SettingsFile::Project((
            worktree_id,
            util::rel_path::RelPath::empty_arc(),
        )))?
        .project
        .context_servers
        .get(&id.0)
        .cloned()
}

/// The form an entry opens in. An extension's settings have no place in it.
fn form_transport(entry: &ContextServerSettingsContent) -> Option<McpTransport> {
    match entry {
        ContextServerSettingsContent::Stdio { .. } => Some(McpTransport::Stdio),
        ContextServerSettingsContent::Http { .. } => Some(McpTransport::Http),
        ContextServerSettingsContent::Extension { .. } => None,
    }
}

fn render_edit_project_server_button(
    context_server_id: &ContextServerId,
    worktree_id: WorktreeId,
    file_label: SharedString,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let context_server_id = context_server_id.clone();
    IconButton::new(
        format!("mcp-edit-project-{}", context_server_id.0),
        IconName::Pencil,
    )
    .icon_size(IconSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text(
        "フォームで編集 (このサーバーを定義しているプロジェクトの設定ファイルに保存します)",
    ))
    .on_click(cx.listener(move |this, _event, window, cx| {
        open_project_server_form(
            this,
            context_server_id.clone(),
            worktree_id,
            file_label.clone(),
            window,
            cx,
        );
    }))
    .into_any_element()
}

/// Opens the form on a server a project defines, set to save into the file
/// that defines it.
fn open_project_server_form(
    settings_window: &mut SettingsWindow,
    context_server_id: ContextServerId,
    worktree_id: WorktreeId,
    file_label: SharedString,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    match project_server_form(&context_server_id, worktree_id, file_label, window, cx) {
        Some(form) => push_mcp_server_form(settings_window, form, window, cx),
        None => {
            settings_window
                .mcp_page
                .approval_errors
                .insert(context_server_id, PROJECT_DEFINITION_CHANGED.into());
            cx.notify();
        }
    }
}

fn project_server_form(
    context_server_id: &ContextServerId,
    worktree_id: WorktreeId,
    file_label: SharedString,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Option<McpServerForm> {
    let loaded = project_file_entry(worktree_id, context_server_id, cx)?;
    let transport = form_transport(&loaded)?;
    let values = McpServerFormValues::from_existing(
        transport,
        Some((
            context_server_id.clone(),
            ContextServerSettings::from(loaded.clone()),
        )),
    );
    let mut form = McpServerForm::from_values(values, None, window, cx);
    form.destination = McpFormDestination::ProjectFile {
        worktree_id,
        file_label,
        loaded,
    };
    Some(form)
}

fn open_project_definition(
    settings_window: &mut SettingsWindow,
    settings_file: &LaunchDefinitionFile,
    cx: &mut Context<SettingsWindow>,
) {
    // The workspace whose project defines the server, which is not always
    // the window's active one.
    let (Some(original_window), Some(workspace)) = (
        settings_window.original_window,
        target_workspace(settings_window, cx),
    ) else {
        return;
    };
    let settings_file = settings_file.clone();
    original_window
        .update(cx, move |_, window, cx| {
            window.activate_window();
            match settings_file {
                // By its path on the host that owns the worktree, which also
                // finds a `.zed/settings.json` in a subfolder.
                LaunchDefinitionFile::Worktree(path) => {
                    let path = std::path::PathBuf::from(path.as_ref());
                    workspace.update(cx, |workspace, cx| {
                        workspace
                            .open_abs_path(
                                path,
                                workspace::OpenOptions {
                                    visible: Some(workspace::OpenVisible::None),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            )
                            .detach_and_log_err(cx);
                    });
                }
                LaunchDefinitionFile::RemoteServerSettings => {
                    let open_settings = workspace.update(cx, |workspace, cx| {
                        workspace
                            .project()
                            .update(cx, |project, cx| project.open_server_settings(cx))
                    });
                    let workspace = workspace.downgrade();
                    window
                        .spawn(cx, async move |cx| {
                            let buffer = open_settings.await?;
                            workspace
                                .update_in(cx, |workspace, window, cx| {
                                    let Some(project_path) = buffer.read(cx).project_path(cx)
                                    else {
                                        return Task::ready(Err(anyhow::anyhow!(
                                            "the server settings file has no location"
                                        )));
                                    };
                                    workspace.open_path(project_path, None, true, window, cx)
                                })?
                                .await?;
                            anyhow::Ok(())
                        })
                        .detach_and_log_err(cx);
                }
                // Resolved by the remote host, with no file to open; the
                // button is not offered for it.
                LaunchDefinitionFile::ResolvedOnRemoteHost => {}
            }
        })
        .log_err();
}

fn render_revoke_button(
    context_server_id: &ContextServerId,
    shown: LaunchApprovalKey,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let context_server_id = context_server_id.clone();
    Button::new(
        SharedString::from(format!("mcp-revoke-{}", context_server_id.0)),
        "承認を取り消す",
    )
    .style(ButtonStyle::Outlined)
    .label_size(LabelSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text(
        "承認を取り消してサーバーを止めます。もう一度起動するには、あらためて許可が必要です。",
    ))
    .on_click(cx.listener(move |this, _event, _window, cx| {
        decide_project_server(
            this,
            context_server_id.clone(),
            shown.clone(),
            ProjectServerDecision::Revoke,
            cx,
        );
    }))
    .into_any_element()
}

/// On a project's row the switch is the approval itself: turning it on allows
/// exactly this definition, turning it off says no to it. It never writes the
/// user's settings, where an entry could not override the project's anyway.
fn render_project_toggle(
    context_server_id: &ContextServerId,
    approval: Option<LaunchApprovalState>,
    shown: Option<LaunchApprovalKey>,
    entry_enabled: bool,
    decision_blocked: bool,
    is_transitioning: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let context_server_id = context_server_id.clone();
    let approved = approval == Some(LaunchApprovalState::Approved);
    let hint_id = SharedString::from(format!("mcp-toggle-hint-{}", context_server_id.0));
    let hint = match approval {
        // Enabled, yet nothing to approve: the lookup failed, which is not
        // the same as the project turning the server off.
        None if entry_enabled => APPROVAL_DETAILS_MISSING,
        None => "プロジェクトの設定で無効になっています。",
        Some(_) if decision_blocked => launch_approval_prompts::DECISION_BLOCKED_HINT,
        Some(LaunchApprovalState::Approved) => {
            "オフにすると、このプロジェクトの定義を「許可しない」にしてサーバーを止めます。"
        }
        Some(LaunchApprovalState::Pending | LaunchApprovalState::Denied) => {
            "オンにすると、このプロジェクトの定義を許可してサーバーを起動します。"
        }
    };
    let switch = Switch::new(
        SharedString::from(format!("mcp-toggle-{}", context_server_id.0)),
        if approved {
            ToggleState::Selected
        } else {
            ToggleState::Unselected
        },
    )
    .disabled(approval.is_none() || decision_blocked || is_transitioning)
    .tab_index(0isize)
    .on_click(cx.listener(move |this, state: &ToggleState, _window, cx| {
        let Some(shown) = shown.clone() else {
            return;
        };
        let decision = if *state == ToggleState::Selected {
            ProjectServerDecision::Approve
        } else {
            ProjectServerDecision::Deny
        };
        decide_project_server(this, context_server_id.clone(), shown, decision, cx);
    }));

    // The switch has no tooltip of its own, and a disabled one would not show
    // it anyway: the reason it cannot be turned on belongs on the wrapper.
    div()
        .id(hint_id)
        .tooltip(Tooltip::text(hint))
        .child(switch)
        .into_any_element()
}

#[derive(Clone, Copy)]
enum ProjectServerDecision {
    Approve,
    Deny,
    Revoke,
}

/// `shown` is the key of the definition the clicked control was rendered
/// with: the store refuses the decision if the definition has changed since.
fn decide_project_server(
    settings_window: &mut SettingsWindow,
    context_server_id: ContextServerId,
    shown: LaunchApprovalKey,
    decision: ProjectServerDecision,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(store) = get_context_server_store(settings_window, cx) else {
        return;
    };
    settings_window
        .mcp_page
        .approval_errors
        .remove(&context_server_id);
    let task = store.update(cx, |store, cx| match decision {
        ProjectServerDecision::Approve => store.approve_server(&context_server_id, &shown, cx),
        ProjectServerDecision::Deny => store.deny_server(&context_server_id, &shown, cx),
        ProjectServerDecision::Revoke => {
            store.revoke_server_approval(&context_server_id, &shown, cx)
        }
    });
    cx.spawn(async move |this, cx| {
        let Err(error) = task.await else {
            return;
        };
        let error = error.to_string();
        // The row already shows the new definition; the message only has to
        // say why nothing happened.
        let message = if error == CHANGED_SINCE_SHOWN_MESSAGE {
            error
        } else {
            match decision {
                ProjectServerDecision::Approve => format!("許可を保存できませんでした: {error}"),
                ProjectServerDecision::Deny => {
                    format!("「許可しない」を保存できませんでした: {error}")
                }
                ProjectServerDecision::Revoke => format!("承認を取り消せませんでした: {error}"),
            }
        };
        this.update(cx, |this, cx| {
            this.mcp_page
                .approval_errors
                .insert(context_server_id, message.into());
            cx.notify();
        })
        .log_err();
    })
    .detach();
    cx.notify();
}

const APPROVAL_DETAILS_MISSING: &str = "このサーバーの承認の情報を取得できませんでした。";

fn render_approval_block(
    settings_window: &SettingsWindow,
    context_server_id: &ContextServerId,
    launch: Option<&LaunchApprovalDetails>,
    project_definition: Option<(&LaunchDefinitionFile, &LaunchHost)>,
    settings: Option<&ContextServerSettings>,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let approval_error = settings_window
        .mcp_page
        .approval_errors
        .get(context_server_id)
        .cloned();
    let border_color = cx.theme().colors().border_variant;
    let from_project = project_definition.is_some();

    let body = match (launch, project_definition) {
        (Some(details), _) => {
            let state_notice = match details.state {
                LaunchApprovalState::Pending => Some(launch_approval_prompts::render_notice(
                    IconName::Warning,
                    Color::Warning,
                    if from_project {
                        "承認待ち: このプロジェクトの設定がこのサーバーの起動を求めています。内容を確認してから許可してください。"
                    } else {
                        "承認待ち: 接続先で決まったこのコマンドを起動するには許可が必要です。内容を確認してから許可してください。"
                    }
                    .to_string(),
                )),
                LaunchApprovalState::Denied => Some(launch_approval_prompts::render_notice(
                    IconName::Info,
                    Color::Muted,
                    if from_project {
                        "このプロジェクトの定義は起動を許可していません。"
                    } else {
                        "この内容での起動は許可していません。"
                    }
                    .to_string(),
                )),
                LaunchApprovalState::Approved => None,
            };
            let buttons = match details.state {
                LaunchApprovalState::Pending | LaunchApprovalState::Denied => Some(
                    render_approval_buttons(
                        context_server_id,
                        &details.key,
                        details.state,
                        details.store_error.is_some(),
                        cx,
                    )
                    .into_any_element(),
                ),
                // A project's row carries 「承認を取り消す」 among its actions;
                // an extension server keeps its own trash button there.
                LaunchApprovalState::Approved => (!from_project).then(|| {
                    h_flex()
                        .w_full()
                        .justify_end()
                        .child(render_revoke_button(
                            context_server_id,
                            details.key.clone(),
                            cx,
                        ))
                        .into_any_element()
                }),
            };
            v_flex()
                .gap_1()
                .children(state_notice)
                .child(launch_approval_prompts::render_launch_details(details, cx))
                .children(buttons)
        }
        // Nothing to approve: either the project turned the server off, or the
        // lookup failed. What it would run is worth seeing either way.
        (None, Some((settings_file, host))) => {
            let enabled = settings.is_some_and(ContextServerSettings::enabled);
            v_flex()
                .gap_1()
                .child(launch_approval_prompts::render_notice(
                    IconName::Info,
                    Color::Muted,
                    if enabled {
                        APPROVAL_DETAILS_MISSING
                    } else {
                        "プロジェクトの設定で無効になっています。"
                    }
                    .to_string(),
                ))
                .child(launch_approval_prompts::render_detail_row(
                    "定義元",
                    launch_approval_prompts::definition_file_label(settings_file, host),
                    true,
                    cx,
                ))
                .when_some(settings.and_then(settings_target), |this, target| {
                    this.child(launch_approval_prompts::render_target_rows(&target, cx))
                })
        }
        (None, None) => return div().into_any_element(),
    };

    v_flex()
        .mt_1()
        .p_2()
        .gap_1()
        .w_full()
        .min_w_0()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .child(body)
        .when_some(approval_error, |this, error| {
            this.child(render_form_error(error))
        })
        .into_any_element()
}

fn render_approval_buttons(
    context_server_id: &ContextServerId,
    shown: &LaunchApprovalKey,
    state: LaunchApprovalState,
    decision_blocked: bool,
    cx: &mut Context<SettingsWindow>,
) -> impl IntoElement {
    let deny_id = context_server_id.clone();
    let approve_id = context_server_id.clone();
    let deny_key = shown.clone();
    let approve_key = shown.clone();
    h_flex()
        .w_full()
        .gap_1()
        .justify_end()
        .when(state == LaunchApprovalState::Pending, |this| {
            this.child(
                Button::new(
                    SharedString::from(format!("mcp-deny-{}", deny_id.0)),
                    "許可しない",
                )
                .style(ButtonStyle::Subtle)
                .label_size(LabelSize::Small)
                .tab_index(0isize)
                .disabled(decision_blocked)
                .on_click(cx.listener(move |this, _event, _window, cx| {
                    decide_project_server(
                        this,
                        deny_id.clone(),
                        deny_key.clone(),
                        ProjectServerDecision::Deny,
                        cx,
                    );
                })),
            )
        })
        .child(
            Button::new(
                SharedString::from(format!("mcp-approve-{}", approve_id.0)),
                "許可して起動",
            )
            .style(ButtonStyle::Filled)
            .label_size(LabelSize::Small)
            .tab_index(0isize)
            .disabled(decision_blocked)
            .when(decision_blocked, |button| {
                button.tooltip(Tooltip::text(
                    launch_approval_prompts::DECISION_BLOCKED_HINT,
                ))
            })
            .on_click(cx.listener(move |this, _event, _window, cx| {
                decide_project_server(
                    this,
                    approve_id.clone(),
                    approve_key.clone(),
                    ProjectServerDecision::Approve,
                    cx,
                );
            })),
        )
}

/// What a server's settings would launch, shown for a project entry that is
/// disabled and so has no approval details to show it from.
fn settings_target(settings: &ContextServerSettings) -> Option<LaunchTarget> {
    match settings {
        ContextServerSettings::Stdio { command, .. } => Some(LaunchTarget::Command {
            program: command.path.to_string_lossy().to_string().into(),
            arguments: command
                .args
                .iter()
                .map(|argument| argument.clone().into())
                .collect(),
        }),
        ContextServerSettings::Http { url, .. } => Some(LaunchTarget::Url(url.clone().into())),
        ContextServerSettings::Extension { .. } => None,
    }
}

/// Opens this page with `server_id`'s row highlighted, for a notification
/// that asked the user to look at it.
fn show_context_server(
    settings_window: &mut SettingsWindow,
    server_id: ContextServerId,
    owning_workspace: Option<WeakEntity<Workspace>>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.mcp_page.target_workspace = owning_workspace;
    let open_page_index = settings_window
        .sub_page_stack
        .iter()
        .rposition(|sub_page| sub_page.link.r#type == crate::SubPageType::McpServers);
    if let Some(open_page_index) = open_page_index {
        // Back down to the page itself: a form or paste page above it would
        // hide the row.
        while settings_window.sub_page_stack.len() > open_page_index + 1 {
            settings_window.pop_sub_page(window, cx);
        }
        subscribe_to_context_server_store(settings_window, cx);
        refresh_all_tool_lists(settings_window, cx);
    } else {
        let Some((page_index, link, section_header)) = mcp_servers_link(settings_window) else {
            log::error!("the MCP servers page is missing from the settings pages");
            return;
        };
        // A search in progress could hide the page the link lives on.
        settings_window.opening_link = false;
        settings_window.search_bar.update(cx, |editor, cx| {
            editor.set_text(String::new(), window, cx);
        });
        for page_filter in &mut settings_window.filter_table {
            page_filter.fill(true);
        }
        settings_window.has_query = false;
        settings_window.filter_matches_to_file();
        if let Some(navbar_entry_index) = settings_window
            .navbar_entries
            .iter()
            .position(|entry| entry.is_root && entry.page_index == page_index)
        {
            settings_window.open_and_scroll_to_navbar_entry(
                navbar_entry_index,
                None,
                false,
                window,
                cx,
            );
        }
        settings_window.push_sub_page(link, section_header.into(), window, cx);
    }
    settings_window.mcp_page.focused_server = Some(server_id);
    cx.notify();
}

fn mcp_servers_link(
    settings_window: &SettingsWindow,
) -> Option<(usize, crate::SubPageLink, &'static str)> {
    settings_window
        .pages
        .iter()
        .enumerate()
        .find_map(|(page_index, page)| {
            let mut section_header = "";
            page.items.iter().find_map(|item| match item {
                SettingsPageItem::SectionHeader(header) => {
                    section_header = header;
                    None
                }
                SettingsPageItem::SubPageLink(link)
                    if link.r#type == crate::SubPageType::McpServers =>
                {
                    Some((page_index, link.clone(), section_header))
                }
                _ => None,
            })
        })
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

/// The context server client reports its request timeout in English; the rest
/// of an error (a parse failure's detail, say) is passed through as it is.
fn server_error_text(message: &str) -> String {
    message.replace(
        "Context server request timeout",
        "MCP サーバーが応答しませんでした (タイムアウト)",
    )
}

fn render_tool_list(
    context_server_id: &ContextServerId,
    list: &McpToolList,
    cx: &mut Context<SettingsWindow>,
) -> Option<AnyElement> {
    let muted_line = |icon: IconName, color: Color, message: SharedString| {
        h_flex()
            .py_1()
            .min_w_0()
            .w_full()
            .gap_2()
            .child(Icon::new(icon).size(IconSize::XSmall).color(color))
            .child(
                div().min_w_0().flex_1().child(
                    Label::new(message)
                        .color(Color::Muted)
                        .size(LabelSize::Small),
                ),
            )
            .into_any_element()
    };

    let tools = match &list.state {
        McpToolListState::Unsupported => return None,
        McpToolListState::Loading => {
            return Some(muted_line(
                IconName::ToolHammer,
                Color::Muted,
                "ツール一覧を取得しています…".into(),
            ));
        }
        McpToolListState::Failed(error) => {
            return Some(muted_line(
                IconName::Warning,
                Color::Warning,
                format!("ツール一覧を取得できませんでした: {error}").into(),
            ));
        }
        McpToolListState::Loaded(tools) if tools.is_empty() => {
            return Some(muted_line(
                IconName::ToolHammer,
                Color::Muted,
                "ツールはありません".into(),
            ));
        }
        McpToolListState::Loaded(tools) => tools,
    };

    let expanded = list.expanded;
    let toggle_id = context_server_id.clone();
    let toggle = Button::new(
        SharedString::from(format!("mcp-tools-toggle-{}", context_server_id.0)),
        format!("ツール {} 個", tools.len()),
    )
    .style(ButtonStyle::Subtle)
    .label_size(LabelSize::Small)
    .color(Color::Muted)
    .start_icon(
        Icon::new(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        })
        .size(IconSize::XSmall)
        .color(Color::Muted),
    )
    .tab_index(0isize)
    .on_click(cx.listener(move |this, _event, _window, cx| {
        if let Some(list) = this.mcp_page.tool_lists.get_mut(&toggle_id) {
            list.expanded = !list.expanded;
        }
        cx.notify();
    }));

    Some(
        v_flex()
            .min_w_0()
            .w_full()
            .child(h_flex().child(toggle))
            .when(expanded, |this| {
                this.child(
                    v_flex()
                        .pl_5()
                        .pb_1()
                        .gap_0p5()
                        .min_w_0()
                        .w_full()
                        .children(tools.iter().map(|tool| {
                            h_flex()
                                .min_w_0()
                                .w_full()
                                .gap_2()
                                .child(
                                    Label::new(tool.name.clone())
                                        .size(LabelSize::Small)
                                        .buffer_font(cx)
                                        .flex_shrink_0(),
                                )
                                .when_some(tool.description.clone(), |this, description| {
                                    this.child(
                                        div().min_w_0().flex_1().child(
                                            Label::new(description)
                                                .size(LabelSize::Small)
                                                .color(Color::Muted)
                                                .truncate(),
                                        ),
                                    )
                                })
                        })),
                )
            })
            .into_any_element(),
    )
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
                Err(error) => McpConnectionTestOutcome::Failed(server_error_text(&error).into()),
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
    disabled: bool,
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
    .disabled(disabled)
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
                                    Label::new(server_error_text(error))
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
                    let open_in_settings_window =
                        |open: fn(&mut SettingsWindow, &mut Window, &mut Context<SettingsWindow>)| {
                            let settings_window = settings_window.clone();
                            move |window: &mut Window, cx: &mut App| {
                                settings_window
                                    .update(cx, |this, cx| open(this, window, cx))
                                    .log_err();
                            }
                        };
                    menu.custom_entry(
                        add_menu_entry(
                            "テンプレートから追加",
                            "ファイルシステム・GitHub など、よく使うサーバーの設定をフォームに読み込みます。",
                        ),
                        open_in_settings_window(open_mcp_templates_page),
                    )
                    .custom_entry(
                        add_menu_entry(
                            "ローカルサーバーを追加",
                            "コマンド型: 実行ファイルを起動し、標準入出力でやり取りします。",
                        ),
                        open_in_settings_window(|this, window, cx| {
                            open_mcp_server_form(this, McpTransport::Stdio, None, window, cx)
                        }),
                    )
                    .custom_entry(
                        add_menu_entry(
                            "リモートサーバーを追加",
                            "HTTP 型: URL で公開されているサーバーに HTTP で接続します。",
                        ),
                        open_in_settings_window(|this, window, cx| {
                            open_mcp_server_form(this, McpTransport::Http, None, window, cx)
                        }),
                    )
                    .custom_entry(
                        add_menu_entry(
                            "JSON から貼り付け",
                            "README などに載っている mcpServers の JSON をフォームに読み込みます。",
                        ),
                        open_in_settings_window(open_mcp_paste_page),
                    )
                    .separator()
                    .custom_entry(
                        add_menu_entry(
                            "拡張機能からインストール",
                            "拡張機能由来: 拡張機能がコマンドを用意します。設定は拡張機能ごとのフォームで行います。",
                        ),
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
                        },
                    )
                }))
            }
        });

    div()
        .rounded_md()
        .border_1()
        .border_color(border_color)
        .child(popover)
}

/// One line saying what kind of server the entry adds: the three kinds differ
/// in who starts the server and how Zed talks to it, which the labels alone
/// do not say.
fn add_menu_entry(
    label: &'static str,
    description: &'static str,
) -> impl Fn(&mut Window, &mut App) -> AnyElement + 'static {
    move |_window, _cx| {
        v_flex()
            .w(rems(22.))
            .py_0p5()
            .child(Label::new(label))
            .child(
                Label::new(description)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .into_any_element()
    }
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
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
    /// What a template asked the user to put in the value, which then may not
    /// be saved empty.
    prompt: Option<String>,
}

struct ArgumentRow {
    editor: Entity<Editor>,
    /// Added with 追加 rather than loaded: left blank, it is not an argument,
    /// whereas an empty argument the settings already had is kept.
    added: bool,
    /// As for [`KeyValueRow::prompt`].
    prompt: Option<String>,
}

/// Which settings file saving the form writes to.
enum McpFormDestination {
    UserSettings,
    /// The `.zed/settings.json` that defines the server. Only an edit there
    /// takes effect: the project's entry replaces any entry of the user's with
    /// the same name, so writing the user's file would only leave a copy
    /// behind that nothing runs.
    ProjectFile {
        worktree_id: WorktreeId,
        file_label: SharedString,
        /// The entry as the file had it when the form opened; the save is
        /// refused if the file no longer says this.
        loaded: ContextServerSettingsContent,
    },
}

/// Editor-backed state for the custom MCP server add/edit form.
pub(crate) struct McpServerForm {
    transport: McpTransport,
    /// `Some` when editing an existing server (used to remove the old entry on rename).
    original_id: Option<ContextServerId>,
    name: Entity<Editor>,
    command: Entity<Editor>,
    /// One input per argument, so an argument may contain spaces.
    args: Vec<ArgumentRow>,
    url: Entity<Editor>,
    timeout: Entity<Editor>,
    oauth_client_id: Entity<Editor>,
    env: Vec<KeyValueRow>,
    headers: Vec<KeyValueRow>,
    /// Run the server on the SSH host instead of this machine. Only meaningful
    /// for a stdio server in a remote project, but round-tripped in every case
    /// so saving the form never drops a hand-written value.
    remote: bool,
    /// Not editable here, but carried through so that saving an edit neither
    /// re-enables a disabled server nor drops a hand-written client secret.
    enabled: bool,
    oauth_client_secret: Option<String>,
    /// What a pasted snippet contained that the form could not take.
    import_note: Option<SharedString>,
    error: Option<SharedString>,
    destination: McpFormDestination,
    /// Set when the command could not be found; a second save of the same
    /// command goes ahead regardless (it may only exist where the server runs).
    command_warning: Option<(String, SharedString)>,
    /// A save that has to wait (for the command lookup or the project file's
    /// write) is under way.
    saving: bool,
}

impl McpServerForm {
    fn from_values(
        values: McpServerFormValues,
        import_note: Option<SharedString>,
        window: &mut Window,
        cx: &mut Context<SettingsWindow>,
    ) -> Self {
        Self {
            transport: values.transport,
            original_id: values.original_id,
            name: new_input("my-mcp-server", Some(values.name.as_str()), window, cx),
            command: new_input("/path/to/server", Some(values.command.as_str()), window, cx),
            args: values
                .args
                .iter()
                .enumerate()
                .map(|(index, arg)| {
                    let prompt = values.prompts.args.get(index).cloned().flatten();
                    ArgumentRow {
                        editor: match &prompt {
                            Some(prompt) => new_input(
                                &prompt_placeholder(prompt),
                                Some(arg.as_str()),
                                window,
                                cx,
                            ),
                            None => new_arg_input(Some(arg.as_str()), window, cx),
                        },
                        added: false,
                        prompt,
                    }
                })
                .collect(),
            url: new_input(
                "https://example.com/mcp",
                Some(values.url.as_str()),
                window,
                cx,
            ),
            timeout: new_input("60", Some(values.timeout.as_str()), window, cx),
            oauth_client_id: new_input(
                "OAuth クライアント ID (省略可)",
                Some(values.oauth_client_id.as_str()),
                window,
                cx,
            ),
            env: values
                .env
                .iter()
                .enumerate()
                .map(|(index, (key, value))| {
                    let prompt = values.prompts.env.get(index).cloned().flatten();
                    new_kv_row(Some(key.as_str()), Some(value.as_str()), prompt, window, cx)
                })
                .collect(),
            headers: values
                .headers
                .iter()
                .enumerate()
                .map(|(index, (key, value))| {
                    let prompt = values.prompts.headers.get(index).cloned().flatten();
                    new_kv_row(Some(key.as_str()), Some(value.as_str()), prompt, window, cx)
                })
                .collect(),
            remote: values.remote,
            enabled: values.enabled,
            oauth_client_secret: values.oauth_client_secret,
            import_note,
            error: None,
            destination: McpFormDestination::UserSettings,
            command_warning: None,
            saving: false,
        }
    }
}

impl McpServerFormValues {
    fn empty(transport: McpTransport) -> Self {
        Self {
            transport,
            original_id: None,
            name: String::new(),
            command: String::new(),
            args: Vec::new(),
            url: String::new(),
            timeout: String::new(),
            oauth_client_id: String::new(),
            oauth_client_secret: None,
            env: Vec::new(),
            headers: Vec::new(),
            remote: false,
            enabled: true,
            prompts: TemplatePrompts::default(),
        }
    }

    /// Pre-fills from the raw settings so invalid values (e.g. a malformed URL
    /// the user typed directly into settings.json) still load into the form
    /// for correction, rather than being dropped during resolution.
    fn from_existing(
        transport: McpTransport,
        existing: Option<(ContextServerId, ContextServerSettings)>,
    ) -> Self {
        let mut values = Self::empty(transport);
        let Some((id, settings)) = existing else {
            return values;
        };
        values.name = id.0.to_string();
        values.original_id = Some(id);
        values.enabled = settings.enabled();
        values.remote = configured_remote(Some(&settings), None);

        match settings {
            ContextServerSettings::Stdio { command, .. } => {
                values.command = command.path.to_string_lossy().to_string();
                values.args = command.args;
                values.timeout = command
                    .timeout
                    .map(|timeout| timeout.to_string())
                    .unwrap_or_default();
                if let Some(env) = &command.env {
                    values.env = sorted_pairs(env);
                }
            }
            ContextServerSettings::Http {
                url,
                headers,
                timeout,
                oauth,
                ..
            } => {
                values.url = url;
                values.timeout = timeout
                    .map(|timeout| timeout.to_string())
                    .unwrap_or_default();
                values.headers = sorted_pairs(&headers);
                if let Some(oauth) = oauth {
                    values.oauth_client_id = oauth.client_id;
                    values.oauth_client_secret = oauth.client_secret;
                }
            }
            ContextServerSettings::Extension { .. } => {}
        }
        values
    }

    fn from_import(server: &ImportedServer) -> Self {
        match &server.definition {
            ImportedDefinition::Local {
                command,
                args,
                env,
                timeout,
                remote,
            } => {
                let mut values = Self::empty(McpTransport::Stdio);
                values.name = server.name.clone();
                values.command = command.clone();
                values.args = args.clone();
                values.env = env.clone();
                values.timeout = timeout
                    .map(|timeout| timeout.to_string())
                    .unwrap_or_default();
                values.remote = *remote;
                values.enabled = server.enabled;
                values
            }
            ImportedDefinition::Remote {
                url,
                headers,
                timeout,
                oauth_client_id,
                oauth_client_secret,
            } => {
                let mut values = Self::empty(McpTransport::Http);
                values.name = server.name.clone();
                values.url = url.clone();
                values.headers = headers.clone();
                values.timeout = timeout
                    .map(|timeout| timeout.to_string())
                    .unwrap_or_default();
                values.oauth_client_id = oauth_client_id.clone().unwrap_or_default();
                values.oauth_client_secret = oauth_client_secret.clone();
                values.enabled = server.enabled;
                values
            }
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
    prompt: Option<String>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> KeyValueRow {
    let value_placeholder = prompt
        .as_deref()
        .map_or_else(|| "値".to_string(), prompt_placeholder);
    KeyValueRow {
        key: new_input("キー", key, window, cx),
        value: new_input(&value_placeholder, value, window, cx),
        prompt,
    }
}

fn prompt_placeholder(prompt: &str) -> String {
    format!("入力してください: {prompt}")
}

fn new_arg_input(
    value: Option<&str>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> Entity<Editor> {
    new_input("引数", value, window, cx)
}

/// Creates the form state and pushes the form sub-page onto the stack.
pub(crate) fn open_mcp_server_form(
    settings_window: &mut SettingsWindow,
    transport: McpTransport,
    existing: Option<(ContextServerId, ContextServerSettings)>,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let values = McpServerFormValues::from_existing(transport, existing);
    let form = McpServerForm::from_values(values, None, window, cx);
    push_mcp_server_form(settings_window, form, window, cx);
}

fn push_mcp_server_form(
    settings_window: &mut SettingsWindow,
    form: McpServerForm,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let is_edit = form.original_id.is_some();
    let transport = form.transport;
    settings_window.mcp_server_form = Some(form);

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
    let saving = form.saving;
    let command_warning = form
        .command_warning
        .as_ref()
        .map(|(_, message)| message.clone());
    let project_file = match &form.destination {
        McpFormDestination::UserSettings => None,
        McpFormDestination::ProjectFile { file_label, .. } => Some(file_label.clone()),
    };
    // Outside a remote project the setting has no effect (the server always runs
    // here), so offering the choice would only invite a wrong expectation.
    let show_execution_host = get_context_server_store(settings_window, cx)
        .is_some_and(|store| store.read(cx).is_remote_project());
    let kind_description = match transport {
        McpTransport::Stdio => {
            "コマンド型のサーバーです。Zed が実行ファイルを起動し、標準入出力でやり取りします。"
        }
        McpTransport::Http => {
            "HTTP 型のサーバーです。Zed が URL に接続し、HTTP でやり取りします。接続先のホストはネットワーク許可リストの対象です。"
        }
    };
    let working_folder_notice = if project_file.is_some() {
        "プロジェクトの設定で定義したサーバーは、許可するとプロジェクトのフォルダーで起動します。"
    } else {
        "このサーバーは Zed 専用の空のフォルダーで起動します (プロジェクトのフォルダーではありません)。プロジェクトを対象にするときは、引数にフォルダーの絶対パスを指定してください。"
    };

    let fields = v_flex()
        .w_full()
        .gap_4()
        .child(
            Label::new(kind_description)
                .size(LabelSize::Small)
                .color(Color::Muted),
        )
        .when_some(project_file, |this, file_label| {
            this.child(launch_approval_prompts::render_notice(
                IconName::Info,
                Color::Warning,
                format!(
                    "このサーバーはプロジェクトの設定ファイル {file_label} で定義されています。保存するとこのファイルを書き換えます (ユーザー設定には書きません)。内容が変わるため、起動するにはもう一度許可が必要です。"
                ),
            ))
        })
        .when_some(form.import_note.clone(), |this, note| {
            this.child(render_import_note(note))
        })
        .child(render_form_field(
            settings_window,
            "サーバー名",
            "必須。この MCP サーバーを識別する一意の名前です。",
            &form.name,
            cx,
        ))
        .map(|this| match transport {
            McpTransport::Stdio => this
                .child(render_command_field(
                    settings_window,
                    &form.command,
                    show_execution_host && remote,
                    cx,
                ))
                .child(render_args_section(settings_window, &form.args, cx))
                // Servers from the user's settings start in an empty folder of
                // Zed's own rather than in whichever project happens to be open.
                .child(launch_approval_prompts::render_notice(
                    IconName::Info,
                    Color::Muted,
                    working_folder_notice.to_string(),
                ))
                .child(render_kv_section(
                    settings_window,
                    "環境変数",
                    "サーバーのプロセスに渡す環境変数です。API キーやトークンなど、サーバーの説明書が求める値をここに入力します。",
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
                    "必須。リモート MCP サーバーのエンドポイントの URL です (例: https://example.com/mcp)。",
                    &form.url,
                    cx,
                ))
                .child(render_kv_section(
                    settings_window,
                    "ヘッダー",
                    "サーバーへの各リクエストに付与する HTTP ヘッダーです。トークンで認証するサーバーでは Authorization に「Bearer <トークン>」を指定します。",
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
                    "サーバーの認証に使う OAuth クライアント ID です (省略可)。サーバーが自動登録に対応していれば空欄のままで、接続時にブラウザーでの認証を求められます。",
                    &form.oauth_client_id,
                    cx,
                )),
        })
        .when_some(command_warning, |this, warning| {
            this.child(launch_approval_prompts::render_notice(
                IconName::Warning,
                Color::Warning,
                warning.to_string(),
            ))
        })
        .when_some(error, |this, error| this.child(render_form_error(error)))
        .child(render_form_actions(saving, cx));

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

/// Shared framed text field for the settings form pages.
pub(crate) fn input_box(editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let border_focused = cx.theme().colors().border_focused;
    // All form inputs share tab index 0, so tab order follows render (insertion)
    // order. Tracking the editor's focus handle makes the field a tab stop and
    // routes keyboard focus into the editor when tabbed to.
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    // The frame is rendered unfocused and the focused border is applied through
    // the element's focus style, so no `Window` is needed here.
    ui::input_frame(false, cx)
        .min_w_64()
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(border_focused))
        .child(editor.clone())
}

/// [`input_box`] for a row that also holds buttons: it gives way to them
/// rather than keeping a fixed minimum width that pushes them out of view.
fn row_input_box(editor: &Entity<Editor>, cx: &App) -> impl IntoElement {
    let border_focused = cx.theme().colors().border_focused;
    let focus_handle = editor.focus_handle(cx).tab_index(0).tab_stop(true);
    ui::input_frame(false, cx)
        .w_full()
        .min_w_0()
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(border_focused))
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
        .w_64()
        .min_w_0()
        .gap_2()
        .children(rows.iter().enumerate().map(|(ix, row)| {
            v_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .child(
                    h_flex()
                        .w_full()
                        .min_w_0()
                        .gap_1()
                        .items_center()
                        .child(div().flex_1().min_w_0().child(row_input_box(&row.key, cx)))
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
                .child(row_input_box(&row.value, cx))
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
                    let row = new_kv_row(None, None, None, window, cx);
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

fn render_command_field(
    settings_window: &SettingsWindow,
    command: &Entity<Editor>,
    runs_on_ssh_host: bool,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    // A file picked on this machine means nothing to a server that is launched
    // on the SSH host.
    let browse_button = IconButton::new("mcp-command-browse", IconName::FolderOpen)
        .icon_size(IconSize::Small)
        .icon_color(Color::Muted)
        .tab_index(0isize)
        .disabled(runs_on_ssh_host)
        .tooltip(Tooltip::text(if runs_on_ssh_host {
            "SSH 接続先で実行する場合は、接続先のパスを直接入力してください。"
        } else {
            "ファイルを選択"
        }))
        .on_click(cx.listener(|this, _event, window, cx| {
            pick_command_file(this, window, cx);
        }));
    let control = h_flex()
        .w_64()
        .min_w_0()
        .gap_1()
        .items_center()
        .child(div().flex_1().min_w_0().child(row_input_box(command, cx)))
        .child(browse_button)
        .into_any_element();

    crate::render_settings_item_layout(
        settings_window,
        "コマンド",
        "必須。サーバーを起動する実行ファイルのパス、または PATH にあるコマンドの名前 (npx・uvx など) です。保存するときに、見つかるかどうかだけを確かめます (実行はしません)。",
        control,
        None,
        None,
        None,
        false,
        cx,
    )
    .into_any_element()
}

/// Where the command picker opens: the folder holding the command already
/// entered when that is a folder on this machine that exists, and the home
/// folder otherwise.
async fn command_picker_starting_directory(command: &str, fs: &dyn fs::Fs) -> PathBuf {
    let command = Path::new(command.trim());
    // The command may come from a pasted snippet, and looking up a UNC path
    // would send the user's credentials to the host it names.
    if util::paths::is_local_absolute_path(command)
        && let Some(folder) = command.parent()
        && fs.is_dir(folder).await
    {
        return folder.to_path_buf();
    }
    paths::home_dir().clone()
}

fn pick_command_file(
    settings_window: &SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window.mcp_server_form.as_ref() else {
        return;
    };
    let command = form.command.read(cx).text(cx);
    let fs = <dyn fs::Fs>::global(cx);
    cx.spawn_in(window, async move |this, cx| {
        let starting_directory = command_picker_starting_directory(&command, fs.as_ref()).await;
        // Attached to the settings window: a dialog shown for the whole
        // application goes to whichever window happens to be active.
        let Some(paths) = this
            .update_in(cx, |_, window, cx| {
                window.prompt_for_paths(
                    gpui::PathPromptOptions {
                        files: true,
                        directories: false,
                        multiple: false,
                        prompt: Some("選択".into()),
                        starting_directory: Some(starting_directory),
                    },
                    cx,
                )
            })
            .log_err()
        else {
            return;
        };
        let picked = match paths.await {
            Ok(Ok(Some(paths))) => paths.into_iter().next(),
            // Dismissed, or the dialog went away with the window.
            Ok(Ok(None)) | Err(_) => None,
            Ok(Err(error)) => {
                this.update(cx, |this, cx| {
                    if let Some(form) = this.mcp_server_form.as_mut() {
                        form.error =
                            Some(format!("ファイルを選択できませんでした: {error}").into());
                    }
                    cx.notify();
                })
                .log_err();
                None
            }
        };
        let Some(path) = picked else {
            return;
        };
        this.update_in(cx, |this, window, cx| {
            if let Some(form) = this.mcp_server_form.as_ref() {
                form.command.update(cx, |editor, cx| {
                    editor.set_text(path.to_string_lossy().to_string(), window, cx);
                });
            }
        })
        .log_err();
    })
    .detach();
}

fn render_args_section(
    settings_window: &SettingsWindow,
    args: &[ArgumentRow],
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let arg_count = args.len();
    let control = v_flex()
        .w_64()
        .min_w_0()
        .gap_2()
        .children(args.iter().enumerate().map(|(index, arg)| {
            h_flex()
                .w_full()
                .min_w_0()
                .gap_1()
                .items_center()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .child(row_input_box(&arg.editor, cx)),
                )
                .child(
                    IconButton::new(("mcp-arg-up", index), IconName::ArrowUp)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .disabled(index == 0)
                        .tooltip(Tooltip::text("上へ移動"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(form) = this.mcp_server_form.as_mut()
                                && index > 0
                                && index < form.args.len()
                            {
                                form.args.swap(index - 1, index);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    IconButton::new(("mcp-arg-down", index), IconName::ArrowDown)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .disabled(index + 1 >= arg_count)
                        .tooltip(Tooltip::text("下へ移動"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(form) = this.mcp_server_form.as_mut()
                                && index + 1 < form.args.len()
                            {
                                form.args.swap(index, index + 1);
                            }
                            cx.notify();
                        })),
                )
                .child(
                    IconButton::new(("mcp-arg-remove", index), IconName::Close)
                        .icon_size(IconSize::Small)
                        .icon_color(Color::Muted)
                        .tooltip(Tooltip::text("削除"))
                        .on_click(cx.listener(move |this, _, _window, cx| {
                            if let Some(form) = this.mcp_server_form.as_mut()
                                && index < form.args.len()
                            {
                                form.args.remove(index);
                            }
                            cx.notify();
                        })),
                )
        }))
        .child(
            Button::new("mcp-arg-add", "追加")
                .style(ButtonStyle::Outlined)
                .label_size(LabelSize::Small)
                .start_icon(
                    Icon::new(IconName::Plus)
                        .size(IconSize::Small)
                        .color(Color::Muted),
                )
                .on_click(cx.listener(|this, _, window, cx| {
                    let arg = new_arg_input(None, window, cx);
                    arg.focus_handle(cx).focus(window, cx);
                    if let Some(form) = this.mcp_server_form.as_mut() {
                        form.args.push(ArgumentRow {
                            editor: arg,
                            added: true,
                            prompt: None,
                        });
                    }
                    cx.notify();
                })),
        )
        .into_any_element();

    crate::render_settings_item_layout(
        settings_window,
        "引数",
        "コマンドに渡す引数です。1 行に 1 つずつ入力します (空白を含む引数もそのまま渡されます)。追加したまま空の行は無視されます。",
        control,
        None,
        None,
        None,
        false,
        cx,
    )
    .into_any_element()
}

fn render_import_note(note: SharedString) -> impl IntoElement {
    h_flex()
        .w_full()
        .gap_2()
        .items_start()
        .child(
            Icon::new(IconName::Info)
                .size(IconSize::Small)
                .color(Color::Warning),
        )
        .child(Label::new(note).size(LabelSize::Small).color(Color::Muted))
}

fn import_note(ignored_keys: &[String]) -> Option<SharedString> {
    (!ignored_keys.is_empty()).then(|| {
        format!(
            "次の項目は Zed では使わないため読み込みませんでした: {}",
            ignored_keys.join(", ")
        )
        .into()
    })
}

/// One line for the list of pasted servers; the approval details show every
/// argument separately instead. For showing, not for running: arguments are
/// passed to the server as a list, never through a shell. Backslashes are
/// left alone so that a Windows path reads exactly as it runs.
fn display_command_line(command: &str, args: &[String]) -> String {
    std::iter::once(command)
        .chain(args.iter().map(String::as_str))
        .map(|part| {
            let part = escape_for_display(part);
            if part.is_empty()
                || part
                    .chars()
                    .any(|character| character.is_whitespace() || character == '"')
            {
                format!("\"{}\"", part.replace('"', "\\\""))
            } else {
                part.into_owned()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn join_names(pairs: &[(String, String)]) -> String {
    pairs
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

fn open_mcp_paste_page(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let editor = cx.new(|cx| {
        let mut editor = Editor::auto_height(8, 24, window, cx);
        editor.set_placeholder_text(
            r#"{ "mcpServers": { "サーバー名": { "command": "…", "args": ["…"] } } }"#,
            window,
            cx,
        );
        editor
    });
    settings_window.mcp_page.paste = Some(McpPasteState {
        editor: editor.clone(),
        outcome: None,
    });
    settings_window.push_dynamic_sub_page(
        "JSON から MCP サーバーを追加",
        "エージェントの設定",
        Some("context_servers"),
        false,
        render_mcp_paste_page,
        window,
        cx,
    );
    editor.focus_handle(cx).focus(window, cx);
}

fn render_mcp_paste_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let Some(paste) = settings_window.mcp_page.paste.as_ref() else {
        return div().into_any_element();
    };
    let existing_ids = existing_server_ids(settings_window, cx);

    let colors = cx.theme().colors();
    let border_focused = colors.border_focused;
    let focus_handle = paste.editor.focus_handle(cx).tab_index(0).tab_stop(true);
    // Not `input_box`: its frame has the fixed height of a single line.
    let input = div()
        .w_full()
        .px_2()
        .py_1()
        .rounded_md()
        .border_1()
        .border_color(colors.border)
        .bg(colors.editor_background)
        .track_focus(&focus_handle)
        .focus(|style| style.border_color(border_focused))
        .child(paste.editor.clone());

    v_flex()
        .id("mcp-paste-page")
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
                        "MCP サーバーの README などに載っている設定の JSON を貼り付けて「読み込む」を押してください。Claude Desktop・Cursor の mcpServers、VS Code の servers、Zed の context_servers の形式に対応しています。読み込んだ内容はフォームに入り、「保存」を押すまで設定には書き込まれません。",
                    )
                    .size(LabelSize::Small)
                    .color(Color::Muted),
                )
                .child(input)
                .child(
                    h_flex().w_full().justify_end().child(
                        Button::new("mcp-paste-load", "読み込む")
                            .style(ButtonStyle::Filled)
                            .on_click(cx.listener(|this, _, window, cx| {
                                load_pasted_json(this, window, cx);
                            })),
                    ),
                )
                .map(|this| match &paste.outcome {
                    None => this,
                    Some(McpPasteOutcome::Failed(error)) => {
                        this.child(render_form_error(error.clone()))
                    }
                    Some(McpPasteOutcome::Several {
                        servers,
                        confirming_add_all,
                        error,
                    }) => this.child(render_imported_servers(
                        servers,
                        *confirming_add_all,
                        error.clone(),
                        &existing_ids,
                        cx,
                    )),
                }),
        )
        .into_any_element()
}

fn render_imported_servers(
    servers: &[ImportedServer],
    confirming_add_all: bool,
    error: Option<SharedString>,
    existing_ids: &[ContextServerId],
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let border_color = cx.theme().colors().border_variant;
    let count = servers.len();
    let any_name_taken = servers.iter().any(|server| {
        existing_ids
            .iter()
            .any(|id| id.0.as_ref() == server.name.trim())
    });

    let cards = servers
        .iter()
        .enumerate()
        .map(|(index, server)| {
            let name_exists = existing_ids
                .iter()
                .any(|id| id.0.as_ref() == server.name.trim());
            let (kind, summary, names) = match &server.definition {
                ImportedDefinition::Local {
                    command, args, env, ..
                } => (
                    "ローカル",
                    display_command_line(command, args),
                    (!env.is_empty()).then(|| format!("環境変数: {}", join_names(env))),
                ),
                ImportedDefinition::Remote { url, headers, .. } => (
                    "リモート",
                    url.clone(),
                    (!headers.is_empty()).then(|| format!("ヘッダー: {}", join_names(headers))),
                ),
            };

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
                                .child(Label::new(server.display_name().to_string()))
                                .when(!server.enabled, |this| {
                                    this.child(
                                        Label::new("無効")
                                            .size(LabelSize::Small)
                                            .color(Color::Muted),
                                    )
                                })
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
                            Button::new(("mcp-paste-open", index), "フォームで開く")
                                .style(ButtonStyle::Outlined)
                                .label_size(LabelSize::Small)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    open_imported_server(this, index, window, cx);
                                })),
                        ),
                )
                .child(
                    div().min_w_0().child(
                        Label::new(summary)
                            .size(LabelSize::Small)
                            .color(Color::Muted)
                            .buffer_font(cx),
                    ),
                )
                .when_some(names, |this, names| {
                    this.child(Label::new(names).size(LabelSize::Small).color(Color::Muted))
                })
                .when_some(import_note(&server.ignored_keys), |this, note| {
                    this.child(
                        Label::new(note)
                            .size(LabelSize::Small)
                            .color(Color::Warning),
                    )
                })
        })
        .collect::<Vec<_>>();

    let footer = if confirming_add_all {
        h_flex()
            .w_full()
            .gap_2()
            .justify_end()
            .child(
                Label::new(format!(
                    "{count} 件のサーバーをすべてユーザー設定に追加します。よろしいですか?"
                ))
                .size(LabelSize::Small),
            )
            .child(
                Button::new("mcp-paste-add-all-cancel", "キャンセル")
                    .style(ButtonStyle::Subtle)
                    .on_click(cx.listener(|this, _, _window, cx| {
                        set_confirming_add_all(this, false, cx);
                    })),
            )
            .child(
                Button::new("mcp-paste-add-all-confirm", "追加する")
                    .style(ButtonStyle::Filled)
                    .on_click(cx.listener(|this, _, window, cx| {
                        add_all_imported_servers(this, window, cx);
                    })),
            )
    } else {
        h_flex().w_full().justify_end().child(
            Button::new("mcp-paste-add-all", format!("{count} 件すべてを追加"))
                .style(ButtonStyle::Outlined)
                // Refusing only after the confirmation would waste the click:
                // the cards already say which names are taken.
                .disabled(any_name_taken)
                .when(any_name_taken, |button| {
                    button.tooltip(Tooltip::text(
                        "同名のサーバーがあるため、まとめては追加できません。「同名のサーバーがあります」と出ているサーバーをフォームで開き、名前を変えてから追加してください。",
                    ))
                })
                .on_click(cx.listener(|this, _, _window, cx| {
                    set_confirming_add_all(this, true, cx);
                })),
        )
    };

    v_flex()
        .w_full()
        .gap_2()
        .child(
            Label::new(format!(
                "{count} 件のサーバーが見つかりました。1 件ずつフォームで開いて確認するか、まとめて追加できます。"
            ))
            .size(LabelSize::Small),
        )
        .children(cards)
        .when_some(error, |this, error| this.child(render_form_error(error)))
        .child(footer)
        .into_any_element()
}

fn load_pasted_json(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(text) = settings_window
        .mcp_page
        .paste
        .as_ref()
        .map(|paste| paste.editor.read(cx).text(cx))
    else {
        return;
    };

    let outcome = match parse_mcp_json_snippet(&text) {
        Err(error) => McpPasteOutcome::Failed(error),
        Ok(mut servers) => {
            if servers.len() == 1
                && let Some(server) = servers.pop()
            {
                // The form takes this page's place, so that saving or
                // cancelling it goes straight back to the server list.
                settings_window.mcp_page.paste = None;
                settings_window.pop_sub_page(window, cx);
                open_imported_server_form(settings_window, &server, window, cx);
                return;
            }
            McpPasteOutcome::Several {
                servers,
                confirming_add_all: false,
                error: None,
            }
        }
    };
    if let Some(paste) = settings_window.mcp_page.paste.as_mut() {
        paste.outcome = Some(outcome);
    }
    cx.notify();
}

fn imported_servers(settings_window: &SettingsWindow) -> Option<&[ImportedServer]> {
    match settings_window.mcp_page.paste.as_ref()?.outcome.as_ref()? {
        McpPasteOutcome::Several { servers, .. } => Some(servers.as_slice()),
        McpPasteOutcome::Failed(_) => None,
    }
}

fn open_imported_server(
    settings_window: &mut SettingsWindow,
    index: usize,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(server) = imported_servers(settings_window).and_then(|servers| servers.get(index))
    else {
        return;
    };
    let server = server.clone();
    open_imported_server_form(settings_window, &server, window, cx);
}

fn open_imported_server_form(
    settings_window: &mut SettingsWindow,
    server: &ImportedServer,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let values = McpServerFormValues::from_import(server);
    let form = McpServerForm::from_values(values, import_note(&server.ignored_keys), window, cx);
    push_mcp_server_form(settings_window, form, window, cx);
}

fn set_confirming_add_all(
    settings_window: &mut SettingsWindow,
    confirming: bool,
    cx: &mut Context<SettingsWindow>,
) {
    if let Some(McpPasteState {
        outcome:
            Some(McpPasteOutcome::Several {
                confirming_add_all,
                error,
                ..
            }),
        ..
    }) = settings_window.mcp_page.paste.as_mut()
    {
        *confirming_add_all = confirming;
        *error = None;
    }
    cx.notify();
}

fn add_all_imported_servers(
    settings_window: &mut SettingsWindow,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(servers) = imported_servers(settings_window).map(<[ImportedServer]>::to_vec) else {
        return;
    };
    let existing_ids = existing_server_ids(settings_window, cx);

    match settings_for_imported_servers(&servers, &existing_ids) {
        Ok(entries) => {
            let fs = <dyn fs::Fs>::global(cx);
            // The settings writer puts each new key first in its object, so
            // adding them one at a time from the last one leaves them in the
            // snippet's order.
            for (id, content) in entries.into_iter().rev() {
                settings::update_settings_file(fs.clone(), cx, move |settings, _| {
                    settings.project.context_servers.insert(id.0, content);
                });
            }
            settings_window.mcp_page.paste = None;
            settings_window.pop_sub_page(window, cx);
        }
        Err(message) => {
            if let Some(McpPasteState {
                outcome:
                    Some(McpPasteOutcome::Several {
                        confirming_add_all,
                        error,
                        ..
                    }),
                ..
            }) = settings_window.mcp_page.paste.as_mut()
            {
                *confirming_add_all = false;
                *error = Some(message);
            }
            cx.notify();
        }
    }
}

/// Checked the way the form checks one server, and all or nothing: a partly
/// applied batch would leave the user to work out which servers made it.
fn settings_for_imported_servers(
    servers: &[ImportedServer],
    existing_ids: &[ContextServerId],
) -> Result<Vec<(ContextServerId, ContextServerSettingsContent)>, SharedString> {
    let mut entries: Vec<(ContextServerId, ContextServerSettingsContent)> = Vec::new();
    for server in servers {
        let (id, _, content) = build_settings_from_values(&McpServerFormValues::from_import(
            server,
        ))
        .map_err(|error| SharedString::from(format!("「{}」: {error}", server.display_name())))?;
        if name_collides_with_other_server(&id, None, existing_ids)
            || entries.iter().any(|(added, _)| added == &id)
        {
            return Err(format!(
                "「{}」という名前のサーバーは既に存在します。フォームで開いて名前を変えてから追加してください。",
                id.0
            )
            .into());
        }
        entries.push((id, content));
    }
    Ok(entries)
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

fn render_form_actions(saving: bool, cx: &mut Context<SettingsWindow>) -> impl IntoElement {
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
            Button::new(
                "mcp-form-save",
                if saving {
                    "保存しています…"
                } else {
                    "保存"
                },
            )
            .style(ButtonStyle::Filled)
            .disabled(saving)
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
        if form.saving {
            return;
        }
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
    let collides_with_other_server = name_collides_with_other_server(
        &id,
        original_id.as_ref(),
        &existing_server_ids(settings_window, cx),
    );
    if collides_with_other_server {
        if let Some(form) = settings_window.mcp_server_form.as_mut() {
            form.error = Some(format!("「{}」という名前のサーバーは既に存在します。", id.0).into());
        }
        cx.notify();
        return;
    }

    let is_remote_project = get_context_server_store(settings_window, cx)
        .is_some_and(|store| store.read(cx).is_remote_project());
    let Some(form) = settings_window.mcp_server_form.as_mut() else {
        return;
    };
    let Some(command) = command_to_look_up(&content, is_remote_project).filter(|command| {
        form.command_warning
            .as_ref()
            .is_none_or(|(warned_about, _)| warned_about != &command.program)
    }) else {
        write_mcp_server_form(settings_window, id, original_id, content, window, cx);
        return;
    };

    form.saving = true;
    form.error = None;
    let form_id = form.name.entity_id();
    cx.notify();
    let fs = <dyn fs::Fs>::global(cx);
    let path_variable = command
        .path_variable
        .clone()
        .or_else(|| std::env::var("PATH").ok());
    let extensions = executable_extensions();
    cx.spawn_in(window, async move |this, cx| {
        let lookup = look_up_command(
            &command.program,
            path_variable.as_deref(),
            &extensions,
            fs.as_ref(),
        )
        .await;
        this.update_in(cx, |this, window, cx| {
            if !is_showing_form(this, form_id) {
                return;
            }
            let Some(form) = this.mcp_server_form.as_mut() else {
                return;
            };
            form.saving = false;
            match lookup {
                CommandLookup::Missing => {
                    let message = missing_command_message(&command.program);
                    form.command_warning = Some((command.program, message));
                    cx.notify();
                }
                CommandLookup::Found | CommandLookup::NotChecked => {
                    form.command_warning = None;
                    write_mcp_server_form(this, id, original_id, content, window, cx);
                }
            }
        })
        .log_err();
    })
    .detach();
}

fn write_mcp_server_form(
    settings_window: &mut SettingsWindow,
    id: ContextServerId,
    original_id: Option<ContextServerId>,
    content: ContextServerSettingsContent,
    window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(form) = settings_window.mcp_server_form.as_ref() else {
        return;
    };
    let (worktree_id, loaded) = match &form.destination {
        McpFormDestination::UserSettings => {
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
            return;
        }
        McpFormDestination::ProjectFile {
            worktree_id,
            loaded,
            ..
        } => (*worktree_id, loaded.clone()),
    };

    let written = write_project_server(
        settings_window,
        worktree_id,
        original_id,
        loaded,
        id,
        content,
        cx,
    );
    let Some(form) = settings_window.mcp_server_form.as_mut() else {
        return;
    };
    let done = match written {
        Ok(done) => done,
        Err(error) => {
            form.error = Some(error);
            cx.notify();
            return;
        }
    };
    form.saving = true;
    form.error = None;
    let form_id = form.name.entity_id();
    cx.notify();
    cx.spawn_in(window, async move |this, cx| {
        let outcome = done.await;
        this.update_in(cx, |this, window, cx| {
            let showing = is_showing_form(this, form_id);
            let Some(form) = this
                .mcp_server_form
                .as_mut()
                .filter(|form| form.name.entity_id() == form_id)
            else {
                return;
            };
            form.saving = false;
            match outcome {
                Ok(()) => {
                    this.mcp_server_form = None;
                    if showing {
                        this.pop_sub_page(window, cx);
                    }
                }
                Err(error) => {
                    form.error = Some(error);
                    cx.notify();
                }
            }
        })
        .log_err();
    })
    .detach();
}

/// A save that had to wait finishes only on the form it was started from, and
/// closes the page only while that page is still on top: the breadcrumb leaves
/// the form without dropping it, and another form may have opened meanwhile.
fn is_showing_form(settings_window: &SettingsWindow, form_id: EntityId) -> bool {
    type RenderPage =
        fn(&SettingsWindow, &ScrollHandle, &mut Window, &mut Context<SettingsWindow>) -> AnyElement;
    settings_window
        .mcp_server_form
        .as_ref()
        .is_some_and(|form| form.name.entity_id() == form_id)
        && settings_window.sub_page_stack.last().is_some_and(|page| {
            std::ptr::fn_addr_eq(page.link.render, render_mcp_server_form_page as RenderPage)
        })
}

const PROJECT_DEFINITION_CHANGED: &str = "フォームを開いた後に、プロジェクトの設定ファイルでこのサーバーの定義が変わりました。フォームを開き直してください。";

/// Writes an edit of a server a project defines back into the
/// `.zed/settings.json` it came from, never into the user's settings. What
/// comes back resolves once the file is saved.
fn write_project_server(
    settings_window: &SettingsWindow,
    worktree_id: WorktreeId,
    original_id: Option<ContextServerId>,
    loaded: ContextServerSettingsContent,
    id: ContextServerId,
    content: ContextServerSettingsContent,
    cx: &mut Context<SettingsWindow>,
) -> Result<Task<Result<(), SharedString>>, SharedString> {
    let original_id = original_id.ok_or_else(|| SharedString::from(PROJECT_DEFINITION_CHANGED))?;
    let project = target_workspace(settings_window, cx)
        .map(|workspace| workspace.read(cx).project().clone())
        .ok_or_else(|| {
            SharedString::from(
                "プロジェクトが開かれていないため、プロジェクトの設定ファイルに保存できません。",
            )
        })?;

    // Checked here as well as when the file is rewritten: what the file said a
    // moment ago catches most changes before the save is shown as under way.
    let current = cx
        .global::<settings::SettingsStore>()
        .get_content_for_file(settings::SettingsFile::Project((
            worktree_id,
            util::rel_path::RelPath::empty_arc(),
        )))
        .and_then(|file| file.project.context_servers.get(&original_id.0));
    if current != Some(&loaded) {
        return Err(PROJECT_DEFINITION_CHANGED.into());
    }

    // Stays `None` when the edit never ran: the queue leaves a file it cannot
    // parse alone and still reports the save as done.
    let applied = Rc::new(RefCell::new(None::<Result<(), SharedString>>));
    let done = crate::update_project_setting_file_with_completion(
        &project,
        worktree_id,
        paths::local_settings_file_relative_path().into_arc(),
        {
            let applied = applied.clone();
            move |settings, _| {
                applied.replace(Some(apply_project_server_edit(
                    &mut settings.project.context_servers,
                    &original_id,
                    &loaded,
                    &id,
                    content,
                )));
            }
        },
        cx.entity().downgrade(),
        cx,
    )
    .map_err(|error| {
        SharedString::from(format!(
            "プロジェクトの設定ファイルに保存できませんでした: {error}"
        ))
    })?;

    Ok(cx.spawn(async move |_, _| match done.await {
        Ok(Ok(())) => applied.take().unwrap_or_else(|| {
            Err(
                "プロジェクトの設定ファイルを読み取れないため、保存できませんでした。ファイルの JSON の誤りを直してから、もう一度保存してください。"
                    .into(),
            )
        }),
        Ok(Err(error)) => Err(format!(
            "プロジェクトの設定ファイルに保存できませんでした: {error}"
        )
        .into()),
        Err(_) => Err(
            "プロジェクトの設定ファイルに保存できませんでした: 保存が取り消されました。".into(),
        ),
    }))
}

/// The edit itself, applied to the entries of the file being rewritten. The
/// whole entry is replaced by one of the kind the form was opened on, and only
/// if the file still holds what the form loaded: `context_servers` entries are
/// told apart by their fields alone, so an entry merged with another kind's
/// fields could read as a different server.
fn apply_project_server_edit(
    servers: &mut collections::HashMap<Arc<str>, ContextServerSettingsContent>,
    original_id: &ContextServerId,
    loaded: &ContextServerSettingsContent,
    id: &ContextServerId,
    content: ContextServerSettingsContent,
) -> Result<(), SharedString> {
    if servers.get(&original_id.0) != Some(loaded) {
        return Err(PROJECT_DEFINITION_CHANGED.into());
    }
    if original_id.0 != id.0 {
        if servers.contains_key(&id.0) {
            return Err(format!("「{}」という名前のサーバーは既に存在します。", id.0).into());
        }
        servers.remove(&original_id.0);
    }
    servers.insert(id.0.clone(), content);
    Ok(())
}

struct CommandToLookUp {
    program: String,
    /// The server's own `PATH`, which is the one its command is looked up in.
    path_variable: Option<String>,
}

/// The command of a stdio server that starts on this machine; one that
/// starts on the SSH host can only be looked up over there.
fn command_to_look_up(
    content: &ContextServerSettingsContent,
    is_remote_project: bool,
) -> Option<CommandToLookUp> {
    let ContextServerSettingsContent::Stdio {
        remote, command, ..
    } = content
    else {
        return None;
    };
    if is_remote_project && *remote {
        return None;
    }
    let path_variable = command.env.as_ref().and_then(|env| {
        env.iter()
            .find(|(name, _)| {
                if cfg!(windows) {
                    name.eq_ignore_ascii_case("PATH")
                } else {
                    name.as_str() == "PATH"
                }
            })
            .map(|(_, value)| value.clone())
    });
    Some(CommandToLookUp {
        program: command.path.to_string_lossy().trim().to_string(),
        path_variable,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CommandLookup {
    Found,
    Missing,
    /// A relative path or a network path: where it points depends on things
    /// this check does not look at.
    NotChecked,
}

fn executable_extensions() -> Vec<String> {
    if cfg!(windows) {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .map(str::trim)
            .filter(|extension| !extension.is_empty())
            .map(str::to_string)
            .collect()
    } else {
        Vec::new()
    }
}

/// Whether `program` names a file that exists, directly or in a folder of
/// `path_variable`, the way the server's launch would find it. Nothing is run.
async fn look_up_command(
    program: &str,
    path_variable: Option<&str>,
    extensions: &[String],
    fs: &dyn fs::Fs,
) -> CommandLookup {
    let program = program.trim();
    if program.is_empty() {
        return CommandLookup::NotChecked;
    }
    let candidates = |base: &Path| {
        std::iter::once(base.to_path_buf())
            .chain(extensions.iter().map(|extension| {
                let mut with_extension = base.as_os_str().to_owned();
                with_extension.push(extension);
                PathBuf::from(with_extension)
            }))
            .collect::<Vec<_>>()
    };
    let path = Path::new(program);
    if path.components().count() > 1 || path.is_absolute() {
        // Looking up a UNC path would send the user's credentials to the host
        // it names, as it would for the command picker.
        if !util::paths::is_local_absolute_path(path) {
            return CommandLookup::NotChecked;
        }
        for candidate in candidates(path) {
            if fs.is_file(&candidate).await {
                return CommandLookup::Found;
            }
        }
        return CommandLookup::Missing;
    }
    let Some(path_variable) = path_variable else {
        return CommandLookup::Missing;
    };
    for directory in std::env::split_paths(path_variable) {
        if !util::paths::is_local_absolute_path(&directory) {
            continue;
        }
        for candidate in candidates(&directory.join(path)) {
            if fs.is_file(&candidate).await {
                return CommandLookup::Found;
            }
        }
    }
    CommandLookup::Missing
}

fn missing_command_message(program: &str) -> SharedString {
    let name = Path::new(program)
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    let hint = match name.as_str() {
        "npx" | "npm" | "node" => "Node.js をインストールすると使えるようになります。",
        "uvx" | "uv" => "uv をインストールすると使えるようになります。",
        "docker" => "Docker をインストールすると使えるようになります。",
        _ => "",
    };
    format!(
        "コマンド「{program}」が見つかりません (PATH にも、指定した場所にもありません。コマンドは実行していません)。{hint}このまま保存する場合は、もう一度「保存」を押してください。"
    )
    .into()
}

/// Plain (editor-free) snapshot of the form's contents, so the validation /
/// build logic can be exercised without a GPUI context.
#[derive(Debug)]
struct McpServerFormValues {
    transport: McpTransport,
    original_id: Option<ContextServerId>,
    name: String,
    command: String,
    args: Vec<String>,
    url: String,
    timeout: String,
    oauth_client_id: String,
    oauth_client_secret: Option<String>,
    env: Vec<(String, String)>,
    headers: Vec<(String, String)>,
    remote: bool,
    enabled: bool,
    prompts: TemplatePrompts,
}

/// What a template asked for in each argument, environment variable and
/// header, index for index with the values they belong to.
#[derive(Debug, Default)]
struct TemplatePrompts {
    args: Vec<Option<String>>,
    env: Vec<Option<String>>,
    headers: Vec<Option<String>>,
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
    let (args, arg_prompts) = form
        .args
        .iter()
        .map(|row| (row.editor.read(cx).text(cx), row.added, row.prompt.clone()))
        .filter(|(text, added, _)| keeps_argument(text, *added))
        .map(|(text, _, prompt)| (text, prompt))
        .unzip();
    let values = McpServerFormValues {
        transport: form.transport,
        original_id: form.original_id.clone(),
        name: form.name.read(cx).text(cx),
        command: form.command.read(cx).text(cx),
        args,
        url: form.url.read(cx).text(cx),
        timeout: form.timeout.read(cx).text(cx),
        oauth_client_id: form.oauth_client_id.read(cx).text(cx),
        oauth_client_secret: form.oauth_client_secret.clone(),
        env: read_kv(&form.env, cx),
        headers: read_kv(&form.headers, cx),
        remote: form.remote,
        enabled: form.enabled,
        prompts: TemplatePrompts {
            args: arg_prompts,
            env: form.env.iter().map(|row| row.prompt.clone()).collect(),
            headers: form.headers.iter().map(|row| row.prompt.clone()).collect(),
        },
    };
    build_settings_from_values(&values)
}

/// A row added with 追加 and left blank is not an argument; an empty argument
/// the settings already had is kept, since a server may expect `""`.
fn keeps_argument(text: &str, added: bool) -> bool {
    !(added && text.trim().is_empty())
}

#[cfg(test)]
fn arguments_to_save(rows: impl IntoIterator<Item = (String, bool)>) -> Vec<String> {
    rows.into_iter()
        .filter(|(text, added)| keeps_argument(text, *added))
        .map(|(text, _)| text)
        .collect()
}

/// The first blank a template asked the user to fill that is still empty.
fn unfilled_template_input(values: &McpServerFormValues) -> Option<SharedString> {
    let unfilled_value = |rows: &[(String, String)], prompts: &[Option<String>], label: &str| {
        rows.iter()
            .zip(prompts)
            .find(|((_, value), prompt)| prompt.is_some() && value.trim().is_empty())
            .map(|((key, _), _)| format!("{label}「{}」の値を入力してください。", key.trim()))
    };
    let message = match values.transport {
        McpTransport::Stdio => values
            .args
            .iter()
            .zip(&values.prompts.args)
            .find_map(|(argument, prompt)| {
                prompt
                    .as_ref()
                    .filter(|_| argument.trim().is_empty())
                    .map(|prompt| format!("「{prompt}」を入力してください。"))
            })
            .or_else(|| unfilled_value(&values.env, &values.prompts.env, "環境変数")),
        McpTransport::Http => unfilled_value(&values.headers, &values.prompts.headers, "ヘッダー"),
    };
    message.map(SharedString::from)
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
    if let Some(error) = unfilled_template_input(values) {
        return Err(error);
    }

    let timeout = parse_timeout(&values.timeout)?;

    let content = match values.transport {
        McpTransport::Stdio => {
            let command = values.command.trim().to_string();
            if command.is_empty() {
                return Err("コマンドを入力してください。".into());
            }
            // Taken verbatim, surrounding spaces and empty arguments included,
            // so that editing a server writes back exactly what it was loaded
            // with.
            let args = values.args.clone();
            let env = collect_kv(&values.env, "環境変数")?;
            ContextServerSettingsContent::Stdio {
                enabled: values.enabled,
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
                client_secret: values.oauth_client_secret.clone(),
            });
            ContextServerSettingsContent::Http {
                enabled: values.enabled,
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
            // A row left empty is one added and never used; a value with no
            // name would otherwise vanish on save without a word.
            if value.is_empty() {
                continue;
            }
            return Err(format!(
                "{label}の名前が空の行があります。名前を入力するか、その行を削除してください。"
            )
            .into());
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
    use fs::{FakeFs, Fs as _};
    use gpui::TestAppContext;
    use serde_json::json;
    use util::path;

    fn values(transport: McpTransport) -> McpServerFormValues {
        let mut values = McpServerFormValues::empty(transport);
        values.name = "my-server".into();
        values
    }

    fn id(name: &str) -> ContextServerId {
        ContextServerId(name.into())
    }

    fn init_command_picker_test(cx: &mut TestAppContext) -> Arc<FakeFs> {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            let queue = crate::ProjectSettingsUpdateQueue::new(cx);
            cx.set_global(queue);
        });
        let fs = FakeFs::new(cx.executor());
        cx.update(|cx| <dyn fs::Fs>::set_global(fs.clone(), cx));
        fs
    }

    #[gpui::test]
    async fn command_picker_opens_in_the_commands_folder_attached_to_the_settings_window(
        cx: &mut TestAppContext,
    ) {
        let fs = init_command_picker_test(cx);
        fs.insert_tree(path!("/tools/bin"), json!({ "server": "" }))
            .await;
        let (settings_window, cx) = cx.add_window_view(|window, cx| {
            let mut settings_window = SettingsWindow::test(window, cx);
            let mut values = McpServerFormValues::empty(McpTransport::Stdio);
            values.command = path!("/tools/bin/server").into();
            settings_window.mcp_server_form =
                Some(McpServerForm::from_values(values, None, window, cx));
            settings_window
        });

        let settings_window_handle =
            settings_window.update_in(cx, |settings_window, window, cx| {
                pick_command_file(settings_window, window, cx);
                window.window_handle()
            });
        cx.run_until_parked();

        let (options, owner) = cx
            .pending_path_prompt()
            .expect("the command picker should be open");
        assert_eq!(owner, Some(settings_window_handle));
        assert_eq!(
            options.starting_directory,
            Some(PathBuf::from(path!("/tools/bin")))
        );
        assert!(options.files && !options.directories && !options.multiple);

        let picked = PathBuf::from(path!("/tools/bin/other-server"));
        cx.simulate_path_prompt_response({
            let picked = picked.clone();
            move |_options| Some(vec![picked])
        });
        cx.run_until_parked();

        let command = settings_window.read_with(cx, |settings_window, cx| {
            settings_window
                .mcp_server_form
                .as_ref()
                .map(|form| form.command.read(cx).text(cx))
        });
        assert_eq!(command, Some(picked.to_string_lossy().into_owned()));
    }

    #[gpui::test]
    async fn command_picker_starts_in_the_home_folder_without_a_usable_command(
        cx: &mut TestAppContext,
    ) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/tools"), json!({ "server": "" }))
            .await;
        let home = paths::home_dir().clone();

        for command in [
            "",
            "   ",
            "server",
            "bin/server",
            path!("/missing/bin/server"),
        ] {
            assert_eq!(
                command_picker_starting_directory(command, fs.as_ref()).await,
                home,
                "{command:?}"
            );
        }
        assert_eq!(
            command_picker_starting_directory(
                &format!("  {}  ", path!("/tools/server")),
                fs.as_ref()
            )
            .await,
            PathBuf::from(path!("/tools"))
        );

        #[cfg(windows)]
        {
            // The fake file system ignores the prefix and would report each of
            // these folders as present, so only the prefix keeps the picker
            // from looking at another host.
            assert!(fs.is_dir(Path::new(r"\\host\share\tools")).await);
            for command in [
                r"\\host\share\tools\server.exe",
                r"\\host@SSL\DavWWWRoot\tools\server.exe",
                r"\\?\UNC\host\share\tools\server.exe",
                r"C:tools\server.exe",
            ] {
                assert_eq!(
                    command_picker_starting_directory(command, fs.as_ref()).await,
                    home,
                    "{command:?}"
                );
            }
            // A mapped drive is a drive letter like any other, and its folder
            // is used as typed rather than resolved to the share behind it.
            assert_eq!(
                command_picker_starting_directory(r"Z:\tools\server.exe", fs.as_ref()).await,
                PathBuf::from(r"Z:\tools")
            );
        }
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
        values.args = vec!["--flag".into(), "value".into()];
        values.timeout = "30".into();
        // Empty values are kept, and a row left entirely empty is ignored.
        values.env = vec![
            ("KEY".into(), "VALUE".into()),
            ("EMPTY".into(), String::new()),
            ("   ".into(), String::new()),
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

    /// Regression: arguments used to be one space-separated string, so any
    /// argument containing a space was split apart on save.
    #[test]
    fn arguments_keep_spaces_quotes_and_backslashes() {
        let mut values = values(McpTransport::Stdio);
        values.command = r"C:\Program Files\Server\server.exe".into();
        values.args = vec![
            r"C:\Users\me\My Documents".into(),
            "--name=two words".into(),
            r#""quoted""#.into(),
            r"\\fileserver\share\".into(),
            "  padded  ".into(),
        ];

        let (_, _, content) = build_settings_from_values(&values).unwrap();
        let ContextServerSettingsContent::Stdio { command, .. } = content else {
            panic!("expected a stdio server");
        };
        assert_eq!(
            command.path,
            std::path::PathBuf::from(r"C:\Program Files\Server\server.exe")
        );
        assert_eq!(command.args, values.args);
    }

    /// Opening a server in the form and saving it without touching anything
    /// must write back exactly what the settings file said, including the
    /// values the form has no field for.
    #[test]
    fn editing_an_existing_server_writes_back_exactly_what_was_loaded() {
        let cases = [
            // The user's real server.
            r#"{
                "enabled": true,
                "command": "I:\\FITELnet-mcp\\.venv\\Scripts\\fitelnet-mcp.exe",
                "args": ["--stdio"],
                "timeout": 120
            }"#,
            r#"{
                "enabled": false,
                "remote": true,
                "command": "C:\\Program Files\\nodejs\\node.exe",
                "args": ["C:\\srv\\my server\\index.js", "--label", "two words", "\"quoted\""],
                "env": { "B": "2", "A": "1" }
            }"#,
            r#"{
                "url": "https://example.com/mcp",
                "headers": { "Authorization": "Bearer x" },
                "timeout": 30,
                "oauth": { "client_id": "id", "client_secret": "secret" }
            }"#,
        ];

        for json in cases {
            let content: ContextServerSettingsContent =
                serde_json::from_str(json).unwrap_or_else(|error| panic!("{json}: {error}"));
            let settings = ContextServerSettings::from(content.clone());
            let transport = match settings {
                ContextServerSettings::Http { .. } => McpTransport::Http,
                _ => McpTransport::Stdio,
            };
            let values =
                McpServerFormValues::from_existing(transport, Some((id("fitelnet"), settings)));

            let (saved_id, original_id, saved) = build_settings_from_values(&values).unwrap();
            assert_eq!(saved_id, id("fitelnet"));
            assert_eq!(original_id, Some(id("fitelnet")));
            assert_eq!(saved, content, "{json}");
        }
    }

    #[test]
    fn existing_arguments_load_one_per_row() {
        let settings = ContextServerSettings::Stdio {
            enabled: true,
            remote: false,
            command: ContextServerCommand {
                path: "npx".into(),
                args: vec!["-y".into(), r"C:\Users\me\My Documents".into()],
                env: None,
                timeout: None,
            },
        };
        let values =
            McpServerFormValues::from_existing(McpTransport::Stdio, Some((id("fs"), settings)));
        assert_eq!(
            values.args,
            vec!["-y".to_string(), r"C:\Users\me\My Documents".to_string()]
        );
    }

    fn imported(name: &str, definition: ImportedDefinition) -> ImportedServer {
        ImportedServer {
            name: name.into(),
            definition,
            enabled: true,
            ignored_keys: Vec::new(),
        }
    }

    #[test]
    fn only_blank_rows_the_user_added_are_dropped() {
        assert_eq!(
            arguments_to_save([
                ("--prefix".to_string(), false),
                (String::new(), false),
                ("   ".to_string(), true),
                ("value".to_string(), true),
            ]),
            vec!["--prefix".to_string(), String::new(), "value".to_string()]
        );
    }

    #[test]
    fn an_empty_argument_from_the_settings_survives_an_edit() {
        let content: ContextServerSettingsContent =
            serde_json::from_str(r#"{ "command": "npx", "args": ["--prefix", ""] }"#)
                .expect("valid server settings");
        let settings = ContextServerSettings::from(content.clone());
        let values =
            McpServerFormValues::from_existing(McpTransport::Stdio, Some((id("npx"), settings)));
        let (_, _, saved) = build_settings_from_values(&values).unwrap();
        assert_eq!(saved, content);
    }

    #[test]
    fn a_value_without_a_name_is_reported_rather_than_dropped() {
        let mut values = values(McpTransport::Stdio);
        values.command = "/bin/server".into();
        values.env = vec![("   ".into(), "secret".into())];
        assert_eq!(
            build_settings_from_values(&values).unwrap_err().as_ref(),
            "環境変数の名前が空の行があります。名前を入力するか、その行を削除してください。"
        );
    }

    #[test]
    fn quotes_inside_arguments_are_escaped_in_the_command_line() {
        assert_eq!(
            display_command_line("node", &[r#"say "hi""#.into()]),
            r#"node "say \"hi\"""#
        );
    }

    #[test]
    fn the_request_timeout_is_reported_in_japanese() {
        assert_eq!(
            server_error_text("Context server request timeout"),
            "MCP サーバーが応答しませんでした (タイムアウト)"
        );
        assert_eq!(
            server_error_text("failed to start: Context server request timeout"),
            "failed to start: MCP サーバーが応答しませんでした (タイムアウト)"
        );
        assert_eq!(
            server_error_text("expected value at line 1 column 1"),
            "expected value at line 1 column 1"
        );
    }

    #[test]
    fn a_pasted_server_that_zed_had_disabled_stays_disabled() {
        let mut server = imported_local("parked", "uvx");
        server.enabled = false;
        let (_, _, content) =
            build_settings_from_values(&McpServerFormValues::from_import(&server)).unwrap();
        assert!(matches!(
            content,
            ContextServerSettingsContent::Stdio { enabled: false, .. }
        ));
    }

    fn imported_local(name: &str, command: &str) -> ImportedServer {
        imported(
            name,
            ImportedDefinition::Local {
                command: command.into(),
                args: vec!["--stdio".into()],
                env: vec![("TOKEN".into(), "<YOUR_TOKEN>".into())],
                timeout: Some(90),
                remote: true,
            },
        )
    }

    #[test]
    fn a_pasted_server_fills_the_form_it_belongs_to() {
        let local = McpServerFormValues::from_import(&imported_local("fetch", "uvx"));
        assert_eq!(local.transport, McpTransport::Stdio);
        assert_eq!(local.original_id, None);
        assert_eq!(local.name, "fetch");
        assert_eq!(local.command, "uvx");
        assert_eq!(local.args, vec!["--stdio".to_string()]);
        assert_eq!(
            local.env,
            vec![("TOKEN".to_string(), "<YOUR_TOKEN>".to_string())]
        );
        assert_eq!(local.timeout, "90");
        assert!(local.remote);
        assert!(local.enabled);

        let remote = McpServerFormValues::from_import(&imported(
            "figma",
            ImportedDefinition::Remote {
                url: "https://mcp.figma.com/mcp".into(),
                headers: vec![("X-Test".into(), "1".into())],
                timeout: None,
                oauth_client_id: Some("client-id".into()),
                oauth_client_secret: Some("client-secret".into()),
            },
        ));
        assert_eq!(remote.transport, McpTransport::Http);
        assert_eq!(remote.url, "https://mcp.figma.com/mcp");
        assert_eq!(
            remote.headers,
            vec![("X-Test".to_string(), "1".to_string())]
        );
        assert_eq!(remote.timeout, "");
        assert_eq!(remote.oauth_client_id, "client-id");
        assert_eq!(remote.oauth_client_secret.as_deref(), Some("client-secret"));
    }

    #[test]
    fn adding_every_pasted_server_is_all_or_nothing() {
        let servers = vec![
            imported_local("fetch", "uvx"),
            imported_local("memory", "npx"),
        ];

        let entries = settings_for_imported_servers(&servers, &[id("other")]).unwrap();
        assert_eq!(
            entries
                .iter()
                .map(|(id, _)| id.0.to_string())
                .collect::<Vec<_>>(),
            vec!["fetch".to_string(), "memory".to_string()]
        );

        assert_eq!(
            settings_for_imported_servers(&servers, &[id("memory")])
                .unwrap_err()
                .as_ref(),
            "「memory」という名前のサーバーは既に存在します。フォームで開いて名前を変えてから追加してください。"
        );

        let broken = vec![
            imported_local("fetch", "uvx"),
            imported_local("blank", "  "),
        ];
        assert_eq!(
            settings_for_imported_servers(&broken, &[])
                .unwrap_err()
                .as_ref(),
            "「blank」: コマンドを入力してください。"
        );
    }

    #[test]
    fn shows_a_command_line_with_spaced_arguments_quoted() {
        assert_eq!(
            display_command_line(
                r"C:\Program Files\nodejs\node.exe",
                &["server.js".into(), "two words".into(), String::new()]
            ),
            r#""C:\Program Files\nodejs\node.exe" server.js "two words" """#
        );
        // The user's real server: a Windows path is shown exactly as it runs.
        assert_eq!(
            display_command_line(
                r"I:\FITELnet-mcp\.venv\Scripts\fitelnet-mcp.exe",
                &["--stdio".into()]
            ),
            r"I:\FITELnet-mcp\.venv\Scripts\fitelnet-mcp.exe --stdio"
        );
    }

    #[test]
    fn a_denied_project_server_is_shown_as_stopped_not_as_waiting() {
        assert_eq!(
            row_status(
                &ContextServerStatus::ApprovalRequired,
                Some(LaunchApprovalState::Pending)
            ),
            AiSettingItemStatus::ApprovalRequired
        );
        assert_eq!(
            row_status(
                &ContextServerStatus::ApprovalRequired,
                Some(LaunchApprovalState::Denied)
            ),
            AiSettingItemStatus::Stopped
        );
        assert_eq!(
            row_status(&ContextServerStatus::ApprovalRequired, None),
            AiSettingItemStatus::ApprovalRequired
        );
        assert_eq!(
            row_status(
                &ContextServerStatus::Running,
                Some(LaunchApprovalState::Approved)
            ),
            AiSettingItemStatus::Running
        );
    }

    #[test]
    fn a_disabled_project_server_still_shows_what_it_would_run() {
        assert_eq!(
            settings_target(&stdio_settings(None)),
            Some(LaunchTarget::Command {
                program: "/bin/server".into(),
                arguments: Vec::new(),
            })
        );
        assert_eq!(
            settings_target(&http_settings()),
            Some(LaunchTarget::Url("https://example.com/mcp".into()))
        );
        assert_eq!(
            settings_target(&ContextServerSettings::Extension {
                enabled: true,
                remote: false,
                settings: serde_json::json!({}),
            }),
            None
        );
    }

    #[test]
    fn import_note_names_what_was_left_behind() {
        assert_eq!(import_note(&[]), None);
        assert_eq!(
            import_note(&["cwd".into(), "envFile".into()]).as_deref(),
            Some("次の項目は Zed では使わないため読み込みませんでした: cwd, envFile")
        );
    }

    fn stdio_content(command: &str, args: &[&str]) -> ContextServerSettingsContent {
        ContextServerSettingsContent::Stdio {
            enabled: true,
            remote: false,
            command: ContextServerCommand {
                path: command.into(),
                args: args.iter().map(|arg| arg.to_string()).collect(),
                env: None,
                timeout: None,
            },
        }
    }

    fn servers(
        entries: &[(&str, ContextServerSettingsContent)],
    ) -> collections::HashMap<Arc<str>, ContextServerSettingsContent> {
        entries
            .iter()
            .map(|(name, content)| (Arc::from(*name), content.clone()))
            .collect()
    }

    #[test]
    fn a_project_server_edit_replaces_only_the_entry_it_was_opened_on() {
        let loaded = stdio_content("old-server", &["--stdio"]);
        let other = ContextServerSettingsContent::Http {
            enabled: true,
            url: "https://example.com/mcp".into(),
            headers: Default::default(),
            timeout: None,
            oauth: None,
        };
        let mut entries = servers(&[("shared", loaded.clone()), ("other", other.clone())]);
        let edited = stdio_content("new-server", &["--stdio", "--verbose"]);

        apply_project_server_edit(
            &mut entries,
            &id("shared"),
            &loaded,
            &id("shared"),
            edited.clone(),
        )
        .expect("the file still has what the form loaded");
        assert_eq!(
            entries,
            servers(&[("shared", edited.clone()), ("other", other.clone())])
        );

        apply_project_server_edit(
            &mut entries,
            &id("shared"),
            &edited,
            &id("renamed"),
            edited.clone(),
        )
        .expect("renaming to a free name");
        assert_eq!(
            entries,
            servers(&[("renamed", edited.clone()), ("other", other)])
        );
    }

    #[test]
    fn a_project_server_edit_is_refused_when_the_file_changed_underneath() {
        let loaded = stdio_content("old-server", &[]);
        let edited = stdio_content("new-server", &[]);

        // Rewritten by something else since the form opened, including into
        // another kind of server under the same name.
        for current in [
            stdio_content("planted-server", &[]),
            ContextServerSettingsContent::Extension {
                enabled: true,
                remote: false,
                settings: serde_json::json!({}),
            },
        ] {
            let mut entries = servers(&[("shared", current.clone())]);
            assert_eq!(
                apply_project_server_edit(
                    &mut entries,
                    &id("shared"),
                    &loaded,
                    &id("shared"),
                    edited.clone(),
                ),
                Err(SharedString::from(PROJECT_DEFINITION_CHANGED))
            );
            assert_eq!(entries, servers(&[("shared", current)]));
        }

        let mut entries = servers(&[]);
        assert!(
            apply_project_server_edit(&mut entries, &id("shared"), &loaded, &id("shared"), edited)
                .is_err(),
            "a removed entry is not brought back"
        );
        assert!(entries.is_empty());
    }

    #[test]
    fn a_project_server_cannot_be_renamed_over_another_one_in_its_file() {
        let loaded = stdio_content("old-server", &[]);
        let taken = stdio_content("taken-server", &[]);
        let mut entries = servers(&[("shared", loaded.clone()), ("taken", taken.clone())]);
        assert!(
            apply_project_server_edit(
                &mut entries,
                &id("shared"),
                &loaded,
                &id("taken"),
                stdio_content("new-server", &[]),
            )
            .is_err()
        );
        assert_eq!(entries, servers(&[("shared", loaded), ("taken", taken)]));
    }

    #[gpui::test]
    async fn editing_a_project_server_writes_the_project_file_and_not_the_users(
        cx: &mut TestAppContext,
    ) {
        let fs = init_command_picker_test(cx);
        fs.insert_tree(
            path!("/project"),
            json!({
                ".zed": {
                    "settings.json": r#"{
  // Shared with the team.
  "context_servers": {
    "shared": { "command": "old-server", "args": ["--stdio"] }
  }
}"#
                },
                "src": {}
            }),
        )
        .await;
        fs.insert_tree(path!("/tools"), json!({ "new-server": "" }))
            .await;
        let project = project::Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let multi_workspace = cx.add_window(|window, cx| {
            workspace::MultiWorkspace::test_new(project.clone(), window, cx)
        });
        cx.run_until_parked();
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project should have its worktree")
                .read(cx)
                .id()
        });

        let (settings_window, cx) = cx.add_window_view(|window, cx| {
            let mut settings_window = SettingsWindow::test(window, cx);
            settings_window.original_window = Some(multi_workspace);
            settings_window
        });
        settings_window.update_in(cx, |settings_window, window, cx| {
            let form = project_server_form(
                &id("shared"),
                worktree_id,
                path!("/project/.zed/settings.json").into(),
                window,
                cx,
            )
            .expect("the project's own entry opens in the form");
            form.command.update(cx, |editor, cx| {
                editor.set_text(path!("/tools/new-server"), window, cx);
            });
            push_mcp_server_form(settings_window, form, window, cx);
            save_mcp_server_form(settings_window, window, cx);
        });
        cx.run_until_parked();

        let form_error = settings_window.read_with(cx, |settings_window, _| {
            settings_window
                .mcp_server_form
                .as_ref()
                .and_then(|form| form.error.clone())
        });
        assert_eq!(form_error, None);
        settings_window.read_with(cx, |settings_window, _| {
            assert!(
                settings_window.mcp_server_form.is_none()
                    && settings_window.sub_page_stack.is_empty(),
                "the form closes once the project file is saved"
            );
        });
        let project_file = fs
            .load(path!("/project/.zed/settings.json").as_ref())
            .await
            .expect("the project file is still there");
        assert!(
            project_file.contains(&serde_json::to_string(path!("/tools/new-server")).unwrap()),
            "{project_file}"
        );
        assert!(!project_file.contains("old-server"), "{project_file}");
        assert!(
            project_file.contains("// Shared with the team."),
            "the rest of the file is left as it was: {project_file}"
        );

        let user_file = fs.load(paths::settings_file()).await.unwrap_or_default();
        assert!(!user_file.contains("shared"), "{user_file}");
        let user_settings_have_it = cx.update(|_, cx| {
            cx.global::<settings::SettingsStore>()
                .raw_user_settings()
                .is_some_and(|settings| {
                    settings
                        .content
                        .project
                        .context_servers
                        .contains_key("shared")
                })
        });
        assert!(!user_settings_have_it);
    }

    /// The command lookup makes the save wait; leaving the form meanwhile (the
    /// breadcrumb keeps the form around) must not close whatever page is on
    /// top by then, nor write what the user walked away from.
    #[gpui::test]
    async fn a_save_that_waited_finishes_only_on_the_form_it_started_from(cx: &mut TestAppContext) {
        let fs = init_command_picker_test(cx);
        fs.insert_tree(path!("/tools"), json!({ "new-server": "" }))
            .await;
        let (settings_window, cx) =
            cx.add_window_view(|window, cx| SettingsWindow::test(window, cx));
        let open_filled_form = |name: &'static str, cx: &mut gpui::VisualTestContext| {
            settings_window.update_in(cx, |settings_window, window, cx| {
                let mut values = McpServerFormValues::empty(McpTransport::Stdio);
                values.name = name.into();
                values.command = path!("/tools/new-server").into();
                let form = McpServerForm::from_values(values, None, window, cx);
                push_mcp_server_form(settings_window, form, window, cx);
                save_mcp_server_form(settings_window, window, cx);
            });
        };
        let user_file = |fs: Arc<FakeFs>| async move {
            fs.load(paths::settings_file()).await.unwrap_or_default()
        };

        open_filled_form("stayed", cx);
        cx.run_until_parked();
        settings_window.read_with(cx, |settings_window, _| {
            assert!(settings_window.sub_page_stack.is_empty());
            assert!(settings_window.mcp_server_form.is_none());
        });
        assert!(user_file(fs.clone()).await.contains("stayed"));

        open_filled_form("left", cx);
        settings_window.update_in(cx, |settings_window, window, cx| {
            settings_window.pop_sub_page(window, cx);
            open_mcp_templates_page(settings_window, window, cx);
        });
        cx.run_until_parked();
        settings_window.read_with(cx, |settings_window, _| {
            assert_eq!(
                settings_window
                    .sub_page_stack
                    .iter()
                    .map(|page| page.link.title.clone())
                    .collect::<Vec<_>>(),
                vec![SharedString::from("テンプレートから MCP サーバーを追加")],
                "the page opened meanwhile stays open"
            );
        });
        assert!(!user_file(fs.clone()).await.contains("left"));
    }

    #[gpui::test]
    async fn looks_up_a_command_without_running_it(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/node"),
            json!({ "npx.cmd": "", "node.exe": "", "tool": "" }),
        )
        .await;
        let path_variable = std::env::join_paths([path!("/missing"), path!("/node")])
            .expect("joinable")
            .into_string()
            .expect("unicode");
        let extensions = vec![".exe".to_string(), ".cmd".to_string()];
        let look_up = |program: &'static str, path_variable: Option<String>| {
            let fs = fs.clone();
            let extensions = extensions.clone();
            async move {
                look_up_command(program, path_variable.as_deref(), &extensions, fs.as_ref()).await
            }
        };

        assert_eq!(
            look_up("npx", Some(path_variable.clone())).await,
            CommandLookup::Found
        );
        assert_eq!(
            look_up("tool", Some(path_variable.clone())).await,
            CommandLookup::Found
        );
        assert_eq!(
            look_up("uvx", Some(path_variable.clone())).await,
            CommandLookup::Missing
        );
        assert_eq!(look_up("npx", None).await, CommandLookup::Missing);
        assert_eq!(
            look_up(path!("/node/node"), None).await,
            CommandLookup::Found
        );
        assert_eq!(
            look_up(path!("/node/python"), None).await,
            CommandLookup::Missing
        );
        assert_eq!(
            look_up("bin/server", Some(path_variable.clone())).await,
            CommandLookup::NotChecked
        );
        assert_eq!(look_up("  ", None).await, CommandLookup::NotChecked);
        #[cfg(windows)]
        assert_eq!(
            look_up(r"\\host\share\server.exe", None).await,
            CommandLookup::NotChecked
        );
    }

    #[test]
    fn only_a_server_that_starts_here_has_its_command_looked_up() {
        let mut content = stdio_content("npx", &[]);
        assert_eq!(
            command_to_look_up(&content, false).map(|command| command.program),
            Some("npx".to_string())
        );
        if let ContextServerSettingsContent::Stdio {
            remote, command, ..
        } = &mut content
        {
            *remote = true;
            command.env = Some(
                [("Path".to_string(), "C:\\node".to_string())]
                    .into_iter()
                    .collect(),
            );
        }
        assert!(
            command_to_look_up(&content, true).is_none(),
            "it starts on the SSH host"
        );
        let local = command_to_look_up(&content, false).expect("it starts here");
        assert_eq!(
            local.path_variable,
            cfg!(windows).then(|| "C:\\node".to_string()),
            "Windows names are case-insensitive"
        );
        assert!(
            command_to_look_up(
                &ContextServerSettingsContent::Http {
                    enabled: true,
                    url: "https://example.com/mcp".into(),
                    headers: Default::default(),
                    timeout: None,
                    oauth: None,
                },
                false
            )
            .is_none()
        );
    }

    #[test]
    fn a_missing_command_says_what_would_provide_it() {
        assert!(missing_command_message("npx").contains("Node.js"));
        assert!(missing_command_message("uvx.exe").contains("uv をインストール"));
        assert!(!missing_command_message("my-server").contains("インストール"));
    }
}
