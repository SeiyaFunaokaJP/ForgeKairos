use context_server::ContextServerId;
use gpui::{
    AnyElement, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Subscription,
    WeakEntity, WindowHandle,
};
use project::Project;
use project::project_launch_approvals::{
    CHANGED_SINCE_SHOWN_MESSAGE, ExecutionHost, LaunchApprovalDetails, LaunchApprovalKey,
    LaunchDecision, LaunchDefinitionFile, LaunchHost, LaunchKind, LaunchTarget,
    ProjectLaunchApprovals, ProjectLaunchApprovalsEvent, UnboundWorktreeContent,
    escape_for_display,
};
use ui::{Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

/// Shown instead of letting a decision be made that could not be stored.
pub(super) const DECISION_BLOCKED_HINT: &str = "承認の記録を読み込めないため、決定できません。";

/// Marks the notifications shown here, one per pending launch.
struct PendingLaunchNotification;

/// Shows a notification in every workspace of the project a pending launch
/// came from. The notification only leads to where the definition can be
/// read in full: allowing a launch from a toast, without seeing the command,
/// is exactly what ADR-F42 exists to prevent.
pub(crate) fn init(cx: &mut App) {
    let approvals = ProjectLaunchApprovals::global(cx);
    cx.subscribe(
        &approvals,
        |_, event: &ProjectLaunchApprovalsEvent, cx| match event {
            ProjectLaunchApprovalsEvent::PendingLaunchAppeared(details) => {
                show_pending_launch(details, false, cx);
            }
            // The old key can no longer be decided; the notification has to
            // lead to what would run now.
            ProjectLaunchApprovalsEvent::PendingLaunchReplaced { old, new } => {
                dismiss_pending_launch(old, cx);
                show_pending_launch(new, true, cx);
            }
            ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(key)
            | ProjectLaunchApprovalsEvent::LaunchDecided { key, .. }
            | ProjectLaunchApprovalsEvent::LaunchApprovalRevoked(key) => {
                dismiss_pending_launch(key, cx);
            }
        },
    )
    .detach();
}

fn notification_id(key: &LaunchApprovalKey) -> NotificationId {
    NotificationId::composite::<PendingLaunchNotification>(SharedString::from(key.to_string()))
}

fn multi_workspace_windows(cx: &App) -> Vec<WindowHandle<MultiWorkspace>> {
    cx.windows()
        .into_iter()
        .filter_map(|window| window.downcast::<MultiWorkspace>())
        .collect()
}

fn workspaces_owning(
    details: &LaunchApprovalDetails,
    cx: &App,
) -> Vec<(WindowHandle<MultiWorkspace>, Entity<Workspace>)> {
    let mut owners = Vec::new();
    for window in multi_workspace_windows(cx) {
        let Ok(multi_workspace) = window.read(cx) else {
            continue;
        };
        for workspace in multi_workspace.workspaces() {
            if project_owns_launch(workspace.read(cx).project().read(cx), details, cx) {
                owners.push((window, workspace.clone()));
            }
        }
    }
    owners
}

fn project_owns_launch(project: &Project, details: &LaunchApprovalDetails, cx: &App) -> bool {
    let host = project
        .remote_connection_options(cx)
        .map(|options| LaunchHost::from_remote_connection(&options))
        .unwrap_or(LaunchHost::ThisMachine);
    host == details.host
        && project
            .worktrees(cx)
            .any(|worktree| *worktree.read(cx).abs_path() == *details.worktree_abs_path)
}

/// `changed` marks a launch whose definition changed while it waited, which
/// the user may already have looked at in its earlier form.
fn show_pending_launch(details: &LaunchApprovalDetails, changed: bool, cx: &mut App) {
    let id = notification_id(&details.key);
    for (window, workspace) in workspaces_owning(details, cx) {
        let details = details.clone();
        let weak_workspace = workspace.downgrade();
        workspace.update(cx, |workspace, cx| {
            workspace.show_notification(id.clone(), cx, |cx| {
                cx.new(|cx| {
                    pending_launch_notification(details, changed, window, weak_workspace, cx)
                })
            });
        });
    }
}

fn dismiss_pending_launch(key: &LaunchApprovalKey, cx: &mut App) {
    let id = notification_id(key);
    let workspaces = multi_workspace_windows(cx)
        .into_iter()
        .filter_map(|window| {
            window
                .read(cx)
                .ok()
                .map(|multi_workspace| multi_workspace.workspaces().cloned().collect::<Vec<_>>())
        })
        .flatten()
        .collect::<Vec<_>>();
    for workspace in workspaces {
        workspace.update(cx, |workspace, cx| workspace.dismiss_notification(&id, cx));
    }
}

fn pending_launch_notification(
    details: LaunchApprovalDetails,
    changed: bool,
    window_handle: WindowHandle<MultiWorkspace>,
    workspace: WeakEntity<Workspace>,
    cx: &mut App,
) -> MessageNotification {
    let lines = pending_launch_lines(details.kind, &details.name, changed);
    MessageNotification::new_from_builder(cx, move |_window, _cx| {
        v_flex()
            .gap_0p5()
            .children(lines.iter().map(|line| Label::new(line.clone())))
            .into_any_element()
    })
    .primary_message("確認する")
    .primary_on_click(move |window, cx| match details.kind {
        LaunchKind::ContextServer => {
            show_context_server_in_settings(
                ContextServerId(details.name.to_string().into()),
                window_handle,
                workspace.clone(),
                cx,
            );
        }
        LaunchKind::LanguageServerBinary | LaunchKind::ExternalFormatter => {
            let details = details.clone();
            workspace
                .update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        LaunchApprovalDialog::new(details, window, cx)
                    });
                })
                .log_err();
        }
    })
    .show_suppress_button(false)
}

