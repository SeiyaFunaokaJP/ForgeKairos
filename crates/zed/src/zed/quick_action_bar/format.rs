use editor::{
    Editor, EditorEvent,
    actions::{Format, FormatSelections},
};
use gpui::{AnyElement, Focusable};
use settings::{Formatter, LanguageServerFormatterSpecifier};
use ui::{Tooltip, prelude::*};
use workspace::ItemHandle;

use super::QuickActionBar;

const FORMAT_DOCUMENT_LABEL: &str = "ファイル全体をフォーマット";
const FORMAT_SELECTION_LABEL: &str = "選択範囲をフォーマット";
const NO_FORMATTER_MESSAGE: &str = "このファイルにはフォーマッタが設定されていません。";

impl QuickActionBar {
    /// The toolbar happens to repaint whenever the editor notifies, but the
    /// enabled state of the selection button must never lag behind the
    /// selection, so the dependency is made explicit instead of relying on a
    /// redraw that some other entity caused.
    pub(super) fn observe_format_target(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        cx: &mut Context<Self>,
    ) {
        self._format_selection_subscription = active_pane_item
            .and_then(|item| item.downcast::<Editor>())
            .map(|editor| {
                cx.subscribe(&editor, |_, _, event: &EditorEvent, cx| {
                    if matches!(event, EditorEvent::SelectionsChanged { .. }) {
                        cx.notify();
                    }
                })
            });
    }

    pub(super) fn render_format_buttons(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        // `downcast` rather than `act_as`: the Markdown preview hands out the
        // editor it renders, and that pane is not the one being formatted.
        let editor = self
            .active_item
            .as_ref()?
            .downcast::<Editor>()
            .filter(|editor| {
                let editor = editor.read(cx);
                editor.mode().is_full() && !editor.read_only(cx)
            })?;

        // Building the display snapshot needs `&mut App`, so it has to happen
        // before the reads below borrow the context.
        let has_selection = editor.update(cx, |editor, cx| {
            editor.has_non_empty_selection(&editor.display_snapshot(cx))
        });

        let editor_ref = editor.read(cx);
        // Formatting runs through the project's LSP store, and both buttons
        // speak about "this file", so a multibuffer (search results, diffs) is
        // out of scope even though it is a full editor.
        let project = editor_ref.project()?.clone();
        let buffer = editor_ref.buffer().read(cx).as_singleton()?;
        let supports_range_formatting = project.read(cx).supports_range_formatting(&buffer, cx);
        let language_settings = editor_ref.buffer().read(cx).language_settings(cx);
        let formatter = formatter_display_name(
            language_settings.formatter.as_ref(),
            language_settings.prettier.allowed,
        );
        let focus_handle = editor_ref.focus_handle(cx);

        let document_tooltip = match &formatter {
            Some(formatter) => format!("このファイルは {formatter} でフォーマットします。"),
            None => NO_FORMATTER_MESSAGE.to_string(),
        };
        let selection_tooltip = match &formatter {
            None => NO_FORMATTER_MESSAGE.to_string(),
            // Reported before the "no selection" case: this one holds for the
            // whole file, so it explains why the button never lights up.
            Some(formatter) if !supports_range_formatting => {
                if external_formatter_missing_line_placeholders(
                    language_settings.formatter.as_ref(),
                ) {
                    format!(
                        "{formatter} は選択範囲だけのフォーマットに対応していません。\
                         引数に {{start_line}} / {{end_line}} を追加すると対応できます。"
                    )
                } else {
                    format!(
                        "{formatter} は選択範囲だけのフォーマットに対応していません。\
                         ファイル全体のフォーマットを使用してください。"
                    )
                }
            }
            Some(_) if !has_selection => {
                "選択範囲がありません。フォーマットする範囲を選択してください。".to_string()
            }
            Some(formatter) => format!("選択範囲を {formatter} でフォーマットします。"),
        };

        let format_document = IconButton::new("format-document", IconName::FormatDocument)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .disabled(formatter.is_none())
            .tooltip({
                let focus_handle = focus_handle.clone();
                move |_window, cx| {
                    Tooltip::with_meta_in(
                        FORMAT_DOCUMENT_LABEL,
                        Some(&Format),
                        document_tooltip.clone(),
                        &focus_handle,
                        cx,
                    )
                }
            })
            .on_click({
                let focus_handle = focus_handle.clone();
                // The action goes to the editor this toolbar belongs to rather
                // than to whatever holds focus, which is the toolbar itself
                // right after the click.
                move |_, window, cx| focus_handle.dispatch_action(&Format, window, cx)
            });

        let format_selection = IconButton::new("format-selection", IconName::FormatSelection)
            .icon_size(IconSize::Small)
            .style(ButtonStyle::Subtle)
            .disabled(formatter.is_none() || !supports_range_formatting || !has_selection)
            .tooltip({
                let focus_handle = focus_handle.clone();
                move |_window, cx| {
                    Tooltip::with_meta_in(
                        FORMAT_SELECTION_LABEL,
                        Some(&FormatSelections),
                        selection_tooltip.clone(),
                        &focus_handle,
                        cx,
                    )
                }
            })
            .on_click(move |_, window, cx| {
                focus_handle.dispatch_action(&FormatSelections, window, cx)
            });

        Some(
            h_flex()
                .gap(DynamicSpacing::Base01.rems(cx))
                .child(format_document)
                .child(format_selection)
                .into_any_element(),
        )
    }
}

