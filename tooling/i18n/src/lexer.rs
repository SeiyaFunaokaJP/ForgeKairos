//! A permissive Rust lexer: it never fails, so half-edited or macro-heavy files still produce a
//! usable token stream. Unterminated literals and comments simply run to the end of the input.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TokenKind {
    Ident,
    Lifetime,
    /// Character and byte character literals (`'a'`, `b'\n'`).
    Char,
    Str(StringLiteral),
    Number,
    /// A single punctuation character. Multi-character operators are split so that token
    /// equality never depends on how an operator happened to be spelled.
    Punct,
    LineComment {
        doc: bool,
    },
    BlockComment {
        doc: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StringPrefix {
    None,
    Byte,
    C,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StringLiteral {
    /// The literal's value with escapes resolved.
    pub value: String,
    pub prefix: StringPrefix,
    pub raw: bool,
    pub terminated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    /// Byte offsets into the (line-ending normalized) source.
    pub start: usize,
    pub end: usize,
}

impl Token {
    pub fn is_comment(&self) -> bool {
        matches!(
            self.kind,
            TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
        )
    }

    pub fn string_literal(&self) -> Option<&StringLiteral> {
        match &self.kind {
            TokenKind::Str(literal) => Some(literal),
            _ => None,
        }
    }
}

/// rustc normalizes CRLF to LF when it loads a source file, so literal values must be computed
/// on normalized text.
pub fn normalize_line_endings(source: &str) -> String {
    source.replace("\r\n", "\n")
}

pub fn tokenize(source: &str) -> Vec<Token> {
    let mut lexer = Lexer {
        source,
        position: 0,
        tokens: Vec::new(),
    };
    lexer.run();
    lexer.tokens
}

struct Lexer<'a> {
    source: &'a str,
    position: usize,
    tokens: Vec<Token>,
}

fn is_identifier_start(character: char) -> bool {
    character == '_' || character.is_alphabetic()
}

fn is_identifier_continue(character: char) -> bool {
    character == '_' || character.is_alphanumeric()
}

impl<'a> Lexer<'a> {
    fn char_at(&self, offset: usize) -> Option<char> {
        self.source
            .get(offset..)
            .and_then(|rest| rest.chars().next())
    }

    fn current(&self) -> Option<char> {
        self.char_at(self.position)
    }

    /// The character `count` characters after the current one.
    fn peek(&self, count: usize) -> Option<char> {
        self.source
            .get(self.position..)
            .and_then(|rest| rest.chars().nth(count))
    }

    fn advance(&mut self) -> Option<char> {
        let character = self.current()?;
        self.position += character.len_utf8();
        Some(character)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        self.tokens.push(Token {
            kind,
            start,
            end: self.position,
        });
    }

    fn run(&mut self) {
        while let Some(character) = self.current() {
            let start = self.position;
            if character.is_whitespace() {
                self.advance();
                continue;
            }
            if character == '/' && self.peek(1) == Some('/') {
                self.line_comment(start);
                continue;
            }
            if character == '/' && self.peek(1) == Some('*') {
                self.block_comment(start);
                continue;
            }
            if character == '"' {
                self.advance();
                self.quoted_string(start, StringPrefix::None);
                continue;
            }
            if character == '\'' {
                self.quote(start);
                continue;
            }
            if character.is_ascii_digit() {
                self.number(start);
                continue;
            }
            if is_identifier_start(character) {
                if self.try_prefixed_literal(start) {
                    continue;
                }
                self.identifier(start);
                continue;
            }
            self.advance();
            self.push(TokenKind::Punct, start);
        }
    }

    fn line_comment(&mut self, start: usize) {
        let rest = &self.source[start..];
        let doc = (rest.starts_with("///") && !rest.starts_with("////")) || rest.starts_with("//!");
        match rest.find('\n') {
            Some(newline) => self.position = start + newline,
            None => self.position = self.source.len(),
        }
        self.push(TokenKind::LineComment { doc }, start);
    }

    fn block_comment(&mut self, start: usize) {
        let rest = &self.source[start..];
        let doc =
            (rest.starts_with("/**") && !rest.starts_with("/***") && !rest.starts_with("/**/"))
                || rest.starts_with("/*!");
        self.position += 2;
        let mut depth = 1usize;
        while depth > 0 {
            match self.current() {
                None => break,
                Some('/') if self.peek(1) == Some('*') => {
                    self.position += 2;
                    depth += 1;
                }
                Some('*') if self.peek(1) == Some('/') => {
                    self.position += 2;
                    depth -= 1;
                }
                Some(_) => {
                    self.advance();
                }
            }
        }
        self.push(TokenKind::BlockComment { doc }, start);
    }

    /// Handles `b"…"`, `c"…"`, `r"…"`, `r#"…"#`, `br"…"`, `cr#"…"#`, `b'…'` and raw identifiers.
    fn try_prefixed_literal(&mut self, start: usize) -> bool {
        let rest = &self.source[start..];
        let (prefix, prefix_length) = if rest.starts_with("br") || rest.starts_with("cr") {
            let prefix = if rest.starts_with('b') {
                StringPrefix::Byte
            } else {
                StringPrefix::C
            };
            (Some(prefix), 2)
        } else if rest.starts_with('b') {
            (Some(StringPrefix::Byte), 1)
        } else if rest.starts_with('c') {
            (Some(StringPrefix::C), 1)
        } else if rest.starts_with('r') {
            (Some(StringPrefix::None), 1)
        } else {
            (None, 0)
        };
        let Some(prefix) = prefix else {
            return false;
        };
        let raw = rest[..prefix_length].ends_with('r');
        let after = &rest[prefix_length..];
        if raw {
            let hashes = after
                .chars()
                .take_while(|character| *character == '#')
                .count();
            if after[hashes..].starts_with('"') {
                self.position = start + prefix_length + hashes + 1;
                self.raw_string(start, prefix, hashes);
                return true;
            }
            if prefix_length == 1
                && hashes == 1
                && after[1..].chars().next().is_some_and(is_identifier_start)
            {
                self.position = start + 2;
                self.identifier(start);
                return true;
            }
            return false;
        }
        if after.starts_with('"') {
            self.position = start + prefix_length + 1;
            self.quoted_string(start, prefix);
            return true;
        }
        if prefix == StringPrefix::Byte && after.starts_with('\'') {
            self.position = start + prefix_length;
            self.quote(start);
            return true;
        }
        false
    }

    fn identifier(&mut self, start: usize) {
        while let Some(character) = self.current() {
            if !is_identifier_continue(character) {
                break;
            }
            self.advance();
        }
        self.push(TokenKind::Ident, start);
    }

    fn number(&mut self, start: usize) {
        let is_radix_prefixed = self.current() == Some('0')
            && matches!(self.peek(1), Some('x' | 'o' | 'b' | 'X' | 'O' | 'B'));
        while let Some(character) = self.current() {
            if is_identifier_continue(character) {
                let exponent = !is_radix_prefixed && matches!(character, 'e' | 'E');
                self.advance();
                if exponent && matches!(self.current(), Some('+' | '-')) {
                    self.advance();
                }
                continue;
            }
            // `1.5` is one number, but `1..2`, `1.max(2)` and `x.0.1` are not.
            if character == '.'
                && !is_radix_prefixed
                && self.peek(1).is_some_and(|next| next.is_ascii_digit())
                && !self.source[start..self.position].contains('.')
            {
                self.advance();
                continue;
            }
            break;
        }
        self.push(TokenKind::Number, start);
    }

    /// At a `'`: a character literal or a lifetime / label.
    fn quote(&mut self, start: usize) {
        self.advance();
        match self.current() {
            Some('\\') => {
                while let Some(character) = self.advance() {
                    if character == '\\' {
                        self.advance();
                        continue;
                    }
                    if character == '\'' || character == '\n' {
                        break;
                    }
                }
                self.push(TokenKind::Char, start);
            }
            Some(character) if self.peek(1) == Some('\'') && character != '\n' => {
                self.advance();
                self.advance();
                self.push(TokenKind::Char, start);
            }
            Some(character) if is_identifier_start(character) => {
                while let Some(character) = self.current() {
                    if !is_identifier_continue(character) {
                        break;
                    }
                    self.advance();
                }
                self.push(TokenKind::Lifetime, start);
            }
            _ => self.push(TokenKind::Punct, start),
        }
    }

    /// Called just after the opening `"`.
    fn quoted_string(&mut self, start: usize, prefix: StringPrefix) {
        let mut value = String::new();
        let mut terminated = false;
        while let Some(character) = self.advance() {
            match character {
                '"' => {
                    terminated = true;
                    break;
                }
                '\\' => self.escape(&mut value),
                other => value.push(other),
            }
        }
        self.push(
            TokenKind::Str(StringLiteral {
                value,
                prefix,
                raw: false,
                terminated,
            }),
            start,
        );
    }

    /// Called just after the `\`.
    fn escape(&mut self, value: &mut String) {
        let Some(character) = self.advance() else {
            value.push('\\');
            return;
        };
        match character {
            'n' => value.push('\n'),
            'r' => value.push('\r'),
            't' => value.push('\t'),
            '\\' => value.push('\\'),
            '0' => value.push('\0'),
            '\'' => value.push('\''),
            '"' => value.push('"'),
            'x' => {
                let digits: String = [self.peek(0), self.peek(1)].into_iter().flatten().collect();
                match u8::from_str_radix(&digits, 16) {
                    Ok(byte) if digits.len() == 2 => {
                        self.advance();
                        self.advance();
                        value.push(char::from(byte));
                    }
                    _ => value.push_str("\\x"),
                }
            }
            'u' if self.current() == Some('{') => {
                let rest = &self.source[self.position..];
                let parsed = rest.find('}').and_then(|close| {
                    let digits: String = rest[1..close].chars().filter(|c| *c != '_').collect();
                    let scalar = u32::from_str_radix(&digits, 16).ok()?;
                    Some((close, char::from_u32(scalar)?))
                });
                match parsed {
                    Some((close, decoded)) => {
                        self.position += close + 1;
                        value.push(decoded);
                    }
                    None => value.push_str("\\u"),
                }
            }
            // Line continuation: the newline and all whitespace that follows it are dropped.
            '\n' => {
                while let Some(next) = self.current() {
                    if !matches!(next, ' ' | '\t' | '\n' | '\r') {
                        break;
                    }
                    self.advance();
                }
            }
            other => {
                value.push('\\');
                value.push(other);
            }
        }
    }

    /// Called just after the opening `"` of a raw string with `hashes` hashes.
    fn raw_string(&mut self, start: usize, prefix: StringPrefix, hashes: usize) {
        let body_start = self.position;
        let mut closing = String::from("\"");
        closing.push_str(&"#".repeat(hashes));
        let (value, terminated) = match self.source[body_start..].find(&closing) {
            Some(offset) => {
                self.position = body_start + offset + closing.len();
                (
                    self.source[body_start..body_start + offset].to_string(),
                    true,
                )
            }
            None => {
                self.position = self.source.len();
                (self.source[body_start..].to_string(), false)
            }
        };
        self.push(
            TokenKind::Str(StringLiteral {
                value,
                prefix,
                raw: true,
                terminated,
            }),
            start,
        );
    }
}

/// Offsets of the first byte of every line, for turning byte offsets into 1-based line numbers.
pub struct LineIndex {
    line_starts: Vec<usize>,
}

impl LineIndex {
    pub fn new(source: &str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(
            source
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(offset, _)| offset + 1),
        );
        Self { line_starts }
    }

    pub fn line_of(&self, offset: usize) -> usize {
        match self.line_starts.binary_search(&offset) {
            Ok(index) => index + 1,
            Err(index) => index,
        }
    }

    pub fn line_start(&self, line: usize) -> usize {
        self.line_starts
            .get(line.saturating_sub(1))
            .copied()
            .unwrap_or(0)
    }

    pub fn line_end(&self, line: usize, source_length: usize) -> usize {
        self.line_starts
            .get(line)
            .map(|next| next.saturating_sub(1))
            .unwrap_or(source_length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn kinds_and_texts(source: &str) -> Vec<(String, String)> {
        tokenize(source)
            .into_iter()
            .map(|token| {
                let label = match &token.kind {
                    TokenKind::Ident => "ident".to_string(),
                    TokenKind::Lifetime => "lifetime".to_string(),
                    TokenKind::Char => "char".to_string(),
                    TokenKind::Str(literal) => format!("str={:?}", literal.value),
                    TokenKind::Number => "number".to_string(),
                    TokenKind::Punct => "punct".to_string(),
                    TokenKind::LineComment { doc } => format!("line_comment doc={doc}"),
                    TokenKind::BlockComment { doc } => format!("block_comment doc={doc}"),
                };
                (label, source[token.start..token.end].to_string())
            })
            .collect()
    }

    fn string_values(source: &str) -> Vec<String> {
        tokenize(source)
            .into_iter()
            .filter_map(|token| token.string_literal().map(|literal| literal.value.clone()))
            .collect()
    }

    #[test]
    fn raw_strings() {
        assert_eq!(
            string_values(r####"r"a\nb" r#"x "quoted" y"# br##"z"#w"## cr"c""####),
            vec!["a\\nb", "x \"quoted\" y", "z\"#w", "c"]
        );
    }

    #[test]
    fn escapes() {
        assert_eq!(
            string_values(r#""tab\there \"q\" \\ \x41 \u{3042} \u{1F600} \0 \'""#),
            vec!["tab\there \"q\" \\ A あ \u{1F600} \0 '"]
        );
        assert_eq!(string_values(r#"b"\x7f\n""#), vec!["\u{7f}\n"]);
    }

    #[test]
    fn line_continuation_drops_newline_and_leading_whitespace() {
        let source = "\"first \\\n        second\\\n\n  third\"";
        assert_eq!(string_values(source), vec!["first secondthird"]);
    }

    #[test]
    fn nested_block_comments() {
        let source =
            "a /* outer /* inner \"not a string\" */ still */ b /** doc */ c /*! inner doc */";
        assert_eq!(
            kinds_and_texts(source),
            vec![
                ("ident".into(), "a".into()),
                (
                    "block_comment doc=false".into(),
                    "/* outer /* inner \"not a string\" */ still */".into()
                ),
                ("ident".into(), "b".into()),
                ("block_comment doc=true".into(), "/** doc */".into()),
                ("ident".into(), "c".into()),
                ("block_comment doc=true".into(), "/*! inner doc */".into()),
            ]
        );
    }

    #[test]
    fn line_and_doc_comments() {
        let source = "/// doc\n//// not doc\n//! inner\n// plain \"x\"\ny";
        assert_eq!(
            kinds_and_texts(source),
            vec![
                ("line_comment doc=true".into(), "/// doc".into()),
                ("line_comment doc=false".into(), "//// not doc".into()),
                ("line_comment doc=true".into(), "//! inner".into()),
                ("line_comment doc=false".into(), "// plain \"x\"".into()),
                ("ident".into(), "y".into()),
            ]
        );
    }

    #[test]
    fn lifetimes_and_character_literals() {
        let source =
            "fn f<'a>(x: &'a str) -> char { 'b' } '\\'' '\\n' b'x' 'label: loop {} 'static '字'";
        let labels: Vec<(String, String)> = kinds_and_texts(source)
            .into_iter()
            .filter(|(kind, _)| kind == "lifetime" || kind == "char")
            .collect();
        assert_eq!(
            labels,
            vec![
                ("lifetime".into(), "'a".into()),
                ("lifetime".into(), "'a".into()),
                ("char".into(), "'b'".into()),
                ("char".into(), "'\\''".into()),
                ("char".into(), "'\\n'".into()),
                ("char".into(), "b'x'".into()),
                ("lifetime".into(), "'label".into()),
                ("lifetime".into(), "'static".into()),
                ("char".into(), "'字'".into()),
            ]
        );
    }

    #[test]
    fn a_quote_in_a_character_literal_does_not_start_a_string() {
        assert_eq!(
            string_values("let q = '\"'; let s = \"real\";"),
            vec!["real"]
        );
    }

    #[test]
    fn numbers_and_ranges() {
        let texts: Vec<String> = kinds_and_texts("1.5e-3 0..10 x.0.1 0xFF_u8 1.max(2)")
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert_eq!(
            texts,
            vec![
                "1.5e-3", "0", ".", ".", "10", "x", ".", "0.1", "0xFF_u8", "1", ".", "max", "(",
                "2", ")"
            ]
        );
    }

    #[test]
    fn raw_identifiers_are_not_raw_strings() {
        assert_eq!(
            kinds_and_texts("r#type r#\"s\"#"),
            vec![
                ("ident".into(), "r#type".into()),
                ("str=\"s\"".into(), "r#\"s\"#".into()),
            ]
        );
    }

    #[test]
    fn unterminated_literals_run_to_the_end() {
        let tokens = tokenize("let a = \"never closed");
        let literal = tokens.last().and_then(Token::string_literal).cloned();
        assert_eq!(literal.map(|literal| literal.terminated), Some(false));
    }

    #[test]
    fn line_index() {
        let source = "a\nbc\n\nd";
        let index = LineIndex::new(source);
        assert_eq!(index.line_of(0), 1);
        assert_eq!(index.line_of(2), 2);
        assert_eq!(index.line_of(5), 3);
        assert_eq!(index.line_of(6), 4);
    }
}
