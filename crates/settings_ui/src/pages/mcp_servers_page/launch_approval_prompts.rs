//! The ADR-F42 approval UI that is not part of one program's own page:
//! the notification about a launch waiting for a decision, the dialog for
//! language servers and formatters, and the settings page that lists every
//! decision so that it can be taken back. The MCP servers page draws its
//! own rows from what is shared here.

use collections::HashMap;
use context_server::ContextServerId;
use gpui::{
    AnyElement, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, ScrollHandle,
    Subscription, WeakEntity, WindowHandle,
};
use project::Project;
use project::project_launch_approvals::{
    CHANGED_SINCE_SHOWN_MESSAGE, ExecutionHost, LaunchApprovalDetails, LaunchApprovalKey,
    LaunchDecision, LaunchDecisionRecord, LaunchDefinitionFile, LaunchHost, LaunchKind,
    LaunchTarget, ProjectLaunchApprovals, ProjectLaunchApprovalsEvent, UnboundWorktreeContent,
    escape_for_display,
};
use time::{OffsetDateTime, UtcOffset};
use ui::{Divider, Tooltip, prelude::*};
use util::ResultExt as _;
use workspace::{
    ModalView, MultiWorkspace, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

use crate::SettingsWindow;

/// Shown instead of letting a decision be made that could not be stored.
pub(super) const DECISION_BLOCKED_HINT: &str =
    i18n::msg!("The approval records can't be read, so no decision can be made.");

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

/// One notification stands for every wait on the same launch, so it only goes
/// away once nothing is waiting any more: a language server that is started
/// twice waits twice under one key, and the first of the two to give up must
/// not take the notification the other still needs (ADR-F42).
fn dismiss_pending_launch(key: &LaunchApprovalKey, cx: &mut App) {
    if ProjectLaunchApprovals::try_global(cx)
        .is_some_and(|approvals| approvals.read(cx).pending_launch(key).is_some())
    {
        return;
    }
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
    .primary_message(i18n::tc!("launch approval", "Review"))
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
        i18n::t!("This project's settings want to start a program").to_string(),
        launch_title(kind, name),
    ];
    if changed {
        lines.push(i18n::tr(CHANGED_SINCE_SHOWN_MESSAGE).to_string());
    }
    lines
}

/// Names come from the project's settings: control, zero-width and
/// direction-changing characters in them are spelled out rather than obeyed.
fn launch_title(kind: LaunchKind, name: &str) -> String {
    let name = escape_for_display(name);
    match kind {
        LaunchKind::ContextServer => i18n::t!("MCP server \"{name}\""),
        LaunchKind::LanguageServerBinary => i18n::t!("Binary for language server \"{name}\""),
        LaunchKind::ExternalFormatter => i18n::t!("External formatter \"{name}\""),
    }
}

pub(super) fn definition_file_label(file: &LaunchDefinitionFile, host: &LaunchHost) -> String {
    match (file, host) {
        (LaunchDefinitionFile::Worktree(path), LaunchHost::ThisMachine) => path.to_string(),
        (LaunchDefinitionFile::Worktree(path), LaunchHost::Remote { display_name, .. }) => {
            format!("{path} ({display_name})")
        }
        (LaunchDefinitionFile::RemoteServerSettings, LaunchHost::Remote { display_name, .. }) => {
            i18n::t!("Server settings on {display_name}")
        }
        (LaunchDefinitionFile::RemoteServerSettings, LaunchHost::ThisMachine) => {
            i18n::t!("Server settings on the remote host").to_string()
        }
        (LaunchDefinitionFile::ResolvedOnRemoteHost, LaunchHost::Remote { display_name, .. }) => {
            i18n::t!("Command chosen by an extension on {display_name} (no settings file)")
        }
        (LaunchDefinitionFile::ResolvedOnRemoteHost, LaunchHost::ThisMachine) => {
            i18n::t!("Command chosen by an extension on the remote host (no settings file)")
                .to_string()
        }
    }
}

fn execution_host_label(host: &ExecutionHost) -> String {
    match host {
        ExecutionHost::ThisMachine => i18n::t!("This computer").to_string(),
        ExecutionHost::Remote { display_name } => i18n::t!("Remote host ({display_name})"),
    }
}

