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
        // Inside a fenced code block the language at the cursor is the block's
        // own, so asking only about the cursor would do nothing there without a
        // word. The buffer is asked as well, and the block is stepped over below.
        let is_markdown_buffer = self.is_markdown_buffer(cx);
        if self.read_only(cx) || !(is_markdown_buffer || self.is_in_markdown_language(cx)) {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let template = action.diagram.template();
        // A fence cannot open inside another fence: the new diagram would close
        // the block it was dropped into and take the rest of it with it. A
        // selection in a code block sends the diagram past the block's closing
        // line instead, as `new_mermaid_fence_target` does.
        let (blocks, unclosed_start) = if is_markdown_buffer {
            let lines = buffer_lines(&snapshot);
            (fenced_code_blocks(&lines), unclosed_fence_start(&lines))
        } else {
            (Vec::new(), None)
        };

        let mut edits = Vec::new();
        let mut cursors = Vec::new();
        let mut moved_past = Vec::new();
        let mut moved_before_unclosed = false;
        let max_row = snapshot.max_point().row;
        for selection in self.selections.all::<Point>(&self.display_snapshot(cx)) {
            let enclosing = blocks.iter().find(|block| {
                block.rows().contains(&selection.start.row)
                    || block.rows().contains(&selection.end.row)
            });
            let selection = match enclosing {
                Some(block) => {
                    // Several cursors in one block would otherwise stack one
                    // diagram per cursor after it.
                    if moved_past.contains(&block.body_end) {
                        continue;
                    }
                    moved_past.push(block.body_end);
                    let after_block = Point::new(
                        block.body_end,
                        snapshot.line_len(MultiBufferRow(block.body_end)),
                    );
                    after_block..after_block
                }
                // A block that is never closed runs to the end of the document,
                // so there is no line after it, and a diagram written inside it
                // would close it and turn the rest of the document from code
                // into Markdown. The diagram goes in front of its opening fence.
                None => match unclosed_start.filter(|start| selection.end.row >= *start) {
                    Some(start) => {
                        if moved_before_unclosed {
                            continue;
                        }
                        moved_before_unclosed = true;
                        let before_block = Point::new(start, 0);
                        before_block..before_block
                    }
                    None => selection.start..selection.end,
                },
            };
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
    pub const ALL: [MermaidDiagramKind; 13] = [
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
        MermaidDiagramKind::Journey,
        MermaidDiagramKind::QuadrantChart,
        MermaidDiagramKind::XyChart,
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
            Self::Journey => "ユーザージャーニー",
            Self::QuadrantChart => "4 象限チャート",
            Self::XyChart => "XY チャート (棒・折れ線)",
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
            Self::Journey => {
                r#"journey
    %% 作業 : 満足度 (1〜5) : 関わる人
    title 購入までの流れ
    section 探す
      商品を検索する: 4: 利用者
      比較する: 3: 利用者
    section 買う
      カートに入れる: 5: 利用者
      支払う: 2: 利用者, 店舗"#
            }
            Self::QuadrantChart => {
                r#"quadrantChart
    %% 点は 名前: [横, 縦] で、どちらも 0〜1
    title 施策の優先度
    x-axis 労力が小さい --> 労力が大きい
    y-axis 効果が小さい --> 効果が大きい
    quadrant-1 計画して進める
    quadrant-2 すぐやる
    quadrant-3 後回し
    quadrant-4 見直す
    施策A: [0.3, 0.8]
    施策B: [0.7, 0.6]
    施策C: [0.2, 0.3]"#
            }
            Self::XyChart => {
                r#"xychart-beta
    %% bar は棒、line は折れ線。値の数は x-axis の項目の数と揃える
    title 月別の売上
    x-axis ["1月", "2月", "3月", "4月"]
    y-axis "売上 (万円)" 0 --> 100
    bar [30, 45, 60, 80]
    line [30, 45, 60, 80]"#
            }
        }
    }
}

impl MarkdownTextColor {
    /// Every preset, in the order the format bar lists them.
    pub const ALL: [MarkdownTextColor; 8] = [
        MarkdownTextColor::Red,
        MarkdownTextColor::Orange,
        MarkdownTextColor::Yellow,
        MarkdownTextColor::Green,
        MarkdownTextColor::Blue,
        MarkdownTextColor::Purple,
        MarkdownTextColor::Pink,
        MarkdownTextColor::Gray,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Red => "赤",
            Self::Orange => "橙",
            Self::Yellow => "黄",
            Self::Green => "緑",
            Self::Blue => "青",
            Self::Purple => "紫",
            Self::Pink => "桃",
            Self::Gray => "灰",
        }
    }

    /// The color as `0xRRGGBB`.
    pub fn rgb(self) -> u32 {
        match self {
            Self::Red => 0xd44c47,
            Self::Orange => 0xd9730d,
            Self::Yellow => 0xcb912f,
            Self::Green => 0x448361,
            Self::Blue => 0x337ea9,
            Self::Purple => 0x9065b0,
            Self::Pink => 0xc14c8a,
            Self::Gray => 0x787774,
        }
    }
}

/// The background the preview gives a bare `<mark>`. Mirrors
/// `markdown::html::html_parser::DEFAULT_MARK_BACKGROUND`, which this crate
/// cannot reach.
const BARE_MARK_BACKGROUND: u32 = 0xffff00;

impl MarkdownMarkerColor {
    /// Every marker color, in the order the format bar lists them.
    pub const ALL: [MarkdownMarkerColor; 4] = [
        MarkdownMarkerColor::Yellow,
        MarkdownMarkerColor::Green,
        MarkdownMarkerColor::Blue,
        MarkdownMarkerColor::Pink,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Yellow => "黄",
            Self::Green => "緑",
            Self::Blue => "青",
            Self::Pink => "桃",
        }
    }

    /// The background written into the `<mark>` as `0xRRGGBB`, or `None` for
    /// yellow, which is written as a bare `<mark>`.
    pub fn rgb(self) -> Option<u32> {
        match self {
            Self::Yellow => None,
            Self::Green => Some(0xb7ebc6),
            Self::Blue => Some(0xbfe0ff),
            Self::Pink => Some(0xffc9e0),
        }
    }

    /// The color the preview draws the marker in, for a swatch.
    pub fn swatch_rgb(self) -> u32 {
        self.rgb().unwrap_or(BARE_MARK_BACKGROUND)
    }
}

impl MarkdownTextSize {
    /// Every size, in the order the format bar lists them.
    pub const ALL: [MarkdownTextSize; 3] = [
        MarkdownTextSize::Small,
        MarkdownTextSize::Large,
        MarkdownTextSize::ExtraLarge,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::Small => "小",
            Self::Large => "大",
            Self::ExtraLarge => "特大",
        }
    }

    /// The `font-size` written for the paragraph.
    pub fn css(self) -> &'static str {
        match self {
            Self::Small => "0.85em",
            Self::Large => "1.25em",
            Self::ExtraLarge => "1.5em",
        }
    }
}

impl Editor {
    pub fn set_markdown_text_color(
        &mut self,
        action: &SetTextColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_inline_style(
            InlineStyleRequest::SetTextColor(action.color.rgb()),
            window,
            cx,
        );
    }

    pub fn remove_markdown_text_color(
        &mut self,
        _: &RemoveTextColor,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_inline_style(InlineStyleRequest::RemoveTextColor, window, cx);
    }

    pub fn set_markdown_marker(
        &mut self,
        action: &SetMarker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_inline_style(
            InlineStyleRequest::SetMarker(action.color.rgb()),
            window,
            cx,
        );
    }

    pub fn toggle_markdown_marker(
        &mut self,
        _: &ToggleMarker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_inline_style(InlineStyleRequest::ToggleMarker, window, cx);
    }

    pub fn remove_markdown_marker(
        &mut self,
        _: &RemoveMarker,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_inline_style(InlineStyleRequest::RemoveMarker, window, cx);
    }

    pub fn set_markdown_text_size(
        &mut self,
        action: &SetTextSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_paragraph_size(Some(action.size.css()), window, cx);
    }

    pub fn reset_markdown_text_size(
        &mut self,
        _: &ResetTextSize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.apply_markdown_paragraph_size(None, window, cx);
    }

    fn apply_markdown_inline_style(
        &mut self,
        request: InlineStyleRequest,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((text, selections)) = self.markdown_style_input(cx) else {
            return;
        };
        let plan = plan_inline_style(&text, &selections, request);
        self.apply_markdown_style_plan(plan, window, cx);
    }

    fn apply_markdown_paragraph_size(
        &mut self,
        size: Option<&str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some((text, selections)) = self.markdown_style_input(cx) else {
            return;
        };
        let plan = plan_paragraph_size(&text, &selections, size);
        self.apply_markdown_style_plan(plan, window, cx);
    }

    /// The buffer's text and the selections as offsets into it, when the
    /// styling actions may edit this buffer at all.
    fn markdown_style_input(&self, cx: &mut Context<Self>) -> Option<(String, Vec<Range<usize>>)> {
        if self.read_only(cx) || !self.is_in_markdown_language(cx) {
            return None;
        }
        let text = self.buffer.read(cx).snapshot(cx).text();
        let selections = self
            .selections
            .all::<MultiBufferOffset>(&self.display_snapshot(cx))
            .into_iter()
            .map(|selection| selection.start.0..selection.end.0)
            .collect();
        Some((text, selections))
    }

    fn apply_markdown_style_plan(
        &mut self,
        plan: Result<StylePlan, StyleRefusal>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let plan = match plan {
            Ok(plan) => plan,
            Err(refusal) => {
                self.show_markdown_style_refusal(refusal.message().into(), window, cx);
                return;
            }
        };
        // What was left alone is told even though the rest was styled: the
        // part that stayed plain is easy to miss in a long selection.
        if let Some(refusal) = plan.partial_refusal {
            self.show_markdown_style_refusal(refusal.partial_message().into(), window, cx);
        }
        if plan.edits.is_empty() {
            return;
        }

        let snapshot = self.buffer.read(cx).snapshot(cx);
        let edits = plan
            .edits
            .into_iter()
            .map(|edit| {
                let start = snapshot.anchor_before(MultiBufferOffset(edit.range.start));
                let end = if edit.range.is_empty() {
                    start
                } else {
                    snapshot.anchor_after(MultiBufferOffset(edit.range.end))
                };
                (start..end, edit.text)
            })
            .collect::<Vec<_>>();
        let selections = plan
            .selections
            .into_iter()
            .map(|range| MultiBufferOffset(range.start)..MultiBufferOffset(range.end))
            .collect::<Vec<_>>();

        self.transact(window, cx, |this, window, cx| {
            this.buffer
                .update(cx, |buffer, cx| buffer.edit(edits, None, cx));
            this.change_selections(Default::default(), window, cx, |s| {
                s.select_ranges(selections);
            });
            this.request_autoscroll(Autoscroll::fit(), cx);
        });
    }

    /// Tells the user why a styling action changed nothing, or only some of
    /// what was selected, since a format bar button that silently does nothing
    /// reads as a broken one.
    fn show_markdown_style_refusal(
        &self,
        message: Cow<'static, str>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(workspace) = self.workspace() else {
            return;
        };
        struct MarkdownStyleRefused;
        // The workspace may be mid-update when the action arrives, so the toast
        // waits until that update is over.
        window.defer(cx, move |_, cx| {
            workspace.update(cx, |workspace, cx| {
                workspace.show_toast(
                    Toast::new(NotificationId::unique::<MarkdownStyleRefused>(), message),
                    cx,
                );
            });
        });
    }
}

/// What an inline styling action asks for. Colors are `0xRRGGBB`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InlineStyleRequest {
    SetTextColor(u32),
    RemoveTextColor,
    /// `None` is the bare, yellow `<mark>`.
    SetMarker(Option<u32>),
    ToggleMarker,
    RemoveMarker,
}

/// Why a styling action left the document as it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleRefusal {
    InsideCode,
    InsideTag,
    CrossesTags,
    CrossesTableCells,
    CrossesLink,
    InsideLink,
    StartsHtmlBlock,
    NothingSelected,
    NoTextColor,
    NoMarker,
    NoParagraph,
    NoTextSize,
}

impl StyleRefusal {
    fn message(self) -> &'static str {
        match self {
            Self::InsideCode => "コードの中は書式を変更できません。",
            Self::InsideTag => "HTML のタグやコメントの中には書式を付けられません。",
            Self::CrossesTags => {
                "選択範囲が既存の HTML タグ (文字色・マーカーなど) をまたいでいるため、変更できません。"
            }
            Self::CrossesTableCells => "表のセルをまたぐ範囲には書式を付けられません。",
            Self::CrossesLink => {
                "選択範囲がリンクや画像の書き方 ([文字](URL) など) の一部だけにかかっているため、変更できません。リンク全体か、リンクの文字だけを選んでください。"
            }
            Self::InsideLink => "リンクの URL や画像の書き方の中には書式を付けられません。",
            Self::StartsHtmlBlock => {
                "HTML ブロックの先頭からタグで包むと HTML ブロックが段落に変わってしまうため、変更できません。要素の中の文字だけを選んでください。"
            }
            Self::NothingSelected => "書式を付ける文字がありません。",
            Self::NoTextColor => "選択範囲に文字色の付いた部分がありません。",
            Self::NoMarker => "選択範囲にマーカーの付いた部分がありません。",
            Self::NoParagraph => {
                "文字サイズは段落にだけ付けられます (見出し・コードブロック・表・HTML ブロックは対象外です)。"
            }
            Self::NoTextSize => "文字サイズの指定された段落がありません。",
        }
    }

    /// Why part of the selection was left as it was while the rest was styled.
    fn partial_message(self) -> String {
        format!(
            "選択範囲の一部には書式を付けられませんでした: {}",
            self.message()
        )
    }

    /// Whether this refusal leaves part of what was asked for undone. There
    /// being nothing to take off in one paragraph, while another has a color to
    /// remove, is just how a removal over several paragraphs goes.
    fn leaves_request_undone(self) -> bool {
        !matches!(
            self,
            Self::NothingSelected | Self::NoTextColor | Self::NoMarker | Self::NoTextSize
        )
    }
}