/// The notification lives in the workspace that owns the definition, which
/// need not be the window's active one by the time the settings open.
fn show_context_server_in_settings(
    server_id: ContextServerId,
    window_handle: WindowHandle<MultiWorkspace>,
    owning_workspace: WeakEntity<Workspace>,
    cx: &mut App,
) {
    crate::open_settings_editor_with(
        Some(window_handle),
        cx,
        move |settings_window, window, cx| {
            super::show_context_server(
                settings_window,
                server_id,
                Some(owning_workspace),
                window,
                cx,
            );
        },
    );
}

/// The notification's lines: what asks, then what it would start on a line
/// of its own, so that a long name never leaves a lone character dangling.
fn pending_launch_lines(kind: LaunchKind, name: &str, changed: bool) -> Vec<String> {
    let mut lines = vec![
        "このプロジェクトの設定が起動を求めています".to_string(),
        launch_title(kind, name),
    ];
    if changed {
        lines.push(CHANGED_SINCE_SHOWN_MESSAGE.to_string());
    }
    lines
}

/// Names come from the project's settings: control, zero-width and
/// direction-changing characters in them are spelled out rather than obeyed.
fn launch_title(kind: LaunchKind, name: &str) -> String {
    let name = escape_for_display(name);
    match kind {
        LaunchKind::ContextServer => format!("MCP サーバー「{name}」"),
        LaunchKind::LanguageServerBinary => format!("言語サーバー「{name}」の実行ファイル"),
        LaunchKind::ExternalFormatter => format!("外部フォーマッター「{name}」"),
    }
}

pub(super) fn definition_file_label(file: &LaunchDefinitionFile, host: &LaunchHost) -> String {
    match (file, host) {
        (LaunchDefinitionFile::Worktree(path), LaunchHost::ThisMachine) => path.to_string(),
        (LaunchDefinitionFile::Worktree(path), LaunchHost::Remote { display_name, .. }) => {
            format!("{path} ({display_name})")
        }
        (LaunchDefinitionFile::RemoteServerSettings, LaunchHost::Remote { display_name, .. }) => {
            format!("{display_name} のサーバー設定")
        }
        (LaunchDefinitionFile::RemoteServerSettings, LaunchHost::ThisMachine) => {
            "接続先のサーバー設定".to_string()
        }
        (LaunchDefinitionFile::ResolvedOnRemoteHost, LaunchHost::Remote { display_name, .. }) => {
            format!("{display_name} で拡張機能が決めたコマンド (設定ファイルはありません)")
        }
        (LaunchDefinitionFile::ResolvedOnRemoteHost, LaunchHost::ThisMachine) => {
            "接続先で拡張機能が決めたコマンド (設定ファイルはありません)".to_string()
        }
    }
}

fn execution_host_label(host: &ExecutionHost) -> String {
    match host {
        ExecutionHost::ThisMachine => "この PC".to_string(),
        ExecutionHost::Remote { display_name } => format!("接続先 ({display_name})"),
    }
}

