use std::{path::PathBuf, rc::Rc, time::Duration};

use acp_thread::MentionUri;
use agent_client_protocol::schema::v1 as acp;
use editor::Editor;
use gpui::{
    Animation, AnimationExt, AnyView, Context, IntoElement, TaskExt, WeakEntity, Window,
    pulsating_between,
};
use language::Buffer;
use rope::Point;
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{ButtonLike, ContextMenu, TintColor, Tooltip, prelude::*, right_click_menu};
use workspace::{OpenOptions, Workspace};

use crate::mention_set::{AttachmentEstimate, AttachmentMode, format_token_estimate};
use crate::open_abs_path_at_point;

#[derive(IntoElement)]
pub struct MentionCrease {
    id: ElementId,
    icon: SharedString,
    label: SharedString,
    mention_uri: Option<MentionUri>,
    workspace: Option<WeakEntity<Workspace>>,
    is_toggled: bool,
    is_loading: bool,
    tooltip: Option<SharedString>,
    image_preview: Option<Box<dyn Fn(&mut Window, &mut App) -> AnyView + 'static>>,
    estimate: Option<AttachmentEstimate>,
    on_mode_change: Option<Rc<dyn Fn(AttachmentMode, &mut App)>>,
}

impl MentionCrease {
    pub fn new(
        id: impl Into<ElementId>,
        icon: impl Into<SharedString>,
        label: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon: icon.into(),
            label: label.into(),
            mention_uri: None,
            workspace: None,
            is_toggled: false,
            is_loading: false,
            tooltip: None,
            image_preview: None,
            estimate: None,
            on_mode_change: None,
        }
    }

    pub fn estimate(mut self, estimate: Option<AttachmentEstimate>) -> Self {
        self.estimate = estimate;
        self
    }

    pub fn on_mode_change(mut self, on_mode_change: Rc<dyn Fn(AttachmentMode, &mut App)>) -> Self {
        self.on_mode_change = Some(on_mode_change);
        self
    }

    pub fn mention_uri(mut self, mention_uri: Option<MentionUri>) -> Self {
        self.mention_uri = mention_uri;
        self
    }

    pub fn workspace(mut self, workspace: Option<WeakEntity<Workspace>>) -> Self {
        self.workspace = workspace;
        self
    }

    pub fn is_toggled(mut self, is_toggled: bool) -> Self {
        self.is_toggled = is_toggled;
        self
    }

    pub fn is_loading(mut self, is_loading: bool) -> Self {
        self.is_loading = is_loading;
        self
    }

    pub fn tooltip(mut self, tooltip: impl Into<SharedString>) -> Self {
        self.tooltip = Some(tooltip.into());
        self
    }

    pub fn image_preview(
        mut self,
        builder: impl Fn(&mut Window, &mut App) -> AnyView + 'static,
    ) -> Self {
        self.image_preview = Some(Box::new(builder));
        self
    }
}

impl RenderOnce for MentionCrease {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let settings = ThemeSettings::get_global(cx);
        let font_size = settings.agent_buffer_font_size(cx);
        let buffer_font = settings.buffer_font.clone();
        let is_loading = self.is_loading;
        let image_preview = self.image_preview;
        let estimate = self.estimate;
        let tooltip = match (self.tooltip, estimate) {
            (tooltip, Some(estimate)) => estimate_tooltip(tooltip, &estimate),
            (tooltip, None) => tooltip,
        };
        let mode_menu = self
            .on_mode_change
            .zip(estimate.filter(|estimate| estimate.switchable));
        let id = self.id;

        let button_height = DefiniteLength::Absolute(AbsoluteLength::Pixels(
            px(window.line_height().into()) - px(1.),
        ));

        let button = ButtonLike::new(id.clone())
            .style(ButtonStyle::Outlined)
            .size(ButtonSize::Compact)
            .height(button_height)
            .selected_style(ButtonStyle::Tinted(TintColor::Accent))
            .toggle_state(self.is_toggled)
            .when_some(
                self.mention_uri.clone().zip(self.workspace.clone()),
                |this, (mention_uri, workspace)| {
                    this.on_click(move |_event, window, cx| {
                        open_mention_uri(mention_uri.clone(), &workspace, window, cx);
                    })
                },
            )
            .child(
                h_flex()
                    .pb_px()
                    .gap_1()
                    .font(buffer_font)
                    .text_size(font_size)
                    .child(
                        Icon::from_path(self.icon.clone())
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    )
                    .child(self.label.clone())
                    .when_some(
                        estimate.and_then(|estimate| Some((estimate_label(&estimate)?, estimate))),
                        |this, (label, estimate)| {
                            this.child(Label::new(label).size(LabelSize::Small).color(
                                if estimate.reduced_for_budget {
                                    Color::Warning
                                } else {
                                    Color::Muted
                                },
                            ))
                        },
                    )
                    .map(|this| {
                        if is_loading {
                            this.with_animation(
                                "loading-context-crease",
                                Animation::new(Duration::from_secs(2))
                                    .repeat()
                                    .with_easing(pulsating_between(0.4, 0.8)),
                                |label, delta| label.opacity(delta),
                            )
                            .into_any()
                        } else {
                            this.into_any()
                        }
                    }),
            )
            .map(|button| {
                if let Some(image_preview) = image_preview {
                    button.hoverable_tooltip(image_preview)
                } else {
                    button.when_some(tooltip, |this, tooltip_text| {
                        this.tooltip(Tooltip::text(tooltip_text))
                    })
                }
            });

