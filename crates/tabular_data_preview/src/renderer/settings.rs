use gpui::{Action as _, prelude::FluentBuilder as _};
use ui::{
    ActiveTheme as _, AnyElement, Button, ButtonCommon as _, ButtonSize, ButtonStyle, Checkbox,
    Clickable as _, Color, Context, ContextMenu, DropdownMenu, ElementId, Icon, IconButton,
    IconName, IconPosition, IconSize, IntoElement as _, Label, LabelCommon as _, LabelSize,
    ParentElement as _, Styled as _, ToggleState, Tooltip, Window, div, h_flex,
};

use crate::{
    ClearCells, DeleteColumns, DeleteRows, InsertColumnLeft, InsertColumnRight, InsertRowAbove,
    InsertRowBelow, OpenAsText, TabularDataPreviewPane, ToggleHeaderRow,
    settings::{FilterSortOrder, RowIdentifiers, VerticalAlignment},
};

impl TabularDataPreviewPane {
    /// The toolbar above the grid.
    pub(crate) fn render_settings_panel(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let colors = cx.theme().colors();

        h_flex()
            .gap_3()
            .p_1()
            .bg(colors.surface_background)
            .border_b_1()
            .border_color(colors.border)
            .flex_wrap()
            .child(
                Button::new(
                    ElementId::Name("tabular-open-as-text".into()),
                    "テキストを横に開く",
                )
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .start_icon(Icon::new(IconName::FileTextOutlined).size(IconSize::Small))
                .tooltip(Tooltip::text("元のテキストを隣のペインで開きます"))
                .on_click(|_event, window, cx| {
                    window.dispatch_action(OpenAsText.boxed_clone(), cx);
                }),
            )
            .child(
                Checkbox::new(
                    ElementId::Name("tabular-first-row-is-header".into()),
                    if self.settings.first_row_is_header {
                        ToggleState::Selected
                    } else {
                        ToggleState::Unselected
                    },
                )
                .label("先頭行を見出しにする")
                .tooltip(Tooltip::text("オフにすると 1 行目もデータとして扱います"))
                .on_click(|_state, window, cx| {
                    window.dispatch_action(ToggleHeaderRow.boxed_clone(), cx);
                }),
            )
            .child(self.render_row_and_column_buttons())
            .child(self.render_color_buttons(cx))
            .child(self.render_view_menu(window, cx))
            .child(div().flex_1())
            .child(self.render_grid_status(cx))
            .into_any_element()
    }

    fn render_row_and_column_buttons(&self) -> AnyElement {
        h_flex()
            .gap_1()
            .items_center()
            .child(Label::new("行").size(LabelSize::Small).color(Color::Muted))
            .child(toolbar_action_button(
                "tabular-insert-row-above",
                IconName::ArrowUp,
                "上に行を挿入",
                InsertRowAbove.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-insert-row-below",
                IconName::ArrowDown,
                "下に行を挿入",
                InsertRowBelow.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-delete-rows",
                IconName::Trash,
                "選択中の行を削除",
                DeleteRows.boxed_clone(),
            ))
            .child(Label::new("列").size(LabelSize::Small).color(Color::Muted))
            .child(toolbar_action_button(
                "tabular-insert-column-left",
                IconName::ArrowLeft,
                "左に列を挿入",
                InsertColumnLeft.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-insert-column-right",
                IconName::ArrowRight,
                "右に列を挿入",
                InsertColumnRight.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-delete-columns",
                IconName::Trash,
                "選択中の列を削除",
                DeleteColumns.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-clear-cells",
                IconName::Eraser,
                "選択範囲の内容を消去",
                ClearCells.boxed_clone(),
            ))
            .into_any_element()
    }