/// The program, then every argument in a box of its own: no quoting is needed
/// to tell where one argument ends, so a Windows path reads exactly as it
/// runs. Each piece is escaped separately.
pub(super) fn render_target_rows(target: &LaunchTarget, cx: &App) -> AnyElement {
    match target {
        LaunchTarget::Url(url) => {
            render_detail_row("URL", url.to_string(), true, cx).into_any_element()
        }
        LaunchTarget::Command { program, arguments } => v_flex()
            .w_full()
            .min_w_0()
            .gap_0p5()
            .child(render_detail_row("コマンド", program.to_string(), true, cx))
            .when(!arguments.is_empty(), |this| {
                this.child(render_argument_row(arguments, cx))
            })
            .into_any_element(),
    }
}

fn render_argument_row(arguments: &[SharedString], cx: &App) -> impl IntoElement {
    let border_color = cx.theme().colors().border_variant;
    h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_start()
        .child(
            div().w(rems(5.5)).flex_none().child(
                Label::new("引数")
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            ),
        )
        .child(
            h_flex()
                .min_w_0()
                .flex_1()
                .flex_wrap()
                .gap_1()
                .children(arguments.iter().map(|argument| {
                    // An empty argument still occupies a place in the list.
                    let (text, color) = if argument.is_empty() {
                        ("\"\"".to_string(), Color::Muted)
                    } else {
                        (escape_for_display(argument).into_owned(), Color::Default)
                    };
                    div()
                        .px_1()
                        .rounded_sm()
                        .border_1()
                        .border_color(border_color)
                        .child(
                            Label::new(text)
                                .size(LabelSize::Small)
                                .color(color)
                                .buffer_font(cx),
                        )
                })),
        )
}

/// Values of environment variables and headers often carry tokens, so only
/// the names are ever put on screen.
fn names_without_values(names: &[SharedString]) -> Option<String> {
    (!names.is_empty()).then(|| {
        format!(
            "{} (値は表示しません)",
            names
                .iter()
                .map(|name| name.as_ref())
                .collect::<Vec<&str>>()
                .join(", ")
        )
    })
}

/// Variables that decide which code the program loads (`PATH`,
/// `NODE_OPTIONS`, …) are shown with their values, since judging the launch
/// needs them; every other value may be a token and stays hidden.
fn environment_lines(
    names: &[SharedString],
    code_loading: &[(SharedString, SharedString)],
) -> (Vec<String>, Option<String>) {
    let shown = code_loading
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect();
    let hidden = names
        .iter()
        .filter(|name| {
            !code_loading
                .iter()
                .any(|(shown_name, _)| shown_name == *name)
        })
        .cloned()
        .collect::<Vec<_>>();
    (shown, names_without_values(&hidden))
}

/// The files the approval is bound to by their contents (the program and an
/// interpreter's script): changing them asks again, but what they do still
/// has to be read by the user.
fn bound_files_notice(files: &[SharedString]) -> Option<String> {
    (!files.is_empty()).then(|| {
        format!(
            "中身を承認に含めるファイル: {}。中身が変わるともう一度確認を求めますが、コマンドラインからは何をするかわからないため、中身も確認してから許可してください。",
            files
                .iter()
                .map(|file| file.as_ref())
                .collect::<Vec<&str>>()
                .join(", ")
        )
    })
}

const NOT_RECHECKED: &str = "後から中身が変わっても再確認されません。";

/// Unlike a bound file, this content is not part of what gets approved.
fn unbound_content_warning(content: &UnboundWorktreeContent) -> String {
    match content {
        UnboundWorktreeContent::WorkingDirectory {
            program,
            working_directory,
        } => format!(
            "{program} はプロジェクトのフォルダー {working_directory} で起動し、そこにあるコードを読み込む可能性があります。{NOT_RECHECKED}"
        ),
        UnboundWorktreeContent::File(path) => format!(
            "{path} はプロジェクト内のファイルですが、中身は承認に含まれません。{NOT_RECHECKED}"
        ),
        UnboundWorktreeContent::Directory(path) => {
            format!("フォルダー {path} の中身は承認に含まれません。{NOT_RECHECKED}")
        }
        UnboundWorktreeContent::ArgumentFile(path) => {
            format!("引数ファイル {path} の中身は承認に含まれません。{NOT_RECHECKED}")
        }
        UnboundWorktreeContent::Wrapper(program) => {
            format!("{program} は別のプログラムを起動します。中身は承認に含まれません。")
        }
        UnboundWorktreeContent::RemoteFile(path) => format!(
            "接続先のファイル {path} の中身はここから確かめられないため、承認に含まれません。{NOT_RECHECKED}"
        ),
        // Always comes with a session-only approval, which the details
        // already say on a line of their own.
        UnboundWorktreeContent::Unreadable(path) => {
            format!("{path} を読めないため、中身を確かめられません。")
        }
    }
}