/// The edits a styling action makes, and where the selections end up.
#[derive(Debug, Default, PartialEq, Eq)]
struct StylePlan {
    /// Sorted, and never overlapping or touching one another.
    edits: Vec<StyleEdit>,
    /// Offsets into the text as it reads once `edits` are applied.
    selections: Vec<Range<usize>>,
    /// Why part of the selections was left alone while the rest was styled.
    partial_refusal: Option<StyleRefusal>,
}

/// A replacement of one range of the source, together with where a selection
/// endpoint that sits on the range lands in the new text: past an opening tag,
/// before a closing one, inside the new tags of a wrapped selection.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StyleEdit {
    range: Range<usize>,
    text: String,
    /// Where an offset at `range.start` lands in `text`.
    start_lands: usize,
    /// Where an offset strictly inside `range` lands in `text`.
    inside_lands: usize,
    /// Where an offset at `range.end` of a non-empty `range` lands in `text`.
    end_lands: usize,
}

impl StyleEdit {
    /// Rewrites an opening tag: the text that follows it stays after it.
    fn opening_tag(range: Range<usize>, text: String) -> Self {
        let length = text.len();
        Self {
            range,
            text,
            start_lands: 0,
            inside_lands: length,
            end_lands: length,
        }
    }

    /// Rewrites a closing tag: the text before it stays before it.
    fn closing_tag(range: Range<usize>, text: String) -> Self {
        let length = text.len();
        Self {
            range,
            text,
            start_lands: 0,
            inside_lands: 0,
            end_lands: length,
        }
    }

    /// Inserts an opening tag, which an offset at the insertion point ends up
    /// in front of, with the text it opens.
    fn insert_opening(at: usize, text: String) -> Self {
        let length = text.len();
        Self {
            range: at..at,
            text,
            start_lands: length,
            inside_lands: length,
            end_lands: length,
        }
    }

    /// Inserts a closing tag, which an offset at the insertion point stays in
    /// front of.
    fn insert_closing(at: usize, text: String) -> Self {
        Self {
            range: at..at,
            text,
            start_lands: 0,
            inside_lands: 0,
            end_lands: 0,
        }
    }

    /// Replaces `range` with `open`, the rewritten `inner` and `close`, leaving
    /// the selection on the inner text.
    fn wrap(range: Range<usize>, open: &str, inner: &str, close: &str) -> Self {
        Self {
            range,
            text: format!("{open}{inner}{close}"),
            start_lands: open.len(),
            inside_lands: open.len(),
            end_lands: open.len() + inner.len(),
        }
    }

    fn touches(&self, other: &StyleEdit) -> bool {
        self.range.start <= other.range.end && other.range.start <= self.range.end
    }
}

/// Where `offset` ends up once `edits` (sorted, not overlapping) are applied.
fn map_offset_through_edits(offset: usize, edits: &[StyleEdit]) -> usize {
    let mut shifted = offset;
    for edit in edits {
        let Range { start, end } = edit.range;
        if offset < start {
            break;
        }
        let new_start = start + shifted - offset;
        if offset == start {
            return new_start + edit.start_lands;
        }
        if offset < end {
            return new_start + edit.inside_lands;
        }
        if offset == end {
            return new_start + edit.end_lands;
        }
        shifted = shifted + edit.text.len() - edit.range.len();
    }
    shifted
}

/// Applies `edits`, all of which lie inside `range`, to that slice of `text`.
fn apply_edits_to_slice(text: &str, range: Range<usize>, edits: &[StyleEdit]) -> String {
    let mut result = String::new();
    let mut position = range.start;
    for edit in edits {
        result.push_str(text.get(position..edit.range.start).unwrap_or_default());
        result.push_str(&edit.text);
        position = edit.range.end;
    }
    result.push_str(text.get(position..range.end).unwrap_or_default());
    result
}

/// The inline HTML elements the styling actions write and edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StyleTagName {
    Span,
    Mark,
}

/// A balanced `<span>…</span>` or `<mark>…</mark>` in a block's text, paired
/// the way the preview pairs them: a closing tag only closes the innermost
/// open element of its own name.
#[derive(Clone, Debug, PartialEq, Eq)]
struct StyledElement {
    name: StyleTagName,
    open: Range<usize>,
    close: Range<usize>,
}

impl StyledElement {
    fn outer(&self) -> Range<usize> {
        self.open.start..self.close.end
    }

    fn inner(&self) -> Range<usize> {
        self.open.end..self.close.start
    }

    /// Whether `segment` sits in the element's text, or is the whole element.
    fn encloses(&self, segment: &Range<usize>) -> bool {
        (self.open.end <= segment.start && segment.end <= self.close.start)
            || self.outer() == *segment
    }

    /// Whether both tags lie inside `segment`.
    fn is_inside(&self, segment: &Range<usize>) -> bool {
        segment.start <= self.open.start && self.close.end <= segment.end
    }

    /// Whether wrapping `segment` would leave one of the element's tags inside
    /// the new element and the other outside, or would cut a tag in two.
    fn crosses(&self, segment: &Range<usize>) -> bool {
        tags_cross(&self.open, &self.close, segment)
    }
}

/// Whether wrapping `segment` would leave one of an element's two tags inside
/// the new element and the other outside, or would cut a tag in two.
fn tags_cross(open: &Range<usize>, close: &Range<usize>, segment: &Range<usize>) -> bool {
    let cuts = |tag: &Range<usize>| {
        (tag.start < segment.start && segment.start < tag.end)
            || (tag.start < segment.end && segment.end < tag.end)
    };
    if cuts(open) || cuts(close) {
        return true;
    }
    if segment.is_empty() {
        return false;
    }
    let contains = |tag: &Range<usize>| segment.start <= tag.start && tag.end <= segment.end;
    contains(open) != contains(close)
}

/// The HTML of a block's text that a new tag must be kept out of: every tag,
/// comment and autolink, and the pairs of tags of any name, so that
/// `<b>a</b>` is not cut into `<b><span>a</b></span>` and a cursor inside
/// `<a href="…">` does not get a tag written into the attribute.
struct BlockMarkup {
    tags: Vec<Range<usize>>,
    elements: Vec<(Range<usize>, Range<usize>)>,
}

impl BlockMarkup {
    fn scan(text: &str, range: Range<usize>, code_spans: &[Range<usize>]) -> Self {
        let mut tags = Vec::new();
        let mut elements = Vec::new();
        let mut open: Vec<(String, Range<usize>)> = Vec::new();
        let mut index = range.start;

        while index < range.end {
            let Some(relative) = text.get(index..range.end).and_then(|rest| rest.find('<')) else {
                break;
            };
            let start = index + relative;
            if let Some(code) = code_spans
                .iter()
                .find(|code| code.start <= start && start < code.end)
            {
                index = code.end;
                continue;
            }
            if start > 0 && text.as_bytes().get(start - 1) == Some(&b'\\') {
                index = start + 1;
                continue;
            }
            let rest = text.get(start..range.end).unwrap_or_default();
            if rest.starts_with("<!--") {
                let end = rest
                    .find("-->")
                    .map_or(range.end, |end| start + end + "-->".len());
                tags.push(start..end);
                index = end;
                continue;
            }
            let starts_like_a_tag = rest
                .strip_prefix('<')
                .map(|after| after.strip_prefix('/').unwrap_or(after))
                .and_then(|after| after.chars().next())
                .is_some_and(|character| {
                    character.is_ascii_alphabetic() || matches!(character, '!' | '?')
                });
            let Some(length) = html_tag_length(rest).filter(|_| starts_like_a_tag) else {
                index = start + 1;
                continue;
            };
            let tag_range = start..start + length;
            tags.push(tag_range.clone());
            if let Some((name, closing)) = rest.get(..length).and_then(html_tag_name) {
                if closing {
                    if let Some(position) =
                        open.iter().rposition(|(open_name, _)| *open_name == name)
                    {
                        let (_, open_range) = open.remove(position);
                        elements.push((open_range, tag_range.clone()));
                    }
                } else {
                    open.push((name, tag_range.clone()));
                }
            }
            index = tag_range.end;
        }

        Self { tags, elements }
    }

    /// Whether `offset` sits strictly inside a tag, a comment or an autolink.
    fn is_inside_tag(&self, offset: usize) -> bool {
        self.tags
            .iter()
            .any(|tag| tag.start < offset && offset < tag.end)
    }

    fn crosses(&self, segment: &Range<usize>) -> bool {
        self.is_inside_tag(segment.start)
            || self.is_inside_tag(segment.end)
            || self
                .elements
                .iter()
                .any(|(open, close)| tags_cross(open, close, segment))
    }
}

/// The lowercased name of an opening or closing tag, and whether it is the
/// closing one. A self-closing tag has no partner and gets `None`.
fn html_tag_name(tag: &str) -> Option<(String, bool)> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    if inner.ends_with('/') {
        return None;
    }
    let (inner, closing) = match inner.strip_prefix('/') {
        Some(inner) => (inner, true),
        None => (inner, false),
    };
    let name_end = inner
        .find(|character: char| !(character.is_ascii_alphanumeric() || character == '-'))
        .unwrap_or(inner.len());
    let name = inner.get(..name_end)?;
    if !name.starts_with(|character: char| character.is_ascii_alphabetic()) {
        return None;
    }
    Some((name.to_ascii_lowercase(), closing))
}

/// An attribute of an opening tag, with its range inside the tag.
struct TagAttribute {
    name: String,
    range: Range<usize>,
    value: Option<String>,
}

/// The attributes of an opening tag such as `<span style="color: red">`, or
/// `None` when `tag` is not one.
fn opening_tag_attributes(tag: &str) -> Option<Vec<TagAttribute>> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let name_end = inner
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(inner.len());
    if name_end == 0 {
        return None;
    }

    let mut attributes = Vec::new();
    let mut position = name_end;
    loop {
        let rest = inner.get(position..)?;
        let trimmed = rest.trim_start();
        position += rest.len() - trimmed.len();
        if trimmed.is_empty() {
            break;
        }

        let name_length = trimmed
            .find(|character: char| character == '=' || character.is_whitespace())
            .unwrap_or(trimmed.len());
        let name = trimmed.get(..name_length)?.to_ascii_lowercase();
        let attribute_start = position;
        position += name_length;

        let rest = inner.get(position..)?;
        let after_space = rest.trim_start();
        let mut value = None;
        if let Some(after_equals) = after_space.strip_prefix('=') {
            let value_rest = after_equals.trim_start();
            let value_start = position + (rest.len() - value_rest.len());
            match value_rest.chars().next() {
                Some(quote @ ('"' | '\'')) => {
                    let body = value_rest.get(1..)?;
                    let closing = body.find(quote)?;
                    value = Some(body.get(..closing)?.to_string());
                    position = value_start + 1 + closing + 1;
                }
                Some(_) => {
                    let length = value_rest
                        .find(char::is_whitespace)
                        .unwrap_or(value_rest.len());
                    value = Some(value_rest.get(..length)?.to_string());
                    position = value_start + length;
                }
                None => return None,
            }
        }

        // Offsets are into `tag`, which starts with the `<` that `inner` lost.
        attributes.push(TagAttribute {
            name,
            range: attribute_start + 1..position + 1,
            value,
        });
    }
    Some(attributes)
}

/// The declarations of a `style` attribute, in order, with lowercased
/// properties.
fn style_declarations(style: &str) -> Vec<(String, String)> {
    // A `;` inside quotes or parentheses, as in
    // `background: url(data:image/png;base64,…)`, belongs to the value: cutting
    // there would write back a declaration that lost half of itself.
    let mut declarations = Vec::new();
    let mut quote = None;
    let mut parentheses = 0usize;
    let mut start = 0;
    for (index, character) in style.char_indices() {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '(') => parentheses += 1,
            (None, ')') => parentheses = parentheses.saturating_sub(1),
            (None, ';') if parentheses == 0 => {
                declarations.push(style.get(start..index).unwrap_or_default());
                start = index + 1;
            }
            _ => {}
        }
    }
    declarations.push(style.get(start..).unwrap_or_default());

    declarations
        .into_iter()
        .filter_map(|declaration| {
            let (property, value) = declaration.split_once(':')?;
            let property = property.trim().to_ascii_lowercase();
            let value = value.trim();
            (!property.is_empty() && !value.is_empty()).then(|| (property, value.to_string()))
        })
        .collect()
}

/// The `style` declarations an element's opening tag carries, and whether it
/// has attributes besides `style`.
fn element_style(text: &str, element: &StyledElement) -> (Vec<(String, String)>, bool) {
    let attributes = text
        .get(element.open.clone())
        .and_then(opening_tag_attributes)
        .unwrap_or_default();
    let declarations = attributes
        .iter()
        .find(|attribute| attribute.name == "style")
        .and_then(|attribute| attribute.value.as_deref())
        .map(style_declarations)
        .unwrap_or_default();
    let has_other_attributes = attributes.iter().any(|attribute| attribute.name != "style");
    (declarations, has_other_attributes)
}

fn declares(text: &str, element: &StyledElement, property: &str) -> bool {
    element_style(text, element)
        .0
        .iter()
        .any(|(declared, _)| declared == property)
}

