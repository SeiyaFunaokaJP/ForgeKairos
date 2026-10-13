use gpui::{Action as _, Anchor, Entity, Modifiers};
use ui::{
    Checkbox, ContextMenu, ContextMenuEntry, DocumentationSide, PopoverMenu, ToggleState, Tooltip,
    prelude::*, text_for_keystroke,
};

use crate::{
    ClearCells, DeleteColumns, DeleteRows, InsertColumnLeft, InsertColumnRight, InsertRowAbove,
    InsertRowBelow, OpenAsText, TabularDataPreviewPane, ToggleHeaderRow,
    renderer::table_header::ClearAllFilters,
    settings::{FilterSortOrder, TableViewSettings, VerticalAlignment},
};

///// Settings related /////
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
                    i18n::t!("Open Text Beside"),
                )
                .style(ButtonStyle::Subtle)
                .size(ButtonSize::Compact)
                .start_icon(Icon::new(IconName::FileTextOutlined).size(IconSize::Small))
                .tooltip(|_window, cx| {
                    Tooltip::with_meta(
                        i18n::t!("Opens the underlying text in the adjacent pane"),
                        Some(&OpenAsText::default()),
                        i18n::t!(
                            "{} to switch this tab instead",
                            text_for_keystroke(&Modifiers::alt(), "click", cx)
                        ),
                        cx,
                    )
                })
                .on_click(|event, window, cx| {
                    // Alt swaps this very tab for the text one instead of
                    // opening the text beside the grid, which is the only way
                    // to have the file's text in this pane: the two cannot
                    // share it.
                    let in_same_pane = event.modifiers().alt;
                    window.dispatch_action(OpenAsText { in_same_pane }.boxed_clone(), cx);
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
                .label(i18n::t!("Use First Row as Header"))
                .tooltip(Tooltip::text(i18n::t!(
                    "When off, the first row is treated as data too"
                )))
                .on_click(|_state, window, cx| {
                    window.dispatch_action(ToggleHeaderRow.boxed_clone(), cx);
                }),
            )
            .child(self.render_row_and_column_buttons())
            .child(self.render_color_buttons(cx))
            .child(self.render_clear_filters_button())
            .child(self.render_diff_menu(window, cx))
            .child(settings_popover_menu(cx.entity()))
            .child(div().flex_1())
            .child(self.render_grid_status(cx))
            .into_any_element()
    }

    fn render_row_and_column_buttons(&self) -> AnyElement {
        h_flex()
            .gap_1()
            .items_center()
            .child(
                Label::new(i18n::t!("Rows"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(toolbar_action_button(
                "tabular-insert-row-above",
                IconName::ArrowUp,
                i18n::tc!("table grid", "Insert Row Above"),
                InsertRowAbove.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-insert-row-below",
                IconName::ArrowDown,
                i18n::tc!("table grid", "Insert Row Below"),
                InsertRowBelow.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-delete-rows",
                IconName::Trash,
                i18n::t!("Delete Selected Rows"),
                DeleteRows.boxed_clone(),
            ))
            .child(
                Label::new(i18n::t!("Columns"))
                    .size(LabelSize::Small)
                    .color(Color::Muted),
            )
            .child(toolbar_action_button(
                "tabular-insert-column-left",
                IconName::ArrowLeft,
                i18n::tc!("table grid", "Insert Column Left"),
                InsertColumnLeft.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-insert-column-right",
                IconName::ArrowRight,
                i18n::tc!("table grid", "Insert Column Right"),
                InsertColumnRight.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-delete-columns",
                IconName::Trash,
                i18n::t!("Delete Selected Columns"),
                DeleteColumns.boxed_clone(),
            ))
            .child(toolbar_action_button(
                "tabular-clear-cells",
                IconName::Eraser,
                i18n::t!("Clear Selection Contents"),
                ClearCells.boxed_clone(),
            ))
            .into_any_element()
    }

    /// Drops every column's filter at once.
    ///
    /// Sits in the toolbar rather than only in the column menus for two reasons:
    /// getting back out of a table narrowed on several columns otherwise takes a
    /// visit to each one, and a button that reads "絞り込み" is how a user who
    /// never noticed the headings' arrows learns that the grid filters at all.
    fn render_clear_filters_button(&self) -> AnyElement {
        Button::new(
            ElementId::Name("tabular-clear-all-filters".into()),
            i18n::t!("Clear Filters"),
        )
        .style(ButtonStyle::Subtle)
        .size(ButtonSize::Compact)
        .start_icon(Icon::new(IconName::Filter).size(IconSize::Small))
        .disabled(!self.engine.has_any_filters())
        .tooltip(Tooltip::text(i18n::t!(
            "Clears the filters of every column. Filter a single column from the ▾ in its heading"
        )))
        .on_click(|_event, window, cx| {
            window.dispatch_action(ClearAllFilters.boxed_clone(), cx);
        })
        .into_any_element()
    }

    fn render_grid_status(&self, cx: &Context<Self>) -> AnyElement {
        let header_rows = usize::from(self.has_header_row());
        let total_rows = self.engine.contents.rows.len() + header_rows;
        let shown_rows = self.body_row_count() + header_rows;
        let cols = self.grid_column_count();
        let rows = if shown_rows == total_rows {
            i18n::tn!(total_rows, "{count} row", "{count} rows")
        } else {
            i18n::tn!(
                total_rows,
                "{shown_rows} of {count} row",
                "{shown_rows} of {count} rows"
            )
        };
        let columns = i18n::tn!(cols, "{count} column", "{count} columns");
        let counts = format!("{rows} × {columns}");

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
                    Label::new(i18n::t!("Alt+Enter for a line break in a cell"))
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
                    Label::new(i18n::t!("Loading…"))
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

/// Adds a toggleable entry that applies `set` to the pane's settings when clicked. `description`,
/// when given, shows as a documentation aside explaining what the entry does.
fn toggle_entry(
    menu: ContextMenu,
    label: &'static str,
    description: Option<&'static str>,
    selected: bool,
    view_entity: &Entity<TabularDataPreviewPane>,
    set: impl Fn(&mut TableViewSettings) + 'static,
) -> ContextMenu {
    let view_entity = view_entity.clone();
    let entry = ContextMenuEntry::new(label)
        .toggleable(IconPosition::Start, selected)
        .handler(move |_, cx| {
            view_entity.update(cx, |this, cx| {
                set(&mut this.settings);
                cx.notify();
            });
        });
    let entry = if let Some(description) = description {
        entry.documentation_aside(DocumentationSide::Right, move |_| {
            Label::new(description).into_any_element()
        })
    } else {
        entry
    };
    menu.item(entry)
}

pub(crate) fn settings_popover_menu(
    view_entity: Entity<TabularDataPreviewPane>,
) -> PopoverMenu<ContextMenu> {
    PopoverMenu::new("table-settings-menu")
        .trigger_with_tooltip(
            // Not upstream's filter icon: the grid's toolbar, where this menu sits, already has
            // a filter-icon button that clears the column filters.
            IconButton::new("table-settings-trigger", IconName::Settings)
                .icon_size(IconSize::Small)
                .size(ButtonSize::Compact),
            Tooltip::text(i18n::t!("Table Settings")),
        )
        .anchor(Anchor::TopRight)
        .menu(move |window, cx| {
            let view_entity = view_entity.clone();
            Some(ContextMenu::build_persistent(
                window,
                cx,
                move |menu, _window, cx| {
                    let settings = view_entity.read(cx).settings.clone();

                    let menu = toggle_entry(
                        menu.header(i18n::t!("Text Alignment")),
                        i18n::t!("Top"),
                        Some(i18n::t!("Choose vertical text alignment within cells")),
                        matches!(settings.vertical_alignment, VerticalAlignment::Top),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Top,
                    );
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("Center"),
                        None,
                        matches!(settings.vertical_alignment, VerticalAlignment::Center),
                        &view_entity,
                        |settings| settings.vertical_alignment = VerticalAlignment::Center,
                    );

                    let menu = menu.separator().header(i18n::t!("Filter Sort"));
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("A-Z, then Count"),
                        Some(i18n::t!("Choose how filter values are sorted in the filter menu")),
                        settings.filter_sort_order == FilterSortOrder::AlphaThenCount,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::AlphaThenCount,
                    );
                    let menu = toggle_entry(
                        menu,
                        i18n::t!("Count, then A-Z"),
                        None,
                        settings.filter_sort_order == FilterSortOrder::CountThenAlpha,
                        &view_entity,
                        |settings| settings.filter_sort_order = FilterSortOrder::CountThenAlpha,
                    );

                    let menu = toggle_entry(
                        menu.separator(),
                        i18n::t!("Display multiline rows"),
                        Some(
                            i18n::t!("When enabled, row height grows to show all content. \
                             When disabled, only the first line is visible — hover a cell to see the rest."),
                        ),
                        settings.multiline_cells_enabled,
                        &view_entity,
                        |settings| settings.multiline_cells_enabled = !settings.multiline_cells_enabled,
                    );

                    #[cfg(feature = "dev-tools")]
                    let menu = append_dev_only_entries(menu, &view_entity, &settings);

                    menu
                },
            ))
        })
}

#[cfg(feature = "dev-tools")]
fn append_dev_only_entries(
    menu: ContextMenu,
    view_entity: &Entity<TabularDataPreviewPane>,
    settings: &TableViewSettings,
) -> ContextMenu {
    use crate::settings::RowRenderMechanism;

    let menu = menu
        .separator()
        .header(i18n::t!("Dev-only: Rendering Mode"));
    let menu = toggle_entry(
        menu,
        i18n::t!("Variable Height"),
        Some(i18n::t!(
            "Dev-only section used for debugging purposes.\n\
             Will be removed on public release of the tabular data preview feature"
        )),
        settings.rendering_with == RowRenderMechanism::VariableList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::VariableList,
    );
    let menu = toggle_entry(
        menu,
        i18n::t!("Uniform Height"),
        None,
        settings.rendering_with == RowRenderMechanism::UniformList,
        view_entity,
        |settings| settings.rendering_with = RowRenderMechanism::UniformList,
    );

    let menu = toggle_entry(
        menu.separator(),
        i18n::t!("Show perf metrics"),
        None,
        settings.show_perf_metrics_overlay,
        view_entity,
        |settings| settings.show_perf_metrics_overlay = !settings.show_perf_metrics_overlay,
    );
    toggle_entry(
        menu,
        i18n::t!("Show cell positions"),
        None,
        settings.show_debug_info,
        view_entity,
        |settings| settings.show_debug_info = !settings.show_debug_info,
    )
}