const SESSION_ONLY_NOTICE: &str = "この承認は Zed を終了するまで有効です。";

fn replaced_user_entry_warning(kind: LaunchKind) -> &'static str {
    match kind {
        LaunchKind::ContextServer => "ユーザー設定の同名サーバーを置き換えています",
        LaunchKind::LanguageServerBinary => {
            "ユーザー設定の同じ言語サーバーの実行ファイル指定を置き換えています"
        }
        LaunchKind::ExternalFormatter => "ユーザー設定のフォーマッター指定を置き換えています",
    }
}

/// Everything a decision about a launch should be made on, shared by the MCP
/// servers page and the dialog for language servers and formatters.
pub(super) fn render_launch_details(details: &LaunchApprovalDetails, cx: &App) -> AnyElement {
    let (shown_environment, hidden_environment) = environment_lines(
        &details.environment_variable_names,
        &details.code_loading_environment,
    );
    // Only the first environment row carries the label; the rest line up
    // under it.
    let rows = shown_environment
        .into_iter()
        .map(|line| (line, true))
        .chain(hidden_environment.map(|names| (names, false)))
        .enumerate()
        .map(|(index, (line, monospace))| {
            (if index == 0 { "環境変数" } else { "" }, line, monospace)
        })
        .chain(names_without_values(&details.header_names).map(|names| ("ヘッダー", names, false)))
        .chain(std::iter::once((
            "実行ホスト",
            execution_host_label(&details.execution_host),
            false,
        )))
        .collect::<Vec<_>>();

    v_flex()
        .w_full()
        .min_w_0()
        .gap_0p5()
        .child(render_detail_row(
            "定義元",
            definition_file_label(&details.definition_file, &details.host),
            true,
            cx,
        ))
        .child(render_target_rows(&details.target, cx))
        .children(
            rows.into_iter()
                .map(|(label, value, monospace)| render_detail_row(label, value, monospace, cx)),
        )
        .children(details.unbound_worktree_content.iter().map(|content| {
            render_notice(
                IconName::Warning,
                Color::Warning,
                unbound_content_warning(content),
            )
        }))
        .when_some(
            bound_files_notice(&details.referenced_worktree_files),
            |this, warning| this.child(render_notice(IconName::Warning, Color::Warning, warning)),
        )
        .when(details.approval_is_session_only, |this| {
            this.child(render_notice(
                IconName::Info,
                Color::Muted,
                SESSION_ONLY_NOTICE.to_string(),
            ))
        })
        .when(details.replaces_user_entry, |this| {
            this.child(render_notice(
                IconName::Warning,
                Color::Warning,
                replaced_user_entry_warning(details.kind).to_string(),
            ))
        })
        .when_some(details.store_error.clone(), |this, error| {
            this.child(render_notice(
                IconName::XCircle,
                Color::Error,
                format!("承認の記録を読み込めないため、起動しません: {error}"),
            ))
        })
        .into_any_element()
}

pub(super) fn render_detail_row(
    label: &'static str,
    value: String,
    monospace: bool,
    cx: &App,
) -> impl IntoElement {
    // Paths, variables and names here come from the project: nothing in them
    // may hide or reorder what is shown.
    let value = Label::new(escape_for_display(&value).into_owned()).size(LabelSize::Small);
    let value = if monospace {
        value.buffer_font(cx)
    } else {
        value
    };
    h_flex()
        .w_full()
        .min_w_0()
        .gap_2()
        .items_start()
        .child(
            div()
                .w(rems(5.5))
                .flex_none()
                .child(Label::new(label).size(LabelSize::Small).color(Color::Muted)),
        )
        .child(div().min_w_0().flex_1().child(value))
}

pub(super) fn render_notice(icon: IconName, color: Color, message: String) -> impl IntoElement {
    h_flex()
        .w_full()
        .min_w_0()
        .gap_1p5()
        .items_start()
        .child(Icon::new(icon).size(IconSize::XSmall).color(color))
        .child(
            div().min_w_0().flex_1().child(
                Label::new(escape_for_display(&message).into_owned())
                    .size(LabelSize::Small)
                    .color(color),
            ),
        )
}

/// Asks about a language server binary or an external formatter that a
/// project's settings want to run. MCP servers are decided on their settings
/// page instead, next to everything else about them.
struct LaunchApprovalDialog {
    details: LaunchApprovalDetails,
    focus_handle: FocusHandle,
    deciding: bool,
    /// The definition changed while the dialog was open and `details` now
    /// shows the new one.
    changed: bool,
    error: Option<SharedString>,
    _approvals_subscription: Option<Subscription>,
}