/// `tag` with its `style` attribute replaced by `declarations`, dropped when
/// there are none, or added when the tag had no `style` yet.
fn opening_tag_with_style(tag: &str, declarations: &[(String, String)]) -> String {
    let style = declarations
        .iter()
        .map(|(property, value)| format!("{property}: {value}"))
        .collect::<Vec<_>>()
        .join("; ");
    // A value such as `font-family: "Yu Gothic"` came from a single-quoted
    // attribute; wrapping it in double quotes again would end the attribute
    // in the middle of the value.
    let attribute = if !style.contains('"') {
        format!("style=\"{style}\"")
    } else if !style.contains('\'') {
        format!("style='{style}'")
    } else {
        format!("style=\"{}\"", style.replace('"', "&quot;"))
    };
    let existing = opening_tag_attributes(tag)
        .unwrap_or_default()
        .into_iter()
        .find(|attribute| attribute.name == "style");

    match existing {
        Some(existing) => {
            let before = tag.get(..existing.range.start).unwrap_or_default();
            let after = tag.get(existing.range.end..).unwrap_or_default();
            if style.is_empty() {
                format!("{}{after}", before.trim_end())
            } else {
                format!("{before}{attribute}{after}")
            }
        }
        None if style.is_empty() => tag.to_string(),
        None => {
            let end = tag
                .strip_suffix("/>")
                .map(|head| head.len())
                .unwrap_or(tag.len().saturating_sub(1));
            let head = tag.get(..end).unwrap_or_default().trim_end();
            let tail = tag.get(end..).unwrap_or_default();
            format!("{head} {attribute}{tail}")
        }
    }
}

/// Sets `property` on an element (replacing its value when it is already
/// declared) and removes the `removed` properties. No edit when the tag would
/// come out as it went in.
fn set_element_declaration(
    text: &str,
    element: &StyledElement,
    property: Option<(&str, &str)>,
    removed: &[&str],
) -> Vec<StyleEdit> {
    let Some(tag) = text.get(element.open.clone()) else {
        return Vec::new();
    };
    let (mut declarations, _) = element_style(text, element);
    declarations.retain(|(declared, _)| !removed.contains(&declared.as_str()));
    if let Some((property, value)) = property {
        match declarations
            .iter_mut()
            .find(|(declared, _)| declared == property)
        {
            Some((_, existing)) => *existing = value.to_string(),
            None => declarations.push((property.to_string(), value.to_string())),
        }
    }
    let rewritten = opening_tag_with_style(tag, &declarations);
    if rewritten == tag {
        Vec::new()
    } else {
        vec![StyleEdit::opening_tag(element.open.clone(), rewritten)]
    }
}

/// Removes an element's tags, keeping its text.
fn unwrap_element(element: &StyledElement) -> Vec<StyleEdit> {
    vec![
        StyleEdit::opening_tag(element.open.clone(), String::new()),
        StyleEdit::closing_tag(element.close.clone(), String::new()),
    ]
}

/// Removes `properties` from an element, and the element itself once a
/// `<span>` is left with nothing to say.
fn remove_element_declarations(
    text: &str,
    element: &StyledElement,
    properties: &[&str],
) -> Vec<StyleEdit> {
    let (declarations, has_other_attributes) = element_style(text, element);
    if !declarations
        .iter()
        .any(|(declared, _)| properties.contains(&declared.as_str()))
    {
        return Vec::new();
    }
    let left = declarations
        .iter()
        .filter(|(declared, _)| !properties.contains(&declared.as_str()))
        .count();
    if element.name == StyleTagName::Span && left == 0 && !has_other_attributes {
        unwrap_element(element)
    } else {
        set_element_declaration(text, element, None, properties)
    }
}

/// The `<span>` and `<mark>` elements in `range`, skipping the tags that are
/// only text: inside code spans, inside comments, or escaped.
fn scan_styled_elements(
    text: &str,
    range: Range<usize>,
    code_spans: &[Range<usize>],
) -> Vec<StyledElement> {
    let mut elements = Vec::new();
    let mut open: Vec<(StyleTagName, Range<usize>)> = Vec::new();
    let mut index = range.start;

    while index < range.end {
        let Some(relative) = text.get(index..range.end).and_then(|rest| rest.find('<')) else {
            break;
        };
        let start = index + relative;
        if let Some(code) = code_spans
            .iter()
            .find(|code| code.start <= start && start < code.end)
        {
            index = code.end;
            continue;
        }
        if start > 0 && text.as_bytes().get(start - 1) == Some(&b'\\') {
            index = start + 1;
            continue;
        }
        let rest = text.get(start..range.end).unwrap_or_default();
        if rest.starts_with("<!--") {
            index = match rest.find("-->") {
                Some(end) => start + end + "-->".len(),
                None => range.end,
            };
            continue;
        }
        let Some(length) = html_tag_length(rest) else {
            index = start + 1;
            continue;
        };
        let tag_range = start..start + length;
        match rest.get(..length).and_then(style_tag_name) {
            Some((name, false)) => open.push((name, tag_range.clone())),
            Some((name, true)) => {
                if open.last().is_some_and(|(open_name, _)| *open_name == name)
                    && let Some((name, open_range)) = open.pop()
                {
                    elements.push(StyledElement {
                        name,
                        open: open_range,
                        close: tag_range.clone(),
                    });
                }
            }
            None => {}
        }
        index = tag_range.end;
    }

    elements
}

/// The length of the HTML tag `rest` starts with, up to its unquoted `>`.
fn html_tag_length(rest: &str) -> Option<usize> {
    let mut quote = None;
    for (index, character) in rest.char_indices().skip(1) {
        match (quote, character) {
            (Some(open), _) if character == open => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(character),
            (None, '>') => return Some(index + 1),
            (None, '<') => return None,
            _ => {}
        }
    }
    None
}

/// The element a `<span>`, `</span>`, `<mark>` or `</mark>` tag belongs to,
/// and whether it is the closing one.
fn style_tag_name(tag: &str) -> Option<(StyleTagName, bool)> {
    let inner = tag.strip_prefix('<')?.strip_suffix('>')?;
    if inner.ends_with('/') {
        return None;
    }
    let (inner, closing) = match inner.strip_prefix('/') {
        Some(inner) => (inner, true),
        None => (inner, false),
    };
    let name_end = inner
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(inner.len());
    let after_name = inner.get(name_end..)?;
    if !(after_name.is_empty() || after_name.starts_with(char::is_whitespace)) {
        return None;
    }
    let name = match inner.get(..name_end)?.to_ascii_lowercase().as_str() {
        "span" => StyleTagName::Span,
        "mark" => StyleTagName::Mark,
        _ => return None,
    };
    Some((name, closing))
}

/// The code spans in `range`, backticks included, the way CommonMark finds
/// them: a run of backticks is closed by the next run of the same length.
fn inline_code_spans(text: &str, range: Range<usize>) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let run_length = |start: usize| {
        bytes
            .get(start..range.end)
            .unwrap_or_default()
            .iter()
            .take_while(|byte| **byte == b'`')
            .count()
    };

    let mut spans = Vec::new();
    let mut index = range.start;
    while index < range.end {
        match bytes.get(index) {
            // A backslash escapes the next character, a backtick included. Every
            // character this looks for is ASCII, so stepping over a single byte
            // of a multi-byte character cannot mistake it for one of them.
            Some(b'\\') => index += 2,
            Some(b'`') => {
                let opening = run_length(index);
                let mut search = index + opening;
                let mut closed_at = None;
                while search < range.end {
                    if bytes.get(search) == Some(&b'`') {
                        let closing = run_length(search);
                        if closing == opening {
                            closed_at = Some(search + closing);
                            break;
                        }
                        search += closing;
                    } else {
                        search += 1;
                    }
                }
                match closed_at {
                    Some(end) => {
                        spans.push(index..end);
                        index = end;
                    }
                    None => index += opening,
                }
            }
            Some(_) => index += 1,
            None => break,
        }
    }
    spans
}

/// Where a line's inline text starts: past its indentation, its block quote
/// markers and its list marker.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LinePrefix {
    /// Past everything, list marker (and task checkbox) included.
    text_start: usize,
    /// Past the block quote markers only.
    after_quotes: usize,
    quote_depth: usize,
    has_list_marker: bool,
}

fn line_prefix(line: &str) -> LinePrefix {
    let skip_blanks = |position: usize| {
        let rest = line.get(position..).unwrap_or_default();
        position + (rest.len() - rest.trim_start_matches([' ', '\t']).len())
    };

    let mut position = skip_blanks(0);
    let mut quote_depth = 0;
    while line
        .get(position..)
        .is_some_and(|rest| rest.starts_with('>'))
    {
        quote_depth += 1;
        position = skip_blanks(position + 1);
    }
    let after_quotes = position;

    let rest = line.get(position..).unwrap_or_default();
    let mut has_list_marker = false;
    if !is_thematic_break(rest)
        && let Some((_, marker_length)) = list_marker(rest)
    {
        has_list_marker = true;
        position = skip_blanks(position + marker_length);
    } else if let Some(marker_length) = footnote_definition_marker(rest) {
        // A tag in front of `[^1]:` would turn the footnote into text, so the
        // marker is passed over the way a list marker is.
        has_list_marker = true;
        position = skip_blanks(position + marker_length);
    }

    LinePrefix {
        text_start: position,
        after_quotes,
        quote_depth,
        has_list_marker,
    }
}

/// The length of the `[^label]:` a footnote definition starts with.
fn footnote_definition_marker(content: &str) -> Option<usize> {
    let label = content.strip_prefix("[^")?;
    let label_end = label.find(']')?;
    let label_text = label.get(..label_end)?;
    if label_text.trim().is_empty() || label_text.contains('[') {
        return None;
    }
    label
        .get(label_end + 1..)?
        .starts_with(':')
        .then_some("[^".len() + label_end + "]:".len())
}

/// Whether `content` starts a link reference definition such as
/// `[label]: https://example.com`. Its text is not a paragraph's: a tag around
/// it would turn the definition into text and break every link that uses it.
fn is_link_reference_definition(content: &str) -> bool {
    let Some(label) = content.strip_prefix('[') else {
        return false;
    };
    if label.starts_with('^') {
        return false;
    }
    let Some(label_end) = label.find(']') else {
        return false;
    };
    label
        .get(..label_end)
        .is_some_and(|label_text| !label_text.trim().is_empty() && !label_text.contains('['))
        && label
            .get(label_end + 1..)
            .is_some_and(|rest| rest.starts_with(':'))
}

/// A `---`, `***` or `___` line, which is a rule rather than text.
fn is_thematic_break(content: &str) -> bool {
    let trimmed = content.trim();
    let Some(marker) = trimmed
        .chars()
        .next()
        .filter(|c| matches!(c, '-' | '*' | '_'))
    else {
        return false;
    };
    trimmed
        .chars()
        .all(|character| character == marker || character == ' ' || character == '\t')
        && trimmed
            .chars()
            .filter(|character| *character == marker)
            .count()
            >= 3
}

/// A `===` or `---` line, which turns the paragraph above it into a heading.
fn is_setext_underline(content: &str) -> bool {
    let trimmed = content.trim();
    !trimmed.is_empty()
        && (trimmed.chars().all(|character| character == '=')
            || trimmed.chars().all(|character| character == '-'))
}

/// The block-level HTML tag names a CommonMark HTML block can start with, even
/// in the middle of a paragraph.
const HTML_BLOCK_TAG_NAMES: &[&str] = &[
    "address",
    "article",
    "aside",
    "base",
    "basefont",
    "blockquote",
    "body",
    "caption",
    "center",
    "col",
    "colgroup",
    "dd",
    "details",
    "dialog",
    "dir",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "frame",
    "frameset",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "head",
    "header",
    "hr",
    "html",
    "iframe",
    "legend",
    "li",
    "link",
    "main",
    "menu",
    "menuitem",
    "nav",
    "noframes",
    "ol",
    "optgroup",
    "option",
    "p",
    "param",
    "search",
    "section",
    "summary",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "title",
    "tr",
    "track",
    "ul",
];

/// Whether a line whose text is `content` starts a CommonMark HTML block. A
/// line holding nothing but an inline tag only does so after a blank line.
fn starts_html_block(content: &str, interrupts_paragraph: bool) -> bool {
    let lowered = content.to_ascii_lowercase();
    if !lowered.starts_with('<') {
        return false;
    }
    if lowered.starts_with("<!--") || lowered.starts_with("<?") || lowered.starts_with("<![cdata[")
    {
        return true;
    }
    if lowered.starts_with("<!")
        && lowered
            .as_bytes()
            .get(2)
            .is_some_and(|byte| byte.is_ascii_alphabetic())
    {
        return true;
    }

    let (after_bracket, closing) = match lowered.strip_prefix("</") {
        Some(rest) => (rest, true),
        None => (lowered.get(1..).unwrap_or_default(), false),
    };
    let name_end = after_bracket
        .find(|character: char| !character.is_ascii_alphanumeric())
        .unwrap_or(after_bracket.len());
    let name = after_bracket.get(..name_end).unwrap_or_default();
    let after_name = after_bracket.get(name_end..).unwrap_or_default();
    if name.is_empty() {
        return false;
    }
    let name_ends = after_name.is_empty()
        || after_name.starts_with([' ', '\t', '>'])
        || after_name.starts_with("/>");

    if !closing && matches!(name, "script" | "pre" | "style" | "textarea") && name_ends {
        return true;
    }
    if HTML_BLOCK_TAG_NAMES.contains(&name) && name_ends {
        return true;
    }
    !interrupts_paragraph
        && html_tag_length(content)
            .and_then(|length| content.get(length..))
            .is_some_and(|rest| rest.trim().is_empty())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextBlockKind {
    Paragraph,
    Heading,
    TableRow,
    Html,
}

/// A run of source lines whose text the styling actions work on: a paragraph
/// (also inside a list item or a block quote), a heading, a table row or an
/// HTML block.
#[derive(Clone, Debug, PartialEq, Eq)]
struct TextBlock {
    kind: TextBlockKind,
    rows: Range<usize>,
    /// From past the first row's container markers to the end of the last
    /// row, trailing whitespace left out.
    content: Range<usize>,
    quote_depth: usize,
}

/// The Markdown text of a document cut into lines and text blocks, which is
/// what the styling actions need to know about its structure.
struct MarkdownTextLayout<'a> {
    text: &'a str,
    lines: Vec<Range<usize>>,
    code_rows: Vec<bool>,
    blocks: Vec<TextBlock>,
    block_of_row: Vec<Option<usize>>,
    /// The rows that start a link reference definition, which a link anywhere
    /// in the document may refer to.
    link_definition_rows: Vec<usize>,
    /// The links found so far, by the text they were looked for in: finding
    /// them takes a Markdown parse, which is kept to the text around the
    /// blocks a selection touches instead of the whole document.
    links_by_region: RefCell<Vec<(Range<usize>, Rc<[LinkSyntax]>)>>,
}

