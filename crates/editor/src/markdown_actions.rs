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
