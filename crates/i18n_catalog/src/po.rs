//! A reader for gettext PO files.
//!
//! Errors are collected with their line numbers instead of stopping at the first one,
//! so a single broken entry in a hand-edited file does not hide every other translation.

use std::fmt;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoFile {
    /// Where the text came from, used in error messages (a path or `embedded/ja/x.po`).
    pub origin: String,
    pub header: PoHeader,
    /// Every entry except the header, in file order. Fuzzy entries are kept (with
    /// `fuzzy` set) so tools can report them; catalogs skip them.
    pub entries: Vec<PoEntry>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoHeader {
    pub language: Option<String>,
    /// `X-Language-Name`, the name of the language in that language.
    pub language_name: Option<String>,
    /// The raw `Plural-Forms` value.
    pub plural_forms: Option<String>,
    /// Every `Name: value` field of the header, in order.
    pub fields: Vec<(String, String)>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PoEntry {
    pub context: Option<String>,
    pub msgid: String,
    pub msgid_plural: Option<String>,
    /// One string for a plain entry, `nplurals` strings for a plural one.
    pub msgstr: Vec<String>,
    pub fuzzy: bool,
    /// The 1-based line of the entry's first keyword.
    pub line: usize,
}

impl PoEntry {
    /// Untranslated entries (fuzzy, or with every msgstr empty) must not be used.
    pub fn is_translated(&self) -> bool {
        !self.fuzzy && self.msgstr.iter().any(|string| !string.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoError {
    pub origin: String,
    /// 1-based; 0 when the problem is not tied to a line (such as an unreadable file).
    pub line: usize,
    pub message: String,
}

impl fmt::Display for PoError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.line == 0 {
            write!(formatter, "{}: {}", self.origin, self.message)
        } else {
            write!(formatter, "{}:{}: {}", self.origin, self.line, self.message)
        }
    }
}

impl std::error::Error for PoError {}

/// Reads a PO file. Everything that could be read is returned together with the
/// errors, so callers can use the readable entries of a partly broken file.
pub fn parse_po(origin: &str, text: &str) -> (PoFile, Vec<PoError>) {
    let mut parser = Parser {
        origin,
        file: PoFile {
            origin: origin.to_string(),
            ..PoFile::default()
        },
        errors: Vec::new(),
        entry: EntryBuilder::default(),
        pending_fuzzy: false,
        seen_header: false,
    };
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    for (index, line) in text.split('\n').enumerate() {
        let line = line.strip_suffix('\r').unwrap_or(line);
        parser.line(index + 1, line);
    }
    parser.finish_entry();
    (parser.file, parser.errors)
}

/// Reads only the header, for listing languages without parsing whole catalogs.
pub(crate) fn parse_header_only(origin: &str, text: &str) -> PoHeader {
    let mut end = text.len();
    let mut seen_msgstr = false;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim();
        if trimmed.starts_with("msgstr") {
            seen_msgstr = true;
        } else if seen_msgstr && !trimmed.starts_with('"') {
            end = offset;
            break;
        }
        offset += line.len();
    }
    parse_po(origin, &text[..end]).0.header
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Context,
    Msgid,
    MsgidPlural,
    Msgstr,
    MsgstrIndexed(usize),
}

#[derive(Default)]
struct EntryBuilder {
    first_line: Option<usize>,
    context: Option<String>,
    msgid: Option<String>,
    msgid_plural: Option<String>,
    msgstr: Option<String>,
    msgstr_indexed: Vec<(usize, String)>,
    fuzzy: bool,
    last_field: Option<Field>,
    broken: bool,
    saw_msgstr_keyword: bool,
}

impl EntryBuilder {
    fn has_msgstr(&self) -> bool {
        self.msgstr.is_some() || !self.msgstr_indexed.is_empty()
    }

    fn is_empty(&self) -> bool {
        self.first_line.is_none()
    }

    fn field_mut(&mut self, field: Field) -> Option<&mut String> {
        match field {
            Field::Context => self.context.as_mut(),
            Field::Msgid => self.msgid.as_mut(),
            Field::MsgidPlural => self.msgid_plural.as_mut(),
            Field::Msgstr => self.msgstr.as_mut(),
            Field::MsgstrIndexed(index) => self
                .msgstr_indexed
                .iter_mut()
                .find(|(existing, _)| *existing == index)
                .map(|(_, string)| string),
        }
    }
}

struct Parser<'a> {
    origin: &'a str,
    file: PoFile,
    errors: Vec<PoError>,
    entry: EntryBuilder,
    pending_fuzzy: bool,
    seen_header: bool,
}

impl Parser<'_> {
    fn error(&mut self, line: usize, message: impl Into<String>) {
        self.errors.push(PoError {
            origin: self.origin.to_string(),
            line,
            message: message.into(),
        });
    }

    fn line(&mut self, number: usize, line: &str) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            self.finish_entry();
            return;
        }
        if trimmed.starts_with("#~") {
            return;
        }
        if let Some(comment) = trimmed.strip_prefix('#') {
            if self.entry.has_msgstr() || self.entry.broken {
                self.finish_entry();
            }
            if let Some(flags) = comment.strip_prefix(',')
                && flags.split(',').any(|flag| flag.trim() == "fuzzy")
            {
                self.pending_fuzzy = true;
            }
            return;
        }
        if trimmed.starts_with('"') {
            self.continuation(number, trimmed);
            return;
        }

        let (keyword, rest) = match trimmed.find(|character: char| character.is_whitespace()) {
            Some(position) => (&trimmed[..position], trimmed[position..].trim_start()),
            None => (trimmed, ""),
        };
        let field = match keyword {
            "msgctxt" => Field::Context,
            "msgid" => Field::Msgid,
            "msgid_plural" => Field::MsgidPlural,
            "msgstr" => Field::Msgstr,
            _ => match keyword
                .strip_prefix("msgstr[")
                .and_then(|rest| rest.strip_suffix(']'))
                .map(str::parse::<usize>)
            {
                Some(Ok(index)) => Field::MsgstrIndexed(index),
                _ => {
                    if self.entry.has_msgstr() {
                        self.finish_entry();
                    }
                    self.error(number, format!("unexpected text {trimmed:?}"));
                    self.mark_broken(number);
                    return;
                }
            },
        };
        let is_msgstr = matches!(field, Field::Msgstr | Field::MsgstrIndexed(_));
        let starts_entry = matches!(field, Field::Context | Field::Msgid);

        if self.entry.broken {
            // Skip the rest of a broken entry, but do not swallow the next entry when
            // entries are not separated by blank lines.
            let starts_next_entry =
                field == Field::Context || (field == Field::Msgid && self.entry.saw_msgstr_keyword);
            if !starts_next_entry {
                self.entry.saw_msgstr_keyword |= is_msgstr;
                return;
            }
            self.finish_entry();
        }

        let value = match unquote(rest) {
            Ok(value) => value,
            Err(message) => {
                if starts_entry && self.entry.has_msgstr() {
                    self.finish_entry();
                }
                self.error(number, message);
                self.mark_broken(number);
                self.entry.saw_msgstr_keyword |= is_msgstr;
                return;
            }
        };
        self.keyword(number, field, value);
    }

    fn mark_broken(&mut self, number: usize) {
        if self.entry.is_empty() {
            self.entry.first_line = Some(number);
        }
        self.entry.broken = true;
        self.entry.last_field = None;
    }

    fn keyword(&mut self, number: usize, field: Field, value: String) {
        let starts_entry = matches!(field, Field::Context | Field::Msgid);
        if starts_entry && self.entry.has_msgstr() {
            self.finish_entry();
        }
        if self.entry.is_empty() {
            self.entry.first_line = Some(number);
            self.entry.fuzzy = std::mem::take(&mut self.pending_fuzzy);
        }

        let entry = &mut self.entry;
        entry.saw_msgstr_keyword |= matches!(field, Field::Msgstr | Field::MsgstrIndexed(_));
        let problem = match field {
            Field::Context if entry.context.is_some() || entry.msgid.is_some() => {
                Some("msgctxt must come first in an entry and only once")
            }
            Field::Context => {
                entry.context = Some(value);
                None
            }
            Field::Msgid if entry.msgid.is_some() => Some("msgid without msgstr"),
            Field::Msgid => {
                entry.msgid = Some(value);
                None
            }
            Field::MsgidPlural if entry.msgid.is_none() || entry.msgid_plural.is_some() => {
                Some("msgid_plural must follow msgid, once")
            }
            Field::MsgidPlural => {
                entry.msgid_plural = Some(value);
                None
            }
            Field::Msgstr | Field::MsgstrIndexed(_) if entry.msgid.is_none() => {
                Some("msgstr without msgid")
            }
            Field::Msgstr if entry.msgid_plural.is_some() => {
                Some("an entry with msgid_plural needs msgstr[N], not msgstr")
            }
            Field::Msgstr if entry.has_msgstr() => Some("more than one msgstr"),
            Field::Msgstr => {
                entry.msgstr = Some(value);
                None
            }
            Field::MsgstrIndexed(_) if entry.msgid_plural.is_none() => {
                Some("msgstr[N] needs msgid_plural")
            }
            Field::MsgstrIndexed(index) if index != entry.msgstr_indexed.len() => {
                Some("msgstr[N] must be numbered 0, 1, 2, ... in order")
            }
            Field::MsgstrIndexed(index) => {
                entry.msgstr_indexed.push((index, value));
                None
            }
        };
        match problem {
            Some(message) => {
                self.error(number, message);
                self.mark_broken(number);
            }
            None => self.entry.last_field = Some(field),
        }
    }

    fn continuation(&mut self, number: usize, trimmed: &str) {
        if self.entry.broken {
            return;
        }
        let value = match unquote(trimmed) {
            Ok(value) => value,
            Err(message) => {
                self.error(number, message);
                self.mark_broken(number);
                return;
            }
        };
        let Some(field) = self.entry.last_field else {
            self.error(number, "a quoted string must follow a keyword");
            self.mark_broken(number);
            return;
        };
        if let Some(string) = self.entry.field_mut(field) {
            string.push_str(&value);
        }
    }

    fn finish_entry(&mut self) {
        let entry = std::mem::take(&mut self.entry);
        let Some(line) = entry.first_line else {
            return;
        };
        if entry.broken {
            return;
        }
        let Some(msgid) = entry.msgid else {
            self.error(line, "entry without msgid");
            return;
        };
        let msgstr = if entry.msgid_plural.is_some() {
            entry
                .msgstr_indexed
                .into_iter()
                .map(|(_, string)| string)
                .collect::<Vec<_>>()
        } else {
            entry.msgstr.into_iter().collect()
        };
        if msgstr.is_empty() {
            self.error(line, "entry without msgstr");
            return;
        }

        if msgid.is_empty() && entry.context.is_none() {
            if self.seen_header {
                self.error(line, "a second header entry");
            } else {
                self.seen_header = true;
                self.file.header = parse_header(msgstr.first().map_or("", String::as_str));
            }
            return;
        }

        self.file.entries.push(PoEntry {
            context: entry.context,
            msgid,
            msgid_plural: entry.msgid_plural,
            msgstr,
            fuzzy: entry.fuzzy,
            line,
        });
    }
}