    fn render_view_menu(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let view = cx.entity();
        #[cfg(feature = "dev-tools")]
        let dev_tools_view = view.clone();
        let settings = self.settings.clone();
        let menu = ContextMenu::build(window, cx, move |menu, _window, _cx| {
            let menu = menu
                .header("行番号")
                .toggleable_entry(
                    "通し番号",
                    matches!(settings.numbering_type, RowIdentifiers::RowNum),
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.numbering_type = RowIdentifiers::RowNum;
                                this.sync_column_widths(cx);
                                cx.notify();
                            });
                        }
                    },
                )
                .toggleable_entry(
                    "元の行番号",
                    matches!(settings.numbering_type, RowIdentifiers::SrcLines),
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.numbering_type = RowIdentifiers::SrcLines;
                                this.sync_column_widths(cx);
                                cx.notify();
                            });
                        }
                    },
                )
                .separator()
                .header("セル内の配置")
                .toggleable_entry(
                    "上寄せ",
                    matches!(settings.vertical_alignment, VerticalAlignment::Top),
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.vertical_alignment = VerticalAlignment::Top;
                                cx.notify();
                            });
                        }
                    },
                )
                .toggleable_entry(
                    "中央寄せ",
                    matches!(settings.vertical_alignment, VerticalAlignment::Center),
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.vertical_alignment = VerticalAlignment::Center;
                                cx.notify();
                            });
                        }
                    },
                )
                .separator()
                .toggleable_entry(
                    "複数行のセルをすべて表示",
                    settings.multiline_cells_enabled,
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.multiline_cells_enabled =
                                    !this.settings.multiline_cells_enabled;
                                cx.notify();
                            });
                        }
                    },
                )
                .separator()
                .header("絞り込みの並び")
                .toggleable_entry(
                    "名前順",
                    settings.filter_sort_order == FilterSortOrder::AlphaThenCount,
                    IconPosition::Start,
                    None,
                    {
                        let view = view.clone();
                        move |_window, cx| {
                            view.update(cx, |this, cx| {
                                this.settings.filter_sort_order = FilterSortOrder::AlphaThenCount;
                                cx.notify();
                            });
                        }
                    },
                )
                .toggleable_entry(
                    "件数順",
                    settings.filter_sort_order == FilterSortOrder::CountThenAlpha,
                    IconPosition::Start,
                    None,
                    move |_window, cx| {
                        view.update(cx, |this, cx| {
                            this.settings.filter_sort_order = FilterSortOrder::CountThenAlpha;
                            cx.notify();
                        });
                    },
                );

            #[cfg(feature = "dev-tools")]
            let menu = add_dev_only_entries(menu, &settings, dev_tools_view);

            menu
        });

        DropdownMenu::new(ElementId::Name("tabular-view-menu".into()), "表示", menu)
            .trigger_size(ButtonSize::Compact)
            .trigger_tooltip(Tooltip::text("表の見え方を変えます"))
            .into_any_element()
    }

    fn render_grid_status(&self, cx: &Context<Self>) -> AnyElement {
        let header_rows = usize::from(self.has_header_row());
        let total_rows = self.engine.contents.rows.len() + header_rows;
        let shown_rows = self.body_row_count() + header_rows;
        let cols = self.grid_column_count();
        let counts = if shown_rows == total_rows {
            format!("{total_rows} 行 × {cols} 列")
        } else {
            format!("{shown_rows} / {total_rows} 行 × {cols} 列")
        };

        h_flex()
            .gap_2()
            .items_center()
            .child(
                Label::new(self.cursor_address())
                    .size(LabelSize::Small)
                    .color(Color::Accent),
            )
            .child(
                Label::new(counts)
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .when(self.cell_editor_is_multiline(cx), |this| {
                this.child(
                    Label::new("セル内改行は Alt+Enter")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .when_some(self.grid.status_message.clone(), |this, message| {
                this.child(
                    Label::new(message)
                        .size(LabelSize::Small)
                        .color(Color::Warning),
                )
            })
            .when(self.is_parsing, |this| {
                this.child(
                    Label::new("読み込み中…")
                        .size(LabelSize::Small)
                        .color(Color::Muted),
                )
            })
            .into_any_element()
    }
}

fn toolbar_action_button(
    id: &'static str,
    icon: IconName,
    tooltip: &'static str,
    action: Box<dyn gpui::Action>,
) -> IconButton {
    IconButton::new(ElementId::Name(id.into()), icon)
        .icon_size(IconSize::Small)
        .tooltip(Tooltip::text(tooltip))
        .on_click(move |_event, window, cx| {
            window.dispatch_action(action.boxed_clone(), cx);
        })
}

#[cfg(feature = "dev-tools")]
fn add_dev_only_entries(
    menu: ContextMenu,
    settings: &crate::settings::TabularDataPreviewSettings,
    view: gpui::Entity<TabularDataPreviewPane>,
) -> ContextMenu {
    use crate::settings::RowRenderMechanism;

    menu.separator()
        .header("開発用")
        .toggleable_entry(
            "可変行高で描画",
            settings.rendering_with == RowRenderMechanism::VariableList,
            IconPosition::Start,
            None,
            {
                let view = view.clone();
                move |_window, cx| {
                    view.update(cx, |this, cx| {
                        this.settings.rendering_with = RowRenderMechanism::VariableList;
                        cx.notify();
                    });
                }
            },
        )
        .toggleable_entry(
            "均一行高で描画",
            settings.rendering_with == RowRenderMechanism::UniformList,
            IconPosition::Start,
            None,
            {
                let view = view.clone();
                move |_window, cx| {
                    view.update(cx, |this, cx| {
                        this.settings.rendering_with = RowRenderMechanism::UniformList;
                        cx.notify();
                    });
                }
            },
        )
        .toggleable_entry(
            "性能指標を表示",
            settings.show_perf_metrics_overlay,
            IconPosition::Start,
            None,
            {
                let view = view.clone();
                move |_window, cx| {
                    view.update(cx, |this, cx| {
                        this.settings.show_perf_metrics_overlay =
                            !this.settings.show_perf_metrics_overlay;
                        cx.notify();
                    });
                }
            },
        )
        .toggleable_entry(
            "セルの位置を表示",
            settings.show_debug_info,
            IconPosition::Start,
            None,
            {
                move |_window, cx| {
                    view.update(cx, |this, cx| {
                        this.settings.show_debug_info = !this.settings.show_debug_info;
                        cx.notify();
                    });
                }
            },
        )
}