impl<'a> MarkdownTextLayout<'a> {
    fn new(text: &'a str) -> Self {
        let mut lines = Vec::new();
        let mut line_start = 0;
        for line in text.split('\n') {
            lines.push(line_start..line_start + line.len());
            line_start += line.len() + 1;
        }

        let line_text = |row: usize| {
            lines
                .get(row)
                .and_then(|range| text.get(range.clone()))
                .unwrap_or_default()
        };

        // A fence anywhere in a list item or a block quote still fences, so the
        // container markers go before the fence is looked for. A fence that is
        // never closed runs to the end of the document.
        let mut code_rows = vec![false; lines.len()];
        let mut open_fence: Option<(char, usize)> = None;
        for (row, is_code) in code_rows.iter_mut().enumerate() {
            let line = line_text(row);
            let unquoted = line
                .get(line_prefix(line).after_quotes..)
                .unwrap_or_default()
                .trim_start();
            match open_fence {
                Some((marker, length)) => {
                    *is_code = true;
                    if is_closing_fence(unquoted, marker, length) {
                        open_fence = None;
                    }
                }
                None => {
                    let unlisted = line.get(line_prefix(line).text_start..).unwrap_or_default();
                    if let Some((marker, length, _, _)) =
                        parse_opening_fence(unquoted).or_else(|| parse_opening_fence(unlisted))
                    {
                        *is_code = true;
                        open_fence = Some((marker, length));
                    }
                }
            }
        }

        let mut table_rows = vec![false; lines.len()];
        let mut row = 0;
        while row < lines.len() {
            let is_table_candidate = |row: usize| {
                !code_rows.get(row).copied().unwrap_or(true) && is_table_line(line_text(row))
            };
            if !is_table_candidate(row) {
                row += 1;
                continue;
            }
            let start = row;
            while is_table_candidate(row) {
                row += 1;
            }
            let block = (start..row).map(line_text).collect::<Vec<_>>();
            if parse_markdown_table(&block).is_some() {
                for is_table in table_rows.get_mut(start..row).into_iter().flatten() {
                    *is_table = true;
                }
            }
        }

        let mut blocks: Vec<TextBlock> = Vec::new();
        let mut open_block: Option<TextBlock> = None;
        // The row of the latest link reference definition, whose title may
        // follow on the next row.
        let mut link_definition_row = None;
        let mut link_definition_rows = Vec::new();
        let mut in_list = false;
        for row in 0..lines.len() {
            let Some(line_range) = lines.get(row).cloned() else {
                break;
            };
            let line = line_text(row);
            let is_code = code_rows.get(row).copied().unwrap_or(false);
            if is_code || line.trim().is_empty() {
                blocks.extend(open_block.take());
                continue;
            }

            if let Some(block) = open_block.as_mut()
                && block.kind == TextBlockKind::Html
            {
                block.rows.end = row + 1;
                block.content.end = line_range.start + line.trim_end().len();
                continue;
            }

            // An indented code block is as much code as a fenced one. Inside a
            // list item the indentation belongs to the item, which this does
            // not measure, so only lines outside every list count.
            let indentation = line
                .chars()
                .take_while(|character| matches!(character, ' ' | '\t'))
                .fold(0, |columns, character| {
                    if character == '\t' {
                        columns + 4 - columns % 4
                    } else {
                        columns + 1
                    }
                });
            let paragraph_is_open_anywhere = open_block
                .as_ref()
                .is_some_and(|block| block.kind == TextBlockKind::Paragraph);
            if indentation == 0 && !paragraph_is_open_anywhere {
                in_list = false;
            }
            if indentation >= 4 && !in_list && !paragraph_is_open_anywhere {
                blocks.extend(open_block.take());
                if let Some(is_code) = code_rows.get_mut(row) {
                    *is_code = true;
                }
                continue;
            }

            if table_rows.get(row).copied().unwrap_or(false) {
                blocks.extend(open_block.take());
                // The row's outer pipes are not cell text: a tag in front of the
                // first one would become a cell of its own.
                let mut start = line.len() - line.trim_start().len();
                let mut end = line.trim_end().len();
                let cells = line.get(start..end).unwrap_or_default();
                if cells.starts_with('|') {
                    start += 1;
                }
                if cells.len() > 1 && cells.ends_with('|') && !cells.ends_with("\\|") {
                    end -= 1;
                }
                let cells = line.get(start..end).unwrap_or_default();
                start += cells.len() - cells.trim_start().len();
                end = start + cells.trim().len();
                blocks.push(TextBlock {
                    kind: TextBlockKind::TableRow,
                    rows: row..row + 1,
                    content: line_range.start + start..line_range.start + end,
                    quote_depth: 0,
                });
                continue;
            }

            let prefix = line_prefix(line);
            in_list |= prefix.has_list_marker;
            let unquoted = line.get(prefix.after_quotes..).unwrap_or_default();
            let content = line.get(prefix.text_start..).unwrap_or_default();
            let content_end = line_range.start + line.trim_end().len();
            let paragraph_is_open = open_block.as_ref().is_some_and(|block| {
                block.kind == TextBlockKind::Paragraph && block.quote_depth == prefix.quote_depth
            });

            if paragraph_is_open && !prefix.has_list_marker && is_setext_underline(unquoted) {
                if let Some(mut block) = open_block.take() {
                    block.kind = TextBlockKind::Heading;
                    blocks.push(block);
                }
                continue;
            }

            if content.trim().is_empty() || is_thematic_break(unquoted) {
                blocks.extend(open_block.take());
                continue;
            }

            if let Some(level) = heading_level(content) {
                blocks.extend(open_block.take());
                let text_start = content
                    .get(level..)
                    .map(|rest| content.len() - rest.trim_start().len())
                    .unwrap_or(content.len());
                blocks.push(TextBlock {
                    kind: TextBlockKind::Heading,
                    rows: row..row + 1,
                    content: (line_range.start + prefix.text_start + text_start).min(content_end)
                        ..content_end,
                    quote_depth: prefix.quote_depth,
                });
                continue;
            }

            let starts_block = !paragraph_is_open || prefix.has_list_marker;
            let continues_definition = row > 0
                && link_definition_row == Some(row - 1)
                && content.starts_with(['"', '\'', '(']);
            let starts_definition = starts_block && is_link_reference_definition(content);
            if starts_definition || continues_definition {
                blocks.extend(open_block.take());
                link_definition_row = Some(row);
                if starts_definition {
                    link_definition_rows.push(row);
                }
                continue;
            }

            let text_start = line_range.start + prefix.text_start;
            if starts_html_block(content, paragraph_is_open) {
                blocks.extend(open_block.take());
                open_block = Some(TextBlock {
                    kind: TextBlockKind::Html,
                    rows: row..row + 1,
                    content: text_start..content_end,
                    quote_depth: prefix.quote_depth,
                });
                continue;
            }

            if paragraph_is_open && !prefix.has_list_marker {
                if let Some(block) = open_block.as_mut() {
                    block.rows.end = row + 1;
                    block.content.end = content_end;
                }
                continue;
            }

            blocks.extend(open_block.take());
            open_block = Some(TextBlock {
                kind: TextBlockKind::Paragraph,
                rows: row..row + 1,
                content: text_start..content_end,
                quote_depth: prefix.quote_depth,
            });
        }
        blocks.extend(open_block);

        let mut block_of_row = vec![None; lines.len()];
        for (index, block) in blocks.iter().enumerate() {
            for slot in block_of_row
                .get_mut(block.rows.clone())
                .into_iter()
                .flatten()
            {
                *slot = Some(index);
            }
        }

        Self {
            text,
            lines,
            code_rows,
            blocks,
            block_of_row,
            link_definition_rows,
            links_by_region: RefCell::default(),
        }
    }

