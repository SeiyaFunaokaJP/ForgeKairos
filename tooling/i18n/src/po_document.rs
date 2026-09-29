//! PO files as the repository keeps them: entries sorted by msgctxt (entries without one first),
//! then msgid, one line per string, no `#:` locations, LF line endings. Comments written above an
//! entry (`#.`, `#,`, translator comments) stay with it.

use anyhow::{Result, bail};
use i18n_catalog::{PoEntry, PoError, parse_po};
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PoDocument {
    pub header_comments: Vec<String>,
    /// `None` when the file has no header entry.
    pub header: Option<Vec<(String, String)>>,
    pub entries: Vec<DocumentEntry>,
    /// Comments after the last entry.
    pub trailing_comments: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentEntry {
    pub comments: Vec<String>,
    pub entry: PoEntry,
}

pub type EntryKey<'a> = (Option<&'a str>, &'a str);

pub fn entry_key(entry: &PoEntry) -> EntryKey<'_> {
    (entry.context.as_deref(), entry.msgid.as_str())
}

/// The order of entries in a file. `Option` puts entries without a context first; strings compare
/// byte by byte, so the order does not depend on the locale of whoever sorts.
pub fn compare_entries(left: &PoEntry, right: &PoEntry) -> Ordering {
    entry_key(left)
        .cmp(&entry_key(right))
        .then_with(|| left.msgid_plural.cmp(&right.msgid_plural))
}

/// Reads a PO file keeping its comments. Any syntax error makes the whole file unusable here,
/// because rewriting a partly read file would drop the broken entries.
pub fn read_document(origin: &str, text: &str) -> Result<PoDocument, Vec<PoError>> {
    let (file, errors) = parse_po(origin, text);
    if !errors.is_empty() {
        return Err(errors);
    }
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    let comment_lines: Vec<(usize, String)> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim_start().starts_with('#'))
        .map(|(index, line)| (index + 1, line.trim().to_string()))
        .collect();

    // The header is the first `msgid ""` that is not preceded by a msgctxt.
    let header_line = if file.header.fields.is_empty() && !has_empty_header(&lines) {
        None
    } else {
        lines.iter().enumerate().find_map(|(index, line)| {
            let is_header_msgid = line.trim() == "msgid \"\"";
            let after_context = index > 0 && lines[index - 1].trim_start().starts_with("msgctxt");
            (is_header_msgid && !after_context).then_some(index + 1)
        })
    };

    let mut document = PoDocument {
        header: header_line.map(|_| file.header.fields.clone()),
        ..PoDocument::default()
    };
    let mut comments = comment_lines.into_iter().peekable();
    if let Some(header_line) = header_line {
        while let Some((line, _)) = comments.peek()
            && *line < header_line
        {
            if let Some((_, comment)) = comments.next() {
                document.header_comments.push(comment);
            }
        }
    }
    for entry in file.entries {
        let mut entry_comments = Vec::new();
        while let Some((line, _)) = comments.peek()
            && *line < entry.line
        {
            if let Some((_, comment)) = comments.next() {
                entry_comments.push(comment);
            }
        }
        document.entries.push(DocumentEntry {
            comments: entry_comments,
            entry,
        });
    }
    document.trailing_comments = comments.map(|(_, comment)| comment).collect();
    Ok(document)
}

/// A header whose msgstr is empty has no fields but is still a header.
fn has_empty_header(lines: &[&str]) -> bool {
    lines
        .iter()
        .position(|line| line.trim() == "msgid \"\"")
        .is_some_and(|index| {
            (index == 0 || !lines[index - 1].trim_start().starts_with("msgctxt"))
                && lines
                    .get(index + 1)
                    .is_some_and(|next| next.trim() == "msgstr \"\"")
        })
}

pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len() + 2);
    for character in text.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\t' => escaped.push_str("\\t"),
            '\r' => escaped.push_str("\\r"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Every string is one line, line breaks included, so that a changed message is a one-line diff
/// and a merge never has to line up continuation lines.
fn write_string(block: &mut String, keyword: &str, value: &str) {
    writeln!(block, "{keyword} \"{}\"", escape(value)).ok();
}

/// Writes the document in its current entry order.
pub fn render(document: &PoDocument) -> String {
    let mut blocks: Vec<String> = Vec::new();
    if let Some(fields) = &document.header {
        let mut block = String::new();
        for comment in &document.header_comments {
            block.push_str(comment);
            block.push('\n');
        }
        block.push_str("msgid \"\"\nmsgstr \"\"\n");
        for (name, value) in fields {
            block.push('"');
            block.push_str(&escape(&format!("{name}: {value}\n")));
            block.push_str("\"\n");
        }
        blocks.push(block);
    }
    for DocumentEntry { comments, entry } in &document.entries {
        let mut block = String::new();
        for comment in comments {
            block.push_str(comment);
            block.push('\n');
        }
        if let Some(context) = &entry.context {
            write_string(&mut block, "msgctxt", context);
        }
        write_string(&mut block, "msgid", &entry.msgid);
        match &entry.msgid_plural {
            Some(plural) => {
                write_string(&mut block, "msgid_plural", plural);
                for (index, form) in entry.msgstr.iter().enumerate() {
                    write_string(&mut block, &format!("msgstr[{index}]"), form);
                }
            }
            None => {
                let msgstr = entry.msgstr.first().map_or("", String::as_str);
                write_string(&mut block, "msgstr", msgstr);
            }
        }
        blocks.push(block);
    }
    if !document.trailing_comments.is_empty() {
        let mut block = String::new();
        for comment in &document.trailing_comments {
            block.push_str(comment);
            block.push('\n');
        }
        blocks.push(block);
    }
    blocks.join("\n")
}

/// Sorts the entries and writes the canonical text. Duplicate keys are an error: which one to
/// keep is a decision, and `po-union` makes it.
pub fn format_document(origin: &str, mut document: PoDocument) -> Result<String> {
    document
        .entries
        .sort_by(|left, right| compare_entries(&left.entry, &right.entry));
    let duplicates: Vec<String> = document
        .entries
        .windows(2)
        .filter(|pair| entry_key(&pair[0].entry) == entry_key(&pair[1].entry))
        .map(|pair| {
            format!(
                "{origin}:{}: duplicate of the entry on line {}: {}",
                pair[1].entry.line.max(pair[0].entry.line),
                pair[1].entry.line.min(pair[0].entry.line),
                describe_key(&pair[1].entry)
            )
        })
        .collect();
    if !duplicates.is_empty() {
        bail!(
            "{}\n(merge duplicates with `po-union {origin} --out {origin}`)",
            duplicates.join("\n")
        );
    }
    Ok(render(&document))
}

pub fn describe_key(entry: &PoEntry) -> String {
    match &entry.context {
        Some(context) => format!("[{context}] {:?}", entry.msgid),
        None => format!("{:?}", entry.msgid),
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct UnionConflict {
    pub description: String,
}

/// The union of several documents. Entries with the same key must agree; an entry with an empty
/// msgstr gives way to a filled one. The header is the first document's,
/// and the others must have the same header or none.
pub fn union(documents: &[(String, PoDocument)]) -> Result<PoDocument, Vec<UnionConflict>> {
    let mut conflicts = Vec::new();
    let mut result = PoDocument::default();
    let mut header_origin = String::new();
    for (origin, document) in documents {
        let Some(header) = &document.header else {
            continue;
        };
        match &result.header {
            None => {
                result.header = Some(header.clone());
                result.header_comments = document.header_comments.clone();
                header_origin = origin.clone();
            }
            Some(existing) if existing != header => conflicts.push(UnionConflict {
                description: format!("the headers of {header_origin} and {origin} differ"),
            }),
            Some(_) => merge_comments(&mut result.header_comments, &document.header_comments),
        }
    }

    let mut merged: BTreeMap<(Option<String>, String), (String, DocumentEntry)> = BTreeMap::new();
    for (origin, document) in documents {
        for candidate in &document.entries {
            let key = (
                candidate.entry.context.clone(),
                candidate.entry.msgid.clone(),
            );
            let Some((existing_origin, existing)) = merged.get_mut(&key) else {
                merged.insert(key, (origin.clone(), candidate.clone()));
                continue;
            };
            let same_translation = existing.entry.msgid_plural == candidate.entry.msgid_plural
                && existing.entry.msgstr == candidate.entry.msgstr
                && existing.entry.fuzzy == candidate.entry.fuzzy;
            let same_shape = existing.entry.msgid_plural == candidate.entry.msgid_plural;
            // A fuzzy mark is a decision about the translation, so only an empty msgstr gives way.
            let keep_existing =
                same_shape && is_empty(&candidate.entry) && !is_empty(&existing.entry);
            let take_candidate =
                same_shape && is_empty(&existing.entry) && !is_empty(&candidate.entry);
            if same_translation {
                merge_comments(&mut existing.comments, &candidate.comments);
            } else if take_candidate {
                *existing = candidate.clone();
                *existing_origin = origin.clone();
            } else if !keep_existing {
                conflicts.push(UnionConflict {
                    description: format!(
                        "{}: {existing_origin}:{} has {:?}{}, {origin}:{} has {:?}{}",
                        describe_key(&candidate.entry),
                        existing.entry.line,
                        existing.entry.msgstr,
                        fuzzy_note(&existing.entry),
                        candidate.entry.line,
                        candidate.entry.msgstr,
                        fuzzy_note(&candidate.entry),
                    ),
                });
            }
        }
        merge_comments(&mut result.trailing_comments, &document.trailing_comments);
    }
    if !conflicts.is_empty() {
        return Err(conflicts);
    }
    result.entries = merged.into_values().map(|(_, entry)| entry).collect();
    result
        .entries
        .sort_by(|left, right| compare_entries(&left.entry, &right.entry));
    Ok(result)
}

fn is_empty(entry: &PoEntry) -> bool {
    entry.msgstr.iter().all(String::is_empty)
}

fn fuzzy_note(entry: &PoEntry) -> &'static str {
    if entry.fuzzy { " (fuzzy)" } else { "" }
}

fn merge_comments(target: &mut Vec<String>, source: &[String]) {
    for comment in source {
        if !target.contains(comment) {
            target.push(comment.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    const HEADER: &str = "msgid \"\"\nmsgstr \"\"\n\"Language: ja\\n\"\n\"Plural-Forms: nplurals=1; plural=0;\\n\"\n";

    fn document(text: &str) -> PoDocument {
        read_document("test.po", text).expect("parses")
    }

    #[test]
    fn formatting_sorts_escapes_and_keeps_comments() {
        let text = concat!(
            "# translator note\n",
            "msgid \"\"\n",
            "msgstr \"\"\n",
            "\"Language: ja\\n\"\n",
            "\"Plural-Forms: nplurals=1; plural=0;\\n\"\n",
            "\n\n\n",
            "msgctxt \"tab\"\r\n",
            "msgid \"Close\"\n",
            "msgstr \"閉じる\"\n",
            "#. editor: open\n",
            "msgid \"b\"\n",
            "msgstr \"\"\n",
            "\"multi\\n\"\n",
            "\"line\\t\\\"x\\\"\\n\"\n",
            "\n",
            "#, fuzzy\n",
            "msgid \"Z {count}\"\n",
            "msgid_plural \"Z {count}s\"\n",
            "msgstr[0] \"ぜっと\"\n",
            "msgid \"A\"\n",
            "msgstr \"あ\\n\"\n",
            "\n",
            "# trailing\n",
        );
        let formatted = format_document("test.po", document(text)).expect("formats");
        assert_eq!(
            formatted,
            concat!(
                "# translator note\n",
                "msgid \"\"\n",
                "msgstr \"\"\n",
                "\"Language: ja\\n\"\n",
                "\"Plural-Forms: nplurals=1; plural=0;\\n\"\n",
                "\n",
                "msgid \"A\"\n",
                "msgstr \"あ\\n\"\n",
                "\n",
                "#, fuzzy\n",
                "msgid \"Z {count}\"\n",
                "msgid_plural \"Z {count}s\"\n",
                "msgstr[0] \"ぜっと\"\n",
                "\n",
                "#. editor: open\n",
                "msgid \"b\"\n",
                "msgstr \"multi\\nline\\t\\\"x\\\"\\n\"\n",
                "\n",
                "msgctxt \"tab\"\n",
                "msgid \"Close\"\n",
                "msgstr \"閉じる\"\n",
                "\n",
                "# trailing\n",
            )
        );
        assert_eq!(
            format_document("again.po", document(&formatted)).expect("formats"),
            formatted
        );
    }

    #[test]
    fn formatting_refuses_duplicates_and_syntax_errors() {
        let text = format!("{HEADER}\nmsgid \"A\"\nmsgstr \"1\"\n\nmsgid \"A\"\nmsgstr \"2\"\n");
        let error = format_document("dup.po", document(&text)).expect_err("duplicate");
        assert!(
            error
                .to_string()
                .contains("dup.po:9: duplicate of the entry on line 6: \"A\""),
            "{error:#}"
        );
        assert!(read_document("broken.po", "msgid \"A\nmsgstr \"x\"\n").is_err());
    }

    #[test]
    fn union_merges_and_reports_conflicts() {
        let left = document(&format!(
            "{HEADER}\n#. left\nmsgid \"Shared\"\nmsgstr \"共有\"\n\nmsgid \"Left\"\nmsgstr \"左\"\n\nmsgid \"Filled\"\nmsgstr \"\"\n"
        ));
        let right = document(&format!(
            "{HEADER}\n#. right\nmsgid \"Shared\"\nmsgstr \"共有\"\n\nmsgid \"Right\"\nmsgstr \"右\"\n\nmsgid \"Filled\"\nmsgstr \"埋めた\"\n\nmsgctxt \"c\"\nmsgid \"Left\"\nmsgstr \"文脈\"\n"
        ));
        let merged = union(&[("left.po".into(), left.clone()), ("right.po".into(), right)])
            .expect("no conflicts");
        assert_eq!(
            render(&merged),
            format!(
                "{HEADER}\nmsgid \"Filled\"\nmsgstr \"埋めた\"\n\nmsgid \"Left\"\nmsgstr \"左\"\n\nmsgid \"Right\"\nmsgstr \"右\"\n\n#. left\n#. right\nmsgid \"Shared\"\nmsgstr \"共有\"\n\nmsgctxt \"c\"\nmsgid \"Left\"\nmsgstr \"文脈\"\n"
            )
        );

        let other = document(&format!(
            "{HEADER}\nmsgid \"Left\"\nmsgstr \"ひだり\"\n\n#, fuzzy\nmsgid \"Shared\"\nmsgstr \"共有\"\n"
        ));
        let conflicts =
            union(&[("left.po".into(), left), ("other.po".into(), other)]).expect_err("conflicts");
        let descriptions: Vec<&str> = conflicts
            .iter()
            .map(|conflict| conflict.description.as_str())
            .collect();
        assert_eq!(
            descriptions,
            vec![
                "\"Left\": left.po:10 has [\"左\"], other.po:6 has [\"ひだり\"]",
                "\"Shared\": left.po:7 has [\"共有\"], other.po:10 has [\"共有\"] (fuzzy)",
            ]
        );
    }

    #[test]
    fn union_of_one_file_merges_identical_duplicates() {
        let text = format!("{HEADER}\nmsgid \"A\"\nmsgstr \"1\"\n\nmsgid \"A\"\nmsgstr \"1\"\n");
        let merged = union(&[("dup.po".into(), document(&text))]).expect("identical");
        assert_eq!(merged.entries.len(), 1);
        let headerless = document("msgid \"B\"\nmsgstr \"2\"\n");
        let different_header =
            document("msgid \"\"\nmsgstr \"\"\n\"Language: fr\\n\"\n\nmsgid \"C\"\nmsgstr \"3\"\n");
        let merged = union(&[
            ("a.po".into(), document(&text)),
            ("b.po".into(), headerless),
        ])
        .expect("a file without a header takes the other's");
        assert!(merged.header.is_some());
        assert!(
            union(&[
                ("a.po".into(), document(&text)),
                ("c.po".into(), different_header)
            ])
            .is_err()
        );
    }
}
