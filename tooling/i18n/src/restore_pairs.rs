//! Recovers `(English original, Japanese replacement)` pairs from the fork's diff.
//!
//! Both revisions of a file are lexed, string literals are replaced by a `<STR>` placeholder and
//! the resulting skeletons are aligned token by token. Because literal values are invisible to the
//! alignment, a pure translation leaves the skeleton untouched, and rustfmt re-wrapping (which only
//! moves whitespace) is invisible too.

use crate::lexer::{self, LineIndex, StringLiteral, Token, TokenKind};
use crate::placeholders::{self, FormatArguments};
use crate::token_diff;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// An aligned literal is only trusted when the unchanged stretch around it is at least this long.
/// Shorter stretches are usually generic fragments such as `.child(Label::new(<STR>));` that the
/// diff paired inside a rewritten region.
const MINIMUM_RUN_LENGTH: usize = 20;
/// Contexts larger than this are narrowed to the literal's own element.
const MAXIMUM_CONTEXT_TOKENS: usize = 150;
/// How far around a literal to look for pluralization that the translation dropped.
const PLURAL_WINDOW: usize = 40;
/// Unchanged tokens required on each side of a literal's context. A context that only just
/// lines up is typical of the diff picking one of several equally long pairings in rewritten
/// code, such as matching `} else { format!(..) }` against the wrong branch.
const CONTEXT_MARGIN: usize = 6;
/// How far around an aligned literal to look for untranslated neighbors whose values changed.
const SIBLING_WINDOW: usize = 40;
const SNIPPET_LIMIT: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairKind {
    Exact,
    Format,
    PluralCollapsed,
    Ambiguous,
    ForkNew,
}

impl PairKind {
    pub const ALL: [PairKind; 5] = [
        PairKind::Exact,
        PairKind::Format,
        PairKind::PluralCollapsed,
        PairKind::Ambiguous,
        PairKind::ForkNew,
    ];