/// The formatter that `editor::Format` will actually run on this buffer, named
/// the way the settings name it. `None` means the buffer is configured not to
/// be formatted at all, so both buttons would be no-ops.
fn formatter_display_name(formatters: &[Formatter], prettier_allowed: bool) -> Option<String> {
    let names = formatters
        .iter()
        .filter_map(|formatter| match formatter {
            Formatter::None => None,
            // `Formatter::Auto` picks one of the two while formatting
            // (`LspStore::format_locally`), so name the one it would pick.
            Formatter::Auto if prettier_allowed => Some("自動 (Prettier)".to_string()),
            Formatter::Auto => Some("自動 (言語サーバー)".to_string()),
            Formatter::Prettier => Some("Prettier".to_string()),
            Formatter::External { command, .. } => Some(command.clone()),
            Formatter::CodeAction(name) => Some(name.clone()),
            Formatter::LanguageServer(LanguageServerFormatterSpecifier::Current) => {
                Some("言語サーバー".to_string())
            }
            Formatter::LanguageServer(LanguageServerFormatterSpecifier::Specific { name }) => {
                Some(name.clone())
            }
        })
        .collect::<Vec<_>>();

    (!names.is_empty()).then(|| names.join(" → "))
}

/// An external formatter can only format a selection when its arguments name
/// the line placeholders, and that is something the user can add, so the
/// disabled button says so instead of claiming the tool is incapable.
fn external_formatter_missing_line_placeholders(formatters: &[Formatter]) -> bool {
    formatters.iter().any(|formatter| match formatter {
        Formatter::External { arguments, .. } => {
            !settings::external_formatter_arguments_support_ranges(arguments.as_deref())
        }
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_formatter_display_name() {
        assert_eq!(formatter_display_name(&[], false), None);
        assert_eq!(formatter_display_name(&[Formatter::None], false), None);

        assert_eq!(
            formatter_display_name(
                &[Formatter::External {
                    command: "clang-format".to_string(),
                    arguments: Some(vec!["--style={}".to_string()]),
                }],
                false,
            )
            .as_deref(),
            Some("clang-format"),
        );

        assert_eq!(
            formatter_display_name(&[Formatter::Auto], true).as_deref(),
            Some("自動 (Prettier)"),
        );
        assert_eq!(
            formatter_display_name(&[Formatter::Auto], false).as_deref(),
            Some("自動 (言語サーバー)"),
        );

        assert_eq!(
            formatter_display_name(
                &[Formatter::LanguageServer(
                    LanguageServerFormatterSpecifier::Specific {
                        name: "rust-analyzer".to_string(),
                    },
                )],
                false,
            )
            .as_deref(),
            Some("rust-analyzer"),
        );

        // A `none` entry in a list is a no-op rather than a formatter, so it is
        // dropped instead of turning the whole list into "not configured".
        assert_eq!(
            formatter_display_name(
                &[
                    Formatter::None,
                    Formatter::LanguageServer(LanguageServerFormatterSpecifier::Current),
                    Formatter::Prettier,
                ],
                false,
            )
            .as_deref(),
            Some("言語サーバー → Prettier"),
        );
    }

    #[test]
    fn test_external_formatter_missing_line_placeholders() {
        let external = |arguments: &[&str]| Formatter::External {
            command: "clang-format".to_string(),
            arguments: Some(arguments.iter().map(|arg| arg.to_string()).collect()),
        };

        assert!(external_formatter_missing_line_placeholders(&[external(
            &["--assume-filename={buffer_path}"]
        )]));
        assert!(!external_formatter_missing_line_placeholders(&[external(
            &["--lines={start_line}:{end_line}"]
        )]));
        assert!(!external_formatter_missing_line_placeholders(&[
            Formatter::Prettier
        ]));
        assert!(external_formatter_missing_line_placeholders(&[
            Formatter::External {
                command: "clang-format".to_string(),
                arguments: None,
            }
        ]));
    }
}
