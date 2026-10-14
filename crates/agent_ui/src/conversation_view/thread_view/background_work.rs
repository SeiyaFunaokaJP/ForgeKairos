//! ADR-F97: the list of an external agent's subagents and of the shell
//! commands it started in the background, in the bar above the message
//! editor, next to the subagents that wait for permission.

use acp_thread::{BackgroundShellTask, ExternalSubagentStatus, requested_model_and_effort};

use super::*;

struct SubagentRow {
    session_id: acp_v1::SessionId,
    name: SharedString,
    description: Option<SharedString>,
    status: ExternalSubagentStatus,
    error: Option<SharedString>,
    model: Option<String>,
    effort: Option<String>,
    elapsed: Duration,
}

pub(super) fn subagent_status_label(status: ExternalSubagentStatus) -> SharedString {
    match status {
        ExternalSubagentStatus::Running => i18n::tc!("subagent status", "Running"),
        ExternalSubagentStatus::AwaitingUser => {
            i18n::tc!("subagent status", "Awaiting Permission")
        }
        ExternalSubagentStatus::Idle => i18n::tc!("subagent status", "Idle"),
        ExternalSubagentStatus::Failed => i18n::tc!("subagent status", "Failed"),
        ExternalSubagentStatus::Cancelled => i18n::tc!("subagent status", "Cancelled"),
        ExternalSubagentStatus::Unknown => i18n::tc!("subagent status", "Unknown"),
    }
    .into()
}

fn subagent_status_color(status: ExternalSubagentStatus) -> Color {
    match status {
        ExternalSubagentStatus::Running => Color::Accent,
        ExternalSubagentStatus::AwaitingUser => Color::Warning,
        ExternalSubagentStatus::Idle => Color::Success,
        ExternalSubagentStatus::Failed => Color::Error,
        ExternalSubagentStatus::Cancelled | ExternalSubagentStatus::Unknown => Color::Muted,
    }
}

/// "Model: … · Effort: …", with what the call left out shown as the default.
pub(super) fn model_and_effort_label(model: Option<&str>, effort: Option<&str>) -> SharedString {
    let default = || i18n::tc!("model or effort", "Default").to_string();
    let model = model.map(ToOwned::to_owned).unwrap_or_else(default);
    let effort = effort.map(ToOwned::to_owned).unwrap_or_else(default);
    format!(
        "{} · {}",
        i18n::t!("Model: {}", model),
        i18n::t!("Effort: {}", effort)
    )
    .into()
}

impl ThreadView {
    /// ADR-F97: keeps the elapsed times moving while a child works, and stops
    /// when none does.
    pub(super) fn sync_background_work_clock(&mut self, cx: &mut Context<Self>) {
        let working = self.thread.read(cx).has_working_external_subagents();
        if !working {
            self._background_work_clock = None;
        } else if self._background_work_clock.is_none() {
            self._background_work_clock = Some(cx.spawn(async move |this, cx| {
                loop {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    if this.update(cx, |_, cx| cx.notify()).is_err() {
                        break;
                    }
                }
            }));
        }
    }

    /// The status the parent reports for this view's thread, when it is an
    /// external agent's child.
    pub(super) fn external_subagent_status(&self, cx: &App) -> Option<ExternalSubagentStatus> {
        let parent_session_id = self.parent_session_id.as_ref()?;
        let session_id = self.thread.read(cx).session_id().clone();
        let server_view = self.server_view.upgrade()?;
        let parent_view = server_view.read(cx).thread_view(parent_session_id)?;
        let status = parent_view
            .read(cx)
            .thread
            .read(cx)
            .external_subagent(&session_id)?
            .status;
        Some(status)
    }