    pub fn name(self) -> &'static str {
        match self {
            PairKind::Exact => "exact",
            PairKind::Format => "format",
            PairKind::PluralCollapsed => "plural_collapsed",
            PairKind::Ambiguous => "ambiguous",
            PairKind::ForkNew => "fork_new",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairRecord {
    #[serde(rename = "crate")]
    pub crate_name: String,
    pub file: String,
    pub head_line: usize,
    pub base_line: Option<usize>,
    pub kind: PairKind,
    pub reason: Option<String>,
    pub in_test: bool,
    pub en: Option<String>,
    /// The other form when the English side chose between a singular and a plural.
    pub en_plural: Option<String>,
    pub ja: String,
    pub ja_as_msgstr: Option<String>,
    pub en_placeholders: Vec<String>,
    pub ja_placeholders: Vec<String>,
    pub call: Option<String>,
    pub base_snippet: Option<String>,
    pub head_snippet: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentCounts {
    pub line_or_block: usize,
    pub doc: usize,
}

pub struct SourceFile {
    pub text: String,
    pub code: Vec<Token>,
    pub comments: Vec<Token>,
    lines: LineIndex,
    parent: Vec<Option<usize>>,
    matching: Vec<Option<usize>>,
}

impl SourceFile {
    pub fn parse(text: &str) -> Self {
        let text = lexer::normalize_line_endings(text);
        let (comments, mut code): (Vec<Token>, Vec<Token>) = lexer::tokenize(&text)
            .into_iter()
            .partition(Token::is_comment);
        // rustfmt adds or removes the trailing comma when it re-wraps a list; it carries no
        // meaning, so it must not break the alignment.
        let is_trailing_comma = |current: &Token, next: Option<&Token>| {
            current.kind == TokenKind::Punct
                && &text[current.start..current.end] == ","
                && next.is_some_and(|next| {
                    next.kind == TokenKind::Punct
                        && matches!(&text[next.start..next.end], ")" | "]" | "}")
                })
        };
        let mut kept = Vec::with_capacity(code.len());
        for index in 0..code.len() {
            if !is_trailing_comma(&code[index], code.get(index + 1)) {
                kept.push(code[index].clone());
            }
        }
        code = kept;
        let lines = LineIndex::new(&text);
        let mut file = Self {
            text,
            code,
            comments,
            lines,
            parent: Vec::new(),
            matching: Vec::new(),
        };
        file.compute_structure();
        file
    }

    fn compute_structure(&mut self) {
        let length = self.code.len();
        let mut parent = vec![None; length];
        let mut matching = vec![None; length];
        let mut stack: Vec<usize> = Vec::new();
        for index in 0..length {
            parent[index] = stack.last().copied();
            let text = self.token_text(index);
            match text {
                "(" | "[" | "{" if self.code[index].kind == TokenKind::Punct => stack.push(index),
                ")" | "]" | "}" if self.code[index].kind == TokenKind::Punct => {
                    let opener = match text {
                        ")" => "(",
                        "]" => "[",
                        _ => "{",
                    };
                    // Be permissive about stray closers: only pop when a matching opener exists.
                    if let Some(position) = stack
                        .iter()
                        .rposition(|open| self.token_text(*open) == opener)
                    {
                        let open = stack[position];
                        stack.truncate(position);
                        matching[open] = Some(index);
                        matching[index] = Some(open);
                        parent[index] = stack.last().copied();
                    }
                }
                _ => {}
            }
        }
        self.parent = parent;
        self.matching = matching;
    }

    pub fn token_text(&self, index: usize) -> &str {
        self.code
            .get(index)
            .map(|token| &self.text[token.start..token.end])
            .unwrap_or("")
    }

    fn is_punct(&self, index: usize, punct: &str) -> bool {
        self.code
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Punct)
            && self.token_text(index) == punct
    }

    fn is_ident(&self, index: usize) -> bool {
        self.code
            .get(index)
            .is_some_and(|token| token.kind == TokenKind::Ident)
    }

    fn is_opener(&self, index: usize) -> bool {
        (self.is_punct(index, "(") || self.is_punct(index, "[") || self.is_punct(index, "{"))
            && self.matching.get(index).copied().flatten().is_some()
    }

    fn is_closer(&self, index: usize) -> bool {
        self.is_punct(index, ")") || self.is_punct(index, "]") || self.is_punct(index, "}")
    }

    pub fn literal(&self, index: usize) -> Option<&StringLiteral> {
        self.code.get(index).and_then(Token::string_literal)
    }

    pub fn line_of_token(&self, index: usize) -> usize {
        self.code
            .get(index)
            .map(|token| self.lines.line_of(token.start))
            .unwrap_or(0)
    }

    /// Start of the path in front of an opening bracket (`Label::new(`, `format!(`, `.child(`).
    /// Returns `open` itself when nothing callee-like precedes it.
    fn callee_start(&self, open: usize) -> usize {
        let mut start = open;
        let mut cursor = open;
        if cursor > 0 && self.is_punct(cursor - 1, "!") {
            cursor -= 1;
            start = cursor;
        }
        loop {
            if cursor == 0 {
                break;
            }
            let previous = cursor - 1;
            if self.is_ident(previous) {
                start = previous;
                cursor = previous;
                if cursor >= 2 && self.is_punct(cursor - 1, ":") && self.is_punct(cursor - 2, ":") {
                    cursor -= 2;
                    continue;
                }
                if cursor >= 1 && self.is_punct(cursor - 1, ".") {
                    start = cursor - 1;
                }
                break;
            }
            if self.is_punct(previous, ">") && start != open {
                let mut depth = 0usize;
                let mut position = previous;
                let mut found = None;
                loop {
                    if self.is_punct(position, ">") {
                        depth += 1;
                    } else if self.is_punct(position, "<") {
                        depth -= 1;
                        if depth == 0 {
                            found = Some(position);
                            break;
                        }
                    } else if self.is_punct(position, ";") || self.is_opener(position) {
                        break;
                    }
                    if position == 0 {
                        break;
                    }
                    position -= 1;
                }
                let Some(less_than) = found else {
                    break;
                };
                start = less_than;
                cursor = less_than;
                if cursor >= 2 && self.is_punct(cursor - 1, ":") && self.is_punct(cursor - 2, ":") {
                    cursor -= 2;
                    start = cursor;
                    continue;
                }
                continue;
            }
            break;
        }
        // A lone `::` left at the start belongs to the path's separator, not to a callee.
        while start < open && self.is_punct(start, ":") {
            start += 1;
        }
        start
    }

    fn joined_text(&self, start: usize, end_exclusive: usize) -> String {
        (start..end_exclusive)
            .map(|index| self.token_text(index))
            .collect::<Vec<_>>()
            .join("")
    }

    /// The element of the enclosing list / block / argument list that contains `index`: bounded
    /// by `,`, `;`, the enclosing brackets, or the end of a preceding block.
    fn element_bounds(&self, index: usize) -> (usize, usize) {
        let parent = self.parent.get(index).copied().flatten();
        let mut low = index;
        let mut cursor = index;
        while cursor > 0 {
            let previous = cursor - 1;
            // A `}` normally ends the previous statement, but in `Variant { .. } => "label"` it
            // closes a pattern that belongs to this element.
            let closes_struct_pattern = self.is_punct(previous, "}")
                && self
                    .matching
                    .get(previous)
                    .copied()
                    .flatten()
                    .is_some_and(|open| self.is_struct_literal(open));
            if Some(previous) == parent
                || self.is_punct(previous, ",")
                || self.is_punct(previous, ";")
                || (self.is_punct(previous, "}") && !closes_struct_pattern)
                || self.is_opener(previous)
            {
                break;
            }
            if self.is_closer(previous) {
                match self.matching.get(previous).copied().flatten() {
                    Some(open) => {
                        low = open;
                        cursor = open;
                        continue;
                    }
                    None => break,
                }
            }
            low = previous;
            cursor = previous;
        }
        let mut high = index;
        let mut cursor = index;
        loop {
            let next = cursor + 1;
            if next >= self.code.len()
                || self.is_closer(next)
                || self.is_punct(next, ",")
                || self.is_punct(next, ";")
            {
                break;
            }
            if self.is_opener(next) {
                match self.matching.get(next).copied().flatten() {
                    Some(close) => {
                        high = close;
                        cursor = close;
                        continue;
                    }
                    None => break,
                }
            }
            high = next;
            cursor = next;
        }
        (low, high)
    }

    /// Whether `low..=high` names something specific enough to recognize the code elsewhere:
    /// an untranslated literal with letters (an id such as `"search_web"`), or a capitalized
    /// name such as an action, variant or type that is not one of the ubiquitous UI types.
    /// Variable names and the literal's own callee do not count.
    fn is_distinctive(
        &self,
        low: usize,
        high: usize,
        except: usize,
        callee_range: Option<(usize, usize)>,
    ) -> bool {
        const UBIQUITOUS: [&str; 30] = [
            "None",
            "Some",
            "Ok",
            "Err",
            "Box",
            "Self",
            "String",
            "SharedString",
            "Vec",
            "Option",
            "Arc",
            "Rc",
            "Default",
            "Color",
            "Label",
            "LabelSize",
            "Button",
            "IconButton",
            "Tooltip",
            "IconName",
            "IconSize",
            "Icon",
            "Headline",
            "ContextMenu",
            "ContextMenuEntry",
            "Menu",
            "MenuItem",
            "App",
            "Window",
            "Context",
        ];
        (low..=high).any(|index| {
            if index == except
                || callee_range.is_some_and(|(start, end)| (start..=end).contains(&index))
            {
                return false;
            }
            if let Some(literal) = self.literal(index) {
                return !contains_japanese(&literal.value)
                    && literal
                        .value
                        .chars()
                        .filter(|character| character.is_ascii_alphabetic())
                        .count()
                        >= 3;
            }
            let text = self.token_text(index);
            self.is_ident(index)
                && text
                    .chars()
                    .next()
                    .is_some_and(|first| first.is_ascii_uppercase())
                && !UBIQUITOUS.contains(&text)
        })
    }

    /// For a token range that forms the body of an `if` / `else` branch, the token range of the
    /// `if` condition (`if count == 1 { "1 file" } else { .. }`).
    fn branch_condition(&self, body_low: usize) -> Option<(usize, usize)> {
        let mut open = self.parent.get(body_low).copied().flatten()?;
        if !self.is_punct(open, "{") {
            return None;
        }
        while open >= 2 && self.token_text(open - 1) == "else" && self.is_punct(open - 2, "}") {
            open = self.matching.get(open - 2).copied().flatten()?;
        }
        let mut cursor = open;
        for _ in 0..40 {
            if cursor == 0 {
                return None;
            }
            let previous = cursor - 1;
            if self.is_ident(previous) && self.token_text(previous) == "if" {
                return Some((previous, open));
            }
            if self.is_punct(previous, ";")
                || self.is_punct(previous, ",")
                || self.is_punct(previous, "{")
                || self.is_punct(previous, "}")
                || self.is_opener(previous)
            {
                return None;
            }
            cursor = if self.is_closer(previous) {
                self.matching.get(previous).copied().flatten()?
            } else {
                previous
            };
        }
        None
    }

    fn is_struct_literal(&self, open: usize) -> bool {
        self.is_punct(open, "{")
            && open > 0
            && self.is_ident(open - 1)
            && self
                .token_text(open - 1)
                .chars()
                .next()
                .is_some_and(|first| first.is_ascii_uppercase())
    }

    fn has_meaningful_identifier(&self, low: usize, high: usize, except: usize) -> bool {
        const GENERIC: [&str; 10] = [
            "into",
            "to_string",
            "to_owned",
            "clone",
            "as_str",
            "as_ref",
            "into_owned",
            "Some",
            "Ok",
            "Err",
        ];
        (low..=high).any(|index| {
            index != except && self.is_ident(index) && !GENERIC.contains(&self.token_text(index))
        })
    }

    /// The token range that must be unchanged for a literal at `index` to count as translated
    /// in place: the whole call for arguments of a small call, otherwise the literal's element,
    /// widened until it names something.
    fn context_range(&self, index: usize) -> (usize, usize) {
        let mut target = index;
        loop {
            let parent = self.parent.get(target).copied().flatten();
            let (element_low, element_high) = self.element_bounds(target);
            if let Some(open) = parent
                && self.is_punct(open, "(")
                && let Some(close) = self.matching.get(open).copied().flatten()
            {
                let callee = self.callee_start(open);
                if close - callee <= MAXIMUM_CONTEXT_TOKENS {
                    if callee < open || self.has_meaningful_identifier(callee, close, index) {
                        return (callee, close);
                    }
                    target = open;
                    continue;
                }
                return (element_low, element_high);
            }
            if self.has_meaningful_identifier(element_low, element_high, index) {
                return (element_low, element_high);
            }
            match parent {
                Some(open) => {
                    let callee = self.callee_start(open);
                    let close = self.matching.get(open).copied().flatten().unwrap_or(open);
                    if close - callee > MAXIMUM_CONTEXT_TOKENS {
                        return self.neighbor_window(open, close, element_low, element_high);
                    }
                    if callee < open && self.has_meaningful_identifier(callee, open, index) {
                        return (callee, close);
                    }
                    target = open;
                }
                None => return (element_low, element_high),
            }
        }
    }

    /// For a literal in a long list: the element plus up to two neighbors on each side.
    fn neighbor_window(
        &self,
        open: usize,
        close: usize,
        element_low: usize,
        element_high: usize,
    ) -> (usize, usize) {
        let mut low = element_low;
        for _ in 0..2 {
            if low <= open + 1 {
                break;
            }
            let (previous_low, _) = self.element_bounds(low.saturating_sub(2));
            low = previous_low.max(open + 1);
        }
        let mut high = element_high;
        for _ in 0..2 {
            if high + 2 >= close {
                break;
            }
            let (_, next_high) = self.element_bounds(high + 2);
            high = next_high.min(close - 1);
        }
        (low, high)
    }

    fn call_name(&self, index: usize) -> Option<String> {
        let open = self.parent.get(index).copied().flatten()?;
        let callee = self.callee_start(open);
        let callee_text = self.joined_text(callee, open);
        if self.is_punct(open, "(") || self.is_punct(open, "[") {
            return Some(if callee_text.is_empty() {
                self.token_text(open).to_string()
            } else {
                callee_text
            });
        }
        let (element_low, _) = self.element_bounds(index);
        if self.is_ident(element_low)
            && self.is_punct(element_low + 1, ":")
            && !self.is_punct(element_low + 2, ":")
        {
            let field = self.token_text(element_low);
            let is_type = callee_text
                .rsplit("::")
                .next()
                .and_then(|last| last.chars().next())
                .is_some_and(char::is_uppercase);
            return Some(if is_type {
                format!("{callee_text}.{field}")
            } else {
                format!("{field}:")
            });
        }
        None
    }

    /// The `,`-separated arguments of the group opened at `open`, as inclusive token ranges.
    fn arguments(&self, open: usize) -> Vec<(usize, usize)> {
        let Some(close) = self.matching.get(open).copied().flatten() else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let mut start = open + 1;
        let mut cursor = open + 1;
        while cursor < close {
            if self.is_opener(cursor) {
                cursor = self
                    .matching
                    .get(cursor)
                    .copied()
                    .flatten()
                    .unwrap_or(cursor)
                    + 1;
                continue;
            }
            if self.is_punct(cursor, ",") {
                if cursor > start {
                    result.push((start, cursor - 1));
                }
                start = cursor + 1;
            }
            cursor += 1;
        }
        if close > start {
            result.push((start, close - 1));
        }
        result
    }

    fn normalized_expression(&self, low: usize, high: usize) -> String {
        (low..=high)
            .map(|index| match self.literal(index) {
                Some(literal) => format!("{:?}", literal.value),
                None => self.token_text(index).to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// When the literal at `index` is the template of a formatting macro, returns the macro
    /// name, where the macro path starts, and its arguments.
    fn format_string_info(&self, index: usize) -> Option<FormatStringInfo> {
        let open = self.parent.get(index).copied().flatten()?;
        if open == 0 || !self.is_punct(open - 1, "!") || open < 2 || !self.is_ident(open - 2) {
            return None;
        }
        let macro_name = self.token_text(open - 2).to_string();
        let first_template_argument = formatting_macro_template_position(&macro_name)?;
        let arguments = self.arguments(open);
        let template_position = arguments
            .iter()
            .enumerate()
            .skip(first_template_argument)
            .find(|(_, (low, high))| low == high && self.literal(*low).is_some())
            .map(|(position, _)| position)?;
        if arguments.get(template_position)?.0 != index {
            return None;
        }
        let mut format_arguments = FormatArguments::default();
        for (low, high) in arguments.iter().skip(template_position + 1) {
            if self.is_ident(*low) && self.is_punct(low + 1, "=") && !self.is_punct(low + 2, "=") {
                format_arguments.named.push((
                    self.token_text(*low).to_string(),
                    self.normalized_expression(low + 2, *high),
                ));
            } else {
                format_arguments
                    .positional
                    .push(self.normalized_expression(*low, *high));
            }
        }
        Some(FormatStringInfo {
            macro_name,
            callee_start: self.callee_start(open),
            open,
            arguments: format_arguments,
        })
    }

    fn plural_signal(&self, indices: impl Iterator<Item = usize>) -> bool {
        indices.into_iter().any(|index| {
            let text = self.token_text(index);
            if self.is_ident(index) && text.to_lowercase().contains("plural") {
                return true;
            }
            if let Some(literal) = self.literal(index) {
                return literal.value == "s" || literal.value == "es";
            }
            let is_one = self
                .code
                .get(index)
                .is_some_and(|token| token.kind == TokenKind::Number)
                && (text == "1" || text.starts_with("1_") || text.starts_with("1u"));
            if !is_one {
                return false;
            }
            let equality_before = index >= 2
                && self.is_punct(index - 1, "=")
                && (self.is_punct(index - 2, "=") || self.is_punct(index - 2, "!"));
            let equality_after = self.is_punct(index + 1, "=")
                && (self.is_punct(index + 2, "=") || self.is_punct(index + 2, ">"));
            equality_before || equality_after
        })
    }

    /// Token ranges covered by `#[cfg(test)]` (or `#[test]`-like) items, with the item's extent
    /// taken from bracket matching.
    fn test_ranges(&self) -> Vec<(usize, usize)> {
        let mut ranges = Vec::new();
        let mut index = 0;
        while index + 1 < self.code.len() {
            let inner = self.is_punct(index, "#")
                && self.is_punct(index + 1, "!")
                && self.is_punct(index + 2, "[");
            let outer = self.is_punct(index, "#") && self.is_punct(index + 1, "[");
            if !inner && !outer {
                index += 1;
                continue;
            }
            let open = if inner { index + 2 } else { index + 1 };
            let Some(close) = self.matching.get(open).copied().flatten() else {
                index += 1;
                continue;
            };
            if !self.is_test_attribute(open + 1, close) {
                index = close + 1;
                continue;
            }
            if inner {
                return vec![(0, self.code.len().saturating_sub(1))];
            }
            let mut cursor = close + 1;
            while self.is_punct(cursor, "#") && self.is_punct(cursor + 1, "[") {
                match self.matching.get(cursor + 1).copied().flatten() {
                    Some(attribute_close) => cursor = attribute_close + 1,
                    None => break,
                }
            }
            let mut end = self.code.len().saturating_sub(1);
            while cursor < self.code.len() {
                if self.is_punct(cursor, "{") {
                    end = self.matching.get(cursor).copied().flatten().unwrap_or(end);
                    break;
                }
                if self.is_punct(cursor, ";") {
                    end = cursor;
                    break;
                }
                if self.is_opener(cursor) {
                    cursor = self
                        .matching
                        .get(cursor)
                        .copied()
                        .flatten()
                        .unwrap_or(cursor);
                }
                cursor += 1;
            }
            ranges.push((index, end));
            index = end + 1;
        }
        ranges
    }

    fn is_test_attribute(&self, low: usize, close: usize) -> bool {
        if low >= close {
            return false;
        }
        if self.token_text(low) == "cfg" && self.is_ident(low) {
            let identifiers: Vec<&str> = (low..close)
                .filter(|index| self.is_ident(*index))
                .map(|index| self.token_text(index))
                .collect();
            return identifiers.contains(&"test") && !identifiers.contains(&"not");
        }
        let mut cursor = low;
        let mut last_identifier = "";
        while cursor < close {
            if self.is_ident(cursor) {
                last_identifier = self.token_text(cursor);
                cursor += 1;
            } else if self.is_punct(cursor, ":") {
                cursor += 1;
            } else {
                break;
            }
        }
        last_identifier == "test" && (cursor == close || self.is_punct(cursor, "("))
    }

    fn snippet(&self, low: usize, high: usize, focus: usize) -> String {
        let (Some(first), Some(last), Some(focus_token)) = (
            self.code.get(low),
            self.code.get(high),
            self.code.get(focus),
        ) else {
            return String::new();
        };
        let start = self.lines.line_start(self.lines.line_of(first.start));
        let end = self.lines.line_end(
            self.lines.line_of(last.end.saturating_sub(1)),
            self.text.len(),
        );
        let text = &self.text[start..end.max(start)];
        let character_count = text.chars().count();
        if character_count <= SNIPPET_LIMIT {
            return dedent(text);
        }
        let focus_character = self.text[start..focus_token.start.max(start)]
            .chars()
            .count();
        let window_start = focus_character.saturating_sub(SNIPPET_LIMIT / 3);
        let window: String = text
            .chars()
            .skip(window_start)
            .take(SNIPPET_LIMIT)
            .collect();
        dedent(&window)
    }
}

fn dedent(text: &str) -> String {
    let indentation = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.len() - line.trim_start().len())
        .min()
        .unwrap_or(0);
    text.lines()
        .map(|line| line.get(indentation..).unwrap_or(line.trim_start()))
        .collect::<Vec<_>>()
        .join("\n")
}

struct FormatStringInfo {
    macro_name: String,
    callee_start: usize,
    open: usize,
    arguments: FormatArguments,
}

/// For formatting macros: the first argument position that can hold the template.
fn formatting_macro_template_position(macro_name: &str) -> Option<usize> {
    match macro_name {
        "format" | "format_args" | "print" | "println" | "eprint" | "eprintln" | "panic"
        | "unreachable" | "todo" | "unimplemented" | "anyhow" | "bail" | "format_err" | "log"
        | "trace" | "debug" | "info" | "warn" | "error" | "t" | "tc" => Some(0),
        "write" | "writeln" | "assert" | "debug_assert" | "ensure" => Some(1),
        "assert_eq" | "assert_ne" | "debug_assert_eq" | "debug_assert_ne" => Some(2),
        _ => None,
    }
}

pub fn contains_japanese(text: &str) -> bool {
    text.chars().any(|character| {
        matches!(character as u32,
            0x3040..=0x309F // Hiragana
            | 0x30A0..=0x30FF // Katakana
            | 0x31F0..=0x31FF // Katakana phonetic extensions
            | 0xFF66..=0xFF9F // Halfwidth katakana
            | 0x3400..=0x4DBF // CJK extension A
            | 0x4E00..=0x9FFF // CJK unified ideographs
            | 0xF900..=0xFAFF // CJK compatibility ideographs
        )
    })
}

fn has_ascii_letters(text: &str) -> bool {
    text.chars()
        .any(|character| character.is_ascii_alphabetic())
}

/// The token alignment between the two revisions of one file.
struct Alignment {
    base_ids: Vec<u32>,
    head_ids: Vec<u32>,
    head_to_base: Vec<Option<usize>>,
    base_to_head: Vec<Option<usize>>,
    /// For every aligned head token, the length of the unbroken diagonal run containing it.
    run_length: Vec<usize>,
}

impl Alignment {
    fn new(base: &SourceFile, head: &SourceFile) -> Self {
        let mut interner: HashMap<String, u32> = HashMap::new();
        let mut intern = |file: &SourceFile| -> Vec<u32> {
            (0..file.code.len())
                .map(|index| {
                    let key = if file.literal(index).is_some() {
                        "\0STR".to_string()
                    } else {
                        file.token_text(index).to_string()
                    };
                    let next = interner.len() as u32;
                    *interner.entry(key).or_insert(next)
                })
                .collect()
        };
        let base_ids = intern(base);
        let head_ids = intern(head);
        let matches = token_diff::diff(&base_ids, &head_ids);
        let mut head_to_base = vec![None; head.code.len()];
        let mut base_to_head = vec![None; base.code.len()];
        for (base_index, head_index) in &matches {
            head_to_base[*head_index] = Some(*base_index);
            base_to_head[*base_index] = Some(*head_index);
        }
        let mut run_length = vec![0; head.code.len()];
        let mut run_start = 0;
        while run_start < head.code.len() {
            let Some(base_start) = head_to_base[run_start] else {
                run_start += 1;
                continue;
            };
            let mut run_end = run_start + 1;
            while run_end < head.code.len()
                && head_to_base[run_end] == Some(base_start + (run_end - run_start))
            {
                run_end += 1;
            }
            for length in &mut run_length[run_start..run_end] {
                *length = run_end - run_start;
            }
            run_start = run_end;
        }
        Self {
            base_ids,
            head_ids,
            head_to_base,
            base_to_head,
            run_length,
        }
    }

    /// True when every head token in `low..=high` is aligned, on one diagonal.
    fn is_unchanged(&self, low: usize, high: usize) -> bool {
        let Some(Some(base_low)) = self.head_to_base.get(low) else {
            return false;
        };
        (low..=high)
            .all(|index| self.head_to_base.get(index) == Some(&Some(base_low + (index - low))))
    }

    /// Literal values are invisible to the alignment, so a list of look-alike items that gained
    /// an entry can slide by one item. An untranslated neighbor on the same diagonal whose value
    /// differs (`id: "search_web"` against `id: "rename_symbol"`) exposes that.
    fn mismatched_sibling(
        &self,
        base: &SourceFile,
        head: &SourceFile,
        head_index: usize,
    ) -> Option<String> {
        let base_index = self.head_to_base.get(head_index).copied().flatten()?;
        let low = head_index.saturating_sub(SIBLING_WINDOW);
        let high = (head_index + SIBLING_WINDOW).min(head.code.len().saturating_sub(1));
        for index in low..=high {
            if index == head_index {
                continue;
            }
            let Some(Some(other_base)) = self.head_to_base.get(index) else {
                continue;
            };
            if other_base.wrapping_sub(index) != base_index.wrapping_sub(head_index) {
                continue;
            }
            let (Some(head_literal), Some(base_literal)) =
                (head.literal(index), base.literal(*other_base))
            else {
                continue;
            };
            if !contains_japanese(&head_literal.value) && head_literal.value != base_literal.value {
                return Some(format!(
                    "{:?} -> {:?}",
                    base_literal.value, head_literal.value
                ));
            }
        }
        None
    }

    /// The base and head token ranges (half-open) of the changed region around an unaligned
    /// head token.
    fn changed_region(
        &self,
        head_index: usize,
        base_length: usize,
    ) -> ((usize, usize), (usize, usize)) {
        let previous = (0..head_index)
            .rev()
            .find_map(|index| self.head_to_base[index].map(|base| (index, base)));
        let next = (head_index + 1..self.head_to_base.len())
            .find_map(|index| self.head_to_base[index].map(|base| (index, base)));
        let (head_low, base_low) = previous
            .map(|(head, base)| (head + 1, base + 1))
            .unwrap_or((0, 0));
        let (head_high, base_high) = next.unwrap_or((self.head_to_base.len(), base_length));
        ((base_low, base_high), (head_low, head_high))
    }
}

struct Classification {
    kind: PairKind,
    reason: Option<String>,
    base_index: Option<usize>,
    en: Option<String>,
    en_plural: Option<String>,
    ja_as_msgstr: Option<String>,
    en_placeholders: Vec<String>,
    ja_placeholders: Vec<String>,
}

impl Classification {
    fn new(kind: PairKind, reason: Option<String>) -> Self {
        Self {
            kind,
            reason,
            base_index: None,
            en: None,
            en_plural: None,
            ja_as_msgstr: None,
            en_placeholders: Vec::new(),
            ja_placeholders: Vec::new(),
        }
    }

    fn ambiguous(reason: impl Into<String>, base_index: Option<usize>, en: Option<String>) -> Self {
        Self {
            base_index,
            en,
            ..Self::new(PairKind::Ambiguous, Some(reason.into()))
        }
    }
}

fn placeholder_list(value: &str) -> Vec<String> {
    placeholders::parse(value)
        .map(|template| {
            template
                .placeholders
                .into_iter()
                .map(|placeholder| placeholder.raw)
                .collect()
        })
        .unwrap_or_default()
}

pub struct FileAnalysis {
    pub records: Vec<PairRecord>,
    pub comments: CommentCounts,
}

pub fn analyze_file(
    crate_name: &str,
    head_path: &str,
    base: Option<&SourceFile>,
    head: &SourceFile,
    whole_file_is_test: bool,
) -> FileAnalysis {
    let alignment = base.map(|base| Alignment::new(base, head));
    let test_ranges = head.test_ranges();
    let mut classified: Vec<(usize, Classification)> = Vec::new();
    for index in 0..head.code.len() {
        let Some(literal) = head.literal(index) else {
            continue;
        };
        if !contains_japanese(&literal.value) {
            continue;
        }
        let classification = match (base, &alignment) {
            (Some(base), Some(alignment)) => {
                match classify(base, head, alignment, index, literal, None) {
                    Some(classification) => classification,
                    None => continue,
                }
            }
            _ => Classification::new(PairKind::ForkNew, Some("new_file".to_string())),
        };
        classified.push((index, classification));
    }
    if let (Some(base), Some(alignment)) = (base, &alignment) {
        realign(base, head, alignment, &mut classified);
    }

    let mut records = Vec::new();
    for (index, classification) in classified {
        let Some(literal) = head.literal(index) else {
            continue;
        };
        let in_test = whole_file_is_test
            || test_ranges
                .iter()
                .any(|(low, high)| (*low..=*high).contains(&index));
        let (context_low, context_high) = head.context_range(index);
        let base_snippet = match (base, classification.base_index) {
            (Some(base), Some(base_index)) => {
                let (low, high) = base.context_range(base_index);
                Some(base.snippet(low, high, base_index))
            }
            _ => None,
        };
        let ja_placeholders = if classification.ja_placeholders.is_empty() {
            placeholder_list(&literal.value)
        } else {
            classification.ja_placeholders.clone()
        };
        records.push(PairRecord {
            crate_name: crate_name.to_string(),
            file: head_path.to_string(),
            head_line: head.line_of_token(index),
            base_line: match (base, classification.base_index) {
                (Some(base), Some(base_index)) => Some(base.line_of_token(base_index)),
                _ => None,
            },
            kind: classification.kind,
            reason: classification.reason,
            in_test,
            en: classification.en,
            en_plural: classification.en_plural,
            ja: literal.value.clone(),
            ja_as_msgstr: classification.ja_as_msgstr,
            en_placeholders: classification.en_placeholders,
            ja_placeholders,
            call: head.call_name(index),
            base_snippet,
            head_snippet: head.snippet(context_low, context_high, index),
        });
    }

    let mut comments = CommentCounts::default();
    let base_comment_texts: HashSet<&str> = base
        .map(|base| {
            base.comments
                .iter()
                .map(|token| &base.text[token.start..token.end])
                .collect()
        })
        .unwrap_or_default();
    for token in &head.comments {
        let text = &head.text[token.start..token.end];
        if !contains_japanese(text) || base_comment_texts.contains(text) {
            continue;
        }
        match token.kind {
            TokenKind::LineComment { doc: true } | TokenKind::BlockComment { doc: true } => {
                comments.doc += 1
            }
            _ => comments.line_or_block += 1,
        }
    }
    FileAnalysis { records, comments }
}

/// `realigned_base` is a base literal found by [`realign`]; its surroundings are already known to
/// be identical, so the in-place alignment checks are skipped.
fn classify(
    base: &SourceFile,
    head: &SourceFile,
    alignment: &Alignment,
    head_index: usize,
    head_literal: &StringLiteral,
    realigned_base: Option<usize>,
) -> Option<Classification> {
    let realigned = realigned_base.is_some();
    let Some(base_index) = realigned_base.or(alignment.head_to_base[head_index]) else {
        return Some(classify_unaligned(base, head, alignment, head_index));
    };
    let classification = classify_pair(
        base,
        head,
        alignment,
        head_index,
        head_literal,
        base_index,
        realigned,
    )?;
    if !realigned {
        return Some(classification);
    }
    let reason = match classification.reason {
        Some(reason) => format!("realigned; {reason}"),
        None => "realigned".to_string(),
    };
    Some(Classification {
        reason: Some(reason),
        ..classification
    })
}

fn classify_pair(
    base: &SourceFile,
    head: &SourceFile,
    alignment: &Alignment,
    head_index: usize,
    head_literal: &StringLiteral,
    base_index: usize,
    realigned: bool,
) -> Option<Classification> {
    let base_literal = base.literal(base_index)?;
    if base_literal.value == head_literal.value {
        return None;
    }
    let en = Some(base_literal.value.clone());
    if contains_japanese(&base_literal.value) {
        return Some(Classification::ambiguous(
            "base_literal_already_japanese",
            Some(base_index),
            en,
        ));
    }
    if !realigned && let Some(sibling) = alignment.mismatched_sibling(base, head, head_index) {
        return Some(Classification::ambiguous(
            format!("sibling_literal_mismatch: {sibling}"),
            Some(base_index),
            en,
        ));
    }

    let plural = plural_collapsed(base, head, alignment, base_index, head_index);
    let plural_classification = |reason: &str| {
        let mut classification = Classification::new(
            PairKind::PluralCollapsed,
            Some(format!("plural_collapsed: {reason}")),
        );
        classification.base_index = Some(base_index);
        classification.en = en.clone();
        classification.en_plural = plural.clone().flatten();
        classification.en_placeholders = placeholder_list(&base_literal.value);
        classification.ja_as_msgstr = Some(head_literal.value.clone());
        classification
    };

    let run_is_long = realigned || alignment.run_length[head_index] >= MINIMUM_RUN_LENGTH;
    match (
        head.format_string_info(head_index),
        base.format_string_info(base_index),
    ) {
        (Some(head_format), Some(base_format)) => {
            if head_format.macro_name != base_format.macro_name {
                return Some(Classification::ambiguous(
                    format!(
                        "format_macro_changed: {}! -> {}!",
                        base_format.macro_name, head_format.macro_name
                    ),
                    Some(base_index),
                    en,
                ));
            }
            let macro_unchanged = head_format.open > head_format.callee_start
                && (realigned || alignment.is_unchanged(head_format.callee_start, head_index));
            // The arguments may have been rewritten (`{}` -> `{name}`), so the right margin is
            // checked from the closing parenthesis on, which must still line up.
            let last = head.code.len().saturating_sub(1);
            let head_close = head
                .matching
                .get(head_format.open)
                .copied()
                .flatten()
                .unwrap_or(head_index);
            let base_close = base
                .matching
                .get(base_format.open)
                .copied()
                .flatten()
                .unwrap_or(base_index);
            let margins_unchanged = realigned
                || (alignment.is_unchanged(
                    head_format.callee_start.saturating_sub(CONTEXT_MARGIN),
                    head_index,
                ) && alignment.head_to_base.get(head_close) == Some(&Some(base_close))
                    && alignment.is_unchanged(head_close, (head_close + CONTEXT_MARGIN).min(last)));
            if !macro_unchanged || !run_is_long || !margins_unchanged {
                // Dropping the pluralization argument breaks the margin after `)`, so a
                // collapsed plural is still trusted when the macro itself lines up and the
                // pluralization was inside its own arguments.
                let base_context = base.context_range(base_index);
                if plural.is_some()
                    && macro_unchanged
                    && base.plural_signal(base_context.0..=base_context.1)
                {
                    return Some(plural_classification("format context changed"));
                }
                let reason = if macro_unchanged {
                    "short_alignment"
                } else {
                    "context_changed"
                };
                return Some(Classification::ambiguous(reason, Some(base_index), en));
            }
            if plural.is_some() {
                return Some(plural_classification("format"));
            }
            if !has_ascii_letters(&base_literal.value) {
                return Some(Classification::ambiguous(
                    "en_has_no_letters",
                    Some(base_index),
                    en,
                ));
            }
            let en_placeholders = placeholder_list(&base_literal.value);
            let ja_placeholders = placeholder_list(&head_literal.value);
            match placeholders::rewrite_in_english_vocabulary(
                &base_literal.value,
                &base_format.arguments,
                &head_literal.value,
                &head_format.arguments,
            ) {
                Ok(rewritten) => {
                    let kind = if en_placeholders.is_empty() && ja_placeholders.is_empty() {
                        PairKind::Exact
                    } else {
                        PairKind::Format
                    };
                    Some(Classification {
                        kind,
                        reason: None,
                        base_index: Some(base_index),
                        en,
                        en_plural: None,
                        ja_as_msgstr: Some(rewritten),
                        en_placeholders,
                        ja_placeholders,
                    })
                }
                Err(error) => Some(Classification {
                    en_placeholders,
                    ja_placeholders,
                    ..Classification::ambiguous(
                        format!("placeholder_mismatch: {error}"),
                        Some(base_index),
                        en,
                    )
                }),
            }
        }
        (None, None) => {
            let (low, high) = head.context_range(head_index);
            let last = head.code.len().saturating_sub(1);
            let context_unchanged =
                realigned || alignment.is_unchanged(low.saturating_sub(1), (high + 1).min(last));
            let margins_unchanged = realigned
                || alignment.is_unchanged(
                    low.saturating_sub(CONTEXT_MARGIN),
                    (high + CONTEXT_MARGIN).min(last),
                );
            if !context_unchanged || !run_is_long || !margins_unchanged {
                let reason = if context_unchanged {
                    "short_alignment"
                } else {
                    "context_changed"
                };
                return Some(Classification::ambiguous(reason, Some(base_index), en));
            }
            if plural.is_some() {
                return Some(plural_classification("in place"));
            }
            if !has_ascii_letters(&base_literal.value) {
                return Some(Classification::ambiguous(
                    "en_has_no_letters",
                    Some(base_index),
                    en,
                ));
            }
            if placeholders::has_format_syntax(&base_literal.value)
                || placeholders::has_format_syntax(&head_literal.value)
            {
                return Some(Classification::ambiguous(
                    "braces_in_non_format_literal",
                    Some(base_index),
                    en,
                ));
            }
            Some(Classification {
                kind: PairKind::Exact,
                reason: None,
                base_index: Some(base_index),
                en,
                en_plural: None,
                ja_as_msgstr: Some(head_literal.value.clone()),
                en_placeholders: Vec::new(),
                ja_placeholders: Vec::new(),
            })
        }
        (head_format, _) => {
            let reason = if head_format.is_some() {
                "became_format_template"
            } else {
                "stopped_being_format_template"
            };
            Some(Classification::ambiguous(reason, Some(base_index), en))
        }
    }
}

/// Search distance, in tokens, for [`realign`].
const REALIGN_WINDOW: usize = 3000;

fn is_trusted_pair(kind: PairKind) -> bool {
    matches!(
        kind,
        PairKind::Exact | PairKind::Format | PairKind::PluralCollapsed
    )
}

/// A second chance for literals the diff did not pair in place (a greedy run that slid onto the
/// neighbor, a reordered list, a block moved a little): pair them with the one unused base
/// literal nearby whose surrounding tokens are identical, when that surrounding is distinctive
/// and matches exactly one literal on each side.
fn realign(
    base: &SourceFile,
    head: &SourceFile,
    alignment: &Alignment,
    classified: &mut [(usize, Classification)],
) {
    let mut used_base: HashSet<usize> = classified
        .iter()
        .filter(|(_, classification)| is_trusted_pair(classification.kind))
        .filter_map(|(_, classification)| classification.base_index)
        .collect();
    for (base_index, head_index) in alignment.base_to_head.iter().enumerate() {
        let (Some(base_literal), Some(head_index)) = (base.literal(base_index), head_index) else {
            continue;
        };
        if head
            .literal(*head_index)
            .is_some_and(|head_literal| head_literal.value == base_literal.value)
        {
            used_base.insert(base_index);
        }
    }

    let mut claims: HashMap<usize, Vec<usize>> = HashMap::new();
    for (position, (head_index, classification)) in classified.iter().enumerate() {
        let eligible = match classification.kind {
            PairKind::Ambiguous => classification.reason.as_deref().is_some_and(|reason| {
                reason.starts_with("context_changed")
                    || reason.starts_with("short_alignment")
                    || reason.starts_with("sibling_literal_mismatch")
                    || reason.starts_with("unaligned_in_changed_region")
            }),
            PairKind::ForkNew => classification.reason.as_deref() == Some("pure_addition"),
            _ => false,
        };
        if !eligible {
            continue;
        }
        let head_index = *head_index;
        let head_window = head_index.saturating_sub(REALIGN_WINDOW)
            ..(head_index + REALIGN_WINDOW).min(head.code.len());
        let Some(key) = realign_key(head, head_index) else {
            continue;
        };
        let head_duplicates = head_window
            .filter(|index| {
                head.literal(*index).is_some()
                    && key.matches(head, &alignment.head_ids, head, &alignment.head_ids, *index)
            })
            .count();
        if head_duplicates != 1 {
            continue;
        }
        let estimated_base = (0..head_index)
            .rev()
            .find_map(|index| alignment.head_to_base[index])
            .unwrap_or(0);
        let candidates: Vec<usize> = (estimated_base.saturating_sub(REALIGN_WINDOW)
            ..(estimated_base + REALIGN_WINDOW).min(base.code.len()))
            .filter(|index| {
                base.literal(*index).is_some()
                    && key.matches(head, &alignment.head_ids, base, &alignment.base_ids, *index)
            })
            .collect();
        let [candidate] = candidates[..] else {
            continue;
        };
        if used_base.contains(&candidate) {
            continue;
        }
        claims.entry(candidate).or_default().push(position);
    }

    for (base_index, positions) in claims {
        let [position] = positions[..] else {
            continue;
        };
        let head_index = classified[position].0;
        let Some(head_literal) = head.literal(head_index) else {
            continue;
        };
        if let Some(classification) = classify(
            base,
            head,
            alignment,
            head_index,
            head_literal,
            Some(base_index),
        ) {
            classified[position].1 = classification;
        }
    }

    // An ambiguous record must not suggest an English candidate that a trusted pair owns.
    let owned: HashSet<usize> = classified
        .iter()
        .filter(|(_, classification)| is_trusted_pair(classification.kind))
        .filter_map(|(_, classification)| classification.base_index)
        .collect();
    for (_, classification) in classified.iter_mut() {
        if classification.kind == PairKind::Ambiguous
            && classification
                .base_index
                .is_some_and(|base_index| owned.contains(&base_index))
        {
            classification.base_index = None;
            classification.en = None;
            classification.reason = classification
                .reason
                .take()
                .map(|reason| format!("{reason}; candidate belongs to another pair"));
        }
    }
}

/// A head token range around a literal, used to find the same code elsewhere.
struct RealignKey {
    low: usize,
    high: usize,
    literal: usize,
}

impl RealignKey {
    /// True when the tokens around `index` in `other` equal the key's tokens, including the
    /// values of untranslated literals (which the skeleton alone does not compare).
    fn matches(
        &self,
        key_file: &SourceFile,
        key_ids: &[u32],
        other: &SourceFile,
        other_ids: &[u32],
        index: usize,
    ) -> bool {
        let left = self.literal - self.low;
        let right = self.high - self.literal;
        if index < left || index + right >= other_ids.len() {
            return false;
        }
        let other_low = index - left;
        if other_ids[other_low..=index + right] != key_ids[self.low..=self.high] {
            return false;
        }
        (self.low..=self.high).all(|position| match key_file.literal(position) {
            Some(literal) if position != self.literal && !contains_japanese(&literal.value) => {
                other
                    .literal(other_low + (position - self.low))
                    .is_some_and(|other_literal| other_literal.value == literal.value)
            }
            _ => true,
        })
    }
}

/// The literal's context, widened through the calls and struct literals that contain it (so a
/// struct field brings its sibling fields, ids included) but never into a list or a block,
/// whose other entries change too often. `None` when the result is not distinctive.
fn realign_key(head: &SourceFile, index: usize) -> Option<RealignKey> {
    let last = head.code.len().checked_sub(1)?;
    let (mut low, mut high) = head.context_range(index);
    while let Some(open) = head.parent.get(low).copied().flatten() {
        let widenable = head.is_punct(open, "(") || head.is_struct_literal(open);
        let Some(close) = head.matching.get(open).copied().flatten() else {
            break;
        };
        if !widenable {
            break;
        }
        let (element_low, _) = head.element_bounds(open);
        let (_, element_high) = head.element_bounds(close);
        let wider_low = element_low.min(head.callee_start(open));
        if element_high.saturating_sub(wider_low) > MAXIMUM_CONTEXT_TOKENS {
            break;
        }
        low = wider_low;
        high = element_high.max(close);
    }
    let callee_range = head
        .parent
        .get(index)
        .copied()
        .flatten()
        .map(|open| (head.callee_start(open), open));
    if !head.is_distinctive(low, high, index, callee_range) {
        return None;
    }
    Some(RealignKey {
        low: low.saturating_sub(1),
        high: (high + 1).min(last),
        literal: index,
    })
}

/// `Some(other_form)` when the English side picked between singular and plural forms near the
/// literal and the Japanese side no longer does.
fn plural_collapsed(
    base: &SourceFile,
    head: &SourceFile,
    alignment: &Alignment,
    base_index: usize,
    head_index: usize,
) -> Option<Option<String>> {
    // Only the literal's own expression and the condition of the `if` it sits in count: a
    // pluralized neighbor must not make an unrelated literal look collapsed.
    let (base_low, base_high) = base.context_range(base_index);
    let base_condition = base.branch_condition(base_low);
    let base_signal = base.plural_signal(base_low..=base_high)
        || base_condition.is_some_and(|(low, high)| base.plural_signal(low..=high));
    if !base_signal {
        return None;
    }
    let (head_low, head_high) = head.context_range(head_index);
    let head_condition = head.branch_condition(head_low);
    let head_window_low = head_index.saturating_sub(PLURAL_WINDOW);
    let head_window_high = (head_index + PLURAL_WINDOW).min(head.code.len().saturating_sub(1));
    let inserted_nearby: Vec<usize> = (head_window_low..=head_window_high)
        .filter(|index| alignment.head_to_base[*index].is_none())
        .collect();
    let head_signal = head.plural_signal(head_low..=head_high)
        || head_condition.is_some_and(|(low, high)| head.plural_signal(low..=high))
        || head.plural_signal(inserted_nearby.iter().copied());
    if head_signal {
        return None;
    }
    let base_window_low = base_index.saturating_sub(PLURAL_WINDOW);
    let base_window_high = (base_index + PLURAL_WINDOW).min(base.code.len().saturating_sub(1));
    let deleted_nearby: Vec<usize> = (base_window_low..=base_window_high)
        .filter(|index| alignment.base_to_head[*index].is_none())
        .collect();
    // Literals inside the literal's own call (`pluralize("warning", n)`) are the best guess;
    // the deleted neighborhood may belong to a sibling arm.
    let other_form = (base_low..=base_high)
        .chain(deleted_nearby.iter().copied())
        .filter(|index| *index != base_index)
        .filter_map(|index| base.literal(index))
        .map(|literal| literal.value.clone())
        .find(|value| value.len() > 2 && has_ascii_letters(value));
    Some(other_form)
}

fn classify_unaligned(
    base: &SourceFile,
    head: &SourceFile,
    alignment: &Alignment,
    head_index: usize,
) -> Classification {
    let ((mut base_low, mut base_high), (head_low, head_high)) =
        alignment.changed_region(head_index, base.code.len());
    // Also look at the base side of the literal's own context, so that a literal split into two
    // (`show("Save All")` -> `show(concat("すべて", "保存"))`) is not mistaken for an addition.
    let (context_low, context_high) = head.context_range(head_index);
    for index in context_low..=context_high {
        if let Some(Some(base_index)) = alignment.head_to_base.get(index) {
            base_low = base_low.min(*base_index);
            base_high = base_high.max(base_index + 1);
        }
    }
    let english_candidates: Vec<usize> = (base_low..base_high)
        .filter(|index| {
            let Some(literal) = base.literal(*index) else {
                return false;
            };
            let unchanged = alignment.base_to_head[*index]
                .and_then(|head_index| head.literal(head_index))
                .is_some_and(|head_literal| head_literal.value == literal.value);
            !unchanged && has_ascii_letters(&literal.value) && !contains_japanese(&literal.value)
        })
        .collect();
    let japanese_count = (head_low..head_high)
        .filter(|index| {
            head.literal(*index)
                .is_some_and(|literal| contains_japanese(&literal.value))
        })
        .count();
    if english_candidates.is_empty() {
        return Classification::new(PairKind::ForkNew, Some("pure_addition".to_string()));
    }
    let base_signal = base.plural_signal(base_low..base_high);
    let head_signal = head.plural_signal(head_low..head_high);
    let first_candidate = english_candidates.first().copied();
    let first_value = first_candidate
        .and_then(|index| base.literal(index))
        .map(|literal| literal.value.clone());
    let small_region = base_high - base_low <= PLURAL_WINDOW * 2;
    if small_region
        && base_signal
        && !head_signal
        && english_candidates.len() <= 2
        && japanese_count == 1
    {
        let mut classification = Classification::new(
            PairKind::PluralCollapsed,
            Some("plural_collapsed: unaligned".to_string()),
        );
        classification.base_index = first_candidate;
        classification.en = first_value.clone();
        classification.en_plural = english_candidates
            .get(1)
            .and_then(|index| base.literal(*index))
            .map(|literal| literal.value.clone());
        classification.en_placeholders = first_value
            .as_deref()
            .map(placeholder_list)
            .unwrap_or_default();
        return classification;
    }
    let single = english_candidates.len() == 1;
    Classification::ambiguous(
        format!(
            "unaligned_in_changed_region: en {} / ja {}",
            english_candidates.len(),
            japanese_count
        ),
        if single { first_candidate } else { None },
        if single { first_value } else { None },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn pairs(base: &str, head: &str) -> Vec<(PairKind, Option<String>, String, Option<String>)> {
        let base = SourceFile::parse(base);
        let head = SourceFile::parse(head);
        analyze_file(
            "test_crate",
            "crates/test_crate/src/lib.rs",
            Some(&base),
            &head,
            false,
        )
        .records
        .into_iter()
        .map(|record| (record.kind, record.en, record.ja, record.ja_as_msgstr))
        .collect()
    }

    fn exact(en: &str, ja: &str) -> (PairKind, Option<String>, String, Option<String>) {
        (
            PairKind::Exact,
            Some(en.to_string()),
            ja.to_string(),
            Some(ja.to_string()),
        )
    }

    const PADDING: &str = "fn padding_before() { let alpha = compute(1, 2, 3); other(alpha); }\n";

    #[test]
    fn in_place_translation_is_exact() {
        let base = format!(
            "{PADDING}fn menu() {{ MenuItem::action(\"Zoom In\", zed_actions::IncreaseFontSize {{ persist: false }}); }}"
        );
        let head = format!(
            "{PADDING}fn menu() {{ MenuItem::action(\"拡大\", zed_actions::IncreaseFontSize {{ persist: false }}); }}"
        );
        assert_eq!(pairs(&base, &head), vec![exact("Zoom In", "拡大")]);
    }

    #[test]
    fn rustfmt_wrapping_one_line_into_three() {
        let base = format!(
            "{PADDING}fn menu() {{ items.push(MenuItem::action(\"Open Project Settings File\", super::OpenProjectSettingsFile)); }}"
        );
        let head = format!(
            "{PADDING}fn menu() {{ items.push(MenuItem::action(\n    \"プロジェクト設定ファイルを開く\",\n    super::OpenProjectSettingsFile,\n)); }}"
        );
        assert_eq!(
            pairs(&base, &head),
            vec![exact(
                "Open Project Settings File",
                "プロジェクト設定ファイルを開く"
            )]
        );
    }

    #[test]
    fn reindented_menu_moved_out_of_vec() {
        let base = r#"
pub fn app_menus() -> Vec<Menu> {
    vec![
        Menu {
            name: "Zed".into(),
            items: vec![
                MenuItem::action("About Zed", zed_actions::About),
                MenuItem::action("Check for Updates", auto_update::Check),
            ],
        },
        Menu {
            name: "File".into(),
            items: vec![MenuItem::action("New", workspace::NewFile)],
        },
    ]
}
"#;
        let head = r#"
pub fn app_menus() -> Vec<Menu> {
    let zed_menu = Menu {
        name: "Zed".into(),
        items: vec![
            MenuItem::action("Zed について", zed_actions::About),
            MenuItem::action("更新を確認", auto_update::Check),
        ],
    };
    let file_menu = Menu {
        name: "ファイル".into(),
        items: vec![MenuItem::action("新規", workspace::NewFile)],
    };
    vec![zed_menu, file_menu]
}
"#;
        let result = pairs(base, head);
        assert!(
            result.contains(&exact("About Zed", "Zed について")),
            "{result:#?}"
        );
        assert!(
            result.contains(&exact("Check for Updates", "更新を確認")),
            "{result:#?}"
        );
        assert!(result.contains(&exact("New", "新規")), "{result:#?}");
    }

    #[test]
    fn reordered_format_arguments() {
        let base = format!(
            "{PADDING}fn title(term: &str, path: &Path) -> String {{ let label = format!(\"Select a {{}} for {{}}\", term, path); label }}"
        );
        let head = format!(
            "{PADDING}fn title(term: &str, path: &Path) -> String {{ let label = format!(\"{{path}} の {{term}} を選択\"); label }}"
        );
        assert_eq!(
            pairs(&base, &head),
            vec![(
                PairKind::Format,
                Some("Select a {} for {}".to_string()),
                "{path} の {term} を選択".to_string(),
                Some("{1} の {0} を選択".to_string()),
            )]
        );
    }

    #[test]
    fn misaligned_git_hunks_do_not_pair_neighbors() {
        // Line-based diffs pair "Hide Zed" with the first inserted line; the token alignment
        // must keep each literal with its own action.
        let base = r#"
fn menu() {
    let items = vec![
        MenuItem::separator(),
        #[cfg(target_os = "macos")]
        MenuItem::action("Hide Zed", super::Hide),
        MenuItem::separator(),
    ];
}
"#;
        let head = r#"
fn menu() {
    let items = vec![
        MenuItem::separator(),
        MenuItem::action("キーマップを開く", zed_actions::OpenKeymap),
        MenuItem::action("キーマップファイルを開く", zed_actions::OpenKeymapFile),
        #[cfg(target_os = "macos")]
        MenuItem::action("Zed を非表示", super::Hide),
        MenuItem::separator(),
    ];
}
"#;
        let result = pairs(base, head);
        assert_eq!(result.len(), 3, "{result:#?}");
        assert!(
            result.contains(&exact("Hide Zed", "Zed を非表示")),
            "{result:#?}"
        );
        for (kind, en, ja, _) in &result {
            if ja.starts_with("キーマップ") {
                assert_eq!(*kind, PairKind::ForkNew, "{en:?}");
            }
        }
    }

    #[test]
    fn removed_neighbor_does_not_steal_the_pair() {
        let base = r#"
fn panels() -> Vec<MenuItem> {
    vec![
        MenuItem::action("Project Panel", project_panel::ToggleFocus),
        MenuItem::action("Outline Panel", outline_panel::ToggleFocus),
        MenuItem::action("Collab Panel", collab_panel::ToggleFocus),
        MenuItem::action("Terminal Panel", terminal_panel::Toggle),
        MenuItem::action("Debugger Panel", debug_panel::ToggleFocus),
    ]
}
"#;
        let head = r#"
fn panels() -> Vec<MenuItem> {
    vec![
        MenuItem::action("プロジェクトパネル", project_panel::ToggleFocus),
        MenuItem::action("アウトラインパネル", outline_panel::ToggleFocus),
        MenuItem::action("ターミナルパネル", terminal_panel::Toggle),
        MenuItem::action("デバッガーパネル", debug_panel::ToggleFocus),
    ]
}
"#;
        let result = pairs(base, head);
        assert_eq!(
            result,
            vec![
                exact("Project Panel", "プロジェクトパネル"),
                exact("Outline Panel", "アウトラインパネル"),
                exact("Terminal Panel", "ターミナルパネル"),
                exact("Debugger Panel", "デバッガーパネル"),
            ]
        );
    }

    #[test]
    fn look_alike_items_do_not_slide_onto_an_inserted_item() {
        let base = r#"
const TOOLS: &[ToolInfo] = &[
    ToolInfo { id: "create_directory", name: "Create Directory", description: "Directory creation" },
    ToolInfo { id: "fetch", name: "Fetch", description: "HTTP requests to URLs" },
    ToolInfo { id: "search_web", name: "Web Search", description: "Web search queries" },
];
"#;
        let head = r#"
const TOOLS: &[ToolInfo] = &[
    ToolInfo { id: "create_directory", name: "ディレクトリを作成", description: "ディレクトリの作成" },
    ToolInfo { id: "rename_symbol", name: "シンボルの名前を変更", description: "シンボル名の変更" },
    ToolInfo { id: "fetch", name: "取得", description: "URL への HTTP リクエスト" },
    ToolInfo { id: "search_web", name: "Web 検索", description: "Web 検索のクエリ" },
];
"#;
        let result = pairs(base, head);
        for expected in [
            exact("Create Directory", "ディレクトリを作成"),
            exact("Fetch", "取得"),
            exact("HTTP requests to URLs", "URL への HTTP リクエスト"),
            exact("Web Search", "Web 検索"),
        ] {
            assert!(result.contains(&expected), "{expected:?} in {result:#?}");
        }
        for (kind, en, ja, _) in &result {
            if ja.contains("シンボル") {
                assert!(
                    !matches!(kind, PairKind::Exact | PairKind::Format),
                    "{ja} paired with {en:?}"
                );
            }
        }
    }

    #[test]
    fn collapsed_branches_are_not_paired_with_the_wrong_branch() {
        let base = format!(
            r#"{PADDING}fn render(&self) {{
    let tooltip_label = if following {{
        if self.agent_id.as_ref() == ZED_AGENT_ID.as_ref() {{
            format!("Stop Following the {{}}", self.agent_id)
        }} else {{
            format!("Stop Following {{}}", self.agent_id)
        }}
    }} else {{
        if self.agent_id.as_ref() == ZED_AGENT_ID.as_ref() {{
            format!("Follow the {{}}", self.agent_id)
        }} else {{
            format!("Follow {{}}", self.agent_id)
        }}
    }};
    show(tooltip_label);
}}"#
        );
        let head = format!(
            r#"{PADDING}fn render(&self) {{
    let tooltip_label = if following {{
        format!("{{}} の追従を停止", self.agent_id)
    }} else {{
        format!("{{}} に追従", self.agent_id)
    }};
    show(tooltip_label);
}}"#
        );
        for (kind, en, ja, _) in pairs(&base, &head) {
            let plausible = match ja.as_str() {
                "{} に追従" => en.as_deref() == Some("Follow {}"),
                _ => en
                    .as_deref()
                    .is_some_and(|en| en.starts_with("Stop Following")),
            };
            assert!(
                plausible || !matches!(kind, PairKind::Exact | PairKind::Format),
                "{ja} paired with {en:?} as {kind:?}"
            );
        }
    }

    #[test]
    fn collapsed_plain_branches_are_not_paired_with_the_wrong_branch() {
        let base = format!(
            r#"{PADDING}fn render(&self) {{
    let tooltip_label = if following {{
        if self.agent_id.as_ref() == ZED_AGENT_ID.as_ref() {{
            label("Stop Following the agent", self.agent_id)
        }} else {{
            label("Stop Following", self.agent_id)
        }}
    }} else {{
        if self.agent_id.as_ref() == ZED_AGENT_ID.as_ref() {{
            label("Follow the agent", self.agent_id)
        }} else {{
            label("Follow", self.agent_id)
        }}
    }};
    show(tooltip_label);
}}"#
        );
        let head = format!(
            r#"{PADDING}fn render(&self) {{
    let tooltip_label = if following {{
        label("追従を停止", self.agent_id)
    }} else {{
        label("追従", self.agent_id)
    }};
    show(tooltip_label);
}}"#
        );
        for (kind, en, ja, _) in pairs(&base, &head) {
            let plausible = match ja.as_str() {
                "追従" => en.as_deref() == Some("Follow"),
                _ => en
                    .as_deref()
                    .is_some_and(|en| en.starts_with("Stop Following")),
            };
            assert!(
                plausible || !matches!(kind, PairKind::Exact | PairKind::Format),
                "{ja} paired with {en:?} as {kind:?}"
            );
        }
    }

    #[test]
    fn context_needs_an_unchanged_margin_on_both_sides() {
        let base = format!(
            "{PADDING}fn render() {{ let a = compute(1, 2); let b = compute(3, 4); show(\"Old label\"); first_old(alpha, beta, gamma); }}"
        );
        let head = format!(
            "{PADDING}fn render() {{ let a = compute(1, 2); let b = compute(3, 4); show(\"新しいラベル\"); replaced_call(delta); }}"
        );
        let result = pairs(&base, &head);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, PairKind::Ambiguous, "{result:#?}");
    }

    #[test]
    fn short_generic_alignment_is_not_trusted() {
        // The shared stretch `(); x.child(Label::new(<STR>)); }` has enough margin around the
        // call but is too short to rule out a coincidence.
        let base = "fn a() { first(); }\nfn b() { other(); x.child(Label::new(\"Old text\")); }";
        let head =
            "fn a() { first(); }\nfn c() { different(); x.child(Label::new(\"新しい文\")); }";
        let result = pairs(base, head);
        assert_eq!(result.len(), 1);
        assert_ne!(result[0].0, PairKind::Exact, "{result:#?}");
    }

    #[test]
    fn pluralization_collapsed() {
        let base = format!(
            "{PADDING}fn label(count: usize) -> String {{ format!(\"{{}} file{{}} changed\", count, if count == 1 {{ \"\" }} else {{ \"s\" }}) }}"
        );
        let head = format!(
            "{PADDING}fn label(count: usize) -> String {{ format!(\"{{}} 個のファイルを変更\", count) }}"
        );
        let result = pairs(&base, &head);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, PairKind::PluralCollapsed, "{result:#?}");
    }

    #[test]
    fn split_literal_is_ambiguous() {
        let base = format!("{PADDING}fn a() {{ show(\"Save All\"); }}");
        let head = format!("{PADDING}fn a() {{ show(concat(\"すべて\", \"保存\")); }}");
        let result = pairs(&base, &head);
        assert_eq!(result.len(), 2);
        for (kind, ..) in &result {
            assert_eq!(*kind, PairKind::Ambiguous, "{result:#?}");
        }
    }

    #[test]
    fn test_module_ranges_follow_braces() {
        let head = SourceFile::parse(
            r#"
fn before() { ui("前"); }
#[cfg(test)]
mod tests {
    fn inner() { let s = "試験"; }
}
fn after() { ui("後"); }
#[gpui::test]
async fn standalone() { check("単独"); }
"#,
        );
        let records = analyze_file("c", "crates/c/src/lib.rs", None, &head, false).records;
        let flags: Vec<(String, bool)> = records
            .into_iter()
            .map(|record| (record.ja, record.in_test))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("前".to_string(), false),
                ("試験".to_string(), true),
                ("後".to_string(), false),
                ("単独".to_string(), true),
            ]
        );
    }

    #[test]
    fn call_names() {
        let head = SourceFile::parse(
            "fn a() { Label::new(\"一\"); x.tooltip(\"二\"); anyhow!(\"三\"); Menu { name: \"四\".into() }; }",
        );
        let records = analyze_file("c", "crates/c/src/lib.rs", None, &head, false).records;
        let calls: Vec<Option<String>> = records.into_iter().map(|record| record.call).collect();
        assert_eq!(
            calls,
            vec![
                Some("Label::new".to_string()),
                Some(".tooltip".to_string()),
                Some("anyhow!".to_string()),
                Some("Menu.name".to_string()),
            ]
        );
    }

    #[test]
    fn japanese_detection() {
        assert!(contains_japanese("ひらがな"));
        assert!(contains_japanese("カタカナ"));
        assert!(contains_japanese("漢字"));
        assert!(!contains_japanese("English… — “quoted” ×"));
        assert!(!contains_japanese("、。"));
    }
}