    fn line_text(&self, row: usize) -> &'a str {
        self.lines
            .get(row)
            .and_then(|range| self.text.get(range.clone()))
            .unwrap_or_default()
    }

    fn is_blank_row(&self, row: usize) -> bool {
        self.line_text(row).trim().is_empty()
    }

    /// The rows around `rows` that parse on their own the way they do in the
    /// whole document: back to a line that starts a top-level block after a
    /// blank line, since a list item's or an indented line's meaning depends
    /// on the lines before it, and on to the next blank line, which no link
    /// spans.
    fn link_region(&self, rows: Range<usize>) -> Range<usize> {
        let mut start = rows.start.min(self.lines.len().saturating_sub(1));
        while start > 0 {
            let line = self.line_text(start);
            let starts_top_level_block = !line.starts_with([' ', '\t'])
                && !self.is_blank_row(start)
                && !self.is_code_row(start)
                && self.is_blank_row(start - 1)
                && !self.is_code_row(start - 1);
            if starts_top_level_block {
                break;
            }
            start -= 1;
        }
        let mut end = rows.end.max(start + 1);
        while end < self.lines.len() && !self.is_blank_row(end) {
            end += 1;
        }
        start..end.min(self.lines.len())
    }

    /// The links and images in and around `block`.
    fn links_around(&self, block: &TextBlock) -> Rc<[LinkSyntax]> {
        let rows = self.link_region(block.rows.clone());
        let (Some(first_line), Some(last_line)) =
            (self.lines.get(rows.start), self.lines.get(rows.end - 1))
        else {
            return Rc::from([]);
        };
        let region = first_line.start..last_line.end;
        if let Some((_, links)) = self
            .links_by_region
            .borrow()
            .iter()
            .find(|(cached, _)| *cached == region)
        {
            return links.clone();
        }

        // A reference link is one only if its label is defined, anywhere in
        // the document, so the definitions outside the region go in front of
        // it. Behind it, a fence or a comment the region leaves open would
        // swallow them.
        let mut source = String::new();
        for &definition_row in &self.link_definition_rows {
            if rows.contains(&definition_row) {
                continue;
            }
            // The destination and the title may each take a line of their
            // own. A line that could open a block running past the blank line
            // after it is left out, since it would swallow the region.
            for row in definition_row..(definition_row + 3).min(self.lines.len()) {
                let line = self.line_text(row);
                let content = line.get(line_prefix(line).text_start..).unwrap_or_default();
                if row > definition_row
                    && (self.is_blank_row(row)
                        || self.is_code_row(row)
                        || content.trim_start().starts_with('<'))
                {
                    break;
                }
                source.push_str(content);
                source.push('\n');
            }
            source.push('\n');
        }
        let region_start_in_source = source.len();
        source.push_str(self.text.get(region.clone()).unwrap_or_default());

        let to_document = |offset: usize| offset - region_start_in_source + region.start;
        let links: Rc<[LinkSyntax]> = markdown_link_syntax(&source)
            .into_iter()
            .filter(|link| link.outer.start >= region_start_in_source)
            .map(|link| LinkSyntax {
                outer: to_document(link.outer.start)..to_document(link.outer.end),
                text: link
                    .text
                    .map(|text| to_document(text.start)..to_document(text.end)),
            })
            .collect();
        self.links_by_region
            .borrow_mut()
            .push((region, links.clone()));
        links
    }

    fn row_of(&self, offset: usize) -> usize {
        self.lines
            .partition_point(|line| line.end < offset)
            .min(self.lines.len().saturating_sub(1))
    }

    fn is_code_row(&self, row: usize) -> bool {
        self.code_rows.get(row).copied().unwrap_or(false)
    }

    /// Where the text of `row` starts inside `block`: the block's own start on
    /// its first row, past the row's container markers on the rows after it.
    fn row_text_start(&self, block: &TextBlock, row: usize) -> usize {
        if row <= block.rows.start {
            return block.content.start;
        }
        let Some(line) = self.lines.get(row) else {
            return block.content.end;
        };
        let line_text = self.text.get(line.clone()).unwrap_or_default();
        (line.start + line_prefix(line_text).text_start).min(block.content.end)
    }

    /// The blocks the rows of `selection` belong to, in order. A selection that
    /// ends at the very start of a row does not reach into that row.
    fn blocks_touched_by(&self, selection: &Range<usize>) -> (Vec<usize>, bool) {
        let first_row = self.row_of(selection.start);
        let mut last_row = self.row_of(selection.end);
        if selection.end > selection.start
            && last_row > first_row
            && self
                .lines
                .get(last_row)
                .is_some_and(|line| line.start == selection.end)
        {
            last_row -= 1;
        }

        let mut blocks = Vec::new();
        let mut saw_code = false;
        for row in first_row..=last_row {
            saw_code |= self.is_code_row(row);
            if let Some(Some(block)) = self.block_of_row.get(row)
                && blocks.last() != Some(block)
            {
                blocks.push(*block);
            }
        }
        (blocks, saw_code)
    }

    /// The pieces of `selection` an inline styling action wraps: one per block,
    /// kept to the block's text, and leaving out code. Along with them comes
    /// why a block the selection touches got no piece, if one did not.
    fn inline_segments(
        &self,
        selection: &Range<usize>,
    ) -> Result<(Vec<InlineSegment>, Option<StyleRefusal>), StyleRefusal> {
        if selection.is_empty() {
            let row = self.row_of(selection.start);
            if self.is_code_row(row) {
                return Err(StyleRefusal::InsideCode);
            }
            let Some(block) = self
                .block_of_row
                .get(row)
                .copied()
                .flatten()
                .and_then(|index| self.blocks.get(index))
            else {
                // A blank line, or a list item or quote with nothing in it yet,
                // takes an empty pair to type into. Anything else outside a
                // block is a rule or a heading underline, which a tag would
                // turn into text.
                let line = self.lines.get(row).cloned().unwrap_or_default();
                let line_text = self.text.get(line.clone()).unwrap_or_default();
                let text_start = line_prefix(line_text).text_start;
                if !line_text
                    .get(text_start..)
                    .unwrap_or_default()
                    .trim()
                    .is_empty()
                {
                    return Err(StyleRefusal::NothingSelected);
                }
                let offset = selection.start.max(line.start + text_start);
                return Ok((
                    vec![InlineSegment {
                        range: offset..offset,
                        elements: Vec::new(),
                        wrap_refusal: None,
                    }],
                    None,
                ));
            };
            let offset = selection
                .start
                .max(self.row_text_start(block, row))
                .min(block.content.end);
            let code_spans = inline_code_spans(self.text, block.content.clone());
            if code_spans
                .iter()
                .any(|code| code.start < offset && offset < code.end)
            {
                return Err(StyleRefusal::InsideCode);
            }
            if BlockMarkup::scan(self.text, block.content.clone(), &code_spans)
                .is_inside_tag(offset)
            {
                return Err(StyleRefusal::InsideTag);
            }
            return Ok((
                vec![InlineSegment {
                    range: offset..offset,
                    elements: scan_styled_elements(self.text, block.content.clone(), &code_spans),
                    wrap_refusal: self.wrap_refusal(block, &(offset..offset)),
                }],
                None,
            ));
        }

        let (block_indices, saw_code) = self.blocks_touched_by(selection);
        let mut segments = Vec::new();
        let mut refusal = None;
        for block in block_indices
            .into_iter()
            .filter_map(|index| self.blocks.get(index))
        {
            let mut start = selection.start.max(block.content.start);
            let mut end = selection.end.min(block.content.end);
            let start_row = self.row_of(start);
            start = start.max(self.row_text_start(block, start_row));
            let end_row = self.row_of(end);
            if end_row > block.rows.start && end <= self.row_text_start(block, end_row) {
                end = self
                    .lines
                    .get(end_row - 1)
                    .map_or(end, |line| line.end.min(end));
            }
            if start >= end {
                continue;
            }

            let code_spans = inline_code_spans(self.text, block.content.clone());
            if code_spans.iter().any(|code| {
                (code.start < start && start < code.end) || (code.start < end && end < code.end)
            }) {
                refusal.get_or_insert(StyleRefusal::InsideCode);
                continue;
            }
            if BlockMarkup::scan(self.text, block.content.clone(), &code_spans)
                .crosses(&(start..end))
            {
                refusal.get_or_insert(StyleRefusal::CrossesTags);
                continue;
            }
            if block.kind == TextBlockKind::TableRow
                && contains_unescaped_pipe(self.text.get(start..end).unwrap_or_default())
            {
                refusal.get_or_insert(StyleRefusal::CrossesTableCells);
                continue;
            }
            segments.push(InlineSegment {
                wrap_refusal: self.wrap_refusal(block, &(start..end)),
                range: start..end,
                elements: scan_styled_elements(self.text, block.content.clone(), &code_spans),
            });
        }

        if segments.is_empty() {
            return Err(refusal.unwrap_or(if saw_code {
                StyleRefusal::InsideCode
            } else {
                StyleRefusal::NothingSelected
            }));
        }
        Ok((segments, refusal))
    }

    /// Why new tags around `segment` of `block` would change what the Markdown
    /// around them means, if they would.
    fn wrap_refusal(&self, block: &TextBlock, segment: &Range<usize>) -> Option<StyleRefusal> {
        if self
            .links_around(block)
            .iter()
            .any(|link| link.is_cut_by(segment))
        {
            return Some(if segment.is_empty() {
                StyleRefusal::InsideLink
            } else {
                StyleRefusal::CrossesLink
            });
        }
        // An HTML block is one only while its first line starts with the tag it
        // starts with: a `<span` in front of it turns the block into a
        // paragraph, and its tags into inline HTML.
        (block.kind == TextBlockKind::Html && segment.start == block.content.start)
            .then_some(StyleRefusal::StartsHtmlBlock)
    }
}

/// A Markdown link or image, whose syntax a new tag must not cut into:
/// `[text](url)`, `[text][label]`, `![alt](src)`, `<https://…>` or a bare URL
/// the preview turns into a link.
#[derive(Clone, Debug, PartialEq, Eq)]
struct LinkSyntax {
    outer: Range<usize>,
    /// The text of a `[text](…)` link, where a tag is just more of the link's
    /// text. An image's alt text and an autolink's address have none: a tag
    /// there would be written into the alt text, or cut the address short.
    /// An `<a>` element is no link here, since these events leave inline HTML
    /// unparsed; its tags are kept whole the way every other tag is.
    text: Option<Range<usize>>,
}

impl LinkSyntax {
    /// Whether new tags around `segment` would land in the link's syntax
    /// rather than around the whole link or inside its text.
    fn is_cut_by(&self, segment: &Range<usize>) -> bool {
        if self
            .text
            .as_ref()
            .is_some_and(|text| text.start <= segment.start && segment.end <= text.end)
        {
            return false;
        }
        if segment.is_empty() {
            return self.outer.start < segment.start && segment.start < self.outer.end;
        }
        let overlaps = segment.start < self.outer.end && self.outer.start < segment.end;
        let contains = segment.start <= self.outer.start && self.outer.end <= segment.end;
        overlaps && !contains
    }
}

/// The links and images of `text`, found the way the preview finds them, so
/// that reference links resolve against the document's definitions and bare
/// URLs are the ones the preview links.
fn markdown_link_syntax(text: &str) -> Vec<LinkSyntax> {
    use markdown::parser::{MarkdownEvent, MarkdownTag, MarkdownTagEnd};

    struct OpenLink {
        outer: Range<usize>,
        has_text: bool,
        children: Option<Range<usize>>,
    }

    let mut links = Vec::new();
    let mut open: Vec<OpenLink> = Vec::new();
    for (range, event) in markdown::parser::parse_markdown_events(text) {
        if matches!(
            event,
            MarkdownEvent::End(MarkdownTagEnd::Link | MarkdownTagEnd::Image)
        ) && let Some(link) = open.pop()
        {
            let text_range = link.has_text.then(|| {
                link.children.unwrap_or_else(|| {
                    let after_bracket = link.outer.start + 1;
                    after_bracket..after_bracket
                })
            });
            links.push(LinkSyntax {
                outer: link.outer,
                text: text_range,
            });
        }
        for link in &mut open {
            link.children = Some(match link.children.take() {
                Some(children) => children.start.min(range.start)..children.end.max(range.end),
                None => range.clone(),
            });
        }
        match event {
            MarkdownEvent::Start(MarkdownTag::Link { .. }) => {
                let source = text.get(range.clone()).unwrap_or_default();
                open.push(OpenLink {
                    has_text: source.starts_with('['),
                    outer: range,
                    children: None,
                });
            }
            MarkdownEvent::Start(MarkdownTag::Image { .. }) => open.push(OpenLink {
                outer: range,
                has_text: false,
                children: None,
            }),
            _ => {}
        }
    }
    links
}

fn contains_unescaped_pipe(text: &str) -> bool {
    let mut escaped = false;
    for character in text.chars() {
        match character {
            '\\' if !escaped => escaped = true,
            '|' if !escaped => return true,
            _ => escaped = false,
        }
    }
    false
}

/// A piece of a selection inside one block, with the styled elements of that
/// block.
struct InlineSegment {
    range: Range<usize>,
    elements: Vec<StyledElement>,
    /// Why the segment cannot be wrapped in new tags. Rewriting an element
    /// that is already there is still fine.
    wrap_refusal: Option<StyleRefusal>,
}

impl InlineSegment {
    /// The innermost element of `name` that the segment sits in, or is.
    fn enclosing(
        &self,
        name: StyleTagName,
        accepts: impl Fn(&StyledElement) -> bool,
    ) -> Option<&StyledElement> {
        self.elements
            .iter()
            .filter(|element| {
                element.name == name && element.encloses(&self.range) && accepts(element)
            })
            .min_by_key(|element| element.outer().len())
    }

    fn crosses_any_element(&self) -> bool {
        self.elements
            .iter()
            .any(|element| element.crosses(&self.range))
    }
}

fn css_hex(rgb: u32) -> String {
    format!("#{rgb:06x}")
}

/// Works out the edits of an inline styling action for every selection.
fn plan_inline_style(
    text: &str,
    selections: &[Range<usize>],
    request: InlineStyleRequest,
) -> Result<StylePlan, StyleRefusal> {
    let layout = MarkdownTextLayout::new(text);
    let mut refusal = None;
    let mut partial_refusal = None;
    let mut note_partial = |reason: StyleRefusal| {
        if reason.leaves_request_undone() {
            partial_refusal.get_or_insert(reason);
        }
    };
    let segments_per_selection = selections
        .iter()
        .map(|selection| match layout.inline_segments(selection) {
            Ok((segments, skipped)) => {
                if let Some(reason) = skipped {
                    note_partial(reason);
                }
                segments
            }
            Err(reason) => {
                refusal.get_or_insert(reason);
                note_partial(reason);
                Vec::new()
            }
        })
        .collect::<Vec<_>>();

    // Like the other toggles, the marker comes off only when everything the
    // selections cover already has one.
    let request = match request {
        InlineStyleRequest::ToggleMarker => {
            let mut segments = segments_per_selection.iter().flatten().peekable();
            let all_marked = segments.peek().is_some()
                && segments
                    .all(|segment| segment.enclosing(StyleTagName::Mark, |_| true).is_some());
            if all_marked {
                InlineStyleRequest::RemoveMarker
            } else {
                InlineStyleRequest::SetMarker(None)
            }
        }
        request => request,
    };

    let cursor_positions = selections
        .iter()
        .zip(&segments_per_selection)
        .map(|(selection, segments)| {
            selection
                .is_empty()
                .then(|| segments.first().map(|segment| segment.range.start))
                .flatten()
        })
        .collect::<Vec<_>>();

    let mut edits: Vec<StyleEdit> = Vec::new();
    let mut any_applied = false;
    for segments in segments_per_selection {
        let mut group = Vec::new();
        for segment in &segments {
            match segment_style_edits(text, segment, request) {
                Ok(segment_edits) => {
                    any_applied = true;
                    group.extend(segment_edits);
                }
                Err(reason) => {
                    refusal.get_or_insert(reason);
                    note_partial(reason);
                }
            }
        }
        // Two cursors in the same element ask for the same edit; anything else
        // that meets an edit already planned would leave the two to fight over
        // the same text, so the later selection is left alone.
        group.retain(|edit| !edits.contains(edit));
        if group
            .iter()
            .any(|edit| edits.iter().any(|planned| planned.touches(edit)))
        {
            continue;
        }
        edits.extend(group);
    }

    if !any_applied {
        return Err(refusal.unwrap_or(StyleRefusal::NothingSelected));
    }
    // A cursor in front of a list marker or a heading's `#` had its tags moved
    // to the text, and follows them there.
    let selections = selections
        .iter()
        .zip(&cursor_positions)
        .map(|(selection, cursor)| cursor.map_or(selection.clone(), |cursor| cursor..cursor))
        .collect::<Vec<_>>();
    Ok(StylePlan {
        partial_refusal,
        ..style_plan(edits, &selections)
    })
}

fn style_plan(mut edits: Vec<StyleEdit>, selections: &[Range<usize>]) -> StylePlan {
    edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
    let selections = selections
        .iter()
        .map(|selection| {
            let start = map_offset_through_edits(selection.start, &edits);
            let end = map_offset_through_edits(selection.end, &edits);
            start..end.max(start)
        })
        .collect();
    StylePlan {
        edits,
        selections,
        partial_refusal: None,
    }
}