/// The program, then every argument in a box of its own: no quoting is needed
/// to tell where one argument ends, so a Windows path reads exactly as it
/// runs. Each piece is escaped separately.
pub(super) fn render_target_rows(target: &LaunchTarget, cx: &App) -> AnyElement {
    match target {
        LaunchTarget::Url(url) => {
            render_detail_row(i18n::t!("URL"), url.to_string(), true, cx).into_any_element()
        }
        LaunchTarget::Command { program, arguments } => v_flex()
            .w_full()
            .min_w_0()
            .gap_0p5()
            .child(render_detail_row(
                i18n::t!("Command"),
                program.to_string(),
                true,
                cx,
            ))
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
                Label::new(i18n::t!("Arguments"))
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
        let names = names
            .iter()
            .map(|name| name.as_ref())
            .collect::<Vec<&str>>()
            .join(", ");
        i18n::t!("{names} (values not shown)")
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
        let files = files
            .iter()
            .map(|file| file.as_ref())
            .collect::<Vec<&str>>()
            .join(", ");
        i18n::t!(
            "Files whose contents are part of this approval: {files}. A change to their contents asks for confirmation again, but the command line alone doesn't show what they do, so review their contents too before allowing."
        )
    })
}

/// Unlike a bound file, this content is not part of what gets approved.
fn unbound_content_warning(content: &UnboundWorktreeContent) -> String {
    match content {
        UnboundWorktreeContent::WorkingDirectory {
            program,
            working_directory,
        } => i18n::t!(
            "{program} starts in the project folder {working_directory} and may load code from it. You won't be asked again if the contents change later."
        ),
        UnboundWorktreeContent::File(path) => i18n::t!(
            "{path} is a file in the project, but its contents are not part of the approval. You won't be asked again if the contents change later."
        ),
        UnboundWorktreeContent::Directory(path) => i18n::t!(
            "The contents of the folder {path} are not part of the approval. You won't be asked again if the contents change later."
        ),
        UnboundWorktreeContent::ArgumentFile(path) => i18n::t!(
            "The contents of the argument file {path} are not part of the approval. You won't be asked again if the contents change later."
        ),
        UnboundWorktreeContent::Wrapper(program) => {
            i18n::t!("{program} starts another program. Its contents are not part of the approval.")
        }
        UnboundWorktreeContent::RemoteFile(path) => i18n::t!(
            "The contents of the remote file {path} can't be checked from here, so they are not part of the approval. You won't be asked again if the contents change later."
        ),
        // Always comes with a session-only approval, which the details
        // already say on a line of their own.
        UnboundWorktreeContent::Unreadable(path) => {
            i18n::t!("{path} can't be read, so its contents can't be checked.")
        }
    }
}

const SESSION_ONLY_NOTICE: &str = i18n::msg!("This approval lasts until Zed quits.");

/// The approvals service refuses with [`CHANGED_SINCE_SHOWN_MESSAGE`], an
/// English msgid it compares against; any other error is shown as it is.
fn store_error_text(error: &anyhow::Error) -> String {
    let error = error.to_string();
    if error == CHANGED_SINCE_SHOWN_MESSAGE {
        i18n::tr(CHANGED_SINCE_SHOWN_MESSAGE).to_string()
    } else {
        error
    }
}

