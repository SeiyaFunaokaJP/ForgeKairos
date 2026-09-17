use super::*;

/// A Markdown inline delimiter that `toggle_markdown_inline_wrap` adds or removes.
#[derive(Clone, Copy)]
struct InlineDelimiter {
    text: &'static str,
    /// `text` written twice, when that spelling is a delimiter of its own. `*`
    /// (italic) must not be recognized inside `**` (bold), otherwise toggling
    /// italic would eat half of a bold pair.
    stronger: Option<&'static str>,
}

const BOLD: InlineDelimiter = InlineDelimiter {
    text: "**",
    stronger: None,
};
const ITALIC: InlineDelimiter = InlineDelimiter {
    text: "*",
    stronger: Some("**"),
};
const STRIKETHROUGH: InlineDelimiter = InlineDelimiter {
    text: "~~",
    stronger: None,
};
const INLINE_CODE: InlineDelimiter = InlineDelimiter {
    text: "`",
    stronger: Some("``"),
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MarkdownListKind {
    Bullet,
    Numbered,
    Task,
}

/// The opening tag `insert_markdown_details` writes. The `<summary>` shares the
/// line with it so that the gutter fold, which hides everything below the
/// opening row, leaves the summary on screen.
const DETAILS_OPEN: &str = "<details><summary>";

/// The `<summary>` text `insert_markdown_details` writes, left selected so it
/// can be typed over.
const DETAILS_SUMMARY: &str = "開く";

/// The shortest code fence CommonMark accepts.
const MINIMUM_FENCE_LENGTH: usize = 3;

impl Editor {
    pub fn toggle_markdown_block_quote(
        &mut self,
        _: &ToggleBlockQuote,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.manipulate_mutable_lines_in_markdown(window, cx, |lines| {
            let all_lines_quoted = lines.iter().all(|line| line.starts_with('>'));

            for line in lines.iter_mut() {
                let stripped_line = match line.strip_prefix("> ").or_else(|| line.strip_prefix('>'))
                {
                    Some(rest) => rest.to_string(),
                    None => line.to_string(),
                };

                *line = if all_lines_quoted {
                    Cow::Owned(stripped_line)
                } else if stripped_line.trim().is_empty() {
                    Cow::Borrowed(">")
                } else {
                    Cow::Owned(format!("> {stripped_line}"))
                };
            }
        });
    }

    pub fn toggle_markdown_bold(
        &mut self,
        _: &ToggleBold,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_inline_wrap(BOLD, window, cx);
    }

    pub fn toggle_markdown_italic(
        &mut self,
        _: &ToggleItalic,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_inline_wrap(ITALIC, window, cx);
    }

    pub fn toggle_markdown_strikethrough(
        &mut self,
        _: &ToggleStrikethrough,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_inline_wrap(STRIKETHROUGH, window, cx);
    }

    pub fn toggle_markdown_inline_code(
        &mut self,
        _: &ToggleInlineCode,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_inline_wrap(INLINE_CODE, window, cx);
    }

    pub fn toggle_markdown_heading_1(
        &mut self,
        _: &ToggleHeading1,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_heading(1, window, cx);
    }

    pub fn toggle_markdown_heading_2(
        &mut self,
        _: &ToggleHeading2,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_heading(2, window, cx);
    }

    pub fn toggle_markdown_heading_3(
        &mut self,
        _: &ToggleHeading3,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_heading(3, window, cx);
    }

    pub fn toggle_markdown_bullet_list(
        &mut self,
        _: &ToggleBulletList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_list(MarkdownListKind::Bullet, window, cx);
    }

    pub fn toggle_markdown_numbered_list(
        &mut self,
        _: &ToggleNumberedList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_list(MarkdownListKind::Numbered, window, cx);
    }

    pub fn toggle_markdown_task_list(
        &mut self,
        _: &ToggleTaskList,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_markdown_list(MarkdownListKind::Task, window, cx);
    }

    pub fn insert_markdown_horizontal_rule(
        &mut self,
        _: &InsertHorizontalRule,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let max_row = snapshot.max_point().row;

        let mut edits = Vec::new();
        let mut cursors = Vec::new();
        let mut last_row = None;
        for selection in self.selections.all::<Point>(&self.display_snapshot(cx)) {
            let row = selection.end.row;
            if last_row == Some(row) {
                continue;
            }
            last_row = Some(row);

            let mut text = String::from("\n");
            if !snapshot.is_line_blank(MultiBufferRow(row)) {
                text.push('\n');
            }
            text.push_str("---");
            if row < max_row && !snapshot.is_line_blank(MultiBufferRow(row + 1)) {
                text.push('\n');
                cursors.push(text.len() - 1);
            } else {
                cursors.push(text.len());
            }

            let insert_at =
                snapshot.point_to_offset(Point::new(row, snapshot.line_len(MultiBufferRow(row))));
            let anchor = snapshot.anchor_before(insert_at);
            edits.push((anchor..anchor, text));
        }

        if edits.is_empty() {
            return;
        }

        self.apply_markdown_edits(edits, cursors, window, cx);
    }

    pub fn wrap_markdown_in_code_block(
        &mut self,
        _: &WrapInCodeBlock,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let max_row = snapshot.max_point().row;

        let mut edits = Vec::new();
        let mut open_anchors = Vec::new();
        let mut cursors = Vec::new();
        for selection in self.selections.all::<Point>(&self.display_snapshot(cx)) {
            let start_row = selection.start.row;
            let end_row = selection.end.row;

            let mut open = String::new();
            if start_row > 0 && !snapshot.is_line_blank(MultiBufferRow(start_row - 1)) {
                open.push('\n');
            }
            open.push_str("```\n");
            // Park the cursor right after the opening fence so an info string
            // can be typed immediately.
            cursors.push(open.len() - 1);

            let mut close = String::from("\n```");
            if end_row < max_row && !snapshot.is_line_blank(MultiBufferRow(end_row + 1)) {
                close.push('\n');
            }

            let open_at = snapshot.point_to_offset(Point::new(start_row, 0));
            let close_at = snapshot.point_to_offset(Point::new(
                end_row,
                snapshot.line_len(MultiBufferRow(end_row)),
            ));
            let open_anchor = snapshot.anchor_before(open_at);
            let close_anchor = snapshot.anchor_after(close_at);
            open_anchors.push(open_anchor);
            edits.push((open_anchor..open_anchor, open));
            edits.push((close_anchor..close_anchor, close));
        }

        if edits.is_empty() {
            return;
        }

        self.transact(window, cx, |this, window, cx| {
            let buffer = this.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
                buffer.snapshot(cx)
            });

            let new_selections = open_anchors
                .into_iter()
                .zip(cursors)
                .map(|(anchor, offset)| {
                    let cursor = anchor.to_offset(&buffer) + offset;
                    cursor..cursor
                })
                .collect::<Vec<_>>();

            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(new_selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    /// Wraps the selected lines in a `<details>` disclosure whose body is a
    /// fenced code block, or inserts an empty one at the cursor.
    ///
    /// The disclosure is written the way GitHub needs it: the opening tag and
    /// its `<summary>` share a line, a blank line follows so that the body is
    /// parsed as Markdown rather than as raw HTML, and `</details>` closes it on
    /// a line of its own. The `<summary>` text is left selected so it can be
    /// typed over straight away.
    pub fn insert_markdown_details(
        &mut self,
        _: &InsertDetails,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let max_row = snapshot.max_point().row;

        let mut edits = Vec::new();
        // The anchor the opening text was inserted at, paired with the byte
        // offset of the summary within that text.
        let mut summaries = Vec::new();
        for selection in self.selections.all::<Point>(&self.display_snapshot(cx)) {
            let start_row = selection.start.row;
            let end_row = selection.end.row;
            let body_start = Point::new(start_row, 0);
            let body_end = Point::new(end_row, snapshot.line_len(MultiBufferRow(end_row)));
            // A fence has to outrun the longest run of backticks it holds, and
            // is always spelled with backticks so that a tilde fence in the body
            // cannot close it either.
            let fence = "`".repeat(
                longest_backtick_run(
                    snapshot
                        .text_for_range(body_start..body_end)
                        .flat_map(str::chars),
                )
                .saturating_add(1)
                .max(MINIMUM_FENCE_LENGTH),
            );

            // A fence that touches the line above or below it is not a block of
            // its own to a CommonMark renderer, so a blank line is kept on both
            // sides of the disclosure. Inserting on a blank line already
            // contributes one of the two line breaks a blank line needs, hence
            // the single `\n` there.
            let leading = if selection.is_empty() && selection.start.column > 0 {
                // Splitting the line the cursor sits on would push its tail
                // below the disclosure, so an empty selection inserts the whole
                // block where the cursor is, as `InsertMermaidDiagram` does.
                "\n\n"
            } else if start_row > 0 && !snapshot.is_line_blank(MultiBufferRow(start_row - 1)) {
                "\n"
            } else {
                ""
            };
            let summary_offset = leading.len() + DETAILS_OPEN.len();
            let open = format!("{leading}{DETAILS_OPEN}{DETAILS_SUMMARY}</summary>\n\n{fence}\n");

            let mut close = format!("\n{fence}\n</details>");
            if selection.is_empty()
                && selection.end.column < snapshot.line_len(MultiBufferRow(end_row))
            {
                close.push_str("\n\n");
            } else if end_row < max_row && !snapshot.is_line_blank(MultiBufferRow(end_row + 1)) {
                close.push('\n');
            }

            if selection.is_empty() {
                // The two halves meet at the body, which is a single blank line
                // so that the disclosure has room to be typed into.
                let anchor = snapshot.anchor_before(snapshot.point_to_offset(selection.start));
                summaries.push((anchor, summary_offset));
                edits.push((anchor..anchor, format!("{open}{close}")));
            } else {
                let open_anchor = snapshot.anchor_before(snapshot.point_to_offset(body_start));
                let close_anchor = snapshot.anchor_after(snapshot.point_to_offset(body_end));
                summaries.push((open_anchor, summary_offset));
                edits.push((open_anchor..open_anchor, open));
                edits.push((close_anchor..close_anchor, close));
            }
        }

        if edits.is_empty() {
            return;
        }

        self.transact(window, cx, |this, window, cx| {
            let buffer = this.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
                buffer.snapshot(cx)
            });

            let new_selections = summaries
                .into_iter()
                .map(|(anchor, offset)| {
                    let start = anchor.to_offset(&buffer) + offset;
                    start..start + DETAILS_SUMMARY.len()
                })
                .collect::<Vec<_>>();

            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(new_selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    pub fn insert_mermaid_diagram(
        &mut self,
        action: &InsertMermaidDiagram,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let template = action.diagram.template();

        let mut edits = Vec::new();
        let mut cursors = Vec::new();
        let max_row = snapshot.max_point().row;
        for selection in self.selections.all::<Point>(&self.display_snapshot(cx)) {
            let mut text = String::new();
            // A fence that touches the line above or below it is not a block of
            // its own to a CommonMark renderer, and reads as one paragraph in
            // the source either way, so a blank line is kept on both sides.
            // Inserting on a blank line already contributes one of the two line
            // breaks the blank line needs, hence the single `\n` there.
            if selection.start.column > 0 {
                text.push_str("\n\n");
            } else if selection.start.row > 0
                && !snapshot.is_line_blank(MultiBufferRow(selection.start.row - 1))
            {
                text.push('\n');
            }
            text.push_str("```mermaid\n");
            // The cursor lands on the first line of the diagram body, which is
            // where every template starts with its diagram type keyword.
            cursors.push(text.len());
            text.push_str(template);
            text.push_str("\n```");
            if selection.end.column < snapshot.line_len(MultiBufferRow(selection.end.row)) {
                text.push_str("\n\n");
            } else if selection.end.row < max_row
                && !snapshot.is_line_blank(MultiBufferRow(selection.end.row + 1))
            {
                text.push('\n');
            }

            let start = snapshot.point_to_offset(selection.start);
            let end = snapshot.point_to_offset(selection.end);
            edits.push((
                snapshot.anchor_before(start)..snapshot.anchor_after(end),
                text,
            ));
        }

        if edits.is_empty() {
            return;
        }

        self.apply_markdown_edits(edits, cursors, window, cx);
    }

    pub fn insert_markdown_link(
        &mut self,
        _: &InsertLink,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let selections = self
            .selections
            .all::<MultiBufferOffset>(&self.display_snapshot(cx));

        if selections.iter().all(|selection| selection.is_empty()) {
            let Some(snippet) = Snippet::parse("[$1]($2)$0").log_err() else {
                return;
            };
            let insertion_ranges = selections
                .iter()
                .map(|selection| selection.start..selection.end)
                .collect::<Vec<_>>();
            self.insert_snippet(&insertion_ranges, snippet, window, cx)
                .log_err();
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let mut edits = Vec::new();
        let mut url_anchors = Vec::new();
        for selection in &selections {
            let start = snapshot.anchor_before(selection.start);
            let end = snapshot.anchor_after(selection.end);
            edits.push((start..start, "[".to_string()));
            edits.push((end..end, "]()".to_string()));
            url_anchors.push(end);
        }

        self.transact(window, cx, |this, window, cx| {
            let buffer = this.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
                buffer.snapshot(cx)
            });

            let new_selections = url_anchors
                .into_iter()
                .map(|anchor| {
                    // The right-biased anchor ends up past the inserted `]()`,
                    // so stepping back one byte lands between the parentheses.
                    let cursor = anchor.to_offset(&buffer).saturating_sub_usize(1);
                    cursor..cursor
                })
                .collect::<Vec<_>>();

            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(new_selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    fn toggle_markdown_inline_wrap(
        &mut self,
        delimiter: InlineDelimiter,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let delimiter_len = delimiter.text.len();

        let mut edits = Vec::new();
        // (start anchor, end anchor, bytes to skip forward, bytes to skip back)
        let mut boundaries = Vec::new();

        for selection in self
            .selections
            .all::<MultiBufferOffset>(&self.display_snapshot(cx))
        {
            let start = selection.start;
            let end = selection.end;
            let selected = snapshot.text_for_range(start..end).collect::<String>();

            if selection_is_wrapped(&selected, delimiter) {
                let inner_start = start + delimiter_len;
                let inner_end = end.saturating_sub_usize(delimiter_len);
                edits.push((
                    snapshot.anchor_before(start)..snapshot.anchor_before(inner_start),
                    String::new(),
                ));
                edits.push((
                    snapshot.anchor_before(inner_end)..snapshot.anchor_before(end),
                    String::new(),
                ));
                boundaries.push((
                    snapshot.anchor_after(inner_start),
                    snapshot.anchor_before(inner_end),
                    0,
                    0,
                ));
            } else if let Some((before, after)) =
                surrounding_delimiters(&snapshot, start, end, delimiter)
            {
                edits.push((before, String::new()));
                edits.push((after, String::new()));
                boundaries.push((
                    snapshot.anchor_after(start),
                    snapshot.anchor_before(end),
                    0,
                    0,
                ));
            } else {
                let start_anchor = snapshot.anchor_before(start);
                let end_anchor = snapshot.anchor_after(end);
                edits.push((start_anchor..start_anchor, delimiter.text.to_string()));
                edits.push((end_anchor..end_anchor, delimiter.text.to_string()));
                boundaries.push((start_anchor, end_anchor, delimiter_len, delimiter_len));
            }
        }

        if edits.is_empty() {
            return;
        }

        self.transact(window, cx, |this, window, cx| {
            let buffer = this.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
                buffer.snapshot(cx)
            });

            let new_selections = boundaries
                .into_iter()
                .map(|(start, end, skip_forward, skip_back)| {
                    let start = start.to_offset(&buffer) + skip_forward;
                    let end = end.to_offset(&buffer).saturating_sub_usize(skip_back);
                    start..end.max(start)
                })
                .collect::<Vec<_>>();

            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(new_selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    fn toggle_markdown_heading(
        &mut self,
        level: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.manipulate_mutable_lines_in_markdown(window, cx, |lines| {
            let mut has_content = false;
            let mut all_at_level = true;
            for line in lines.iter() {
                if line.trim().is_empty() {
                    continue;
                }
                has_content = true;
                if heading_level(split_indent(line).1) != Some(level) {
                    all_at_level = false;
                }
            }
            let remove = has_content && all_at_level;

            for line in lines.iter_mut() {
                if has_content && line.trim().is_empty() {
                    continue;
                }
                let (indent, content) = split_indent(line);
                let body = match heading_level(content) {
                    Some(existing) => content[existing..].trim_start(),
                    None => content,
                };
                let new_line = if remove {
                    format!("{indent}{body}")
                } else {
                    let hashes = "#".repeat(level);
                    format!("{indent}{hashes} {body}")
                };
                *line = Cow::Owned(new_line);
            }
        });
    }

    fn toggle_markdown_list(
        &mut self,
        kind: MarkdownListKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.manipulate_mutable_lines_in_markdown(window, cx, |lines| {
            let mut has_content = false;
            let mut all_marked = true;
            for line in lines.iter() {
                if line.trim().is_empty() {
                    continue;
                }
                has_content = true;
                if list_marker(split_indent(line).1).map(|(marker, _)| marker) != Some(kind) {
                    all_marked = false;
                }
            }
            let remove = has_content && all_marked;

            let mut number = 0;
            for line in lines.iter_mut() {
                if has_content && line.trim().is_empty() {
                    continue;
                }
                let (indent, content) = split_indent(line);
                let body = match list_marker(content) {
                    Some((_, marker_len)) => &content[marker_len..],
                    None => content,
                };
                let new_line = if remove {
                    format!("{indent}{body}")
                } else {
                    number += 1;
                    match kind {
                        MarkdownListKind::Bullet => format!("{indent}- {body}"),
                        MarkdownListKind::Numbered => format!("{indent}{number}. {body}"),
                        MarkdownListKind::Task => format!("{indent}- [ ] {body}"),
                    }
                };
                *line = Cow::Owned(new_line);
            }
        });
    }

    fn apply_markdown_edits(
        &mut self,
        edits: Vec<(Range<Anchor>, String)>,
        cursor_offsets: Vec<usize>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let anchors = edits
            .iter()
            .map(|(range, _)| range.start)
            .collect::<Vec<_>>();
        self.transact(window, cx, |this, window, cx| {
            let buffer = this.buffer.update(cx, |buffer, cx| {
                buffer.edit(edits, None, cx);
                buffer.snapshot(cx)
            });

            let new_selections = anchors
                .into_iter()
                .zip(cursor_offsets)
                .map(|(anchor, offset)| {
                    let cursor = anchor.to_offset(&buffer) + offset;
                    cursor..cursor
                })
                .collect::<Vec<_>>();

            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(new_selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    fn manipulate_mutable_lines_in_markdown<Fn>(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        callback: Fn,
    ) where
        Fn: FnMut(&mut Vec<Cow<'_, str>>),
    {
        if !self.is_in_markdown_language(cx) {
            return;
        }

        self.manipulate_mutable_lines(window, cx, callback);
    }

    fn is_in_markdown_language(&self, cx: &mut App) -> bool {
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let head = self
            .selections
            .newest::<MultiBufferOffset>(&self.display_snapshot(cx))
            .head();
        snapshot
            .language_at(head)
            .is_some_and(|language| language.name() == "Markdown")
    }

    /// Whether the buffer as a whole is Markdown, which is the question the
    /// Mermaid entry points have to ask instead of `is_in_markdown_language`:
    /// the body of a ```` ```mermaid ```` fence is an injection layer of its own,
    /// so the language at the cursor there is Mermaid and a gate on it refuses
    /// to edit the very diagram it was pointed at. Same test as the mouse
    /// context menu and `MarkdownPreviewView::is_markdown_file`.
    fn is_markdown_buffer(&self, cx: &App) -> bool {
        self.buffer
            .read(cx)
            .as_singleton()
            .and_then(|buffer| buffer.read(cx).language())
            .is_some_and(|language| language.name() == "Markdown")
    }
}

impl MermaidDiagramKind {
    /// Every kind, in the order the pickers and the context menu list them.
    pub const ALL: [MermaidDiagramKind; 10] = [
        MermaidDiagramKind::Flowchart,
        MermaidDiagramKind::Sequence,
        MermaidDiagramKind::Class,
        MermaidDiagramKind::State,
        MermaidDiagramKind::EntityRelationship,
        MermaidDiagramKind::Gantt,
        MermaidDiagramKind::Pie,
        MermaidDiagramKind::GitGraph,
        MermaidDiagramKind::Mindmap,
        MermaidDiagramKind::Timeline,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Flowchart => "フローチャート",
            Self::Sequence => "シーケンス図",
            Self::Class => "クラス図",
            Self::State => "状態遷移図",
            Self::EntityRelationship => "ER 図",
            Self::Gantt => "ガントチャート",
            Self::Pie => "円グラフ",
            Self::GitGraph => "Git グラフ",
            Self::Mindmap => "マインドマップ",
            Self::Timeline => "タイムライン",
        }
    }

    /// The diagram body, without the surrounding fence and without a trailing
    /// newline.
    ///
    /// The first token of every template is a diagram type that the Markdown
    /// renderer draws; a template starting with anything else would be inserted
    /// as a code block that never turns into a picture.
    pub fn template(self) -> &'static str {
        match self {
            Self::Flowchart => {
                r#"flowchart TD
    %% [] は処理、{} は分岐。|ラベル| で分岐に注釈を付ける
    start[開始] --> condition{条件}
    condition -->|はい| work[処理]
    condition -->|いいえ| finish[終了]
    work --> finish"#
            }
            Self::Sequence => {
                r#"sequenceDiagram
    %% ->> は呼び出し、-->> は応答
    participant User as 利用者
    participant App as アプリ
    User->>App: 要求
    App-->>User: 応答
    Note over App: 補足"#
            }
            Self::Class => {
                r#"classDiagram
    %% <|-- は継承、*-- は合成、o-- は集約
    class Base {
        +field: Type
        +method()
    }
    class Derived {
        +extra()
    }
    Base <|-- Derived"#
            }
            Self::State => {
                r#"stateDiagram-v2
    %% [*] は開始と終了。矢印のあとの : がイベント名
    [*] --> Idle
    Idle --> Running: 開始
    Running --> Idle: 完了
    Running --> [*]: 中止"#
            }
            Self::EntityRelationship => {
                r#"erDiagram
    %% ||--o{ は 1 対多、}o--o{ は多対多
    CUSTOMER ||--o{ ORDER : "発注する"
    ORDER ||--|{ ORDER_ITEM : "含む"
    CUSTOMER {
        int id
        string name
    }"#
            }
            Self::Gantt => {
                r#"gantt
    title 開発スケジュール
    dateFormat YYYY-MM-DD
    axisFormat %m/%d
    section 設計
    要件定義 :done, req, 2026-01-06, 5d
    基本設計 :active, design, after req, 7d
    section 実装
    実装 :impl, after design, 10d"#
            }
            Self::Pie => {
                r#"pie title 内訳
    "設計" : 30
    "実装" : 50
    "テスト" : 20"#
            }
            Self::GitGraph => {
                r#"gitGraph
    commit id: "初期化"
    branch feature
    checkout feature
    commit id: "機能を追加"
    checkout main
    merge feature"#
            }
            Self::Mindmap => {
                // Mindmap nesting is expressed with indentation, so the two
                // space steps below are syntax rather than formatting.
                r#"mindmap
  root((中心テーマ))
    分類A
      項目1
      項目2
    分類B
      項目3"#
            }
            Self::Timeline => {
                r#"timeline
    title 沿革
    2024 : 企画
    2025 : 開発 : 公開
    2026 : 運用"#
            }
        }
    }
}

/// Splits a line into its leading whitespace and the rest.
fn split_indent(line: &str) -> (&str, &str) {
    let content_start = line
        .find(|character: char| !character.is_whitespace())
        .unwrap_or(line.len());
    line.split_at(content_start)
}

/// Number of leading `#` when `content` (already stripped of indentation) opens
/// an ATX heading, i.e. one to six hashes followed by whitespace or nothing.
fn heading_level(content: &str) -> Option<usize> {
    let hashes = content.len() - content.trim_start_matches('#').len();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = content.get(hashes..)?;
    if rest.is_empty() || rest.starts_with(char::is_whitespace) {
        Some(hashes)
    } else {
        None
    }
}

/// The list marker `content` (already stripped of indentation) opens, together
/// with the marker's length in bytes.
fn list_marker(content: &str) -> Option<(MarkdownListKind, usize)> {
    let first = content.chars().next()?;

    if matches!(first, '-' | '*' | '+') {
        let after_bullet = content.strip_prefix([first])?.strip_prefix(' ')?;
        for checkbox in ["[ ] ", "[x] ", "[X] "] {
            if after_bullet.starts_with(checkbox) {
                return Some((MarkdownListKind::Task, 2 + checkbox.len()));
            }
        }
        return Some((MarkdownListKind::Bullet, 2));
    }

    if first.is_ascii_digit() {
        let digits = content.len()
            - content
                .trim_start_matches(|character: char| character.is_ascii_digit())
                .len();
        let rest = content.get(digits..)?;
        if rest.starts_with(['.', ')']) && rest.get(1..).is_some_and(|r| r.starts_with(' ')) {
            return Some((MarkdownListKind::Numbered, digits + 2));
        }
    }

    None
}

/// Whether the selected text already carries `delimiter` on both ends.
fn selection_is_wrapped(selected: &str, delimiter: InlineDelimiter) -> bool {
    if selected.len() < delimiter.text.len() * 2
        || !selected.starts_with(delimiter.text)
        || !selected.ends_with(delimiter.text)
    {
        return false;
    }
    if let Some(stronger) = delimiter.stronger
        && selected.len() >= stronger.len() * 2
        && selected.starts_with(stronger)
        && selected.ends_with(stronger)
    {
        return false;
    }
    true
}

/// The ranges holding `delimiter` immediately outside `start..end`, when both
/// sides carry it.
fn surrounding_delimiters(
    snapshot: &MultiBufferSnapshot,
    start: MultiBufferOffset,
    end: MultiBufferOffset,
    delimiter: InlineDelimiter,
) -> Option<(Range<Anchor>, Range<Anchor>)> {
    let length = delimiter.text.len();
    if start.0 < length || end.0 + length > snapshot.len().0 {
        return None;
    }

    let before_start = start.saturating_sub_usize(length);
    let after_end = end + length;
    if snapshot
        .text_for_range(before_start..start)
        .collect::<String>()
        != delimiter.text
        || snapshot.text_for_range(end..after_end).collect::<String>() != delimiter.text
    {
        return None;
    }

    // `stronger` is the delimiter written twice, so a second copy just outside
    // the one we found means the run belongs to the stronger delimiter instead.
    if delimiter.stronger.is_some() {
        if start.0 >= length * 2
            && snapshot
                .text_for_range(start.saturating_sub_usize(length * 2)..before_start)
                .collect::<String>()
                == delimiter.text
        {
            return None;
        }
        if end.0 + length * 2 <= snapshot.len().0
            && snapshot
                .text_for_range(after_end..end + length * 2)
                .collect::<String>()
                == delimiter.text
        {
            return None;
        }
    }

    Some((
        snapshot.anchor_before(before_start)..snapshot.anchor_before(start),
        snapshot.anchor_after(end)..snapshot.anchor_after(after_end),
    ))
}

/// How a pipe table column is aligned, i.e. which spelling its delimiter cell
/// is written with.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum MarkdownTableAlignment {
    /// `---`, which leaves the alignment to the renderer.
    #[default]
    Unspecified,
    /// `:---`
    Left,
    /// `:---:`
    Center,
    /// `---:`
    Right,
}

impl MarkdownTableAlignment {
    pub fn label(self) -> &'static str {
        match self {
            Self::Unspecified => "既定",
            Self::Left => "左",
            Self::Center => "中央",
            Self::Right => "右",
        }
    }

    /// The next alignment offered by the table editor's cycling button.
    pub fn next_alignment(self) -> Self {
        match self {
            Self::Unspecified => Self::Left,
            Self::Left => Self::Center,
            Self::Center => Self::Right,
            Self::Right => Self::Unspecified,
        }
    }

    /// The delimiter cell for a column that is `width` display columns wide.
    fn delimiter_cell(self, width: usize) -> String {
        let width = width.max(MINIMUM_COLUMN_WIDTH);
        match self {
            Self::Unspecified => "-".repeat(width),
            Self::Left => format!(":{}", "-".repeat(width - 1)),
            Self::Right => format!("{}:", "-".repeat(width - 1)),
            Self::Center => format!(":{}:", "-".repeat(width - 2)),
        }
    }
}

/// A GitHub flavoured Markdown pipe table, in the shape the table editor holds
/// it: every row has exactly `alignments.len()` cells, and the cell text is
/// unescaped.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MarkdownTable {
    pub has_header: bool,
    pub alignments: Vec<MarkdownTableAlignment>,
    /// Every row, the header included as the first one when `has_header` is set.
    pub rows: Vec<Vec<String>>,
}

/// Three dashes are the shortest delimiter cell that can also carry a colon on
/// both ends, so every column is padded to at least this width.
const MINIMUM_COLUMN_WIDTH: usize = 3;

impl MarkdownTable {
    /// The table `markdown::EditTable` starts from when the cursor is not
    /// inside one.
    pub fn empty_with_header() -> Self {
        let mut table = Self {
            has_header: true,
            alignments: vec![MarkdownTableAlignment::Unspecified; 3],
            rows: vec![vec![String::new(); 3]; 3],
        };
        table.normalize();
        table
    }

    pub fn column_count(&self) -> usize {
        self.alignments.len()
    }

    /// Gives every row the same number of cells, so the grid and the rendered
    /// table agree even when the source table had ragged rows.
    pub fn normalize(&mut self) {
        let widest_row = self.rows.iter().map(Vec::len).max().unwrap_or(0);
        let columns = self.alignments.len().max(widest_row).max(1);
        self.alignments
            .resize(columns, MarkdownTableAlignment::Unspecified);
        if self.rows.is_empty() {
            self.rows.push(Vec::new());
        }
        for row in &mut self.rows {
            row.resize(columns, String::new());
        }
    }

    /// The table as Markdown, without a trailing newline, with every column
    /// padded to the display width of its widest cell.
    pub fn to_markdown(&self) -> String {
        let columns = self.column_count();
        let escaped = self
            .rows
            .iter()
            .map(|row| row.iter().map(|cell| escape_table_cell(cell)).collect())
            .collect::<Vec<Vec<String>>>();

        let widths = (0..columns)
            .map(|column| {
                escaped
                    .iter()
                    .filter_map(|row| row.get(column))
                    .map(|cell| display_width(cell))
                    .max()
                    .unwrap_or(0)
                    .max(MINIMUM_COLUMN_WIDTH)
            })
            .collect::<Vec<_>>();

        let mut markdown = String::new();
        let push_row = |cells: &[String], markdown: &mut String| {
            markdown.push('|');
            for (column, width) in widths.iter().enumerate() {
                let cell = cells.get(column).map(String::as_str).unwrap_or("");
                markdown.push(' ');
                markdown.push_str(cell);
                for _ in display_width(cell)..*width {
                    markdown.push(' ');
                }
                markdown.push_str(" |");
            }
            markdown.push('\n');
        };

        let body_start = usize::from(self.has_header);
        if self.has_header
            && let Some(header) = escaped.first()
        {
            push_row(header, &mut markdown);
        }

        markdown.push('|');
        for (column, width) in widths.iter().enumerate() {
            let alignment = self
                .alignments
                .get(column)
                .copied()
                .unwrap_or(MarkdownTableAlignment::Unspecified);
            markdown.push(' ');
            markdown.push_str(&alignment.delimiter_cell(*width));
            markdown.push_str(" |");
        }
        markdown.push('\n');

        for row in escaped.iter().skip(body_start) {
            push_row(row, &mut markdown);
        }

        while markdown.ends_with('\n') {
            markdown.pop();
        }
        markdown
    }
}

/// Where `markdown::EditTable` will write, and the table that is already there.
pub struct MarkdownTableTarget {
    /// The rows of an existing table, or an empty range at the cursor.
    range: Range<Anchor>,
    pub table: MarkdownTable,
    pub replaces_existing: bool,
    /// Newlines that keep an inserted table on lines of its own; empty when an
    /// existing table is being replaced.
    prefix: String,
    suffix: String,
}

impl Editor {
    /// The pipe table to edit for `markdown::EditTable`, or a blank one to
    /// insert when the cursor is not inside a table. `None` when this buffer
    /// cannot take a table at all.
    pub fn markdown_table_target(&self, cx: &mut App) -> Option<MarkdownTableTarget> {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return None;
        }

        let cursor = self
            .selections
            .newest::<Point>(&self.display_snapshot(cx))
            .head();
        let snapshot = self.buffer.read(cx).snapshot(cx);

        if let Some((rows, lines)) = markdown_table_block(&snapshot, cursor.row) {
            let borrowed = lines.iter().map(String::as_str).collect::<Vec<_>>();
            if let Some(table) = parse_markdown_table(&borrowed) {
                let last_row = rows.end.saturating_sub(1);
                let start = Point::new(rows.start, 0);
                let end = Point::new(last_row, snapshot.line_len(MultiBufferRow(last_row)));
                let start = snapshot.point_to_offset(start);
                let end = snapshot.point_to_offset(end);
                return Some(MarkdownTableTarget {
                    range: snapshot.anchor_before(start)..snapshot.anchor_after(end),
                    table,
                    replaces_existing: true,
                    prefix: String::new(),
                    suffix: String::new(),
                });
            }
        }

        let max_row = snapshot.max_point().row;
        let line_len = snapshot.line_len(MultiBufferRow(cursor.row));

        let mut prefix = String::new();
        if cursor.column > 0 {
            prefix.push_str("\n\n");
        } else if cursor.row > 0 && !snapshot.is_line_blank(MultiBufferRow(cursor.row - 1)) {
            prefix.push('\n');
        }

        let mut suffix = String::new();
        if cursor.column < line_len {
            suffix.push_str("\n\n");
        } else if cursor.row < max_row && !snapshot.is_line_blank(MultiBufferRow(cursor.row + 1)) {
            suffix.push('\n');
        }

        let offset = snapshot.point_to_offset(cursor);
        let anchor = snapshot.anchor_before(offset);
        Some(MarkdownTableTarget {
            range: anchor..anchor,
            table: MarkdownTable::empty_with_header(),
            replaces_existing: false,
            prefix,
            suffix,
        })
    }

    /// Writes `table` over the range `target` was resolved against, leaving the
    /// cursor on the table's first line.
    pub fn write_markdown_table(
        &mut self,
        target: &MarkdownTableTarget,
        table: &MarkdownTable,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.read_only(cx) {
            return;
        }

        let text = format!("{}{}{}", target.prefix, table.to_markdown(), target.suffix);
        let cursor = target.prefix.len();
        self.apply_markdown_edits(vec![(target.range.clone(), text)], vec![cursor], window, cx);
    }
}

/// The consecutive non-blank pipe-carrying lines around `row`, with the row
/// range they occupy.
fn markdown_table_block(
    snapshot: &MultiBufferSnapshot,
    row: u32,
) -> Option<(Range<u32>, Vec<String>)> {
    if !is_table_line(&line_text(snapshot, row)) {
        return None;
    }

    let mut start = row;
    while start > 0 && is_table_line(&line_text(snapshot, start - 1)) {
        start -= 1;
    }
    let max_row = snapshot.max_point().row;
    let mut end = row;
    while end < max_row && is_table_line(&line_text(snapshot, end + 1)) {
        end += 1;
    }

    let lines = (start..=end)
        .map(|row| line_text(snapshot, row))
        .collect::<Vec<_>>();
    Some((start..end + 1, lines))
}

fn line_text(snapshot: &MultiBufferSnapshot, row: u32) -> String {
    let start = Point::new(row, 0);
    let end = Point::new(row, snapshot.line_len(MultiBufferRow(row)));
    snapshot.text_for_range(start..end).collect()
}

/// Whether `line` could belong to a pipe table. A table never contains a blank
/// line, which is what bounds the block scanned around the cursor.
fn is_table_line(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.contains('|')
}

/// Parses a block of consecutive table lines. `None` when the block carries no
/// delimiter row, or carries one somewhere other than the first two lines,
/// since neither is a table GitHub would render.
pub fn parse_markdown_table(block: &[&str]) -> Option<MarkdownTable> {
    let delimiter_row = block
        .iter()
        .take(2)
        .position(|line| parse_alignment_row(line).is_some())?;
    let alignments = parse_alignment_row(block.get(delimiter_row)?)?;
    let has_header = delimiter_row == 1;

    let mut rows = Vec::new();
    if has_header {
        rows.push(split_table_row(block.first()?));
    }
    for line in block.get(delimiter_row + 1..)? {
        rows.push(split_table_row(line));
    }

    let mut table = MarkdownTable {
        has_header,
        alignments,
        rows,
    };
    table.normalize();
    Some(table)
}

/// A spreadsheet or CSV payload spread over more than one cell. `None` when the
/// text is a single cell, which should be pasted as ordinary text instead.
pub fn parse_clipboard_table(text: &str) -> Option<Vec<Vec<String>>> {
    let text = text.strip_suffix('\n').unwrap_or(text);
    let text = text.strip_suffix('\r').unwrap_or(text);

    let separator = if text.contains('\t') {
        '\t'
    } else if text.contains('\n') {
        ','
    } else {
        return None;
    };

    let rows = parse_separated_values(text, separator);
    if rows.len() <= 1 && rows.first().is_none_or(|row| row.len() <= 1) {
        return None;
    }
    Some(rows)
}

/// Splits a spreadsheet payload, honouring the RFC 4180 quoting that Excel and
/// most spreadsheets use for cells containing the separator or a line break.
fn parse_separated_values(text: &str, separator: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut characters = text.chars().peekable();

    while let Some(character) = characters.next() {
        if in_quotes {
            match character {
                '"' => {
                    if characters.peek() == Some(&'"') {
                        field.push('"');
                        characters.next();
                    } else {
                        in_quotes = false;
                    }
                }
                '\r' => {}
                _ => field.push(character),
            }
            continue;
        }

        match character {
            '"' if field.is_empty() => in_quotes = true,
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ if character == separator => row.push(std::mem::take(&mut field)),
            _ => field.push(character),
        }
    }

    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// The alignments a delimiter row declares, or `None` when `line` is an
/// ordinary table row.
fn parse_alignment_row(line: &str) -> Option<Vec<MarkdownTableAlignment>> {
    let cells = split_table_row(line);
    if cells.is_empty() {
        return None;
    }
    cells.iter().map(|cell| cell_alignment(cell)).collect()
}

fn cell_alignment(cell: &str) -> Option<MarkdownTableAlignment> {
    let cell = cell.trim();
    let leading_colon = cell.starts_with(':');
    let trailing_colon = cell.len() > 1 && cell.ends_with(':');
    let dashes = cell.get(usize::from(leading_colon)..cell.len() - usize::from(trailing_colon))?;
    if dashes.is_empty() || !dashes.chars().all(|character| character == '-') {
        return None;
    }
    Some(match (leading_colon, trailing_colon) {
        (true, true) => MarkdownTableAlignment::Center,
        (true, false) => MarkdownTableAlignment::Left,
        (false, true) => MarkdownTableAlignment::Right,
        (false, false) => MarkdownTableAlignment::Unspecified,
    })
}

/// Splits a table row on its unescaped pipes, dropping the optional outer ones
/// and turning `\|` back into a literal pipe.
fn split_table_row(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let mut cells = Vec::new();
    let mut cell = String::new();
    let mut ended_on_pipe = false;
    let mut characters = trimmed.chars().peekable();

    while let Some(character) = characters.next() {
        match character {
            '\\' if characters.peek() == Some(&'|') => {
                cell.push('|');
                characters.next();
                ended_on_pipe = false;
            }
            '|' => {
                cells.push(cell.trim().to_string());
                cell = String::new();
                ended_on_pipe = true;
            }
            _ => {
                cell.push(character);
                ended_on_pipe = false;
            }
        }
    }
    cells.push(cell.trim().to_string());

    if ended_on_pipe {
        cells.pop();
    }
    if trimmed.starts_with('|') && !cells.is_empty() {
        cells.remove(0);
    }
    cells
}

/// Cell text as it is written inside a pipe table. A cell cannot span lines, so
/// a line break that came in from a spreadsheet becomes an explicit `<br>`.
fn escape_table_cell(cell: &str) -> String {
    let mut escaped = String::with_capacity(cell.len());
    for character in cell.trim().chars() {
        match character {
            '|' => escaped.push_str("\\|"),
            '\n' => escaped.push_str("<br>"),
            '\r' => {}
            _ => escaped.push(character),
        }
    }
    escaped
}

/// Where `markdown::EditMermaid` will write, and the diagram that is there now.
pub struct MermaidFenceTarget {
    /// The fence's body: the whole lines between the opening and the closing
    /// fence, ending just before the closing fence's line. Empty when the fence
    /// holds nothing yet.
    range: Range<Anchor>,
    /// The body with the fence's own indentation removed, as the Mermaid parser
    /// wants to see it.
    pub source: String,
    /// The opening fence's info string, so the caller can tell a Mermaid fence
    /// from any other fenced block.
    pub info: String,
    /// The indentation the fence sits at, re-applied to every line written back
    /// so that a fence inside a list item stays inside it.
    indent: String,
    /// The fence lines themselves, for a diagram that is not in the buffer yet.
    /// `None` for a fence that is already there.
    fence: Option<FenceToInsert>,
}

/// The text written around the diagram body when the fence has yet to be
/// inserted, so that the whole fence arrives in the one edit that `write_mermaid_fence`
/// makes — and so that dismissing the editor leaves the document untouched.
struct FenceToInsert {
    before: String,
    after: String,
}

impl Editor {
    /// The Mermaid fence `markdown::EditMermaid` acts on: the one the cursor is
    /// in, or the nearest one when the cursor is elsewhere.
    pub fn mermaid_fence_target(&self, cx: &mut App) -> Option<MermaidFenceTarget> {
        if !self.is_markdown_buffer(cx) {
            return None;
        }
        let cursor = self
            .selections
            .newest::<Point>(&self.display_snapshot(cx))
            .head();
        let snapshot = self.buffer.read(cx).snapshot(cx);
        nearest_mermaid_fence_target(&snapshot, cursor.row)
    }

    /// The Mermaid fence covering `offset`, for callers that know where the
    /// diagram is without moving the cursor there — the preview, which resolves
    /// the fence from the diagram the user clicked and must not be redirected to
    /// a neighbouring one.
    pub fn mermaid_fence_target_at(
        &self,
        offset: usize,
        cx: &mut App,
    ) -> Option<MermaidFenceTarget> {
        if !self.is_markdown_buffer(cx) {
            return None;
        }
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let offset = offset.min(snapshot.len().0);
        let row = snapshot.offset_to_point(MultiBufferOffset(offset)).row;
        mermaid_fence_target_at_row(&snapshot, row)
    }

    /// The fence a brand-new diagram goes into, for a Markdown document that
    /// holds none. Nothing is written here: the fence lines travel with the
    /// target and reach the buffer only if the editor is confirmed, so that
    /// dismissing it leaves the document as it was.
    pub fn new_mermaid_fence_target(&self, cx: &mut App) -> Option<MermaidFenceTarget> {
        if !self.is_markdown_buffer(cx) {
            return None;
        }
        let cursor = self
            .selections
            .newest::<Point>(&self.display_snapshot(cx))
            .head();
        let snapshot = self.buffer.read(cx).snapshot(cx);

        // A fence cannot open inside another fence: the new diagram would close
        // the block it was dropped into and take the rest of it with it. A
        // cursor in a code block sends the diagram past its closing line.
        let cursor = match fenced_code_block_at(&buffer_lines(&snapshot), cursor.row) {
            Some(block) => Point::new(
                block.body_end,
                snapshot.line_len(MultiBufferRow(block.body_end)),
            ),
            None => cursor,
        };

        // A fence that touches the line above or below it is not a block of its
        // own to a CommonMark renderer, so a blank line is kept on both sides,
        // the way `insert_mermaid_diagram` does it.
        let mut before = String::new();
        if cursor.column > 0 {
            before.push_str("\n\n");
        } else if cursor.row > 0 && !snapshot.is_line_blank(MultiBufferRow(cursor.row - 1)) {
            before.push('\n');
        }
        before.push_str("```mermaid\n");
        let mut after = String::from("```");
        if cursor.column < snapshot.line_len(MultiBufferRow(cursor.row)) {
            after.push_str("\n\n");
        } else if cursor.row < snapshot.max_point().row
            && !snapshot.is_line_blank(MultiBufferRow(cursor.row + 1))
        {
            after.push('\n');
        }

        let offset = snapshot.point_to_offset(cursor);
        // Both ends take the same bias, so that text arriving at this very
        // offset while the editor is open — a reload, a collaborator — is left
        // alone instead of being swallowed by the insertion.
        Some(MermaidFenceTarget {
            range: snapshot.anchor_before(offset)..snapshot.anchor_before(offset),
            // Empty, so that the structured editor asks which kind of diagram
            // to start rather than assuming a flowchart.
            source: String::new(),
            info: "mermaid".to_string(),
            indent: String::new(),
            fence: Some(FenceToInsert { before, after }),
        })
    }

    /// Whether `markdown::EditMermaid` has anything to do here, for the context
    /// menu entry. Anywhere in a writable Markdown buffer it does: it edits the
    /// diagram nearest the cursor, and starts one when there is no diagram yet.
    pub fn can_edit_mermaid_diagram(&self, cx: &App) -> bool {
        self.is_markdown_buffer(cx) && !self.read_only(cx)
    }

    /// Puts the cursor on the diagram the structured editor is being opened on,
    /// for the case where the cursor was somewhere else entirely and the fence
    /// was reached by falling back to the nearest one: that fence is rewritten
    /// on confirm, so the user has to be able to see which one it is.
    pub fn reveal_mermaid_fence_target(
        &mut self,
        target: &MermaidFenceTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // A fence that is not in the buffer yet goes in at the cursor, which is
        // where the user is already looking.
        if target.fence.is_some() {
            return;
        }
        let snapshot = self.buffer.read(cx).snapshot(cx);
        let body_start = target.range.start.to_offset(&snapshot);
        let first_body_row = snapshot.offset_to_point(body_start).row;
        let closing_fence_row = snapshot
            .offset_to_point(target.range.end.to_offset(&snapshot))
            .row;
        let cursor_row = snapshot
            .offset_to_point(self.selections.newest_anchor().head().to_offset(&snapshot))
            .row;
        // The opening fence line sits one row above the body, so the cursor is
        // already on this diagram from that row down to the closing fence.
        if cursor_row + 1 >= first_body_row && cursor_row <= closing_fence_row {
            return;
        }

        self.change_selections(Default::default(), window, cx, |selections| {
            selections.select_ranges([body_start..body_start]);
        });
        self.request_autoscroll(Autoscroll::center(), cx);
    }

    /// Replaces the body of the fence `target` was resolved against with
    /// `source`, as a single edit so that undo takes the whole diagram back.
    ///
    /// Fails, without writing anything, when the fence no longer holds what it
    /// held when `target` was resolved: the structured editor stays open for
    /// minutes at a time, and another pane — or a reload — may have rewritten
    /// the diagram underneath it. The anchors still point at the right place,
    /// so the text itself is what is compared.
    pub fn write_mermaid_fence(
        &mut self,
        target: &MermaidFenceTarget,
        source: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Result<()> {
        anyhow::ensure!(
            !self.read_only(cx),
            "このバッファーは読み取り専用のため、図を書き戻せません。"
        );
        anyhow::ensure!(
            self.mermaid_fence_body(target, cx) == target.source,
            "図のテキストが編集中に変更されました。もう一度開き直してください。"
        );

        let mut text = String::new();
        if let Some(fence) = &target.fence {
            text.push_str(&fence.before);
        }
        // The cursor lands on the first line of the body, past the fence when
        // the fence is being inserted along with it.
        let cursor_offset = text.len();
        for line in source.lines() {
            if !line.is_empty() {
                text.push_str(&target.indent);
                text.push_str(line);
            }
            text.push('\n');
        }
        if let Some(fence) = &target.fence {
            text.push_str(&fence.after);
        }

        self.apply_markdown_edits(
            vec![(target.range.clone(), text)],
            vec![cursor_offset],
            window,
            cx,
        );
        Ok(())
    }

    /// What the fence `target` points at holds now, spelled the way
    /// [`MermaidFenceTarget::source`] is: the fence's own indentation removed
    /// and no trailing newline.
    fn mermaid_fence_body(&self, target: &MermaidFenceTarget, cx: &App) -> String {
        // A fence that is not in the buffer yet has nothing under its anchors,
        // and its `source` is empty to match.
        let snapshot = self.buffer.read(cx).snapshot(cx);
        snapshot
            .text_for_range(target.range.clone())
            .collect::<String>()
            .lines()
            .map(|line| line.strip_prefix(&target.indent).unwrap_or(line))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The Mermaid fence `row` is in, or, when `row` is somewhere else, the nearest
/// one: the next diagram down the document, and only then the one above it.
fn nearest_mermaid_fence_target(
    snapshot: &MultiBufferSnapshot,
    row: u32,
) -> Option<MermaidFenceTarget> {
    let lines = buffer_lines(snapshot);
    let block = nearest_mermaid_fence(&lines, row)?;
    Some(fence_target(snapshot, &lines, &block))
}

/// The Mermaid fence covering `row`, and nothing else.
fn mermaid_fence_target_at_row(
    snapshot: &MultiBufferSnapshot,
    row: u32,
) -> Option<MermaidFenceTarget> {
    let lines = buffer_lines(snapshot);
    let block = fenced_code_block_at(&lines, row)?;
    if !is_mermaid_fence_info(&block.info) {
        return None;
    }
    Some(fence_target(snapshot, &lines, &block))
}

fn buffer_lines(snapshot: &MultiBufferSnapshot) -> Vec<String> {
    let max_row = snapshot.max_point().row;
    (0..=max_row)
        .map(|row| line_text(snapshot, row))
        .collect::<Vec<_>>()
}

fn fence_target(
    snapshot: &MultiBufferSnapshot,
    lines: &[String],
    block: &FencedCodeBlock,
) -> MermaidFenceTarget {
    let start = snapshot.point_to_offset(Point::new(block.body_start, 0));
    let end = snapshot.point_to_offset(Point::new(block.body_end, 0));
    let source = lines
        .get(block.body_start as usize..block.body_end as usize)
        .unwrap_or_default()
        .iter()
        .map(|line| line.strip_prefix(&block.indent).unwrap_or(line))
        .collect::<Vec<_>>()
        .join("\n");

    MermaidFenceTarget {
        range: snapshot.anchor_before(start)..snapshot.anchor_after(end),
        source,
        info: block.info.clone(),
        indent: block.indent.clone(),
        fence: None,
    }
}

/// Whether `info` opens a fence the Markdown preview draws as a Mermaid
/// diagram. Mirrors `markdown::parse_mermaid_info`, which `editor` cannot call
/// because it does not depend on that crate.
fn is_mermaid_fence_info(info: &str) -> bool {
    info.split_whitespace().next() == Some("mermaid")
}

/// A CommonMark fenced code block: the rows between its fences, the info string
/// on the opening fence, and the indentation both fences sit at.
struct FencedCodeBlock {
    info: String,
    indent: String,
    body_start: u32,
    /// The row of the closing fence, i.e. one past the last body row.
    body_end: u32,
}

impl FencedCodeBlock {
    /// The rows the block occupies, both fence lines included.
    fn rows(&self) -> RangeInclusive<u32> {
        self.body_start.saturating_sub(1)..=self.body_end
    }
}

/// Every closed fenced code block in `lines`, in document order. A block that
/// is never closed is left out: it has no body to write back into.
fn fenced_code_blocks(lines: &[String]) -> Vec<FencedCodeBlock> {
    let mut blocks = Vec::new();
    let mut open: Option<(u32, char, usize, String, String)> = None;

    for (index, line) in lines.iter().enumerate() {
        let index = index as u32;
        match &open {
            Some((start, marker, length, info, indent)) => {
                if !is_closing_fence(line, *marker, *length) {
                    continue;
                }
                blocks.push(FencedCodeBlock {
                    info: info.clone(),
                    indent: indent.clone(),
                    body_start: start + 1,
                    body_end: index,
                });
                open = None;
            }
            None => {
                if let Some((marker, length, info, indent)) = parse_opening_fence(line) {
                    open = Some((index, marker, length, info, indent));
                }
            }
        }
    }

    blocks
}

/// The fenced code block that `row` falls in, counting the fence lines
/// themselves as part of the block. `None` for a row outside every block, and
/// for a block that is never closed, which has no body to write back into.
fn fenced_code_block_at(lines: &[String], row: u32) -> Option<FencedCodeBlock> {
    fenced_code_blocks(lines)
        .into_iter()
        .find(|block| block.rows().contains(&row))
}

/// The Mermaid fence `row` is in, or the nearest one when `row` is outside every
/// Mermaid fence: the next diagram down the document first, so that a cursor
/// standing before the first diagram lands on it rather than on nothing.
fn nearest_mermaid_fence(lines: &[String], row: u32) -> Option<FencedCodeBlock> {
    let fences = fenced_code_blocks(lines)
        .into_iter()
        .filter(|block| is_mermaid_fence_info(&block.info))
        .collect::<Vec<_>>();
    let index = fences
        .iter()
        .position(|block| block.rows().contains(&row))
        .or_else(|| fences.iter().position(|block| *block.rows().start() > row))
        .or_else(|| fences.iter().rposition(|block| block.body_end < row))?;
    fences.into_iter().nth(index)
}

/// The fence character, its run length, the info string and the indentation of
/// an opening fence, or `None` for an ordinary line.
fn parse_opening_fence(line: &str) -> Option<(char, usize, String, String)> {
    let indent_length = line.len() - line.trim_start().len();
    // More than three spaces of indentation makes the line indented code rather
    // than a fence.
    if indent_length > 3 {
        return None;
    }
    let indent = line.get(..indent_length)?.to_string();
    let rest = line.get(indent_length..)?;
    let marker = rest.chars().next().filter(|c| *c == '`' || *c == '~')?;
    let length = rest.chars().take_while(|c| *c == marker).count();
    if length < 3 {
        return None;
    }
    let info = rest.get(length..)?.trim().to_string();
    // A backtick fence cannot carry a backtick in its info string, which is what
    // keeps an inline code span from opening a block.
    if marker == '`' && info.contains('`') {
        return None;
    }
    Some((marker, length, info, indent))
}

/// The length of the longest run of backticks in `text`, which is what decides
/// how long a fence has to be to hold it.
fn longest_backtick_run(text: impl Iterator<Item = char>) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for character in text {
        if character == '`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

pub(crate) fn is_closing_fence(line: &str, marker: char, opening_length: usize) -> bool {
    let trimmed = line.trim_start();
    if line.len() - trimmed.len() > 3 {
        return false;
    }
    let length = trimmed.chars().take_while(|c| *c == marker).count();
    length >= opening_length
        && trimmed
            .get(length..)
            .is_some_and(|rest| rest.trim().is_empty())
}

/// Terminal columns `text` occupies, so that a table of Japanese text lines up
/// in a fixed-width editor: East Asian wide characters count as two. The table
/// editor sizes its columns with the same measure, so a column is drawn as wide
/// as the Markdown it will be written back as.
pub fn display_width(text: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_heading_level() {
        assert_eq!(heading_level("# a"), Some(1));
        assert_eq!(heading_level("###### a"), Some(6));
        assert_eq!(heading_level("#######"), None);
        assert_eq!(heading_level("#not a heading"), None);
        assert_eq!(heading_level("##"), Some(2));
        assert_eq!(heading_level("text"), None);
    }

    #[test]
    fn test_list_marker() {
        assert_eq!(list_marker("- a"), Some((MarkdownListKind::Bullet, 2)));
        assert_eq!(list_marker("* a"), Some((MarkdownListKind::Bullet, 2)));
        assert_eq!(list_marker("+ a"), Some((MarkdownListKind::Bullet, 2)));
        assert_eq!(list_marker("- [ ] a"), Some((MarkdownListKind::Task, 6)));
        assert_eq!(list_marker("- [x] a"), Some((MarkdownListKind::Task, 6)));
        assert_eq!(list_marker("1. a"), Some((MarkdownListKind::Numbered, 3)));
        assert_eq!(list_marker("12) a"), Some((MarkdownListKind::Numbered, 4)));
        assert_eq!(list_marker("-3"), None);
        assert_eq!(list_marker("1.a"), None);
        assert_eq!(list_marker("text"), None);
    }

    #[test]
    fn test_parse_markdown_table() {
        let table = parse_markdown_table(&[
            "| 名前 | 説明 |",
            "|:-----|-----:|",
            "| a \\| b | c |",
            "| d |",
        ])
        .expect("the block is a table");
        assert!(table.has_header);
        assert_eq!(
            table.alignments,
            vec![MarkdownTableAlignment::Left, MarkdownTableAlignment::Right]
        );
        assert_eq!(
            table.rows,
            vec![
                vec!["名前".to_string(), "説明".to_string()],
                vec!["a | b".to_string(), "c".to_string()],
                // The ragged row is padded so the grid stays rectangular.
                vec!["d".to_string(), String::new()],
            ]
        );

        let headerless =
            parse_markdown_table(&["| --- | :---: |", "| a | b |"]).expect("the block is a table");
        assert!(!headerless.has_header);
        assert_eq!(
            headerless.alignments,
            vec![
                MarkdownTableAlignment::Unspecified,
                MarkdownTableAlignment::Center
            ]
        );
        assert_eq!(
            headerless.rows,
            vec![vec!["a".to_string(), "b".to_string()]]
        );

        assert_eq!(parse_markdown_table(&["| a | b |", "| c | d |"]), None);
        // A delimiter row below the first body row is not a table GitHub renders.
        assert_eq!(parse_markdown_table(&["| a |", "| b |", "| --- |"]), None);
    }

    #[test]
    fn test_markdown_table_to_markdown() {
        let table = MarkdownTable {
            has_header: true,
            alignments: vec![
                MarkdownTableAlignment::Left,
                MarkdownTableAlignment::Center,
                MarkdownTableAlignment::Right,
            ],
            rows: vec![
                vec!["項目".into(), "説明".into(), "数".into()],
                vec!["あ".into(), "a|b".into(), "1".into()],
            ],
        };

        // Every column is padded to the display width of its widest cell, with
        // Japanese characters counting as two columns.
        assert_eq!(
            table.to_markdown(),
            "\
| 項目 | 説明 | 数  |
| :--- | :--: | --: |
| あ   | a\\|b | 1   |"
        );

        let mut ragged = MarkdownTable {
            has_header: false,
            alignments: Vec::new(),
            rows: vec![vec!["a".into(), "b".into()], vec!["c".into()]],
        };
        ragged.normalize();
        assert_eq!(
            ragged.to_markdown(),
            "\
| --- | --- |
| a   | b   |
| c   |     |"
        );
    }

    #[test]
    fn test_parse_clipboard_table() {
        assert_eq!(
            parse_clipboard_table("a\tb\nc\td\n"),
            Some(vec![
                vec!["a".to_string(), "b".to_string()],
                vec!["c".to_string(), "d".to_string()],
            ])
        );

        assert_eq!(
            parse_clipboard_table("name,note\r\n\"a,b\",\"言った \"\"ね\"\"\"\r\n"),
            Some(vec![
                vec!["name".to_string(), "note".to_string()],
                vec!["a,b".to_string(), "言った \"ね\"".to_string()],
            ])
        );

        // A single cell, even one holding a comma, is an ordinary paste.
        assert_eq!(parse_clipboard_table("a,b"), None);
        assert_eq!(parse_clipboard_table("a\r\n"), None);
        assert_eq!(parse_clipboard_table(""), None);
    }

    #[test]
    fn test_fenced_code_block_at() {
        let lines = [
            "文章",
            "```mermaid",
            "flowchart TD",
            "  A --> B",
            "```",
            "",
            "~~~mermaid 150",
            "graph LR",
            "~~~",
            "",
            "```rust",
            "fn main() {}",
            "```",
        ]
        .map(String::from);

        let block = fenced_code_block_at(&lines, 2).expect("row 2 is inside the first fence");
        assert_eq!(block.info, "mermaid");
        assert_eq!((block.body_start, block.body_end), (2, 4));
        // The fence lines themselves count as inside the block.
        assert_eq!(
            fenced_code_block_at(&lines, 1).map(|block| block.body_start),
            Some(2)
        );
        assert_eq!(
            fenced_code_block_at(&lines, 4).map(|block| block.body_start),
            Some(2)
        );
        assert!(fenced_code_block_at(&lines, 0).is_none());
        assert!(fenced_code_block_at(&lines, 5).is_none());

        let tilde = fenced_code_block_at(&lines, 7).expect("row 7 is inside the tilde fence");
        assert_eq!(tilde.info, "mermaid 150");
        assert_eq!((tilde.body_start, tilde.body_end), (7, 8));
        assert!(is_mermaid_fence_info(&tilde.info));

        let rust = fenced_code_block_at(&lines, 11).expect("row 11 is inside the Rust fence");
        assert_eq!(rust.info, "rust");
        assert!(!is_mermaid_fence_info(&rust.info));

        // A fence closed by a longer run of the same character, and one that is
        // never closed at all.
        let long_close = ["````mermaid".to_string(), "x".into(), "`````".into()];
        assert_eq!(
            fenced_code_block_at(&long_close, 1).map(|block| block.body_end),
            Some(2)
        );
        let unclosed = ["```mermaid".to_string(), "flowchart TD".into()];
        assert!(fenced_code_block_at(&unclosed, 1).is_none());
        // A tilde fence does not close a backtick one.
        let mismatched = ["```mermaid".to_string(), "x".into(), "~~~".into()];
        assert!(fenced_code_block_at(&mismatched, 1).is_none());
    }

    #[test]
    fn test_nearest_mermaid_fence() {
        let lines = [
            "文章",         // 0
            "```mermaid",   // 1
            "flowchart TD", // 2
            "```",          // 3
            "",             // 4
            "```rust",      // 5
            "fn main() {}", // 6
            "```",          // 7
            "",             // 8
            "~~~mermaid",   // 9
            "flowchart LR", // 10
            "~~~",          // 11
            "",             // 12
        ]
        .map(String::from);

        // Inside a diagram, that diagram wins over either neighbour.
        for row in 1..=3 {
            assert_eq!(
                nearest_mermaid_fence(&lines, row).map(|block| block.body_start),
                Some(2),
                "row {row}"
            );
        }
        for row in 9..=11 {
            assert_eq!(
                nearest_mermaid_fence(&lines, row).map(|block| block.body_start),
                Some(10),
                "row {row}"
            );
        }

        // Outside every diagram the next one down the document wins, and only
        // past the last one does the one above it.
        for (row, expected) in [(0, 2), (4, 10), (6, 10), (8, 10), (12, 10)] {
            assert_eq!(
                nearest_mermaid_fence(&lines, row).map(|block| block.body_start),
                Some(expected),
                "row {row}"
            );
        }

        // Nothing to fall back to when the document holds no diagram.
        let without_a_diagram = ["```rust".to_string(), "fn main() {}".into(), "```".into()];
        assert!(nearest_mermaid_fence(&without_a_diagram, 1).is_none());
    }

    #[test]
    fn test_fenced_code_block_at_keeps_indentation() {
        let lines = ["- 項目", "  ```mermaid", "  flowchart TD", "  ```"].map(String::from);
        let block = fenced_code_block_at(&lines, 2).expect("row 2 is inside the fence");
        assert_eq!(block.indent, "  ");
        assert_eq!((block.body_start, block.body_end), (2, 3));

        // Four spaces is indented code, not a fence.
        let indented = ["    ```mermaid".to_string(), "x".into(), "    ```".into()];
        assert!(fenced_code_block_at(&indented, 1).is_none());
    }

    #[test]
    fn test_longest_backtick_run() {
        assert_eq!(longest_backtick_run("".chars()), 0);
        assert_eq!(longest_backtick_run("no ticks".chars()), 0);
        assert_eq!(longest_backtick_run("a `b` c".chars()), 1);
        assert_eq!(longest_backtick_run("```\nx\n````\n```".chars()), 4);
        // A tilde fence in the body never decides the length, because the fence
        // `insert_markdown_details` writes is always spelled with backticks.
        assert_eq!(longest_backtick_run("~~~~~".chars()), 0);
        assert_eq!(longest_backtick_run("日本語```".chars()), 3);
    }

    #[test]
    fn test_selection_is_wrapped() {
        assert!(selection_is_wrapped("**a**", BOLD));
        assert!(!selection_is_wrapped("*a*", BOLD));
        assert!(selection_is_wrapped("*a*", ITALIC));
        // `**a**` is bold, so toggling italic must not strip half of the pair.
        assert!(!selection_is_wrapped("**a**", ITALIC));
        assert!(selection_is_wrapped("~~a~~", STRIKETHROUGH));
        assert!(selection_is_wrapped("`a`", INLINE_CODE));
        assert!(!selection_is_wrapped("a", INLINE_CODE));
    }
}
