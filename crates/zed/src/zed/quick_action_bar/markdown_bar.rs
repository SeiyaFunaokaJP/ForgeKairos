use editor::actions::MermaidDiagramKind;
use editor::{Editor, EditorEvent, EditorSettings, actions as editor_actions};
use gpui::{
    Action, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Subscription,
    WeakEntity,
};
use markdown_preview::markdown_preview_view::MarkdownPreviewView;
use settings::{Settings, SettingsStore, update_settings_file};
use ui::{
    ButtonLike, ContextMenu, Divider, PopoverMenu, Switch, SwitchLabelPosition, Tooltip, prelude::*,
};
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace};

/// The name the `languages` settings map uses for Markdown. It is a settings
/// key, so it stays in English even though the row's labels are translated.
const MARKDOWN_LANGUAGE_NAME: &str = "Markdown";

/// A toolbar row of Markdown formatting buttons, shown under the tab bar while
/// a Markdown file is open.
pub struct MarkdownFormatBar {
    active_editor: Option<ActiveEditor>,
    enabled: bool,
    workspace: WeakEntity<Workspace>,
    _settings_subscription: Subscription,
}

struct ActiveEditor {
    editor: Entity<Editor>,
    is_markdown: bool,
    _subscription: Subscription,
}

impl MarkdownFormatBar {
    pub fn new(workspace: &Workspace, cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            let enabled = EditorSettings::get_global(cx).toolbar.markdown_format_bar;
            if enabled != this.enabled {
                this.enabled = enabled;
                cx.emit(ToolbarItemEvent::ChangeLocation(this.location()));
            }
            // `hard_break_on_newline` is read while rendering, so the toggle has
            // to be repainted whenever any settings change lands.
            cx.notify();
        });

        Self {
            active_editor: None,
            enabled: EditorSettings::get_global(cx).toolbar.markdown_format_bar,
            workspace: workspace.weak_handle(),
            _settings_subscription: settings_subscription,
        }
    }

    fn location(&self) -> ToolbarItemLocation {
        if self.enabled
            && self
                .active_editor
                .as_ref()
                .is_some_and(|active| active.is_markdown)
        {
            ToolbarItemLocation::Secondary
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    /// A buffer's language is resolved asynchronously, so an editor handed over
    /// by the pane is often not Markdown *yet*: `Pane::activate_item` updates the
    /// toolbar before it even focuses the item. Re-decide once the buffer has
    /// been parsed, otherwise the row stays hidden until the tab happens to be
    /// activated a second time.
    fn active_editor_language_changed(&mut self, cx: &mut Context<Self>) {
        let Some(active) = self.active_editor.as_ref() else {
            return;
        };
        let is_markdown = MarkdownPreviewView::is_markdown_file(&active.editor, cx);
        if active.is_markdown == is_markdown {
            return;
        }
        if let Some(active) = self.active_editor.as_mut() {
            active.is_markdown = is_markdown;
        }
        cx.emit(ToolbarItemEvent::ChangeLocation(self.location()));
        cx.notify();
    }

    fn render_hard_break_toggle(
        &self,
        editor: &Entity<Editor>,
        focus_handle: &FocusHandle,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let fs = self
            .workspace
            .upgrade()
            .map(|workspace| workspace.read(cx).app_state().fs.clone())?;

        // Read the value that actually governs this buffer rather than the user
        // setting the toggle writes, so the toggle still tells the truth when a
        // project-local settings layer overrides it.
        let enabled = editor
            .read(cx)
            .buffer()
            .read(cx)
            .language_settings(cx)
            .hard_break_on_newline;

        let toggle = Switch::new("markdown-hard-break-switch", enabled.into())
            .label("Enter改行")
            .label_position(SwitchLabelPosition::Start)
            .on_click(move |state, _window, cx| {
                let enabled = state.selected();
                update_settings_file(fs.clone(), cx, move |settings, _| {
                    settings
                        .languages_mut()
                        .entry(MARKDOWN_LANGUAGE_NAME.to_string())
                        .or_default()
                        .hard_break_on_newline = Some(enabled);
                });
            });

        Some(
            h_flex()
                .id("markdown-hard-break")
                .child(toggle)
                .tooltip({
                    let focus_handle = focus_handle.clone();
                    move |_window, cx| {
                        Tooltip::with_meta_in(
                            "Enter で行末スペース補完",
                            Some(&editor_actions::NewlineInvertHardBreak),
                            "Enter で改行するときに、行末へ半角スペース 2 つ (hard break) を\
                             補うので、改行が保存後もそのまま残ります。\
                             このキーでは 1 回だけ反対の動きをします。\
                             プレビューは既定で改行をそのまま改行として表示するので、\
                             これは他のツールでも改行にしたいときに使います。",
                            &focus_handle,
                            cx,
                        )
                    }
                })
                .into_any_element(),
        )
    }
}

/// A button that hands the action to the editor the row belongs to, rather than
/// to whatever currently holds focus.
fn format_button(
    id: &'static str,
    tooltip: &'static str,
    action: Box<dyn Action>,
    focus_handle: &FocusHandle,
    content: AnyElement,
) -> ButtonLike {
    ButtonLike::new(id)
        .style(ButtonStyle::Subtle)
        .child(content)
        .tooltip({
            let action = action.boxed_clone();
            let focus_handle = focus_handle.clone();
            move |_window, cx| Tooltip::for_action_in(tooltip, action.as_ref(), &focus_handle, cx)
        })
        .on_click({
            let focus_handle = focus_handle.clone();
            move |_, window, cx| focus_handle.dispatch_action(action.as_ref(), window, cx)
        })
}

fn text_format_button(
    id: &'static str,
    label: Label,
    tooltip: &'static str,
    action: Box<dyn Action>,
    focus_handle: &FocusHandle,
) -> ButtonLike {
    format_button(
        id,
        tooltip,
        action,
        focus_handle,
        h_flex()
            .min_w(rems_from_px(12.0_f32))
            .justify_center()
            .child(label)
            .into_any_element(),
    )
}

fn icon_format_button(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    action: Box<dyn Action>,
    focus_handle: &FocusHandle,
) -> ButtonLike {
    format_button(
        id,
        tooltip,
        action,
        focus_handle,
        Icon::new(icon)
            .size(IconSize::Small)
            .color(Color::Default)
            .into_any_element(),
    )
}

/// A button that opens a menu instead of dispatching one action. The menu is
/// built with the editor's focus handle as its context, so the entries reach
/// the Markdown file rather than the toolbar.
fn menu_format_button(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    focus_handle: &FocusHandle,
    entries: impl Fn(ContextMenu) -> ContextMenu + 'static,
) -> impl IntoElement {
    let focus_handle = focus_handle.clone();
    PopoverMenu::new(id)
        .trigger_with_tooltip(
            ButtonLike::new(id).style(ButtonStyle::Subtle).child(
                h_flex()
                    .gap_0p5()
                    .child(Icon::new(icon).size(IconSize::Small).color(Color::Default))
                    .child(
                        Icon::new(IconName::ChevronDown)
                            .size(IconSize::XSmall)
                            .color(Color::Muted),
                    ),
            ),
            Tooltip::text(tooltip),
        )
        .menu(move |window, cx| {
            let focus_handle = focus_handle.clone();
            Some(ContextMenu::build(window, cx, |menu, _, _| {
                entries(menu.context(focus_handle))
            }))
        })
}

impl Render for MarkdownFormatBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let row = h_flex().id("markdown-format-bar");

        let Some(editor) = self
            .active_editor
            .as_ref()
            .filter(|active| active.is_markdown)
            .map(|active| active.editor.clone())
        else {
            return row;
        };
        let focus_handle = editor.focus_handle(cx);

        let inline = h_flex()
            .gap(DynamicSpacing::Base01.rems(cx))
            .child(text_format_button(
                "markdown-bold",
                Label::new("B")
                    .size(LabelSize::Small)
                    .weight(FontWeight::BOLD),
                "太字",
                editor_actions::ToggleBold.boxed_clone(),
                &focus_handle,
            ))
            .child(text_format_button(
                "markdown-italic",
                Label::new("I").size(LabelSize::Small).italic(),
                "斜体",
                editor_actions::ToggleItalic.boxed_clone(),
                &focus_handle,
            ))
            .child(text_format_button(
                "markdown-strikethrough",
                Label::new("S").size(LabelSize::Small).strikethrough(),
                "取り消し線",
                editor_actions::ToggleStrikethrough.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-inline-code",
                IconName::Code,
                "インラインコード",
                editor_actions::ToggleInlineCode.boxed_clone(),
                &focus_handle,
            ));

        let headings = h_flex()
            .gap(DynamicSpacing::Base01.rems(cx))
            .child(text_format_button(
                "markdown-heading-1",
                Label::new("H1").size(LabelSize::Small),
                "見出し 1",
                editor_actions::ToggleHeading1.boxed_clone(),
                &focus_handle,
            ))
            .child(text_format_button(
                "markdown-heading-2",
                Label::new("H2").size(LabelSize::Small),
                "見出し 2",
                editor_actions::ToggleHeading2.boxed_clone(),
                &focus_handle,
            ))
            .child(text_format_button(
                "markdown-heading-3",
                Label::new("H3").size(LabelSize::Small),
                "見出し 3",
                editor_actions::ToggleHeading3.boxed_clone(),
                &focus_handle,
            ));

        let blocks = h_flex()
            .gap(DynamicSpacing::Base01.rems(cx))
            .child(icon_format_button(
                "markdown-bullet-list",
                IconName::ListTree,
                "箇条書きリスト",
                editor_actions::ToggleBulletList.boxed_clone(),
                &focus_handle,
            ))
            .child(text_format_button(
                "markdown-numbered-list",
                Label::new("1.").size(LabelSize::Small),
                "番号付きリスト",
                editor_actions::ToggleNumberedList.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-task-list",
                IconName::ListTodo,
                "タスクリスト",
                editor_actions::ToggleTaskList.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-block-quote",
                IconName::Quote,
                "引用",
                editor_actions::ToggleBlockQuote.boxed_clone(),
                &focus_handle,
            ));

        let inserts = h_flex()
            .gap(DynamicSpacing::Base01.rems(cx))
            .child(icon_format_button(
                "markdown-link",
                IconName::Link,
                "リンク",
                editor_actions::InsertLink.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-horizontal-rule",
                IconName::Dash,
                "水平線",
                editor_actions::InsertHorizontalRule.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-code-block",
                IconName::TextSnippet,
                "コードブロック",
                editor_actions::WrapInCodeBlock.boxed_clone(),
                &focus_handle,
            ))
            .child(icon_format_button(
                "markdown-table",
                IconName::Table,
                "表を編集",
                markdown_preview::EditTable.boxed_clone(),
                &focus_handle,
            ));

        let views = h_flex()
            .gap(DynamicSpacing::Base01.rems(cx))
            .child(menu_format_button(
                "markdown-mermaid",
                IconName::GitBranch,
                "Mermaid 図を挿入",
                &focus_handle,
                |mut menu| {
                    for diagram in MermaidDiagramKind::ALL {
                        menu = menu.action(
                            diagram.label(),
                            Box::new(editor_actions::InsertMermaidDiagram { diagram }),
                        );
                    }
                    menu.separator().action(
                        "カーソル位置の図を編集…",
                        markdown_preview::EditMermaid.boxed_clone(),
                    )
                },
            ))
            .child(icon_format_button(
                "markdown-preview",
                IconName::Eye,
                "プレビューを横に開く",
                markdown_preview::OpenPreviewToTheSide.boxed_clone(),
                &focus_handle,
            ))
            .child(menu_format_button(
                "markdown-fold",
                IconName::FoldVertical,
                "見出しで折り畳む",
                &focus_handle,
                |menu| {
                    menu.action(
                        "見出し 1 で畳む",
                        Box::new(editor_actions::FoldHeadings { level: 1 }),
                    )
                    .action(
                        "見出し 2 で畳む",
                        Box::new(editor_actions::FoldHeadings { level: 2 }),
                    )
                    .action(
                        "見出し 3 で畳む",
                        Box::new(editor_actions::FoldHeadings { level: 3 }),
                    )
                    .separator()
                    .action("すべて畳む", Box::new(editor_actions::FoldAllHeadings))
                    .action("すべて展開", Box::new(editor_actions::UnfoldAllHeadings))
                },
            ));

        let separator = || Divider::vertical().inset();

        row.w_full()
            .min_h_8()
            .gap(DynamicSpacing::Base04.rems(cx))
            .child(inline)
            .child(separator())
            .child(headings)
            .child(separator())
            .child(blocks)
            .child(separator())
            .child(inserts)
            .child(separator())
            .child(views)
            .child(div().flex_1())
            .children(self.render_hard_break_toggle(&editor, &focus_handle, cx))
    }
}

impl EventEmitter<ToolbarItemEvent> for MarkdownFormatBar {}

impl ToolbarItemView for MarkdownFormatBar {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn workspace::ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        // `act_as::<Editor>` would also match the Markdown preview, which hands
        // out the editor it is previewing: the row would then offer formatting
        // buttons over a pane that cannot be typed into.
        self.active_editor = active_pane_item
            .and_then(|item| item.downcast::<Editor>())
            .map(|editor| ActiveEditor {
                is_markdown: MarkdownPreviewView::is_markdown_file(&editor, cx),
                _subscription: cx.subscribe(&editor, |this, _, event, cx| {
                    if matches!(
                        event,
                        EditorEvent::Reparsed(_) | EditorEvent::FileHandleChanged
                    ) {
                        this.active_editor_language_changed(cx);
                    }
                }),
                editor,
            });
        cx.notify();
        self.location()
    }
}