/// The edits an inline styling action makes to one segment.
fn segment_style_edits(
    text: &str,
    segment: &InlineSegment,
    request: InlineStyleRequest,
) -> Result<Vec<StyleEdit>, StyleRefusal> {
    if segment.crosses_any_element() {
        return Err(StyleRefusal::CrossesTags);
    }
    let range = &segment.range;
    let colored = |element: &StyledElement| declares(text, element, "color");

    match request {
        InlineStyleRequest::SetTextColor(rgb) => {
            let color = css_hex(rgb);
            let mut edits = Vec::new();
            let host = segment.enclosing(StyleTagName::Span, colored).or_else(|| {
                // A span that covers exactly the selection, such as the one a
                // paragraph's size is written with, takes the color as well
                // rather than having another span nested inside it.
                segment.elements.iter().find(|element| {
                    element.name == StyleTagName::Span
                        && !range.is_empty()
                        && (element.inner() == *range || element.outer() == *range)
                })
            });
            let inner_colored = segment
                .elements
                .iter()
                .filter(|element| {
                    element.name == StyleTagName::Span
                        && element.is_inside(range)
                        && Some(*element) != host
                        && colored(element)
                })
                .flat_map(|element| remove_element_declarations(text, element, &["color"]))
                .collect::<Vec<_>>();

            match host {
                Some(host) => {
                    edits.extend(set_element_declaration(
                        text,
                        host,
                        Some(("color", &color)),
                        &[],
                    ));
                    edits.extend(inner_colored);
                }
                None => {
                    if let Some(refusal) = segment.wrap_refusal {
                        return Err(refusal);
                    }
                    // `<mark>` paints its text black, so a color for a whole
                    // marker goes inside it, where it wins.
                    let range = segment
                        .elements
                        .iter()
                        .find(|element| {
                            element.name == StyleTagName::Mark && element.outer() == *range
                        })
                        .map_or(range.clone(), StyledElement::inner);
                    edits.extend(wrap_edits(
                        text,
                        range,
                        &format!("<span style=\"color: {color}\">"),
                        "</span>",
                        inner_colored,
                    ));
                }
            }
            Ok(edits)
        }
        InlineStyleRequest::RemoveTextColor => {
            let host = segment.enclosing(StyleTagName::Span, colored);
            let edits = segment
                .elements
                .iter()
                .filter(|element| {
                    element.name == StyleTagName::Span
                        && (Some(*element) == host || element.is_inside(range))
                })
                .flat_map(|element| remove_element_declarations(text, element, &["color"]))
                .collect::<Vec<_>>();
            if edits.is_empty() {
                return Err(StyleRefusal::NoTextColor);
            }
            Ok(edits)
        }
        InlineStyleRequest::SetMarker(background) => {
            let declaration = background.map(css_hex);
            let host = segment.enclosing(StyleTagName::Mark, |_| true);
            let inner_marks = segment
                .elements
                .iter()
                .filter(|element| {
                    element.name == StyleTagName::Mark
                        && element.is_inside(range)
                        && Some(*element) != host
                })
                .flat_map(unwrap_element)
                .collect::<Vec<_>>();

            match host {
                Some(host) => {
                    // The bare `<mark>` is the yellow one, so yellow is written
                    // by dropping the background rather than by naming it.
                    let mut edits = match declaration.as_deref() {
                        Some(value) => set_element_declaration(
                            text,
                            host,
                            Some(("background-color", value)),
                            &["background"],
                        ),
                        None => set_element_declaration(
                            text,
                            host,
                            None,
                            &["background", "background-color"],
                        ),
                    };
                    edits.extend(inner_marks);
                    Ok(edits)
                }
                None => {
                    if let Some(refusal) = segment.wrap_refusal {
                        return Err(refusal);
                    }
                    // A marker over exactly the text of a span goes around the
                    // span: `<mark>` paints its text black, which would hide a
                    // color the span gives it from inside.
                    let range = segment
                        .elements
                        .iter()
                        .find(|element| {
                            element.name == StyleTagName::Span
                                && !range.is_empty()
                                && element.inner() == *range
                        })
                        .map_or(range.clone(), StyledElement::outer);
                    let open = match &declaration {
                        Some(value) => format!("<mark style=\"background-color: {value}\">"),
                        None => "<mark>".to_string(),
                    };
                    Ok(wrap_edits(text, range, &open, "</mark>", inner_marks))
                }
            }
        }
        InlineStyleRequest::RemoveMarker | InlineStyleRequest::ToggleMarker => {
            let host = segment.enclosing(StyleTagName::Mark, |_| true);
            let edits = segment
                .elements
                .iter()
                .filter(|element| {
                    element.name == StyleTagName::Mark
                        && (Some(*element) == host || element.is_inside(range))
                })
                .flat_map(unwrap_element)
                .collect::<Vec<_>>();
            if edits.is_empty() {
                return Err(StyleRefusal::NoMarker);
            }
            Ok(edits)
        }
    }
}

/// Wraps `range` in `open` and `close`. `inner_edits` rewrite the tags of
/// elements inside the range; when there are any, or when the range is a
/// cursor, the range is replaced as a whole so no two edits meet at one point.
fn wrap_edits(
    text: &str,
    range: Range<usize>,
    open: &str,
    close: &str,
    mut inner_edits: Vec<StyleEdit>,
) -> Vec<StyleEdit> {
    if range.is_empty() || !inner_edits.is_empty() {
        inner_edits.sort_by_key(|edit| (edit.range.start, edit.range.end));
        let inner = apply_edits_to_slice(text, range.clone(), &inner_edits);
        vec![StyleEdit::wrap(range, open, &inner, close)]
    } else {
        vec![
            StyleEdit::insert_opening(range.start, open.to_string()),
            StyleEdit::insert_closing(range.end, close.to_string()),
        ]
    }
}

/// Works out the edits that set (or, with `None`, remove) the `font-size` of
/// every paragraph the selections touch.
fn plan_paragraph_size(
    text: &str,
    selections: &[Range<usize>],
    size: Option<&str>,
) -> Result<StylePlan, StyleRefusal> {
    let layout = MarkdownTextLayout::new(text);
    let mut block_indices = Vec::new();
    let mut refusal = None;
    for selection in selections {
        let (blocks, saw_code) = layout.blocks_touched_by(selection);
        if saw_code && blocks.is_empty() {
            refusal.get_or_insert(StyleRefusal::InsideCode);
        }
        for block in blocks {
            if !block_indices.contains(&block) {
                block_indices.push(block);
            }
        }
    }

    let mut edits = Vec::new();
    let mut any_applied = false;
    for block in block_indices
        .into_iter()
        .filter_map(|index| layout.blocks.get(index))
    {
        if block.kind != TextBlockKind::Paragraph || block.content.is_empty() {
            refusal.get_or_insert(StyleRefusal::NoParagraph);
            continue;
        }
        let code_spans = inline_code_spans(text, block.content.clone());
        let elements = scan_styled_elements(text, block.content.clone(), &code_spans);
        let wrapper = elements
            .iter()
            .filter(|element| {
                element.name == StyleTagName::Span && element.outer() == block.content
            })
            .min_by_key(|element| element.inner().len());

        match (size, wrapper) {
            (Some(size), Some(wrapper)) => {
                any_applied = true;
                edits.extend(set_element_declaration(
                    text,
                    wrapper,
                    Some(("font-size", size)),
                    &[],
                ));
            }
            (Some(size), None) => {
                any_applied = true;
                edits.push(StyleEdit::insert_opening(
                    block.content.start,
                    format!("<span style=\"font-size: {size}\">"),
                ));
                edits.push(StyleEdit::insert_closing(
                    block.content.end,
                    "</span>".to_string(),
                ));
            }
            (None, wrapper) => {
                let removed = wrapper
                    .map(|wrapper| remove_element_declarations(text, wrapper, &["font-size"]))
                    .unwrap_or_default();
                if removed.is_empty() {
                    refusal.get_or_insert(StyleRefusal::NoTextSize);
                } else {
                    any_applied = true;
                    edits.extend(removed);
                }
            }
        }
    }

    if !any_applied {
        return Err(refusal.unwrap_or(StyleRefusal::NoParagraph));
    }
    // A reset has nothing to take off a heading or a table, so only a size
    // that some block could not take is worth telling about.
    let partial_refusal = size
        .is_some()
        .then_some(refusal)
        .flatten()
        .filter(|refusal| refusal.leaves_request_undone());
    Ok(StylePlan {
        partial_refusal,
        ..style_plan(edits, selections)
    })
}

#[cfg(test)]
mod style_tests {
    use super::*;
    use util::test::{generate_marked_text, marked_text_ranges};

    const RED: u32 = 0xd44c47;
    const BLUE: u32 = 0x337ea9;

    fn rendered(text: &str, plan: StylePlan) -> String {
        let new_text = apply_edits_to_slice(text, 0..text.len(), &plan.edits);
        generate_marked_text(&new_text, &plan.selections, false)
    }

    #[track_caller]
    fn inline(marked: &str, request: InlineStyleRequest) -> String {
        let (text, selections) = marked_text_ranges(marked, false);
        match plan_inline_style(&text, &selections, request) {
            Ok(plan) => rendered(&text, plan),
            Err(refusal) => format!("refused: {refusal:?}"),
        }
    }

    #[track_caller]
    fn sized(marked: &str, size: Option<&str>) -> String {
        let (text, selections) = marked_text_ranges(marked, false);
        match plan_paragraph_size(&text, &selections, size) {
            Ok(plan) => rendered(&text, plan),
            Err(refusal) => format!("refused: {refusal:?}"),
        }
    }

    use InlineStyleRequest as Request;