impl LaunchApprovalDialog {
    fn new(details: LaunchApprovalDetails, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window, cx);
        let approvals_subscription = ProjectLaunchApprovals::try_global(cx).map(|approvals| {
            cx.subscribe(
                &approvals,
                |this: &mut Self, _, event: &ProjectLaunchApprovalsEvent, cx| {
                    this.follow(event, cx);
                },
            )
        });
        Self {
            details,
            focus_handle,
            deciding: false,
            changed: false,
            error: None,
            _approvals_subscription: approvals_subscription,
        }
    }

    /// Keeps the dialog on the launch that is actually pending: a decision
    /// must never be made on a definition that is no longer the one shown.
    fn follow(&mut self, event: &ProjectLaunchApprovalsEvent, cx: &mut Context<Self>) {
        match event {
            ProjectLaunchApprovalsEvent::PendingLaunchReplaced { old, new }
                if *old == self.details.key =>
            {
                self.details = new.clone();
                self.changed = true;
                self.error = None;
                cx.notify();
            }
            ProjectLaunchApprovalsEvent::PendingLaunchWithdrawn(key)
            | ProjectLaunchApprovalsEvent::LaunchDecided { key, .. }
                if *key == self.details.key =>
            {
                cx.emit(DismissEvent);
            }
            _ => {}
        }
    }

    /// `shown` is the key the clicked button was rendered with, which may
    /// already be stale if the definition changed in between.
    fn decide(
        &mut self,
        shown: LaunchApprovalKey,
        decision: LaunchDecision,
        cx: &mut Context<Self>,
    ) {
        if self.deciding || self.details.store_error.is_some() {
            return;
        }
        if shown != self.details.key {
            self.error = Some(CHANGED_SINCE_SHOWN_MESSAGE.into());
            cx.notify();
            return;
        }
        let Some(approvals) = ProjectLaunchApprovals::try_global(cx) else {
            self.error = Some("承認の記録を扱えないため、決定を保存できません。".into());
            cx.notify();
            return;
        };
        let key = self.details.key.clone();
        let task = approvals.update(cx, |approvals, cx| match decision {
            LaunchDecision::Approved => approvals.approve_pending(&key, cx),
            LaunchDecision::Denied => approvals.deny_pending(&key, cx),
        });
        self.deciding = true;
        self.error = None;
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |this, cx| {
                this.deciding = false;
                match result {
                    Ok(()) => cx.emit(DismissEvent),
                    Err(error) => {
                        this.error = Some(format!("決定を保存できませんでした: {error}").into());
                        cx.notify();
                    }
                }
            })
            .log_err();
        })
        .detach();
        cx.notify();
    }
}

impl EventEmitter<DismissEvent> for LaunchApprovalDialog {}

