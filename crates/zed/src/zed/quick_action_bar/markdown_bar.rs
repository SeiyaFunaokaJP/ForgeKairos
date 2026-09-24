use editor::actions::{
    MarkdownMarkerColor, MarkdownTextColor, MarkdownTextSize, MermaidDiagramKind,
};
use editor::{Editor, EditorEvent, EditorSettings, actions as editor_actions};
use gpui::{
    Action, AnyElement, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, Stateful,
    Subscription, WeakEntity,
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
                // Holds the toggle against the right edge of whichever line it
                // lands on once the groups before it have wrapped.
                .ml_auto()
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

/// A menu entry with a square of `rgb` in front of its label, so the color can
/// be seen before it is picked. Like the menu's own entries, it hands the
/// action to the editor the row belongs to.
fn swatch_entry(
    menu: ContextMenu,
    label: &'static str,
    rgb: u32,
    action: Box<dyn Action>,
    focus_handle: &FocusHandle,
) -> ContextMenu {
    let focus_handle = focus_handle.clone();
    menu.custom_entry(
        move |_window, cx| {
            h_flex()
                .gap_2()
                .child(
                    div()
                        .size_3()
                        .rounded_xs()
                        .border_1()
                        .border_color(cx.theme().colors().border)
                        .bg(gpui::rgb(rgb)),
                )
                .child(Label::new(label))
                .into_any_element()
        },
        move |window, cx| focus_handle.dispatch_action(action.as_ref(), window, cx),
    )
}

impl Render for MarkdownFormatBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self
            .active_editor
            .as_ref()
            .filter(|active| active.is_markdown)
            .map(|active| active.editor.clone())
        else {
            return h_flex().id("markdown-format-bar");
        };
        let focus_handle = editor.focus_handle(cx);

        format_bar_row(&focus_handle, cx).children(self.render_hard_break_toggle(
            &editor,
            &focus_handle,
            cx,
        ))
    }
}

/// The row without the trailing toggle, which needs the workspace whose
/// settings file the toggle writes to.
fn format_bar_row(focus_handle: &FocusHandle, cx: &App) -> Stateful<Div> {
    h_flex()
        .id("markdown-format-bar")
        .w_full()
        .min_h_8()
        // The row carries more buttons than a narrow editor has width for, and
        // the toolbar's secondary line is free to grow taller, so the groups
        // wrap onto further lines instead of being squeezed or cut off.
        .flex_wrap()
        .gap_x(DynamicSpacing::Base04.rems(cx))
        .gap_y(DynamicSpacing::Base02.rems(cx))
        .debug_selector(|| "markdown_format_bar".into())
        .children(format_button_groups(focus_handle, cx))
}