fn replaced_user_entry_warning(kind: LaunchKind) -> &'static str {
    match kind {
        LaunchKind::ContextServer => {
            i18n::t!("Replaces the server of the same name in your user settings")
        }
        LaunchKind::LanguageServerBinary => {
            i18n::t!("Replaces the binary your user settings specify for the same language server")
        }
        LaunchKind::ExternalFormatter => {
            i18n::t!("Replaces the formatter your user settings specify")
        }
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
            (
                if index == 0 {
                    i18n::t!("Environment")
                } else {
                    ""
                },
                line,
                monospace,
            )
        })
        .chain(
            names_without_values(&details.header_names)
                .map(|names| (i18n::t!("Headers"), names, false)),
        )
        .chain(std::iter::once((
            i18n::t!("Runs on"),
            execution_host_label(&details.execution_host),
            false,
        )))
        .collect::<Vec<_>>();

    v_flex()
        .w_full()
        .min_w_0()
        .gap_0p5()
        .child(render_detail_row(
            i18n::t!("Defined in"),
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
                i18n::tr(SESSION_ONLY_NOTICE).to_string(),
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
                i18n::t!("The approval records can't be read, so it won't be started: {error}"),
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
            self.error = Some(i18n::tr(CHANGED_SINCE_SHOWN_MESSAGE).to_string().into());
            cx.notify();
            return;
        }
        let Some(approvals) = ProjectLaunchApprovals::try_global(cx) else {
            self.error = Some(
                i18n::t!("The approval records are unavailable, so the decision can't be saved.")
                    .into(),
            );
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
        // Detached so that storing the decision runs to its end whatever
        // becomes of this dialog. The approvals service settles the launch
        // itself, and `follow` dismisses the dialog on the `LaunchDecided`
        // that comes with it - before this task is woken - so a dialog that is
        // already gone here is the ordinary path rather than a failure, and
        // must not be logged as one.
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let store_error = result.as_ref().err().map(|error| format!("{error:#}"));
            if this
                .update(cx, |this, cx| {
                    this.deciding = false;
                    match result {
                        Ok(()) => cx.emit(DismissEvent),
                        Err(error) => {
                            let error = store_error_text(&error);
                            this.error =
                                Some(i18n::t!("Couldn't save the decision: {error}").into());
                            cx.notify();
                        }
                    }
                })
                .is_err()
                && let Some(store_error) = store_error
            {
                // Nowhere left to show it, but a decision that was not stored
                // must not pass unnoticed.
                log::error!("could not store a launch decision: {store_error}");
            }
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
                Label::new(i18n::t!(
                    "This project's settings are trying to start a program as shown below. Review it before choosing."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            )
            .when(self.changed, |this| {
                this.child(render_notice(
                    IconName::Warning,
                    Color::Warning,
                    i18n::tr(CHANGED_SINCE_SHOWN_MESSAGE).to_string(),
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
                        Button::new("launch-approval-deny", i18n::t!("Don't Allow"))
                            .style(ButtonStyle::Subtle)
                            .disabled(self.deciding || blocked)
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.decide(deny_key.clone(), LaunchDecision::Denied, cx);
                            })),
                    )
                    .child(
                        Button::new("launch-approval-allow", i18n::t!("Allow and Start"))
                            .style(ButtonStyle::Filled)
                            .disabled(self.deciding || blocked)
                            .when(blocked, |button| {
                                button.tooltip(Tooltip::text(i18n::tr(DECISION_BLOCKED_HINT)))
                            })
                            .on_click(cx.listener(move |this, _, _window, cx| {
                                this.decide(allow_key.clone(), LaunchDecision::Approved, cx);
                            })),
                    ),
            )
    }
}

/// Live state of the page that lists the decisions about what a project's
/// settings would launch.
#[derive(Default)]
pub(crate) struct LaunchApprovalsPageState {
    /// Decisions live in the approval service rather than in `SettingsStore`,
    /// so without these the list would only change when something else redrew
    /// the window. Loading them finishes with a notification rather than an
    /// event, so both are watched.
    subscriptions: Vec<Subscription>,
    /// Why revoking failed, by the key of the decision it was asked for.
    errors: HashMap<String, SharedString>,
}

pub(crate) fn open_launch_approvals_page(
    settings_window: &mut SettingsWindow,
    cx: &mut Context<SettingsWindow>,
) {
    settings_window.launch_approvals.errors.clear();
    if settings_window.launch_approvals.subscriptions.is_empty() {
        let approvals = ProjectLaunchApprovals::global(cx);
        settings_window.launch_approvals.subscriptions = vec![
            cx.subscribe(&approvals, |_, _, _: &ProjectLaunchApprovalsEvent, cx| {
                cx.notify()
            }),
            cx.observe(&approvals, |_, _, cx| cx.notify()),
        ];
    }
}