fn parse_header(text: &str) -> PoHeader {
    let mut header = PoHeader::default();
    for line in text.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let value = value.trim();
        match name {
            "Language" => header.language = Some(value.to_string()),
            "X-Language-Name" => header.language_name = Some(value.to_string()),
            "Plural-Forms" => header.plural_forms = Some(value.to_string()),
            _ => {}
        }
        header.fields.push((name.to_string(), value.to_string()));
    }
    header
}

fn unquote(text: &str) -> Result<String, String> {
    let Some(inner) = text.strip_prefix('"') else {
        return Err(format!("expected a quoted string, found {text:?}"));
    };
    let mut output = String::with_capacity(inner.len());
    let mut characters = inner.chars();
    loop {
        match characters.next() {
            None => return Err("the quoted string is not closed".to_string()),
            Some('"') => break,
            Some('\\') => match characters.next() {
                Some('\\') => output.push('\\'),
                Some('"') => output.push('"'),
                Some('n') => output.push('\n'),
                Some('t') => output.push('\t'),
                Some('r') => output.push('\r'),
                Some(other) => return Err(format!("unknown escape \\{other}")),
                None => return Err("the quoted string is not closed".to_string()),
            },
            Some(character) => output.push(character),
        }
    }
    let rest = characters.as_str().trim();
    if !rest.is_empty() {
        return Err(format!("unexpected text after the quoted string: {rest:?}"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn parse_ok(text: &str) -> PoFile {
        let (file, errors) = parse_po("test.po", text);
        assert_eq!(errors, Vec::new());
        file
    }

    #[test]
    fn header_and_plain_entries() {
        let file = parse_ok(concat!(
            "# translator comment\n",
            "msgid \"\"\n",
            "msgstr \"\"\n",
            "\"Language: ja\\n\"\n",
            "\"X-Language-Name: 日本語\\n\"\n",
            "\"Plural-Forms: nplurals=1; plural=0;\\n\"\n",
            "\n",
            "#. extracted comment\n",
            "#: src/main.rs:1\n",
            "#| msgid \"Old\"\n",
            "msgid \"Go to Definition\"\n",
            "msgstr \"定義へ移動\"\n",
        ));
        assert_eq!(file.header.language.as_deref(), Some("ja"));
        assert_eq!(file.header.language_name.as_deref(), Some("日本語"));
        assert_eq!(
            file.header.plural_forms.as_deref(),
            Some("nplurals=1; plural=0;")
        );
        assert_eq!(
            file.entries,
            vec![PoEntry {
                context: None,
                msgid: "Go to Definition".into(),
                msgid_plural: None,
                msgstr: vec!["定義へ移動".into()],
                fuzzy: false,
                line: 11,
            }]
        );
    }

    #[test]
    fn context_plural_multiline_and_escapes() {
        let file = parse_ok(concat!(
            "msgctxt \"action\"\n",
            "msgid \"editor::GoToDefinition\"\n",
            "msgstr \"定義へ移動\"\n",
            "\n",
            "msgid \"{count} Comment\"\n",
            "msgid_plural \"{count} Comments\"\n",
            "msgstr[0] \"コメント {count} 件\"\n",
            "msgstr[1] \"\"\n",
            "\"複数\"\n",
            "\n",
            "msgid \"\"\n",
            "\"line one\\n\"\n",
            "\"tab\\there \\\"quoted\\\" back\\\\slash\\r\"\n",
            "msgstr \"x\"\n",
        ));
        assert_eq!(file.entries.len(), 3);
        assert_eq!(file.entries[0].context.as_deref(), Some("action"));
        assert_eq!(
            file.entries[1].msgid_plural.as_deref(),
            Some("{count} Comments")
        );
        assert_eq!(file.entries[1].msgstr, vec!["コメント {count} 件", "複数"]);
        assert_eq!(
            file.entries[2].msgid,
            "line one\ntab\there \"quoted\" back\\slash\r"
        );
        assert_eq!(file.entries[2].line, 11);
    }

    #[test]
    fn entries_without_blank_lines_between_them() {
        let file = parse_ok(
            "msgid \"a\"\nmsgstr \"A\"\nmsgid \"b\"\nmsgstr \"B\"\n#, fuzzy\nmsgid \"c\"\nmsgstr \"C\"\n",
        );
        let ids: Vec<_> = file
            .entries
            .iter()
            .map(|entry| entry.msgid.as_str())
            .collect();
        assert_eq!(ids, ["a", "b", "c"]);
        assert!(file.entries[2].fuzzy);
        assert!(!file.entries[1].fuzzy);
    }

    #[test]
    fn fuzzy_empty_and_obsolete_entries() {
        let file = parse_ok(concat!(
            "#, fuzzy, c-format\n",
            "msgid \"Fuzzy\"\n",
            "msgstr \"あいまい\"\n",
            "\n",
            "msgid \"Empty\"\n",
            "msgstr \"\"\n",
            "\n",
            "#~ msgid \"Obsolete\"\n",
            "#~ msgstr \"廃止\"\n",
            "\n",
            "#, c-format\n",
            "msgid \"Kept\"\n",
            "msgstr \"残す\"\n",
        ));
        let ids: Vec<_> = file
            .entries
            .iter()
            .map(|entry| entry.msgid.as_str())
            .collect();
        assert_eq!(ids, ["Fuzzy", "Empty", "Kept"]);
        assert!(file.entries[0].fuzzy);
        assert!(!file.entries[0].is_translated());
        assert!(!file.entries[1].is_translated());
        assert!(file.entries[2].is_translated());
    }

    #[test]
    fn byte_order_mark_and_crlf() {
        let file = parse_ok(
            "\u{feff}msgid \"\"\r\nmsgstr \"Language: fr\\n\"\r\n\r\nmsgid \"a\"\r\nmsgstr \"b\"\r\n",
        );
        assert_eq!(file.header.language.as_deref(), Some("fr"));
        assert_eq!(file.entries[0].msgstr, vec!["b"]);
        assert_eq!(file.entries[0].line, 4);
    }

    #[test]
    fn errors_have_line_numbers_and_do_not_discard_other_entries() {
        let (file, errors) = parse_po(
            "broken.po",
            concat!(
                "msgid \"good one\"\n",
                "msgstr \"1\"\n",
                "\n",
                "msgid \"unterminated\n",
                "msgstr \"x\"\n",
                "\n",
                "msgid \"bad escape\"\n",
                "msgstr \"\\q\"\n",
                "\n",
                "garbage line\n",
                "\n",
                "msgid \"plural\"\n",
                "msgid_plural \"plurals\"\n",
                "msgstr \"wrong\"\n",
                "\n",
                "msgid \"gap\"\n",
                "msgid_plural \"gaps\"\n",
                "msgstr[1] \"x\"\n",
                "\n",
                "msgid \"no translation\"\n",
                "\n",
                "msgstr \"orphan\"\n",
                "\n",
                "\"orphan string\"\n",
                "\n",
                "msgid \"trailing\" junk\n",
                "msgstr \"t\"\n",
                "\n",
                "msgid \"good two\"\n",
                "msgstr \"2\"\n",
            ),
        );
        let ids: Vec<_> = file
            .entries
            .iter()
            .map(|entry| entry.msgid.as_str())
            .collect();
        assert_eq!(ids, ["good one", "good two"]);
        let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
        assert_eq!(lines, [4, 8, 10, 14, 18, 20, 22, 24, 26]);
        assert!(errors.iter().all(|error| error.origin == "broken.po"));
        assert_eq!(
            errors[0].to_string(),
            "broken.po:4: the quoted string is not closed"
        );
    }

    #[test]
    fn a_broken_entry_does_not_swallow_the_next_one_without_a_blank_line() {
        let (file, errors) = parse_po(
            "x.po",
            concat!(
                "msgid \"a\"\n",
                "msgstr \"\\q\"\n",
                "msgid \"b\"\n",
                "msgstr \"B\"\n",
                "msgctxt \"c\"\n",
                "msgctxt \"d\"\n",
                "msgctxt \"e\"\n",
                "msgid \"f\"\n",
                "msgstr \"F\"\n",
            ),
        );
        let keys: Vec<_> = file
            .entries
            .iter()
            .map(|entry| (entry.context.as_deref(), entry.msgid.as_str()))
            .collect();
        assert_eq!(keys, [(None, "b"), (Some("e"), "f")]);
        let lines: Vec<_> = errors.iter().map(|error| error.line).collect();
        assert_eq!(lines, [2, 6]);
    }

    #[test]
    fn arbitrary_bytes_do_not_panic() {
        let samples = [
            "\"",
            "msgid",
            "msgid \"",
            "msgstr[",
            "msgstr[99999999999999999999999] \"x\"",
            "msgid \"a\"\nmsgid_plural \"b\"\nmsgstr[0] \"\"\n\"\\",
            "#,",
            "msgctxt \"c\"\nmsgctxt \"d\"\nmsgid \"e\"\nmsgstr \"f\"",
            "msgid \"\"\nmsgstr \"\"\n\nmsgid \"\"\nmsgstr \"\"",
            "\u{feff}",
            "\r\r\r",
            "msgid \"é\\",
        ];
        for sample in samples {
            parse_po("sample.po", sample);
        }
        let mut state = 0x2545f4914f6cdd1du64;
        let alphabet: Vec<char> = "msgidctxr_plural[]0123 \"\\\n#,~:é".chars().collect();
        for _ in 0..2000 {
            let text: String = (0..64)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    alphabet[(state % alphabet.len() as u64) as usize]
                })
                .collect();
            parse_po("random.po", &text);
        }
    }

    #[test]
    fn header_only() {
        let header = parse_header_only(
            "x.po",
            "# comment\nmsgid \"\"\nmsgstr \"\"\n\"X-Language-Name: Français\\n\"\nmsgid \"a\"\nmsgstr \"b\"\n",
        );
        assert_eq!(header.language_name.as_deref(), Some("Français"));
    }
}