/// The formatting buttons, grouped the way the row shows them. Every group but
/// the last carries the divider that follows it, so a wrap can never leave a
/// divider alone at the start of a line.
fn format_button_groups(focus_handle: &FocusHandle, cx: &App) -> Vec<Div> {
    let focus_handle = focus_handle.clone();
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

    let styles = h_flex()
        .gap(DynamicSpacing::Base01.rems(cx))
        .child(
            div()
                .debug_selector(|| "markdown-text-color".into())
                .child(menu_format_button(
                    "markdown-text-color",
                    IconName::Font,
                    "文字色",
                    &focus_handle,
                    {
                        let focus_handle = focus_handle.clone();
                        move |mut menu| {
                            for color in MarkdownTextColor::ALL {
                                menu = swatch_entry(
                                    menu,
                                    color.label(),
                                    color.rgb(),
                                    Box::new(editor_actions::SetTextColor { color }),
                                    &focus_handle,
                                );
                            }
                            menu.separator()
                                .action("色を外す", editor_actions::RemoveTextColor.boxed_clone())
                        }
                    },
                )),
        )
        .child(
            div()
                .debug_selector(|| "markdown-marker".into())
                .child(menu_format_button(
                    "markdown-marker",
                    IconName::Pencil,
                    "マーカー",
                    &focus_handle,
                    {
                        let focus_handle = focus_handle.clone();
                        move |mut menu| {
                            for color in MarkdownMarkerColor::ALL {
                                menu = swatch_entry(
                                    menu,
                                    color.label(),
                                    color.swatch_rgb(),
                                    Box::new(editor_actions::SetMarker { color }),
                                    &focus_handle,
                                );
                            }
                            menu.separator().action(
                                "マーカーを外す",
                                editor_actions::RemoveMarker.boxed_clone(),
                            )
                        }
                    },
                )),
        )
        .child(
            div()
                .debug_selector(|| "markdown-text-size".into())
                .child(menu_format_button(
                    "markdown-text-size",
                    IconName::FontSize,
                    "段落の文字サイズ",
                    &focus_handle,
                    |mut menu| {
                        for size in MarkdownTextSize::ALL {
                            menu = menu.action(
                                size.label(),
                                Box::new(editor_actions::SetTextSize { size }),
                            );
                        }
                        menu.separator()
                            .action("標準に戻す", editor_actions::ResetTextSize.boxed_clone())
                    },
                )),
        );

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
        // The same action edits the image the cursor is on, so one button
        // covers both. The wrapper only names it for tests, since a
        // `ButtonLike` carries no selector of its own.
        .child(
            div()
                .debug_selector(|| "markdown_image_button".into())
                .child(icon_format_button(
                    "markdown-image",
                    IconName::Image,
                    "画像を挿入",
                    markdown_preview::InsertImage.boxed_clone(),
                    &focus_handle,
                )),
        )
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
                    "Mermaid 図を編集…",
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
        // Folding by heading level lives on in the gutter arrows, the
        // editor's own "…" fold submenu and the command palette, so this
        // slot carries the disclosure the headings cannot express instead.
        .child(icon_format_button(
            "markdown-details",
            IconName::FoldVertical,
            "折り畳み (details) を挿入",
            editor_actions::InsertDetails.boxed_clone(),
            &focus_handle,
        ));

    let followed_by_separator = |group: Div| {
        h_flex()
            .gap(DynamicSpacing::Base04.rems(cx))
            .child(group)
            .child(Divider::vertical().inset())
    };

    vec![
        followed_by_separator(inline),
        followed_by_separator(styles),
        followed_by_separator(headings),
        followed_by_separator(blocks),
        followed_by_separator(inserts),
        views,
    ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualTestContext, px, size};

    struct FormatBarTestView {
        focus_handle: FocusHandle,
    }

    impl Render for FormatBarTestView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            // The window's root element is stretched to the window, so the row
            // only reports the height it asked for from inside a column.
            v_flex().child(format_bar_row(&self.focus_handle, cx))
        }
    }

    fn open_format_bar(cx: &mut TestAppContext) -> &mut VisualTestContext {
        cx.update(|cx| {
            if !cx.has_global::<SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });

        let (_bar, cx) = cx.add_window_view(|_window, cx| FormatBarTestView {
            focus_handle: cx.focus_handle(),
        });
        cx
    }

    fn format_bar_height(width: Pixels, cx: &mut VisualTestContext) -> Pixels {
        cx.simulate_resize(size(width, px(600.)));
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();
        cx.debug_bounds("markdown_format_bar")
            .expect("the row is drawn under its debug selector")
            .size
            .height
    }

    #[gpui::test]
    fn format_bar_wraps_instead_of_overflowing_a_narrow_editor(cx: &mut TestAppContext) {
        let cx = open_format_bar(cx);

        let wide = format_bar_height(px(1600.), cx);
        let narrow = format_bar_height(px(240.), cx);

        assert!(
            narrow > wide,
            "a narrow editor should wrap the row onto more lines, \
             got {narrow:?} against {wide:?}"
        );
    }

    /// Renders the row over a focus handle that records the image action, as
    /// the Markdown editor's handle would receive it.
    struct ImageButtonTestView {
        focus_handle: FocusHandle,
        inserted_images: std::rc::Rc<std::cell::Cell<usize>>,
    }

    impl Render for ImageButtonTestView {
        fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            let inserted_images = self.inserted_images.clone();
            v_flex()
                .track_focus(&self.focus_handle)
                .on_action(move |_: &markdown_preview::InsertImage, _window, _cx| {
                    inserted_images.set(inserted_images.get() + 1);
                })
                .child(format_bar_row(&self.focus_handle, cx))
        }
    }

    #[gpui::test]
    fn format_bar_image_button_opens_the_image_dialog(cx: &mut TestAppContext) {
        open_format_bar(cx);
        let inserted_images = std::rc::Rc::new(std::cell::Cell::new(0));
        let (_view, cx) = cx.add_window_view({
            let inserted_images = inserted_images.clone();
            move |_window, cx| ImageButtonTestView {
                focus_handle: cx.focus_handle(),
                inserted_images,
            }
        });
        cx.simulate_resize(size(px(1600.), px(600.)));
        cx.update(|window, _cx| window.refresh());
        cx.run_until_parked();

        let bounds = cx
            .debug_bounds("markdown_image_button")
            .expect("the image button is drawn in the row");
        cx.simulate_click(bounds.center(), gpui::Modifiers::none());
        assert_eq!(
            inserted_images.get(),
            1,
            "clicking the button hands `markdown::InsertImage` to the editor"
        );
    }

    #[gpui::test]
    fn format_bar_keeps_its_dividers_inside_the_groups(cx: &mut TestAppContext) {
        let cx = open_format_bar(cx);

        // Each divider is a child of the group it follows, so the row's own
        // children are only the six groups: a wrapped line can never start
        // with a divider.
        cx.update(|_window, cx| {
            let focus_handle = cx.focus_handle();
            assert_eq!(format_button_groups(&focus_handle, cx).len(), 6);
        });
    }

    #[gpui::test]
    fn format_bar_swatch_entries_send_the_picked_color_to_the_editor(cx: &mut TestAppContext) {
        use std::{cell::RefCell, rc::Rc};

        struct RecordingView {
            focus_handle: FocusHandle,
            picked: Rc<RefCell<Vec<String>>>,
        }

        impl Render for RecordingView {
            fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
                v_flex()
                    .track_focus(&self.focus_handle)
                    .on_action({
                        let picked = self.picked.clone();
                        move |action: &editor_actions::SetTextColor, _, _| {
                            picked.borrow_mut().push(format!("{action:?}"))
                        }
                    })
                    .on_action({
                        let picked = self.picked.clone();
                        move |action: &editor_actions::SetMarker, _, _| {
                            picked.borrow_mut().push(format!("{action:?}"))
                        }
                    })
                    .child(format_bar_row(&self.focus_handle, cx))
            }
        }

        cx.update(|cx| {
            if !cx.has_global::<SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
        let picked = Rc::new(RefCell::new(Vec::new()));
        let (_view, cx) = cx.add_window_view({
            let picked = picked.clone();
            move |_window, cx| RecordingView {
                focus_handle: cx.focus_handle(),
                picked,
            }
        });
        cx.simulate_resize(size(px(1600.), px(600.)));
        cx.run_until_parked();

        // The swatches are custom entries, which hand the action over
        // themselves rather than through the menu's context; the keyboard
        // confirms them through the same handler a click runs.
        for (selector, steps, expected) in [
            (
                "markdown-text-color",
                0,
                format!(
                    "{:?}",
                    editor_actions::SetTextColor {
                        color: MarkdownTextColor::Red
                    }
                ),
            ),
            (
                "markdown-marker",
                1,
                format!(
                    "{:?}",
                    editor_actions::SetMarker {
                        color: MarkdownMarkerColor::Green
                    }
                ),
            ),
        ] {
            picked.borrow_mut().clear();
            let trigger = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} should be drawn"));
            cx.simulate_click(trigger.center(), gpui::Modifiers::none());
            cx.run_until_parked();
            // The popover focuses its menu two frames after it opens.
            for _ in 0..2 {
                cx.update(|window, cx| window.simulate_next_frame(cx));
            }
            cx.dispatch_action(menu::SelectFirst);
            for _ in 0..steps {
                cx.dispatch_action(menu::SelectNext);
            }
            cx.dispatch_action(menu::Confirm);
            cx.run_until_parked();
            assert_eq!(*picked.borrow(), [expected], "{selector}");
        }
    }

    #[gpui::test]
    fn format_bar_offers_text_color_marker_and_size_menus(cx: &mut TestAppContext) {
        let cx = open_format_bar(cx);
        format_bar_height(px(1600.), cx);

        let mut previous_right = None;
        for selector in [
            "markdown-text-color",
            "markdown-marker",
            "markdown-text-size",
        ] {
            let bounds = cx
                .debug_bounds(selector)
                .unwrap_or_else(|| panic!("{selector} should be drawn"));
            assert!(
                bounds.size.width > px(0.) && bounds.size.height > px(0.),
                "{selector} is drawn empty: {bounds:?}"
            );
            // The three sit side by side, in the order the row lists them.
            if let Some(previous_right) = previous_right {
                assert!(bounds.origin.x >= previous_right, "{selector}: {bounds:?}");
            }
            previous_right = Some(bounds.origin.x + bounds.size.width);
        }
    }
}