/// Every decision ADR-F42 keeps, on a page of its own. The MCP servers page
/// can only offer to revoke a server its store still knows about, and a
/// language server's or a formatter's approval had nowhere to be revoked from
/// at all — which ADR-F50 decision 1 needs, since revoking is also what stops
/// the program on the remote host.
pub(crate) fn render_launch_approvals_page(
    settings_window: &SettingsWindow,
    scroll_handle: &ScrollHandle,
    _window: &mut Window,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let (records, still_loading) = match ProjectLaunchApprovals::try_global(cx) {
        Some(approvals) => {
            let approvals = approvals.read(cx);
            (
                approvals
                    .decision_records()
                    .map_err(|error| SharedString::from(format!("{error}"))),
                approvals.decisions_are_loading(),
            )
        }
        None => (
            Err(SharedString::from(i18n::t!(
                "The approval records are unavailable, so the list can't be shown."
            ))),
            false,
        ),
    };
    // Waiting for the decisions to be read is not the same as a store that
    // could not be read at all, which blocks every launch.
    let (unavailable_icon, unavailable_color) = if still_loading {
        (IconName::Info, Color::Muted)
    } else {
        (IconName::XCircle, Color::Error)
    };
    // Clock and time zone are read once, so that every row of one drawing
    // agrees on what "today" means.
    let now = OffsetDateTime::now_utc();
    let timezone = UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC);

    v_flex()
        .id("launch-approvals-page")
        .size_full()
        .pt_2()
        .pb_16()
        .overflow_y_scroll()
        .track_scroll(scroll_handle)
        .child(
            div().px_8().pb_2().child(
                Label::new(i18n::t!(
                    "Whether each program that a project's settings tried to start was allowed or denied. Revoking a decision stops the program if it is running, and you'll be asked again the next time it needs to start."
                ))
                .size(LabelSize::Small)
                .color(Color::Muted),
            ),
        )
        .map(|this| match records {
            Err(error) => this.child(div().px_8().child(render_notice(
                unavailable_icon,
                unavailable_color,
                error.to_string(),
            ))),
            Ok(records) if records.is_empty() => this.child(
                div()
                    .px_8()
                    .child(
                        Label::new(i18n::t!("No decisions have been recorded yet."))
                            .color(Color::Muted),
                    ),
            ),
            Ok(records) => {
                let records = sorted_for_review(records);
                let row_count = records.len();
                this.children(records.into_iter().enumerate().flat_map(|(index, record)| {
                    let mut elements = vec![render_launch_approval_row(
                        settings_window,
                        &record,
                        now,
                        timezone,
                        cx,
                    )];
                    if index + 1 < row_count {
                        elements.push(
                            div()
                                .px_8()
                                .child(Divider::horizontal().flex_grow_1())
                                .into_any_element(),
                        );
                    }
                    elements
                }))
            }
        })
        .into_any_element()
}

/// Newest first, since a decision is usually revoked soon after it was made;
/// the rest of the order only has to stay the same across redraws.
fn sorted_for_review(mut records: Vec<LaunchDecisionRecord>) -> Vec<LaunchDecisionRecord> {
    records.sort_by(|left, right| {
        right
            .timestamp
            .cmp(&left.timestamp)
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.key.cmp(&right.key))
    });
    records
}

fn render_launch_approval_row(
    settings_window: &SettingsWindow,
    record: &LaunchDecisionRecord,
    now: OffsetDateTime,
    timezone: UtcOffset,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let error = settings_window
        .launch_approvals
        .errors
        .get(&record.key)
        .cloned();
    h_flex()
        .w_full()
        .justify_between()
        .items_start()
        .py_3()
        .px_8()
        .gap_4()
        .child(
            v_flex()
                .gap_0p5()
                .min_w_0()
                .flex_1()
                .child(Label::new(launch_title(record.kind, &record.name)))
                .child(render_detail_row(
                    i18n::tc!("launch approval", "Details"),
                    record.summary.clone(),
                    true,
                    cx,
                ))
                .child(
                    div()
                        .id(SharedString::from(format!(
                            "launch-approval-host-{}",
                            record.key
                        )))
                        .w_full()
                        .min_w_0()
                        .child(render_detail_row(
                            i18n::t!("Host"),
                            record_host_label(record.host.as_deref()),
                            false,
                            cx,
                        ))
                        .when_some(
                            record_host_tooltip(record.host.as_deref()),
                            |this, identity| {
                                // Escaped like everything else the project
                                // chose the text of.
                                this.tooltip(Tooltip::text(
                                    escape_for_display(&identity).into_owned(),
                                ))
                            },
                        ),
                )
                .child(render_detail_row(
                    i18n::t!("Folder"),
                    record.worktree.clone(),
                    true,
                    cx,
                ))
                .child(render_detail_row(
                    i18n::t!("Decision"),
                    decision_label(record.decision, record.timestamp, now, timezone),
                    false,
                    cx,
                ))
                .when(record.session_only, |this| {
                    this.child(render_notice(
                        IconName::Info,
                        Color::Muted,
                        i18n::tr(SESSION_ONLY_NOTICE).to_string(),
                    ))
                })
                .when_some(error, |this, error| {
                    this.child(render_notice(
                        IconName::XCircle,
                        Color::Error,
                        error.to_string(),
                    ))
                }),
        )
        .child(render_record_revoke_button(record, cx))
        .into_any_element()
}