        match mode_menu {
            Some((on_mode_change, estimate)) => right_click_menu(id)
                .trigger(move |_, _, _| button)
                .menu(move |window, cx| {
                    let on_mode_change = on_mode_change.clone();
                    ContextMenu::build(window, cx, move |mut menu, _, _| {
                        for (mode, label) in [
                            (AttachmentMode::Auto, "自動 (大きければアウトライン)"),
                            (AttachmentMode::Full, "全文で送る"),
                            (AttachmentMode::Summary, "要約で送る"),
                        ] {
                            let on_mode_change = on_mode_change.clone();
                            menu = menu.toggleable_entry(
                                label,
                                estimate.mode == mode,
                                IconPosition::Start,
                                None,
                                move |_, cx| on_mode_change(mode, cx),
                            );
                        }
                        menu
                    })
                })
                .into_any_element(),
            None => button.into_any_element(),
        }
    }
}

/// `~4.1k`, with the mode when the user picked one. Attachments sent as a
/// bare link (a directory the agent reads with its tools) have no size to
/// show.
fn estimate_label(estimate: &AttachmentEstimate) -> Option<String> {
    let tokens = (estimate.bytes > 0).then(|| format_token_estimate(estimate.tokens()));
    let mode = match estimate.mode {
        AttachmentMode::Auto => None,
        AttachmentMode::Full => Some("全文"),
        AttachmentMode::Summary => Some("要約"),
    };
    match (tokens, mode) {
        (Some(tokens), Some(mode)) => Some(format!("{tokens} {mode}")),
        (Some(tokens), None) => Some(tokens),
        (None, Some(mode)) => Some(mode.to_string()),
        (None, None) => None,
    }
}

fn estimate_tooltip(
    tooltip: Option<SharedString>,
    estimate: &AttachmentEstimate,
) -> Option<SharedString> {
    let mut lines = tooltip
        .map(|tooltip| vec![tooltip.to_string()])
        .unwrap_or_default();
    if estimate.bytes > 0 {
        lines.push(format!(
            "概算 {} トークン",
            format_token_estimate(estimate.tokens()).trim_start_matches('~')
        ));
    }
    if estimate.reduced_for_budget {
        // Selections, symbols and web pages are reduced to a reference with no
        // content at all, which "outline" alone would hide.
        lines.push("大きいのでアウトライン (または参照だけ) で送ります".to_string());
    }
    if estimate.switchable {
        lines.push("右クリックで全文 / 要約を切り替え".to_string());
    }
    (!lines.is_empty()).then(|| lines.join("\n").into())
}

fn open_mention_uri(
    mention_uri: MentionUri,
    workspace: &WeakEntity<Workspace>,
    window: &mut Window,
    cx: &mut App,
) {
    let Some(workspace) = workspace.upgrade() else {
        return;
    };

    workspace.update(cx, |workspace, cx| match mention_uri {
        MentionUri::File { abs_path } => {
            open_abs_path_at_point(workspace, abs_path, None, window, cx);
        }
        MentionUri::Symbol {
            abs_path,
            line_range,
            ..
        } => {
            open_abs_path_at_point(
                workspace,
                abs_path,
                Some(Point::new(*line_range.start(), 0)),
                window,
                cx,
            );
        }
        MentionUri::Selection {
            abs_path: Some(abs_path),
            line_range,
            column,
        } => {
            open_abs_path_at_point(
                workspace,
                abs_path,
                Some(Point::new(*line_range.start(), column.unwrap_or(0))),
                window,
                cx,
            );
        }
        MentionUri::Directory { abs_path } => {
            reveal_in_project_panel(workspace, abs_path, cx);
        }
        MentionUri::Thread { id, name } => {
            open_thread(workspace, id, name, window, cx);
        }
        MentionUri::Skill {
            skill_file_path, ..
        } => {
            open_skill_file(workspace, skill_file_path, window, cx);
        }
        MentionUri::Rule { name, .. } => {
            open_migrated_rule(workspace, &name, window, cx);
        }
        MentionUri::Fetch { url } => {
            cx.open_url(url.as_str());
        }
        MentionUri::PastedImage { .. }
        | MentionUri::Selection { abs_path: None, .. }
        | MentionUri::Diagnostics { .. }
        | MentionUri::TerminalSelection { .. }
        | MentionUri::GitDiff { .. }
        | MentionUri::MergeConflict { .. } => {}
    });
}