    fn subagent_rows(&self, cx: &App) -> Vec<SubagentRow> {
        let thread = self.thread.read(cx);
        thread
            .external_subagents()
            .iter()
            .map(|child| {
                let (model, effort) = child
                    .parent_tool_call_id
                    .as_ref()
                    .and_then(|id| thread.tool_call(id))
                    .map(|(_, tool_call)| requested_model_and_effort(tool_call))
                    .unwrap_or_default();
                let name = child
                    .title
                    .clone()
                    .or_else(|| child.thread.read(cx).title())
                    .unwrap_or_else(|| i18n::t!("Subagent").into());
                SubagentRow {
                    session_id: child.session_id.clone(),
                    name,
                    description: child.description.clone(),
                    status: child.status,
                    error: child.error.clone(),
                    model,
                    effort,
                    elapsed: child.elapsed(),
                }
            })
            .collect()
    }

    pub(super) fn render_background_work(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let subagents = self.subagent_rows(cx);
        let tasks = self.thread.read(cx).background_shell_tasks(cx);
        if subagents.is_empty() && tasks.is_empty() {
            return None;
        }
        let expanded = self.background_work_expanded;
        Some(
            v_flex()
                .child(self.render_background_work_summary(&subagents, &tasks, expanded, cx))
                .when(expanded, |this| {
                    this.child(self.render_background_work_entries(subagents, tasks, cx))
                })
                .into_any_element(),
        )
    }