impl Focusable for LaunchApprovalDialog {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl ModalView for LaunchApprovalDialog {}

impl Render for LaunchApprovalDialog {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let blocked = self.details.store_error.is_some();
        let deny_key = self.details.key.clone();
        let allow_key = self.details.key.clone();
        v_flex()
            .key_context("LaunchApprovalDialog")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|_, _: &menu::Cancel, _window, cx| cx.emit(DismissEvent)))
            .elevation_3(cx)
            .w(rems(34.))
            .p_4()
            .gap_3()
            .child(
                Headline::new(launch_title(self.details.kind, &self.details.name))
                    .size(HeadlineSize::Small),
            )
            .child(
                Label::new(
                    "このプロジェクトの設定が、次の内容でプログラムを起動しようとしています。内容を確認してから選んでください。",
                )
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.changed, |this| {
                this.child(render_notice(
                    IconName::Warning,
                    Color::Warning,
                    CHANGED_SINCE_SHOWN_MESSAGE.to_string(),
                ))
            })
            // However long the command line, the buttons stay on screen.
            .child(
                div()
                    .id("launch-approval-details")
                    .max_h(vh(0.5, window))
                    .overflow_y_scroll()
                    .child(render_launch_details(&self.details, cx)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(render_notice(
                    IconName::XCircle,
                    Color::Error,
                    error.to_string(),
                ))
            })
            .child(
                h_flex()
                    .w_full()
                    .gap_2()
                    .justify_end()
                    .child(
                        Button::new("launch-approval-deny", "許可しない")
                            .style(ButtonStyle::Subtle)
                            .disabled(self.deciding || blocked)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.decide(deny_key.clone(), LaunchDecision::Denied, cx);
                            })),
                    )
                    .child(
                        Button::new("launch-approval-allow", "許可して起動")
                            .style(ButtonStyle::Filled)
                            .disabled(self.deciding || blocked)
                            .when(blocked, |button| {
                                button.tooltip(Tooltip::text(DECISION_BLOCKED_HINT))
                            })
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.decide(allow_key.clone(), LaunchDecision::Approved, cx);
                            })),
                    ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;
    use project::{FakeFs, project_launch_approvals::LaunchApprovalKey};
    use serde_json::json;
    use util::path;

    #[test]
    fn names_what_the_project_asks_to_launch() {
        assert_eq!(
            pending_launch_lines(LaunchKind::ContextServer, "github", false),
            vec![
                "このプロジェクトの設定が起動を求めています".to_string(),
                "MCP サーバー「github」".to_string(),
            ]
        );
        assert_eq!(
            pending_launch_lines(LaunchKind::ExternalFormatter, "prettier", true),
            vec![
                "このプロジェクトの設定が起動を求めています".to_string(),
                "外部フォーマッター「prettier」".to_string(),
                CHANGED_SINCE_SHOWN_MESSAGE.to_string(),
            ]
        );
        assert_eq!(
            launch_title(LaunchKind::LanguageServerBinary, "rust-analyzer"),
            "言語サーバー「rust-analyzer」の実行ファイル"
        );
    }

    #[test]
    fn names_the_definition_and_the_host_it_comes_from() {
        let remote = LaunchHost::Remote {
            identity: "ssh://me@build".into(),
            display_name: "me@build".into(),
        };
        let worktree_file = LaunchDefinitionFile::Worktree("/work/app/.zed/settings.json".into());
        assert_eq!(
            definition_file_label(&worktree_file, &LaunchHost::ThisMachine),
            "/work/app/.zed/settings.json"
        );
        assert_eq!(
            definition_file_label(&worktree_file, &remote),
            "/work/app/.zed/settings.json (me@build)"
        );
        assert_eq!(
            definition_file_label(&LaunchDefinitionFile::RemoteServerSettings, &remote),
            "me@build のサーバー設定"
        );
        assert_eq!(
            definition_file_label(&LaunchDefinitionFile::ResolvedOnRemoteHost, &remote),
            "me@build で拡張機能が決めたコマンド (設定ファイルはありません)"
        );
        assert_eq!(execution_host_label(&ExecutionHost::ThisMachine), "この PC");
        assert_eq!(
            execution_host_label(&ExecutionHost::Remote {
                display_name: "me@build".into()
            }),
            "接続先 (me@build)"
        );
    }

    #[test]
    fn shows_code_loading_variables_with_their_values_and_hides_the_rest() {
        assert_eq!(environment_lines(&[], &[]), (Vec::new(), None));
        assert_eq!(
            environment_lines(
                &["PATH".into(), "GITHUB_TOKEN".into(), "NODE_OPTIONS".into()],
                &[
                    ("PATH".into(), "/opt/tools/bin".into()),
                    ("NODE_OPTIONS".into(), "--require ./hook.js".into()),
                ],
            ),
            (
                vec![
                    "PATH=/opt/tools/bin".to_string(),
                    "NODE_OPTIONS=--require ./hook.js".to_string(),
                ],
                Some("GITHUB_TOKEN (値は表示しません)".to_string()),
            )
        );
    }

    #[test]
    fn warns_about_project_code_the_approval_cannot_pin_down() {
        for (content, expected) in [
            (
                UnboundWorktreeContent::WorkingDirectory {
                    program: "node".into(),
                    working_directory: "/work/app".into(),
                },
                "node はプロジェクトのフォルダー /work/app で起動し、そこにあるコードを読み込む可能性があります。後から中身が変わっても再確認されません。",
            ),
            (
                UnboundWorktreeContent::File("/work/app/config.toml".into()),
                "/work/app/config.toml はプロジェクト内のファイルですが、中身は承認に含まれません。後から中身が変わっても再確認されません。",
            ),
            (
                UnboundWorktreeContent::Directory("/work/app/scripts".into()),
                "フォルダー /work/app/scripts の中身は承認に含まれません。後から中身が変わっても再確認されません。",
            ),
            (
                UnboundWorktreeContent::ArgumentFile("/work/app/args.txt".into()),
                "引数ファイル /work/app/args.txt の中身は承認に含まれません。後から中身が変わっても再確認されません。",
            ),
            (
                UnboundWorktreeContent::Wrapper("direnv".into()),
                "direnv は別のプログラムを起動します。中身は承認に含まれません。",
            ),
            (
                UnboundWorktreeContent::RemoteFile("/srv/app/server.py".into()),
                "接続先のファイル /srv/app/server.py の中身はここから確かめられないため、承認に含まれません。後から中身が変わっても再確認されません。",
            ),
            (
                UnboundWorktreeContent::Unreadable("/work/app/tool".into()),
                "/work/app/tool を読めないため、中身を確かめられません。",
            ),
        ] {
            assert_eq!(unbound_content_warning(&content), expected);
        }
        assert_eq!(bound_files_notice(&[]), None);
        let notice = bound_files_notice(&["scripts/server.py".into()])
            .expect("a bound file should be listed");
        assert!(
            notice.starts_with("中身を承認に含めるファイル: scripts/server.py。"),
            "{notice}"
        );
        assert_eq!(
            replaced_user_entry_warning(LaunchKind::ContextServer),
            "ユーザー設定の同名サーバーを置き換えています"
        );
    }

    #[test]
    fn a_name_cannot_hide_behind_invisible_characters() {
        let lines = pending_launch_lines(LaunchKind::ContextServer, "git\u{202E}buh", false);
        assert!(
            lines.iter().all(|line| !line.contains('\u{202E}')),
            "{lines:?}"
        );
        let title = launch_title(LaunchKind::ExternalFormatter, "fmt\u{200B}");
        assert!(!title.contains('\u{200B}'), "{title:?}");
    }

    #[test]
    fn shows_names_but_never_other_values() {
        assert_eq!(names_without_values(&[]), None);
        assert_eq!(
            names_without_values(&["GITHUB_TOKEN".into(), "DEBUG".into()]).as_deref(),
            Some("GITHUB_TOKEN, DEBUG (値は表示しません)")
        );
    }

    #[gpui::test]
    async fn test_a_pending_launch_is_announced_only_where_its_project_is_open(
        cx: &mut TestAppContext,
    ) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": {} }))
            .await;
        fs.insert_tree(path!("/other"), json!({ "src": {} })).await;
        let owning_project = Project::test(fs.clone(), [path!("/project").as_ref()], cx).await;
        let other_project = Project::test(fs, [path!("/other").as_ref()], cx).await;
        let owning_window = cx
            .add_window(|window, cx| MultiWorkspace::test_new(owning_project.clone(), window, cx));
        let other_window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(other_project, window, cx));
        cx.update(init);

        let worktree_abs_path = owning_project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project should have its worktree")
                .read(cx)
                .abs_path()
        });
        let mut details = LaunchApprovalDetails::for_tests(
            "launch-key",
            LaunchKind::LanguageServerBinary,
            "rust-analyzer",
            LaunchTarget::Command {
                program: "tools/rust-analyzer".into(),
                arguments: Vec::new(),
            },
        );
        details.worktree_abs_path = worktree_abs_path;
        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.add_pending_launch_for_tests(details, cx);
            });
        });
        cx.run_until_parked();

        let id = notification_id(&LaunchApprovalKey::for_tests("launch-key"));
        let shown_in = |window: WindowHandle<MultiWorkspace>, cx: &mut TestAppContext| {
            window
                .read_with(cx, |multi_workspace, cx| {
                    multi_workspace
                        .workspace()
                        .read(cx)
                        .notification_ids()
                        .contains(&id)
                })
                .expect("the window should still be open")
        };
        assert!(
            shown_in(owning_window, cx),
            "the workspace with the project should be told"
        );
        assert!(
            !shown_in(other_window, cx),
            "an unrelated workspace should not be told"
        );
    }

    async fn open_project_window(
        cx: &mut TestAppContext,
    ) -> (
        WindowHandle<MultiWorkspace>,
        std::sync::Arc<std::path::Path>,
    ) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            // Decisions are stored in the database's key-value store.
            cx.set_global(db::AppDatabase::test_new());
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/project"), json!({ "src": {} }))
            .await;
        let project = Project::test(fs, [path!("/project").as_ref()], cx).await;
        let window =
            cx.add_window(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        cx.update(init);
        let worktree_abs_path = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("the project should have its worktree")
                .read(cx)
                .abs_path()
        });
        (window, worktree_abs_path)
    }

    fn formatter_launch(
        key: &str,
        worktree_abs_path: &std::sync::Arc<std::path::Path>,
        program: &str,
    ) -> LaunchApprovalDetails {
        let mut details = LaunchApprovalDetails::for_tests(
            key,
            LaunchKind::ExternalFormatter,
            "prettier",
            LaunchTarget::Command {
                program: program.to_string().into(),
                arguments: vec!["--write".into()],
            },
        );
        details.worktree_abs_path = worktree_abs_path.clone();
        details
    }

    fn open_dialog(
        window: WindowHandle<MultiWorkspace>,
        details: LaunchApprovalDetails,
        cx: &mut TestAppContext,
    ) -> Entity<LaunchApprovalDialog> {
        window
            .update(cx, |multi_workspace, window, cx| {
                multi_workspace.workspace().update(cx, |workspace, cx| {
                    workspace.toggle_modal(window, cx, |window, cx| {
                        LaunchApprovalDialog::new(details, window, cx)
                    });
                    workspace.active_modal::<LaunchApprovalDialog>(cx)
                })
            })
            .expect("the window should still be open")
            .expect("the dialog should have opened")
    }

    fn dialog_is_open(window: WindowHandle<MultiWorkspace>, cx: &mut TestAppContext) -> bool {
        window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .active_modal::<LaunchApprovalDialog>(cx)
                    .is_some()
            })
            .expect("the window should still be open")
    }

    fn notification_is_shown(
        window: WindowHandle<MultiWorkspace>,
        key: &str,
        cx: &mut TestAppContext,
    ) -> bool {
        let id = notification_id(&LaunchApprovalKey::for_tests(key));
        window
            .read_with(cx, |multi_workspace, cx| {
                multi_workspace
                    .workspace()
                    .read(cx)
                    .notification_ids()
                    .contains(&id)
            })
            .expect("the window should still be open")
    }

    #[gpui::test]
    async fn test_the_dialog_follows_a_replaced_launch_and_refuses_a_stale_decision(
        cx: &mut TestAppContext,
    ) {
        let (window, worktree_abs_path) = open_project_window(cx).await;
        let requester = window
            .read_with(cx, |multi_workspace, _| {
                multi_workspace.workspace().entity_id()
            })
            .expect("the window should still be open");

        let first = formatter_launch("first", &worktree_abs_path, "node_modules/.bin/prettier");
        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.request_launch(first.clone(), requester, cx);
            });
        });
        cx.run_until_parked();
        assert!(notification_is_shown(window, "first", cx));
        let dialog = open_dialog(window, first, cx);

        // The project's formatter definition changes while the dialog is open.
        let second = formatter_launch("second", &worktree_abs_path, "scripts/format.sh");
        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.request_launch(second, requester, cx);
            });
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.details.key, LaunchApprovalKey::for_tests("second"));
            assert!(dialog.changed, "the dialog should say the content changed");
        });
        assert!(!notification_is_shown(window, "first", cx));
        assert!(notification_is_shown(window, "second", cx));

        // A click rendered before the change still carries the old key.
        dialog.update(cx, |dialog, cx| {
            dialog.decide(
                LaunchApprovalKey::for_tests("first"),
                LaunchDecision::Approved,
                cx,
            );
        });
        cx.run_until_parked();
        dialog.read_with(cx, |dialog, _| {
            assert_eq!(dialog.error.as_deref(), Some(CHANGED_SINCE_SHOWN_MESSAGE));
        });
        assert!(dialog_is_open(window, cx));

        dialog.update(cx, |dialog, cx| {
            dialog.decide(
                LaunchApprovalKey::for_tests("second"),
                LaunchDecision::Approved,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(!dialog_is_open(window, cx), "approving closes the dialog");
        assert!(
            !notification_is_shown(window, "second", cx),
            "and the notification"
        );
    }

    #[gpui::test]
    async fn test_denying_closes_the_dialog_and_the_notification(cx: &mut TestAppContext) {
        let (window, worktree_abs_path) = open_project_window(cx).await;
        let launch = formatter_launch("only", &worktree_abs_path, "node_modules/.bin/prettier");
        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.add_pending_launch_for_tests(launch.clone(), cx);
            });
        });
        cx.run_until_parked();
        assert!(notification_is_shown(window, "only", cx));

        let dialog = open_dialog(window, launch, cx);
        dialog.update(cx, |dialog, cx| {
            dialog.decide(
                LaunchApprovalKey::for_tests("only"),
                LaunchDecision::Denied,
                cx,
            );
        });
        cx.run_until_parked();
        assert!(!dialog_is_open(window, cx));
        assert!(!notification_is_shown(window, "only", cx));
    }
}