/// The host a decision was made for. Only its identity is stored with the
/// decision, and that identity is what tells the same definition on two
/// remote hosts apart.
///
/// What the identity is made of is not for reading, though: the scheme repeats
/// what the row already says, and the digest of the connection's own arguments
/// (which the identity carries so that the same name reached through a
/// different jump host counts as a different host) is a line of hex the user
/// cannot act on. So the row shows the part they recognize — `me@build`, or
/// the distro or container name — and [`record_host_tooltip`] keeps the whole
/// identity a hover away.
fn record_host_label(identity: Option<&str>) -> String {
    match identity {
        None => i18n::t!("This computer").to_string(),
        Some(identity) => {
            i18n::t!(
                "Remote host ({display_name})",
                display_name = readable_host(identity)
            )
        }
    }
}

/// The identity with the parts that are not a host name taken off: the scheme
/// and the trailing `args-sha256:…`. Anything left empty by that is shown as
/// it stands rather than trimmed into nothing.
fn readable_host(identity: &str) -> &str {
    let without_digest = identity
        .split_once(" args-sha256:")
        .map_or(identity, |(host, _)| host);
    let readable = without_digest
        .split_once("://")
        .map_or(without_digest, |(_, host)| host);
    if readable.is_empty() {
        without_digest
    } else {
        readable
    }
}

/// The whole identity, for the hover on a row that shows only the host name.
/// `None` where nothing was taken off, so that a row never carries a tooltip
/// repeating what it already says.
fn record_host_tooltip(identity: Option<&str>) -> Option<SharedString> {
    let identity = identity?;
    (readable_host(identity) != identity).then(|| SharedString::from(identity.to_string()))
}

fn decision_label(
    decision: LaunchDecision,
    timestamp: u64,
    now: OffsetDateTime,
    timezone: UtcOffset,
) -> String {
    let decision = match decision {
        LaunchDecision::Approved => i18n::t!("Allowed"),
        LaunchDecision::Denied => i18n::t!("Not allowed"),
    };
    match decided_at(timestamp, now, timezone) {
        Some(decided_at) => format!("{decision} ({decided_at})"),
        None => decision.to_string(),
    }
}

/// `None` for a timestamp no date can hold, which a decision written by a
/// clock far ahead of this one would be.
fn decided_at(timestamp: u64, now: OffsetDateTime, timezone: UtcOffset) -> Option<String> {
    let timestamp = OffsetDateTime::from_unix_timestamp(i64::try_from(timestamp).ok()?).ok()?;
    Some(time_format::format_localized_timestamp(
        timestamp,
        now,
        timezone,
        time_format::TimestampFormat::EnhancedAbsolute,
    ))
}

fn render_record_revoke_button(
    record: &LaunchDecisionRecord,
    cx: &mut Context<SettingsWindow>,
) -> AnyElement {
    let (label, tooltip) = match record.decision {
        LaunchDecision::Approved => (
            i18n::t!("Revoke Approval"),
            i18n::t!(
                "Revokes the approval and stops the program if it is running. Starting it again will need your approval again."
            ),
        ),
        LaunchDecision::Denied => (
            i18n::t!("Revoke Denial"),
            i18n::t!(
                "Removes the record of the denial. The next time the same launch is requested, you'll be asked again."
            ),
        ),
    };
    let key = record.approval_key();
    let stored_key = record.key.clone();
    Button::new(
        SharedString::from(format!("launch-approval-revoke-{}", record.key)),
        label,
    )
    .style(ButtonStyle::Outlined)
    .label_size(LabelSize::Small)
    .tab_index(0isize)
    .tooltip(Tooltip::text(tooltip))
    .on_click(cx.listener(move |this, _event, _window, cx| {
        revoke_decision(this, key.clone(), stored_key.clone(), cx);
    }))
    .into_any_element()
}