    #[test]
    fn test_text_color_wraps_the_selection() {
        assert_eq!(
            inline("a «b» c", Request::SetTextColor(RED)),
            "a <span style=\"color: #d44c47\">«b»</span> c"
        );
        // A cursor gets an empty pair to type into.
        assert_eq!(
            inline("aˇ b", Request::SetTextColor(RED)),
            "a<span style=\"color: #d44c47\">ˇ</span> b"
        );
        // Multi-byte text is wrapped on its character boundaries.
        assert_eq!(
            inline("日本«語»です", Request::SetTextColor(RED)),
            "日本<span style=\"color: #d44c47\">«語»</span>です"
        );
        // A code span inside the selection is carried along untouched.
        assert_eq!(
            inline("«a `b` c»", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">«a `b` c»</span>"
        );
    }

    #[test]
    fn test_text_color_replaces_an_enclosing_color_instead_of_nesting() {
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">aˇb</span>",
                Request::SetTextColor(BLUE)
            ),
            "<span style=\"color: #337ea9\">aˇb</span>"
        );
        assert_eq!(
            inline(
                "x <span style=\"color: #d44c47\">a«b»c</span> y",
                Request::SetTextColor(BLUE)
            ),
            "x <span style=\"color: #337ea9\">a«b»c</span> y"
        );
        // A selection of the whole element, tags included.
        assert_eq!(
            inline(
                "«<span style=\"color: #d44c47\">abc</span>»",
                Request::SetTextColor(BLUE)
            ),
            "«<span style=\"color: #337ea9\">abc</span>»"
        );
        // Colors inside the selection give way to the new one.
        assert_eq!(
            inline(
                "«a <span style=\"color: #d44c47\">b</span> c»",
                Request::SetTextColor(BLUE)
            ),
            "<span style=\"color: #337ea9\">«a b c»</span>"
        );
        // A span that covers exactly the selection takes the color as well.
        assert_eq!(
            inline(
                "<span style=\"font-size: 1.25em\">«abc»</span>",
                Request::SetTextColor(RED)
            ),
            "<span style=\"font-size: 1.25em; color: #d44c47\">«abc»</span>"
        );
    }

    #[test]
    fn test_removing_text_color() {
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">aˇb</span>",
                Request::RemoveTextColor
            ),
            "aˇb"
        );
        // Only the color goes when the span also sets the size.
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47; font-size: 1.25em\">aˇb</span>",
                Request::RemoveTextColor
            ),
            "<span style=\"font-size: 1.25em\">aˇb</span>"
        );
        // A span with other attributes keeps them.
        assert_eq!(
            inline(
                "<span class=\"x\" style=\"color: #d44c47\">aˇb</span>",
                Request::RemoveTextColor
            ),
            "<span class=\"x\">aˇb</span>"
        );
        // Every color inside the selection goes.
        assert_eq!(
            inline(
                "«<span style=\"color: #d44c47\">a</span> <span style=\"color: #337ea9\">b</span>»",
                Request::RemoveTextColor
            ),
            "«a b»"
        );
        assert_eq!(
            inline("plain ˇtext", Request::RemoveTextColor),
            "refused: NoTextColor"
        );
    }

    #[test]
    fn test_inline_styles_split_at_block_boundaries() {
        assert_eq!(
            inline("«one\n\ntwo»", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">«one</span>\n\n<span style=\"color: #d44c47\">two»</span>"
        );
        assert_eq!(
            inline("- «one\n- two»", Request::SetTextColor(RED)),
            "- <span style=\"color: #d44c47\">«one</span>\n- <span style=\"color: #d44c47\">two»</span>"
        );
        // Lines of one paragraph share a span, container markers and all.
        assert_eq!(
            inline("> «one\n> two»", Request::SetTextColor(RED)),
            "> <span style=\"color: #d44c47\">«one\n> two»</span>"
        );
        // A selection that starts before a list marker starts at the text.
        assert_eq!(
            inline("«- item»", Request::SetTextColor(RED)),
            "«- <span style=\"color: #d44c47\">item»</span>"
        );
        assert_eq!(
            inline("ˇ- item", Request::SetTextColor(RED)),
            "- <span style=\"color: #d44c47\">ˇ</span>item"
        );
        assert_eq!(
            inline("ˇ# title", Request::SetTextColor(RED)),
            "# <span style=\"color: #d44c47\">ˇ</span>title"
        );
        // A table row's leading pipe is not cell text.
        assert_eq!(
            inline("ˇ| a | b |\n| --- | --- |", Request::SetTextColor(RED)),
            "| <span style=\"color: #d44c47\">ˇ</span>a | b |\n| --- | --- |"
        );
        // A rule has no text to color.
        assert_eq!(
            inline("a\n\nˇ***", Request::SetTextColor(RED)),
            "refused: NothingSelected"
        );
    }

    #[test]
    fn test_inline_styles_leave_code_alone() {
        assert_eq!(
            inline("`aˇb` c", Request::SetTextColor(RED)),
            "refused: InsideCode"
        );
        assert_eq!(
            inline("a `b«c` d»", Request::SetTextColor(RED)),
            "refused: InsideCode"
        );
        assert_eq!(
            inline("```\naˇb\n```", Request::SetTextColor(RED)),
            "refused: InsideCode"
        );
        assert_eq!(
            inline("- item\n  ```\n  aˇb\n  ```", Request::SetTextColor(RED)),
            "refused: InsideCode"
        );
        // Tags inside a code span are text, not elements to recolor.
        assert_eq!(
            inline("`<span style=\"color: red\">` aˇ", Request::RemoveTextColor),
            "refused: NoTextColor"
        );
        // Only the paragraph part of a selection that runs into a fence is
        // wrapped.
        assert_eq!(
            inline("«a\n```\nb»\n```", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">«a</span>\n```\nb»\n```"
        );
    }

    #[test]
    fn test_inline_styles_refuse_to_cross_existing_tags() {
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">a«b</span>c»",
                Request::SetTextColor(BLUE)
            ),
            "refused: CrossesTags"
        );
        assert_eq!(
            inline("<mark>a«b</mark>c»", Request::SetMarker(None)),
            "refused: CrossesTags"
        );
        assert_eq!(
            inline("| a «| b» |\n| --- | --- |", Request::SetTextColor(RED)),
            "refused: CrossesTableCells"
        );
    }

    #[test]
    fn test_inline_styles_with_several_cursors() {
        assert_eq!(
            inline("ˇa ˇb", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">ˇ</span>a <span style=\"color: #d44c47\">ˇ</span>b"
        );
        // Two cursors in the same span change it once.
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">aˇbˇc</span>",
                Request::SetTextColor(BLUE)
            ),
            "<span style=\"color: #337ea9\">aˇbˇc</span>"
        );
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">aˇbˇc</span>",
                Request::RemoveTextColor
            ),
            "aˇbˇc"
        );
    }

    #[test]
    fn test_markers() {
        assert_eq!(inline("«a»", Request::SetMarker(None)), "<mark>«a»</mark>");
        assert_eq!(
            inline("«a»", Request::SetMarker(Some(0xb7ebc6))),
            "<mark style=\"background-color: #b7ebc6\">«a»</mark>"
        );
        // An existing marker changes color rather than getting another inside.
        assert_eq!(
            inline("<mark>aˇ</mark>", Request::SetMarker(Some(0xb7ebc6))),
            "<mark style=\"background-color: #b7ebc6\">aˇ</mark>"
        );
        assert_eq!(
            inline(
                "<mark style=\"background-color: #b7ebc6\">aˇ</mark>",
                Request::SetMarker(None)
            ),
            "<mark>aˇ</mark>"
        );
        assert_eq!(inline("<mark>a«b»</mark>", Request::RemoveMarker), "a«b»");
        assert_eq!(inline("aˇ", Request::RemoveMarker), "refused: NoMarker");

        assert_eq!(inline("«a»", Request::ToggleMarker), "<mark>«a»</mark>");
        assert_eq!(inline("<mark>«a»</mark>", Request::ToggleMarker), "«a»");
        // One unmarked selection is enough for the toggle to mark them all.
        assert_eq!(
            inline("<mark>«a»</mark> «b»", Request::ToggleMarker),
            "<mark>«a»</mark> <mark>«b»</mark>"
        );

        // A marker over a colored span goes around it, since `<mark>` paints
        // its text black; a color over a whole marker goes inside it.
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">«a»</span>",
                Request::SetMarker(None)
            ),
            "<mark><span style=\"color: #d44c47\">«a»</span></mark>"
        );
        assert_eq!(
            inline("«<mark>a</mark>»", Request::SetTextColor(RED)),
            "«<mark><span style=\"color: #d44c47\">a</span></mark>»"
        );
    }

    #[test]
    fn test_paragraph_size() {
        assert_eq!(
            sized("abˇc", Some("1.25em")),
            "<span style=\"font-size: 1.25em\">abˇc</span>"
        );
        assert_eq!(
            sized("<span style=\"font-size: 1.25em\">abˇc</span>", None),
            "abˇc"
        );
        assert_eq!(
            sized(
                "<span style=\"font-size: 1.25em\">abˇc</span>",
                Some("0.85em")
            ),
            "<span style=\"font-size: 0.85em\">abˇc</span>"
        );
        // A span that colors the whole paragraph takes the size too, and keeps
        // its color when the size is reset.
        assert_eq!(
            sized("<span style=\"color: #d44c47\">abˇc</span>", Some("1.5em")),
            "<span style=\"color: #d44c47; font-size: 1.5em\">abˇc</span>"
        );
        assert_eq!(
            sized(
                "<span style=\"color: #d44c47; font-size: 1.5em\">abˇc</span>",
                None
            ),
            "<span style=\"color: #d44c47\">abˇc</span>"
        );
        // A span over part of the paragraph does not count as its size.
        assert_eq!(
            sized("a <span style=\"font-size: 1.25em\">bˇ</span>", None),
            "refused: NoTextSize"
        );
    }

    #[test]
    fn test_paragraph_size_inside_containers() {
        assert_eq!(
            sized("- itˇem\n- other", Some("1.25em")),
            "- <span style=\"font-size: 1.25em\">itˇem</span>\n- other"
        );
        assert_eq!(
            sized("- [ ] taˇsk", Some("1.25em")),
            "- [ ] <span style=\"font-size: 1.25em\">taˇsk</span>"
        );
        assert_eq!(
            sized("> aˇ\n> b\n\nc", Some("1.25em")),
            "> <span style=\"font-size: 1.25em\">aˇ\n> b</span>\n\nc"
        );
        // Every paragraph the selection touches, and each only once.
        assert_eq!(
            sized("«one\n\ntwo»", Some("1.25em")),
            "<span style=\"font-size: 1.25em\">«one</span>\n\n<span style=\"font-size: 1.25em\">two»</span>"
        );
        assert_eq!(
            sized("aˇbˇc", Some("1.25em")),
            "<span style=\"font-size: 1.25em\">aˇbˇc</span>"
        );
    }

    #[test]
    fn test_paragraph_size_leaves_other_blocks_alone() {
        assert_eq!(sized("# tiˇtle", Some("1.25em")), "refused: NoParagraph");
        assert_eq!(
            sized("Tiˇtle\n=====", Some("1.25em")),
            "refused: NoParagraph"
        );
        assert_eq!(
            sized("```\ncoˇde\n```", Some("1.25em")),
            "refused: InsideCode"
        );
        assert_eq!(
            sized("| aˇ | b |\n| --- | --- |", Some("1.25em")),
            "refused: NoParagraph"
        );
        assert_eq!(
            sized("<div>\nhtˇml\n</div>", Some("1.25em")),
            "refused: NoParagraph"
        );
        // Mixed with a paragraph, only the paragraph changes.
        assert_eq!(
            sized("«# title\n\ntext»", Some("1.25em")),
            "«# title\n\n<span style=\"font-size: 1.25em\">text»</span>"
        );
    }

    #[test]
    fn test_opening_tag_with_style() {
        let color = [("color".to_string(), "#d44c47".to_string())];
        assert_eq!(
            opening_tag_with_style("<mark>", &color),
            "<mark style=\"color: #d44c47\">"
        );
        assert_eq!(
            opening_tag_with_style("<span style='color: red' class=\"a\">", &[]),
            "<span class=\"a\">"
        );
        assert_eq!(
            opening_tag_with_style("<span class=\"a\" style=\"color: red\">", &color),
            "<span class=\"a\" style=\"color: #d44c47\">"
        );
    }

    #[test]
    fn test_rewritten_styles_keep_every_other_declaration_intact() {
        // A `;` inside `url(…)` is part of the value, not the end of it.
        assert_eq!(
            inline(
                "<span style=\"background: url(data:image/png;base64,AAA); color: red\">aˇb</span>",
                Request::SetTextColor(BLUE)
            ),
            "<span style=\"background: url(data:image/png;base64,AAA); color: #337ea9\">aˇb</span>"
        );
        assert_eq!(
            inline(
                "<span style=\"background: url(data:image/png;base64,AAA); color: red\">aˇb</span>",
                Request::RemoveTextColor
            ),
            "<span style=\"background: url(data:image/png;base64,AAA)\">aˇb</span>"
        );
        // A value with double quotes keeps the single quotes it was written
        // in, or the attribute would end in the middle of it.
        assert_eq!(
            inline(
                "<span style='color: red; font-family: \"Yu Gothic\"'>aˇb</span>",
                Request::SetTextColor(BLUE)
            ),
            "<span style='color: #337ea9; font-family: \"Yu Gothic\"'>aˇb</span>"
        );
        assert_eq!(
            sized(
                "<span style='font-family: \"Yu Gothic\"'>aˇb</span>",
                Some("1.25em")
            ),
            "<span style='font-family: \"Yu Gothic\"; font-size: 1.25em'>aˇb</span>"
        );
    }

    #[test]
    fn test_inline_styles_stay_out_of_other_html() {
        // A tag written into an attribute, a comment or an autolink would
        // break it.
        assert_eq!(
            inline("<a href=\"htˇtp://x\">l</a>", Request::SetTextColor(RED)),
            "refused: InsideTag"
        );
        assert_eq!(
            inline("<img src=\"a.pˇng\">", Request::SetMarker(None)),
            "refused: InsideTag"
        );
        assert_eq!(
            inline("<!-- aˇ -->", Request::SetTextColor(RED)),
            "refused: InsideTag"
        );
        assert_eq!(
            inline("<https://exaˇmple.com>", Request::SetTextColor(RED)),
            "refused: InsideTag"
        );
        // Wrapping half of any element, not only a span or a mark, would
        // leave its tags crossed.
        assert_eq!(
            inline("<b>a«b</b>c»", Request::SetTextColor(RED)),
            "refused: CrossesTags"
        );
        assert_eq!(
            inline("a <!-- «x --> b» c", Request::SetTextColor(RED)),
            "refused: CrossesTags"
        );
        assert_eq!(
            inline("<div>\n«a\n</div>»", Request::SetTextColor(RED)),
            "refused: CrossesTags"
        );
        // Whole elements, and text next to them, are still fine.
        assert_eq!(
            inline("«<b>a</b>»", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">«<b>a</b>»</span>"
        );
        assert_eq!(
            inline("<b>«a»</b><br>«b»", Request::SetTextColor(RED)),
            "<b><span style=\"color: #d44c47\">«a»</span></b><br><span style=\"color: #d44c47\">«b»</span>"
        );
        assert_eq!(
            inline("a < b > «c»", Request::SetTextColor(RED)),
            "a < b > <span style=\"color: #d44c47\">«c»</span>"
        );
    }

    #[test]
    fn test_inline_styles_leave_link_syntax_whole() {
        const RED_SPAN: &str = "<span style=\"color: #d44c47\">";

        // Half of a link, or its destination, would come apart around a tag.
        for marked in [
            "a «[li»nk](url) b",
            "a [li«nk](url) b»",
            "[link](u«r»l)",
            "[link]«(url)»",
            "[te«xt][r»ef]\n\n[ref]: https://example.com",
            "![a«l»t](x.png)",
            "[![«a](b.png)» x](url)",
            "see https://exa«mple».com now",
            "a [«b](c) [d»](e)",
        ] {
            assert_eq!(
                inline(marked, Request::SetTextColor(RED)),
                "refused: CrossesLink",
                "{marked}"
            );
            assert_eq!(
                inline(marked, Request::SetMarker(None)),
                "refused: CrossesLink",
                "{marked}"
            );
        }
        assert_eq!(
            inline("[link](urˇl)", Request::SetTextColor(RED)),
            "refused: InsideLink"
        );
        assert_eq!(
            inline("![alˇt](x.png)", Request::ToggleMarker),
            "refused: InsideLink"
        );
        assert_eq!(
            inline(
                "[text][rˇef]\n\n[ref]: https://example.com",
                Request::SetTextColor(RED)
            ),
            "refused: InsideLink"
        );

        // The whole link, or text inside its brackets, is fine.
        assert_eq!(
            inline("«[link](url)» b", Request::SetTextColor(RED)),
            format!("{RED_SPAN}«[link](url)»</span> b")
        );
        assert_eq!(
            inline("«a [link](url)» b", Request::SetTextColor(RED)),
            format!("{RED_SPAN}«a [link](url)»</span> b")
        );
        assert_eq!(
            inline("[l«in»k](url)", Request::SetTextColor(RED)),
            format!("[l{RED_SPAN}«in»</span>k](url)")
        );
        assert_eq!(
            inline("[«リンク»](url)", Request::SetMarker(None)),
            "[<mark>«リンク»</mark>](url)"
        );
        assert_eq!(
            inline(
                "[日«本»語][ref]\n\n[ref]: https://example.com",
                Request::SetTextColor(RED)
            ),
            format!("[日{RED_SPAN}«本»</span>語][ref]\n\n[ref]: https://example.com")
        );
        assert_eq!(
            inline("[liˇnk](url)", Request::SetTextColor(RED)),
            format!("[li{RED_SPAN}ˇ</span>nk](url)")
        );
        assert_eq!(
            inline("«![alt](x.png)»", Request::SetTextColor(RED)),
            format!("{RED_SPAN}«![alt](x.png)»</span>")
        );
        assert_eq!(
            inline("[«![a](b.png)»](url)", Request::SetTextColor(RED)),
            format!("[{RED_SPAN}«![a](b.png)»</span>](url)")
        );
        assert_eq!(
            inline("see «https://example.com» now", Request::SetTextColor(RED)),
            format!("see {RED_SPAN}«https://example.com»</span> now")
        );
        assert_eq!(
            inline("<a href=\"x\">l«in»k</a>", Request::SetTextColor(RED)),
            format!("<a href=\"x\">l{RED_SPAN}«in»</span>k</a>")
        );
        // Brackets that are no link are only text.
        assert_eq!(
            inline("[no«t a» link]", Request::SetTextColor(RED)),
            format!("[no{RED_SPAN}«t a»</span> link]")
        );

        // Changing an element that is already there leaves the link as it is.
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">[li«nk](url) b»</span>",
                Request::SetTextColor(BLUE)
            ),
            "<span style=\"color: #337ea9\">[li«nk](url) b»</span>"
        );
        assert_eq!(
            inline("<mark>[li«nk](url) b»</mark>", Request::RemoveMarker),
            "[li«nk](url) b»"
        );
    }

    #[test]
    fn test_link_syntax_in_containers_and_across_lines() {
        const RED_SPAN: &str = "<span style=\"color: #d44c47\">";

        for marked in [
            "> 引用の [リ«ンク](url) 後»",
            "- 項目の [リ«ンク](url) 後»",
            "1. «前 [リ»ンク](url)",
            "[一行目\n二«行目](url) 後»",
            "> [一行目\n> 二«行目](url) 後»",
        ] {
            assert_eq!(
                inline(marked, Request::SetTextColor(RED)),
                "refused: CrossesLink",
                "{marked}"
            );
        }
        // The brackets of an autolink already read as a tag, and so does half
        // of an `<a>` element.
        for marked in [
            "a <https://exa«mple».com> b",
            "a <https://exaˇmple.com> b",
            "«x <a href=\"u\">li»nk</a>",
            "<a href=\"u\">li«nk</a> y»",
        ] {
            assert!(
                inline(marked, Request::SetTextColor(RED)).starts_with("refused: "),
                "{marked}"
            );
        }
        assert_eq!(
            inline("> 引用の [«リンク»](url) 後", Request::SetTextColor(RED)),
            format!("> 引用の [{RED_SPAN}«リンク»</span>](url) 後")
        );
        assert_eq!(
            inline("- «項目の [リンク](url)» 後", Request::SetMarker(None)),
            "- <mark>«項目の [リンク](url)»</mark> 後"
        );
        assert_eq!(
            inline("[一行目\n«二行目»](url)", Request::SetTextColor(RED)),
            format!("[一行目\n{RED_SPAN}«二行目»</span>](url)")
        );
        // Like the other refusals, one paragraph's refusal leaves the others
        // the selection covers to be styled.
        assert_eq!(
            inline("«a [b](c)\n\nd [e»](f)", Request::SetTextColor(RED)),
            format!("{RED_SPAN}«a [b](c)</span>\n\nd [e»](f)")
        );
        assert_eq!(
            inline("«a» [b](c)\n\n«d» [e](f)", Request::SetMarker(None)),
            "<mark>«a»</mark> [b](c)\n\n<mark>«d»</mark> [e](f)"
        );

        // Existing elements are still changed and taken off inside a link.
        assert_eq!(
            inline(
                "<span style=\"color: #d44c47\">[li«nk](url) b»</span>",
                Request::RemoveTextColor
            ),
            "[li«nk](url) b»"
        );
        assert_eq!(
            inline("<mark>[li«nk](url) b»</mark>", Request::ToggleMarker),
            "[li«nk](url) b»"
        );
        assert_eq!(
            inline(
                "<mark>[li«nk](url) b»</mark>",
                Request::SetMarker(Some(0xb7ebc6))
            ),
            "<mark style=\"background-color: #b7ebc6\">[li«nk](url) b»</mark>"
        );
    }

    #[test]
    fn test_links_are_found_in_the_text_around_the_selection_only() {
        let text = "前の段落\n\n- 項目\n  - 入れ子\n\n    続き\n\n後の段落\n\n    コード\n\n最後";
        let layout = MarkdownTextLayout::new(text);
        let row = |line: &str| {
            text.split('\n')
                .position(|text_line| text_line == line)
                .unwrap_or_else(|| panic!("{line:?} is not in the text"))
        };

        // A paragraph after a blank line stands on its own.
        assert_eq!(
            layout.link_region(row("後の段落")..row("後の段落") + 1),
            row("後の段落")..row("後の段落") + 1
        );
        // An indented line means what it means only after the list it
        // continues, which the region goes back to.
        assert_eq!(
            layout.link_region(row("    続き")..row("    続き") + 1),
            row("- 項目")..row("    続き") + 1
        );
        assert_eq!(
            layout.link_region(row("  - 入れ子")..row("  - 入れ子") + 1),
            row("- 項目")..row("  - 入れ子") + 1
        );
        // Nor does a paragraph after indented code need anything before it.
        assert_eq!(
            layout.link_region(row("最後")..row("最後") + 1),
            row("最後")..row("最後") + 1
        );
    }

    #[test]
    fn test_links_are_found_outside_the_region_they_would_be_parsed_in() {
        for marked in [
            // An indented line alone would be code, with no link in it.
            "- 項目\n  - 入れ子\n\n    続きの [リ«ンク](url) 後»",
            // Definitions are found wherever they are in the document.
            "前\n\n[te«xt][r»ef]\n\n後\n\n> [ref]: https://example.com",
            "[te«xt][r»ef]\n\n- [ref]: https://example.com",
            "[te«xt][r»ef]\n\n[ref]:\n  https://example.com\n  \"題\"",
            "[te«xt][r»ef]\n\n段落\n\n[other]: /x\n[ref]: /y",
            // A fence the region leaves open does not hide them.
            "[te«xt][r»ef]\n```\nコード\n\n続き\n```\n\n[ref]: https://example.com",
        ] {
            assert_eq!(
                inline(marked, Request::SetTextColor(RED)),
                "refused: CrossesLink",
                "{marked}"
            );
        }
        // Without its definition, a reference link is only brackets.
        assert_eq!(
            inline(
                "[te«xt][r»ef]\n\n[other]: https://example.com",
                Request::ToggleMarker
            ),
            "[te<mark>«xt][r»</mark>ef]\n\n[other]: https://example.com"
        );
        // A definition inside a code block defines nothing.
        assert_eq!(
            inline(
                "[te«xt][r»ef]\n\n```\n[ref]: https://example.com\n```",
                Request::ToggleMarker
            ),
            "[te<mark>«xt][r»</mark>ef]\n\n```\n[ref]: https://example.com\n```"
        );
    }

    #[track_caller]
    fn partial_inline(marked: &str, request: InlineStyleRequest) -> Option<StyleRefusal> {
        let (text, selections) = marked_text_ranges(marked, false);
        plan_inline_style(&text, &selections, request)
            .expect("part of the selection is styled")
            .partial_refusal
    }

    #[track_caller]
    fn partial_sized(marked: &str, size: Option<&str>) -> Option<StyleRefusal> {
        let (text, selections) = marked_text_ranges(marked, false);
        plan_paragraph_size(&text, &selections, size)
            .expect("part of the selection is sized")
            .partial_refusal
    }

    #[test]
    fn test_partly_refused_styles_tell_why() {
        // A paragraph refused by the styling itself.
        assert_eq!(
            partial_inline("«a [b](c)\n\nd [e»](f)", Request::SetTextColor(RED)),
            Some(StyleRefusal::CrossesLink)
        );
        assert_eq!(
            partial_inline("«a\n\n<div>b</div>»", Request::SetMarker(None)),
            Some(StyleRefusal::StartsHtmlBlock)
        );
        // A paragraph refused before any styling is worked out.
        assert_eq!(
            partial_inline(
                "«a\n\n<span style=\"color: #d44c47\">b»c</span>",
                Request::SetMarker(None)
            ),
            Some(StyleRefusal::CrossesTags)
        );
        // A cursor refused while another one is styled.
        assert_eq!(
            partial_inline("aˇb `coˇde`", Request::SetTextColor(RED)),
            Some(StyleRefusal::InsideCode)
        );
        assert_eq!(
            partial_inline("aˇb [link](urˇl)", Request::ToggleMarker),
            Some(StyleRefusal::InsideLink)
        );

        // Everything styled, or nothing there to take off, is no refusal.
        assert_eq!(
            partial_inline("«a»\n\n«b»", Request::SetTextColor(RED)),
            None
        );
        assert_eq!(
            partial_inline(
                "«<span style=\"color: #d44c47\">a</span>\n\nb»",
                Request::RemoveTextColor
            ),
            None
        );
        assert_eq!(
            partial_inline("«<mark>a</mark>\n\nb»", Request::RemoveMarker),
            None
        );

        assert_eq!(
            partial_sized("«# 見出し\n\n段落»", Some("1.25em")),
            Some(StyleRefusal::NoParagraph)
        );
        assert_eq!(partial_sized("«一\n\n二»", Some("1.25em")), None);
        assert_eq!(
            partial_sized(
                "«# 見出し\n\n<span style=\"font-size: 1.25em\">段落</span>\n\n並»",
                None
            ),
            None
        );
    }

    #[test]
    fn test_inline_styles_keep_html_blocks_blocks() {
        // A tag in front of an HTML block's first tag turns it into a
        // paragraph.
        for marked in [
            "«<div align=\"center\">中央</div>»",
            "ˇ<div>x</div>",
            "a\n\n«<p>x</p>»\n\nb",
            "- «<div>x</div>»",
            "> «<details><summary>x</summary></details>»",
        ] {
            assert_eq!(
                inline(marked, Request::SetTextColor(RED)),
                "refused: StartsHtmlBlock",
                "{marked}"
            );
            assert_eq!(
                inline(marked, Request::SetMarker(None)),
                "refused: StartsHtmlBlock",
                "{marked}"
            );
        }

        // Text inside the block is fine, and so is an element that starts a
        // paragraph rather than a block.
        assert_eq!(
            inline(
                "<div align=\"center\">«中央»</div>",
                Request::SetTextColor(RED)
            ),
            "<div align=\"center\"><span style=\"color: #d44c47\">«中央»</span></div>"
        );
        assert_eq!(
            inline("<div>\n«<b>a</b>»\n</div>", Request::SetTextColor(RED)),
            "<div>\n<span style=\"color: #d44c47\">«<b>a</b>»</span>\n</div>"
        );
        assert_eq!(
            inline("«<b>a</b> b»", Request::SetTextColor(RED)),
            "<span style=\"color: #d44c47\">«<b>a</b> b»</span>"
        );
        // An element the block starts with may still be recolored in place.
        assert_eq!(
            inline(
                "«<span style=\"color: #d44c47\">\nx\n</span>»",
                Request::SetTextColor(BLUE)
            ),
            "«<span style=\"color: #337ea9\">\nx\n</span>»"
        );
    }

    #[test]
    fn test_styles_leave_definitions_and_indented_code_alone() {
        // A tag in front of a definition turns it into text, which breaks
        // every link or footnote reference that uses it.
        assert_eq!(
            sized("ˇ[ref]: http://x", Some("1.25em")),
            "refused: NoParagraph"
        );
        assert_eq!(
            inline("[ref]: htˇtp://x \"title\"", Request::SetTextColor(RED)),
            "refused: NothingSelected"
        );
        assert_eq!(
            sized("[ref]: http://x\n\"tiˇtle\"", Some("1.25em")),
            "refused: NoParagraph"
        );
        assert_eq!(
            sized("[^1]: noˇte", Some("1.25em")),
            "[^1]: <span style=\"font-size: 1.25em\">noˇte</span>"
        );
        assert_eq!(
            inline("«[^1]: note»", Request::SetTextColor(RED)),
            "«[^1]: <span style=\"color: #d44c47\">note»</span>"
        );

        assert_eq!(
            sized("a\n\n    coˇde", Some("1.25em")),
            "refused: InsideCode"
        );
        assert_eq!(
            inline("\tcoˇde", Request::SetTextColor(RED)),
            "refused: InsideCode"
        );
        // Indentation that continues a paragraph or a list item is text.
        assert_eq!(
            sized("a\n    bˇ", Some("1.25em")),
            "<span style=\"font-size: 1.25em\">a\n    bˇ</span>"
        );
        assert_eq!(
            inline("- a\n\n    coˇntinued", Request::SetTextColor(RED)),
            "- a\n\n    co<span style=\"color: #d44c47\">ˇ</span>ntinued"
        );
    }

    #[test]
    fn test_inline_code_spans() {
        let spans = |text: &str| {
            inline_code_spans(text, 0..text.len())
                .into_iter()
                .map(|range| text[range].to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(spans("a `b` c"), ["`b`"]);
        assert_eq!(spans("a ``b ` c`` d"), ["``b ` c``"]);
        // An unmatched run is literal, and a backslash escapes a backtick.
        assert_eq!(spans("a ``b` c"), Vec::<String>::new());
        assert_eq!(spans("a \\`b` c `d`"), ["` c `"]);
        assert_eq!(spans("日本 `語`"), ["`語`"]);
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

/// The row of the opening fence of a fenced code block that is never closed,
/// which runs to the end of the document.
fn unclosed_fence_start(lines: &[String]) -> Option<u32> {
    let mut open: Option<(u32, char, usize)> = None;
    for (index, line) in lines.iter().enumerate() {
        match open {
            Some((_, marker, length)) => {
                if is_closing_fence(line, marker, length) {
                    open = None;
                }
            }
            None => {
                if let Some((marker, length, _, _)) = parse_opening_fence(line) {
                    open = Some((index as u32, marker, length));
                }
            }
        }
    }
    open.map(|(row, _, _)| row)
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