/// Notify the user that rules became skills and open the skill the rule was
/// migrated into. Migrated skills live in the local global skills dir, so the
/// file is always resolved against the local filesystem (local, SSH, or
/// collab). Does nothing else when no matching skill exists.
pub(crate) fn open_migrated_rule(
    workspace: &mut Workspace,
    name: &str,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    struct RulesMigratedToSkillsToast;
    workspace.show_toast(
        workspace::Toast::new(
            workspace::notifications::NotificationId::unique::<RulesMigratedToSkillsToast>(),
            "ルールはスキルに移行されました。",
        )
        .on_click("ドキュメントを表示", |_, cx| {
            cx.open_url("https://zed.dev/docs/ai/skills");
        })
        .autohide(),
        cx,
    );

    let Some(slug) = agent_skills::slugify_skill_name(name) else {
        return;
    };
    let skill_file_path = agent_skills::global_skills_dir()
        .join(slug)
        .join(agent_skills::SKILL_FILE_NAME);

    if workspace.project().read(cx).is_local() {
        // Local project: open the editable on-disk file if it exists.
        if skill_file_path.exists() {
            open_skill_file(workspace, skill_file_path, window, cx);
        }
        return;
    }

    // Remote/collab: `open_abs_path` targets the remote project, where this
    // local file doesn't exist, so read it locally and show it read-only.
    let fs = workspace.app_state().fs.clone();
    cx.spawn_in(window, async move |workspace, cx| {
        let Ok(content) = fs.load(&skill_file_path).await else {
            return Ok(()); // No readable migrated skill: do nothing.
        };
        let title = skill_content_buffer_title(&skill_file_path);
        workspace.update_in(cx, |workspace, window, cx| {
            open_skill_content_buffer(workspace, title, content, window, cx);
        })
    })
    .detach_and_log_err(cx);
}

fn open_skill_file(
    workspace: &mut Workspace,
    skill_file_path: PathBuf,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    // Built-in skills have synthetic paths with no on-disk file, so show their
    // embedded content in a local buffer instead.
    if let Some(content) = agent_skills::builtin_skill_content(&skill_file_path) {
        let title = skill_content_buffer_title(&skill_file_path);
        open_skill_content_buffer(workspace, title, content, window, cx);
        return;
    }

    workspace
        .open_abs_path(
            skill_file_path,
            OpenOptions {
                focus: Some(true),
                ..Default::default()
            },
            window,
            cx,
        )
        .detach_and_log_err(cx);
}

fn skill_content_buffer_title(skill_file_path: &std::path::Path) -> String {
    skill_file_path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".into())
}

/// Open `content` as a local, read-only Markdown buffer, for skills with no
/// openable file in the active project (built-in skills, and migrated rules on
/// remote/collab projects). It's deliberately not registered with the project's
/// buffer store: that keeps it out of search and avoids
/// `Project::create_local_buffer` panicking on remote projects.
fn open_skill_content_buffer(
    workspace: &mut Workspace,
    title: String,
    content: impl Into<String>,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    let languages = workspace.project().read(cx).languages().clone();
    let buffer = cx.new(|cx| Buffer::local(content, cx));
    // Set markdown highlighting asynchronously — the buffer
    // opens instantly and the highlighting appears once loaded.
    cx.spawn({
        let buffer = buffer.clone();
        async move |_, cx| {
            if let Ok(markdown) = languages.language_for_name("Markdown").await {
                buffer.update(cx, |buffer, cx| buffer.set_language(Some(markdown), cx));
            }
        }
    })
    .detach();
    let editor = cx.new(|cx| {
        let mut editor = Editor::for_buffer(buffer, None, window, cx);
        editor.set_read_only(true);
        editor
            .buffer()
            .update(cx, |buffer, cx| buffer.set_title(title, cx));
        editor
    });
    let pane = workspace.active_pane().clone();
    workspace.add_item(pane, Box::new(editor), None, true, true, window, cx);
}

fn reveal_in_project_panel(
    workspace: &mut Workspace,
    abs_path: PathBuf,
    cx: &mut Context<Workspace>,
) {
    let project = workspace.project();
    let Some(entry_id) = project.update(cx, |project, cx| {
        let path = project.find_project_path(&abs_path, cx)?;
        project.entry_for_path(&path, cx).map(|entry| entry.id)
    }) else {
        return;
    };

    project.update(cx, |_, cx| {
        cx.emit(project::Event::RevealInProjectPanel(entry_id));
    });
}

fn open_thread(
    workspace: &mut Workspace,
    id: acp::SessionId,
    name: String,
    window: &mut Window,
    cx: &mut Context<Workspace>,
) {
    use crate::{Agent, AgentPanel, AgentThreadSource, thread_metadata_store::ThreadMetadataStore};

    let Some(panel) = workspace.panel::<AgentPanel>(cx) else {
        return;
    };

    // Right now we only support loading threads in the native agent.
    panel.update(cx, |panel, cx| {
        let thread_id = ThreadMetadataStore::try_global(cx)
            .and_then(|store| store.read(cx).entry_by_session(&id).map(|m| m.thread_id));
        if let Some(thread_id) = thread_id {
            panel.load_agent_thread(
                Agent::NativeAgent,
                thread_id,
                None,
                Some(name.into()),
                true,
                AgentThreadSource::AgentPanel,
                window,
                cx,
            );
        } else {
            panel.open_thread(id, None, Some(name.into()), window, cx);
        }
    });
}