/// Revoking goes through the approval service itself, the same path the MCP
/// servers page takes: it is what stops a running language server or MCP
/// server here, and what takes the decision back from a remote host.
fn revoke_decision(
    settings_window: &mut SettingsWindow,
    key: LaunchApprovalKey,
    stored_key: String,
    cx: &mut Context<SettingsWindow>,
) {
    let Some(approvals) = ProjectLaunchApprovals::try_global(cx) else {
        settings_window.launch_approvals.errors.insert(
            stored_key,
            i18n::t!("The approval records are unavailable, so the decision can't be revoked.")
                .into(),
        );
        cx.notify();
        return;
    };
    settings_window.launch_approvals.errors.remove(&stored_key);
    let task = approvals.update(cx, |approvals, cx| approvals.revoke(&key, cx));
    cx.spawn(async move |this, cx| {
        let Err(error) = task.await else {
            return;
        };
        let error = store_error_text(&error);
        this.update(cx, |this, cx| {
            this.launch_approvals
                .errors
                .insert(stored_key, i18n::t!("Couldn't revoke: {error}").into());
            cx.notify();
        })
        .log_err();
    })
    .detach();
    cx.notify();
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
                "This project's settings want to start a program".to_string(),
                "MCP server \"github\"".to_string(),
            ]
        );
        assert_eq!(
            pending_launch_lines(LaunchKind::ExternalFormatter, "prettier", true),
            vec![
                "This project's settings want to start a program".to_string(),
                "External formatter \"prettier\"".to_string(),
                CHANGED_SINCE_SHOWN_MESSAGE.to_string(),
            ]
        );
        assert_eq!(
            launch_title(LaunchKind::LanguageServerBinary, "rust-analyzer"),
            "Binary for language server \"rust-analyzer\""
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
            "Server settings on me@build"
        );
        assert_eq!(
            definition_file_label(&LaunchDefinitionFile::ResolvedOnRemoteHost, &remote),
            "Command chosen by an extension on me@build (no settings file)"
        );
        assert_eq!(
            execution_host_label(&ExecutionHost::ThisMachine),
            "This computer"
        );
        assert_eq!(
            execution_host_label(&ExecutionHost::Remote {
                display_name: "me@build".into()
            }),
            "Remote host (me@build)"
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
                Some("GITHUB_TOKEN (values not shown)".to_string()),
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
                "node starts in the project folder /work/app and may load code from it. You won't be asked again if the contents change later.",
            ),
            (
                UnboundWorktreeContent::File("/work/app/config.toml".into()),
                "/work/app/config.toml is a file in the project, but its contents are not part of the approval. You won't be asked again if the contents change later.",
            ),
            (
                UnboundWorktreeContent::Directory("/work/app/scripts".into()),
                "The contents of the folder /work/app/scripts are not part of the approval. You won't be asked again if the contents change later.",
            ),
            (
                UnboundWorktreeContent::ArgumentFile("/work/app/args.txt".into()),
                "The contents of the argument file /work/app/args.txt are not part of the approval. You won't be asked again if the contents change later.",
            ),
            (
                UnboundWorktreeContent::Wrapper("direnv".into()),
                "direnv starts another program. Its contents are not part of the approval.",
            ),
            (
                UnboundWorktreeContent::RemoteFile("/srv/app/server.py".into()),
                "The contents of the remote file /srv/app/server.py can't be checked from here, so they are not part of the approval. You won't be asked again if the contents change later.",
            ),
            (
                UnboundWorktreeContent::Unreadable("/work/app/tool".into()),
                "/work/app/tool can't be read, so its contents can't be checked.",
            ),
        ] {
            assert_eq!(unbound_content_warning(&content), expected);
        }
        assert_eq!(bound_files_notice(&[]), None);
        let notice = bound_files_notice(&["scripts/server.py".into()])
            .expect("a bound file should be listed");
        assert!(
            notice
                .starts_with("Files whose contents are part of this approval: scripts/server.py. "),
            "{notice}"
        );
        assert_eq!(
            replaced_user_entry_warning(LaunchKind::ContextServer),
            "Replaces the server of the same name in your user settings"
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
            Some("GITHUB_TOKEN, DEBUG (values not shown)")
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

    /// A language server that is started twice waits twice for the same
    /// approval. The first of the two waits to be given up used to take the
    /// shared notification with it, leaving the second waiting with nothing
    /// on screen to decide it.
    #[gpui::test]
    async fn test_a_second_wait_for_the_same_launch_keeps_the_notification(
        cx: &mut TestAppContext,
    ) {
        let (window, worktree_abs_path) = open_project_window(cx).await;
        let launch = formatter_launch("shared", &worktree_abs_path, "node_modules/.bin/prettier");
        let (first_wait, second_wait) = cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                (
                    approvals.add_pending_launch_for_tests(launch.clone(), cx),
                    approvals.add_pending_launch_for_tests(launch.clone(), cx),
                )
            })
        });
        cx.run_until_parked();
        assert!(notification_is_shown(window, "shared", cx));

        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.withdraw_pending(&launch.key, first_wait, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            notification_is_shown(window, "shared", cx),
            "the wait that is left still needs the notification"
        );

        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.withdraw_pending(&launch.key, second_wait, cx);
            });
        });
        cx.run_until_parked();
        assert!(
            !notification_is_shown(window, "shared", cx),
            "with nothing waiting the notification goes"
        );
    }

    /// A language server restarted one after the other, rather than side by
    /// side: the old start lets go and the new one asks for the same launch
    /// before anything has looked at the withdrawal.
    #[gpui::test]
    async fn test_a_launch_asked_for_again_as_it_is_withdrawn_keeps_its_notification(
        cx: &mut TestAppContext,
    ) {
        let (window, worktree_abs_path) = open_project_window(cx).await;
        let launch = formatter_launch(
            "restarted",
            &worktree_abs_path,
            "node_modules/.bin/prettier",
        );
        let first_start = cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.add_pending_launch_for_tests(launch.clone(), cx)
            })
        });
        cx.run_until_parked();
        assert!(notification_is_shown(window, "restarted", cx));

        cx.update(|cx| {
            ProjectLaunchApprovals::global(cx).update(cx, |approvals, cx| {
                approvals.withdraw_pending(&launch.key, first_start, cx);
                approvals.add_pending_launch_for_tests(launch.clone(), cx);
            });
        });
        cx.run_until_parked();
        assert!(
            notification_is_shown(window, "restarted", cx),
            "the restarted server waits for the same decision, so the notification stays"
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

    fn decision_record(
        key: &str,
        name: &str,
        timestamp: u64,
        decision: LaunchDecision,
        host: Option<&str>,
    ) -> LaunchDecisionRecord {
        LaunchDecisionRecord {
            key: key.to_string(),
            kind: LaunchKind::LanguageServerBinary,
            name: name.to_string(),
            worktree: path!("/project").to_string(),
            host: host.map(str::to_string),
            summary: "tools/rust-analyzer".to_string(),
            decision,
            timestamp,
            session_only: false,
        }
    }

    #[test]
    fn lists_the_newest_decision_first() {
        let sorted = sorted_for_review(vec![
            decision_record("a", "older", 10, LaunchDecision::Approved, None),
            decision_record("c", "newest", 30, LaunchDecision::Denied, None),
            decision_record("b", "middle", 20, LaunchDecision::Approved, None),
        ]);
        assert_eq!(
            sorted
                .iter()
                .map(|record| record.name.as_str())
                .collect::<Vec<_>>(),
            vec!["newest", "middle", "older"]
        );
    }

    #[test]
    fn names_the_host_and_when_the_decision_was_made() {
        assert_eq!(record_host_label(None), "This computer");
        assert_eq!(record_host_tooltip(None), None);
        // The scheme repeats the row, and the digest of the connection's own
        // arguments is not something the user can act on, so the row shows
        // what they recognize and the hover keeps the whole identity.
        for (identity, shown) in [
            ("ssh://me@build", "me@build"),
            ("ssh://me@build:2222", "me@build:2222"),
            (
                "ssh://me@build args-sha256:0f1e2d3c4b5a69788796a5b4c3d2e1f0",
                "me@build",
            ),
            ("wsl://root@Ubuntu", "root@Ubuntu"),
            ("docker://9a8b7c6d5e4f", "9a8b7c6d5e4f"),
            // Nothing this fork writes, and nothing to take off it either.
            ("build.example", "build.example"),
            ("://", "://"),
        ] {
            assert_eq!(
                record_host_label(Some(identity)),
                format!("Remote host ({shown})"),
                "identity: {identity}"
            );
            assert_eq!(
                record_host_tooltip(Some(identity)),
                (shown != identity).then(|| SharedString::from(identity.to_string())),
                "identity: {identity}"
            );
        }

        let now = OffsetDateTime::from_unix_timestamp(1_760_000_000).expect("a valid instant");
        let today_at =
            decided_at(1_760_000_000, now, UtcOffset::UTC).expect("a representable date");
        let older_at =
            decided_at(1_600_000_000, now, UtcOffset::UTC).expect("a representable date");
        assert_eq!(
            decision_label(LaunchDecision::Approved, 1_760_000_000, now, UtcOffset::UTC),
            format!("Allowed ({today_at})")
        );
        assert_eq!(
            decision_label(LaunchDecision::Denied, 1_600_000_000, now, UtcOffset::UTC),
            format!("Not allowed ({older_at})")
        );
        // A decision made today is named by the day alone, an older one by its
        // date, whatever words the time format uses for either.
        assert!(!today_at.contains("2025"), "{today_at}");
        assert!(older_at.contains("2020"), "{older_at}");
        assert_ne!(
            today_at.replace(char::is_numeric, ""),
            older_at.replace(char::is_numeric, ""),
            "today's and an older decision's are written in different shapes"
        );
        // A clock far enough ahead writes a timestamp no date can hold; the
        // decision itself is still worth showing.
        assert_eq!(
            decision_label(LaunchDecision::Approved, u64::MAX, now, UtcOffset::UTC),
            "Allowed"
        );
    }

    /// The button on the page hands the key of the row it was drawn for to the
    /// approval service, which is what stops the program and asks again.
    #[gpui::test]
    async fn test_the_page_revokes_the_decision_it_lists(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = settings::SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            editor::init(cx);
            menu::init();
            cx.set_global(db::AppDatabase::test_new());
        });
        let approvals = cx.update(ProjectLaunchApprovals::global);
        approvals.update(cx, |approvals, cx| {
            approvals.add_pending_launch_for_tests(
                LaunchApprovalDetails::for_tests(
                    "listed",
                    LaunchKind::LanguageServerBinary,
                    "rust-analyzer",
                    LaunchTarget::Command {
                        program: "tools/rust-analyzer".into(),
                        arguments: Vec::new(),
                    },
                ),
                cx,
            );
        });
        approvals
            .update(cx, |approvals, cx| {
                approvals.approve_pending(&LaunchApprovalKey::for_tests("listed"), cx)
            })
            .await
            .expect("the approval is stored");
        let record = approvals
            .read_with(cx, |approvals, _| {
                approvals
                    .decision_records()
                    .expect("the decisions are readable")
                    .into_iter()
                    .next()
            })
            .expect("the approval is listed for review");
        assert_eq!(record.decision, LaunchDecision::Approved);

        let (settings_window, cx) =
            cx.add_window_view(|window, cx| SettingsWindow::test(window, cx));
        settings_window.update(cx, |settings_window, cx| {
            open_launch_approvals_page(settings_window, cx);
            revoke_decision(
                settings_window,
                record.approval_key(),
                record.key.clone(),
                cx,
            );
        });
        cx.run_until_parked();

        assert_eq!(
            settings_window.read_with(cx, |settings_window, _| settings_window
                .launch_approvals
                .errors
                .values()
                .map(ToString::to_string)
                .collect::<Vec<_>>()),
            Vec::<String>::new()
        );
        assert!(
            approvals.read_with(cx, |approvals, _| approvals
                .decision_records()
                .expect("the decisions are readable")
                .is_empty()),
            "the revoked decision leaves the list"
        );
    }
}