    fn render_background_work_summary(
        &self,
        subagents: &[SubagentRow],
        tasks: &[BackgroundShellTask],
        expanded: bool,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let working = subagents
            .iter()
            .filter(|row| row.status.is_working())
            .count();
        h_flex()
            .id("background_work_summary")
            .debug_selector(|| "background_work_summary".into())
            .p_1()
            .w_full()
            .gap_1()
            .when(expanded, |this| {
                this.border_b_1().border_color(cx.theme().colors().border)
            })
            .child(Disclosure::new("background_work_disclosure", expanded))
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap_2()
                    .when(!subagents.is_empty(), |this| {
                        this.child(
                            Label::new(i18n::t!("Subagents: {}", subagents.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                        .when(working > 0, |this| {
                            this.child(
                                Label::new(i18n::t!("{} working", working))
                                    .size(LabelSize::Small)
                                    .color(Color::Accent),
                            )
                        })
                    })
                    .when(!tasks.is_empty(), |this| {
                        this.child(
                            Label::new(i18n::t!("Background Tasks: {}", tasks.len()))
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        )
                    }),
            )
            .when(!subagents.is_empty(), |this| {
                this.child(
                    div()
                        .id("background_work_stop_hint")
                        .child(
                            Icon::new(IconName::Info)
                                .size(IconSize::XSmall)
                                .color(Color::Muted),
                        )
                        .tooltip(Tooltip::text(i18n::t!(
                            "A subagent cannot be stopped on its own. To stop the subagents, stop the main agent."
                        ))),
                )
            })
            .on_click(cx.listener(|this, _, _, cx| {
                this.background_work_expanded = !this.background_work_expanded;
                cx.notify();
            }))
    }

    fn render_background_work_entries(
        &self,
        subagents: Vec<SubagentRow>,
        tasks: Vec<BackgroundShellTask>,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let entry_bg = cx.theme().colors().editor_background;
        let border = cx.theme().colors().border;
        let own_session_id = self.thread.read(cx).session_id().clone();
        let task_count = tasks.len();

        let subagent_rows = subagents.into_iter().enumerate().map(|(index, row)| {
            let status_color = subagent_status_color(row.status);
            let icon = if row.status.is_working() {
                SpinnerLabel::new()
                    .size(LabelSize::Small)
                    .color(status_color)
                    .into_any_element()
            } else {
                Icon::new(IconName::Circle)
                    .size(IconSize::XSmall)
                    .color(status_color)
                    .into_any_element()
            };
            let session_id = row.session_id.clone();
            let tooltip_text: SharedString = match &row.error {
                Some(error) => error.clone(),
                None => i18n::t!("Open Subagent").into(),
            };
            h_flex()
                .id(("background_work_subagent", index))
                .debug_selector(move || format!("background-work-subagent-{index}"))
                .py_1()
                .px_2()
                .gap_2()
                .w_full()
                .min_w_0()
                .bg(entry_bg)
                .border_b_1()
                .border_color(border)
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(h_flex().w_4().flex_none().justify_center().child(icon))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap_1p5()
                                .min_w_0()
                                .child(Label::new(row.name).size(LabelSize::Small).truncate())
                                .child(
                                    Label::new(subagent_status_label(row.status))
                                        .size(LabelSize::XSmall)
                                        .color(status_color)
                                        .flex_none(),
                                ),
                        )
                        .when_some(row.description, |this, description| {
                            this.child(
                                Label::new(description)
                                    .size(LabelSize::XSmall)
                                    .color(Color::Muted)
                                    .truncate(),
                            )
                        })
                        .child(
                            Label::new(model_and_effort_label(
                                row.model.as_deref(),
                                row.effort.as_deref(),
                            ))
                            .size(LabelSize::XSmall)
                            .color(Color::Muted)
                            .truncate(),
                        ),
                )
                .child(
                    Label::new(duration_alt_display(row.elapsed))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .flex_none(),
                )
                .tooltip(Tooltip::text(tooltip_text))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let session_id = session_id.clone();
                    this.server_view
                        .update(cx, |server_view, cx| {
                            server_view.navigate_to_thread(session_id, window, cx);
                        })
                        .log_err();
                }))
                .into_any_element()
        });

        let task_header = (task_count > 0).then(|| {
            h_flex()
                .py_0p5()
                .px_2()
                .w_full()
                .bg(entry_bg)
                .border_b_1()
                .border_color(border)
                .child(
                    Label::new(i18n::t!("Background Tasks"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted),
                )
                .into_any_element()
        });

        let task_rows = tasks.into_iter().enumerate().map(|(index, task)| {
            let is_own = task.session_id == own_session_id;
            let BackgroundShellTask {
                session_id,
                tool_call_id,
                label,
            } = task;
            h_flex()
                .id(("background_work_task", index))
                .debug_selector(move || format!("background-work-task-{index}"))
                .py_1()
                .px_2()
                .gap_2()
                .w_full()
                .min_w_0()
                .bg(entry_bg)
                .when(index + 1 < task_count, |this| {
                    this.border_b_1().border_color(border)
                })
                .cursor_pointer()
                .hover(|style| style.bg(cx.theme().colors().element_hover))
                .child(
                    h_flex().w_4().flex_none().justify_center().child(
                        Icon::new(IconName::ToolTerminal)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
                )
                .child(
                    Label::new(label)
                        .size(LabelSize::Small)
                        .buffer_font(cx)
                        .truncate()
                        .flex_1(),
                )
                .child(
                    Label::new(i18n::t!("Started (status unknown)"))
                        .size(LabelSize::XSmall)
                        .color(Color::Muted)
                        .flex_none(),
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    if is_own {
                        let entry_ix = this
                            .thread
                            .read(cx)
                            .tool_call(&tool_call_id)
                            .map(|(entry_ix, _)| entry_ix);
                        if let Some(entry_ix) = entry_ix {
                            this.list_state.scroll_to(ListOffset {
                                item_ix: entry_ix,
                                offset_in_item: px(0.0),
                            });
                            cx.notify();
                        }
                    } else {
                        let session_id = session_id.clone();
                        this.server_view
                            .update(cx, |server_view, cx| {
                                server_view.navigate_to_thread(session_id, window, cx);
                            })
                            .log_err();
                    }
                }))
                .into_any_element()
        });

        v_flex()
            .id("background_work_list")
            .max_h_40()
            .overflow_y_scroll()
            .children(subagent_rows)
            .children(task_header)
            .children(task_rows)
    }
}
