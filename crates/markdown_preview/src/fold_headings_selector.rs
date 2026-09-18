//! The picker behind `markdown::FoldHeadingsPicker`.
//!
//! `markdown::FoldHeadings` carries the heading level it folds, and the command
//! palette builds every action from its name alone. An action that cannot be
//! built from an empty payload is therefore left out of the palette, so folding
//! by level had no way in short of a keybinding. This modal is that way in.

use editor::Editor;
use editor::actions::{FoldAllHeadings, FoldHeadings, UnfoldAllHeadings};
use gpui::{
    App, Context, DismissEvent, Entity, EventEmitter, FocusHandle, Focusable, Task, WeakEntity,
    Window,
};
use picker::{Picker, PickerDelegate};
use std::sync::Arc;
use ui::{ListItem, ListItemSpacing, prelude::*};
use util::ResultExt as _;
use workspace::{ModalView, Workspace};

use crate::FoldHeadingsPicker;
use crate::markdown_preview_view::MarkdownPreviewView;

/// The deepest heading Markdown has.
const MAX_HEADING_LEVEL: u32 = 6;

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &FoldHeadingsPicker, window, cx| {
        let Some(editor) =
            MarkdownPreviewView::resolve_active_item_as_markdown_editor(workspace, cx)
        else {
            return;
        };
        workspace.toggle_modal(window, cx, move |window, cx| {
            FoldHeadingsSelector::new(editor.downgrade(), window, cx)
        });
    });
}

pub struct FoldHeadingsSelector {
    picker: Entity<Picker<FoldHeadingsSelectorDelegate>>,
}

impl FoldHeadingsSelector {
    fn new(editor: WeakEntity<Editor>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let delegate = FoldHeadingsSelectorDelegate {
            selector: cx.entity().downgrade(),
            editor,
            matches: FoldHeadingsChoice::all(),
            selected_index: 0,
        };
        let picker = cx.new(|cx| Picker::uniform_list(delegate, window, cx));
        Self { picker }
    }
}

impl Render for FoldHeadingsSelector {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        v_flex().w(rems(24.)).child(self.picker.clone())
    }
}

impl Focusable for FoldHeadingsSelector {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl EventEmitter<DismissEvent> for FoldHeadingsSelector {}
impl ModalView for FoldHeadingsSelector {}

/// One row of the picker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FoldHeadingsChoice {
    /// Fold the body of every heading of this level.
    Level(u32),
    /// Fold every heading, whatever its level.
    Everything,
    Unfold,
}

impl FoldHeadingsChoice {
    fn all() -> Vec<Self> {
        (1..=MAX_HEADING_LEVEL)
            .map(Self::Level)
            .chain([Self::Everything, Self::Unfold])
            .collect()
    }

    fn label(self) -> String {
        match self {
            Self::Level(level) => format!("見出しレベル {level} で畳む"),
            Self::Everything => "見出しをすべて畳む".to_string(),
            Self::Unfold => "見出しをすべて展開".to_string(),
        }
    }

    /// The heading marks the row folds, so `###` finds level 3 as readily as
    /// its name does.
    fn hint(self) -> Option<String> {
        match self {
            Self::Level(level) => Some("#".repeat(level as usize)),
            Self::Everything | Self::Unfold => None,
        }
    }

    fn matches_query(self, query: &str) -> bool {
        query.is_empty()
            || self.label().contains(query)
            || self.hint().is_some_and(|hint| hint == query)
    }
}

struct FoldHeadingsSelectorDelegate {
    selector: WeakEntity<FoldHeadingsSelector>,
    editor: WeakEntity<Editor>,
    matches: Vec<FoldHeadingsChoice>,
    selected_index: usize,
}

