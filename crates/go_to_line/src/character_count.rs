use editor::{Editor, EditorMode, MBTextSummary};
use gpui::{App, Entity, Subscription};
use ui::{
    Button, ButtonCommon, Context, FluentBuilder, IntoElement, LabelSize, ParentElement, Render,
    Tooltip, Window, div,
};
use workspace::{HideStatusItem, StatusItemView, Workspace, item::ItemHandle};

/// Unicode スカラー数から改行文字 (`\n`) の分を除いたものを返す。
/// バッファーの内部表現は行末を常に `\n` 1 文字に正規化しているため、
/// `summary.lines.row` は範囲内の改行文字数と一致する。
fn character_count_excluding_newlines(summary: MBTextSummary) -> usize {
    summary.chars.saturating_sub(summary.lines.row as usize)
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct CharacterCountState {
    count: usize,
    is_selection: bool,
}

/// Markdown / プレーンテキストのバッファーを対象に、文字数 (改行を除く Unicode スカラー数) を
/// ステータスバーに表示する。選択範囲があればその文字数、なければ文書全体の文字数を表示する。
pub struct CharacterCount {
    state: Option<CharacterCountState>,
    _observe_active_editor: Option<Subscription>,
}

impl CharacterCount {
    pub fn new(_workspace: &Workspace) -> Self {
        Self {
            state: None,
            _observe_active_editor: None,
        }
    }

    fn update_state(
        &mut self,
        editor: Entity<Editor>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state = Self::compute_state(&editor, cx);
        cx.notify();
    }

    fn compute_state(editor: &Entity<Editor>, cx: &App) -> Option<CharacterCountState> {
        let editor = editor.read(cx);
        if !matches!(editor.mode(), EditorMode::Full { .. }) {
            return None;
        }

        let language_name = editor
            .active_buffer(cx)
            .and_then(|buffer| buffer.read(cx).language().map(|language| language.name()));
        let is_countable_language = language_name.is_some_and(|name| {
            let name = name.as_ref();
            name == "Markdown" || name == "Plain Text"
        });
        if !is_countable_language {
            return None;
        }

        let snapshot = editor.buffer().read(cx).snapshot(cx);
        let mut selected_chars = 0usize;
        let mut has_selection = false;
        for selection in editor.selections.disjoint_anchors() {
            if selection.start == selection.end {
                continue;
            }
            has_selection = true;
            let summary: MBTextSummary =
                snapshot.text_summary_for_range(selection.start..selection.end);
            selected_chars += character_count_excluding_newlines(summary);
        }

        let (count, is_selection) = if has_selection {
            (selected_chars, true)
        } else {
            (
                character_count_excluding_newlines(snapshot.text_summary()),
                false,
            )
        };

        Some(CharacterCountState {
            count,
            is_selection,
        })
    }
}

impl Render for CharacterCount {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().when_some(self.state, |el, state| {
            let text = i18n::tn!(state.count, "{count} character", "{count} characters");
            let tooltip_text = if state.is_selection {
                i18n::t!("Character Count (Selection)")
            } else {
                i18n::t!("Character Count (Whole Document)")
            };

            el.child(
                Button::new("character-count", text)
                    .label_size(LabelSize::Small)
                    .tooltip(Tooltip::text(tooltip_text)),
            )
        })
    }
}

impl StatusItemView for CharacterCount {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(editor) = active_pane_item.and_then(|item| item.act_as::<Editor>(cx)) {
            self._observe_active_editor = Some(cx.observe_in(&editor, window, Self::update_state));
            self.update_state(editor, window, cx);
        } else {
            self.state = None;
            self._observe_active_editor = None;
        }

        cx.notify();
    }

    fn hide_setting(&self, _: &App) -> Option<HideStatusItem> {
        // Markdown / プレーンテキストのときだけ出る小さな項目なので、専用の設定トグルは持たない
        // (ImageInfo と同じ方針: 表示条件そのものが実質的なオン/オフになる)。
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Point;

    fn summary(chars: usize, newline_count: u32) -> MBTextSummary {
        MBTextSummary {
            chars,
            lines: Point::new(newline_count, 0),
            ..Default::default()
        }
    }

    #[test]
    fn excludes_newlines_from_the_character_count() {
        // "abc\ndef\n" は 8 Unicode スカラー、うち 2 個が改行。
        assert_eq!(character_count_excluding_newlines(summary(8, 2)), 6);
    }

    #[test]
    fn handles_text_with_no_newlines() {
        assert_eq!(character_count_excluding_newlines(summary(5, 0)), 5);
    }
}