impl PickerDelegate for FoldHeadingsSelectorDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "markdown fold headings selector"
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "畳む見出しのレベルを選択…".into()
    }

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_index = ix;
    }

    fn update_matches(
        &mut self,
        query: String,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        let query = query.trim();
        self.matches = FoldHeadingsChoice::all()
            .into_iter()
            .filter(|choice| choice.matches_query(query))
            .collect();
        self.selected_index = 0;
        Task::ready(())
    }

    fn confirm(&mut self, _: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(choice) = self.matches.get(self.selected_index).copied() {
            self.editor
                .update(cx, |editor, cx| match choice {
                    FoldHeadingsChoice::Level(level) => {
                        editor.fold_headings(&FoldHeadings { level }, window, cx)
                    }
                    FoldHeadingsChoice::Everything => {
                        editor.fold_all_headings(&FoldAllHeadings, window, cx)
                    }
                    FoldHeadingsChoice::Unfold => {
                        editor.unfold_all_headings(&UnfoldAllHeadings, window, cx)
                    }
                })
                .log_err();
        }
        self.dismissed(window, cx);
    }

    fn dismissed(&mut self, _window: &mut Window, cx: &mut Context<Picker<Self>>) {
        self.selector
            .update(cx, |_, cx| cx.emit(DismissEvent))
            .log_err();
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let choice = *self.matches.get(ix)?;
        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(Label::new(choice.label()))
                .when_some(choice.hint(), |this, hint| {
                    this.end_slot(Label::new(hint).size(LabelSize::Small).color(Color::Muted))
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor::test::editor_test_context::EditorTestContext;
    use gpui::{TestAppContext, VisualTestContext};
    use indoc::indoc;

    const HEADINGS: &str = indoc! {"
        # 章

        章の本文

        ## 節

        節の本文

        ## 次の節

        次の節の本文
    "};

    #[test]
    fn every_heading_level_is_offered_with_a_way_to_fold_and_unfold_them_all() {
        let choices = FoldHeadingsChoice::all();

        assert_eq!(choices.len(), 8, "{choices:?}");
        assert_eq!(choices.first(), Some(&FoldHeadingsChoice::Level(1)));
        assert_eq!(choices.get(5), Some(&FoldHeadingsChoice::Level(6)));
        assert_eq!(choices.get(6), Some(&FoldHeadingsChoice::Everything));
        assert_eq!(choices.get(7), Some(&FoldHeadingsChoice::Unfold));
    }

    #[test]
    fn a_query_narrows_the_rows_by_level_or_by_name() {
        let matching = |query: &str| {
            FoldHeadingsChoice::all()
                .into_iter()
                .filter(|choice| choice.matches_query(query))
                .collect::<Vec<_>>()
        };

        assert_eq!(matching("3"), vec![FoldHeadingsChoice::Level(3)]);
        assert_eq!(matching("###"), vec![FoldHeadingsChoice::Level(3)]);
        assert_eq!(matching("展開"), vec![FoldHeadingsChoice::Unfold]);
        assert_eq!(
            matching("すべて"),
            vec![FoldHeadingsChoice::Everything, FoldHeadingsChoice::Unfold]
        );
        assert_eq!(matching("").len(), 8);
        assert!(matching("存在しない").is_empty());
    }

    async fn open_picker(
        cx: &mut TestAppContext,
    ) -> (
        EditorTestContext,
        Entity<FoldHeadingsSelector>,
        Arc<workspace::AppState>,
    ) {
        let app_state = cx.update(|cx| {
            let state = workspace::AppState::test(cx);
            editor::init(cx);
            crate::init(cx);
            state
        });
        let mut cx = EditorTestContext::new(cx).await;
        cx.update_buffer(|buffer, cx| buffer.set_language(Some(language::markdown_lang()), cx));
        cx.set_state(&format!("ˇ{HEADINGS}"));
        cx.run_until_parked();

        let editor = cx.editor.clone();
        let selector = cx.update(|window, cx| {
            cx.new(|cx| FoldHeadingsSelector::new(editor.downgrade(), window, cx))
        });
        (cx, selector, app_state)
    }

    fn confirm_row(
        selector: &Entity<FoldHeadingsSelector>,
        query: &str,
        cx: &mut VisualTestContext,
    ) {
        // The picker is taken out of the selector first: `confirm` dismisses
        // the selector, and updating it from inside its own update would panic.
        let picker = selector.read_with(cx, |selector, _cx| selector.picker.clone());
        picker.update_in(cx, |picker, window, cx| {
            picker
                .delegate
                .update_matches(query.to_string(), window, cx)
                .detach();
            picker.delegate.confirm(false, window, cx);
        });
    }

    #[gpui::test]
    async fn confirming_a_level_folds_that_level(cx: &mut TestAppContext) {
        let (mut cx, selector, _app_state) = open_picker(cx).await;

        confirm_row(&selector, "2", &mut cx.cx);

        cx.update_editor(|editor, _window, cx| {
            assert_eq!(
                editor.display_text(cx),
                indoc! {"
                    # 章

                    章の本文

                    ## 節⋯

                    ## 次の節⋯
                "},
                "only the level 2 bodies are folded"
            );
        });
    }

    #[gpui::test]
    async fn confirming_the_last_two_rows_folds_and_unfolds_everything(cx: &mut TestAppContext) {
        let (mut cx, selector, _app_state) = open_picker(cx).await;

        confirm_row(&selector, "すべて畳む", &mut cx.cx);
        cx.update_editor(|editor, _window, cx| {
            assert_eq!(
                editor.display_text(cx),
                indoc! {"
                    # 章⋯
                "},
                "the outermost heading hides the rest of the file"
            );
        });

        confirm_row(&selector, "すべて展開", &mut cx.cx);
        cx.update_editor(|editor, _window, cx| {
            assert_eq!(editor.display_text(cx), HEADINGS);
        });
    }
}
