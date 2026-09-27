//! Checks a translation against its template for mistakes that would make the
//! runtime fall back to English or show the wrong text.

use crate::plural::PluralRule;
use crate::po::{PoEntry, PoFile};
use std::collections::{BTreeSet, HashMap};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LintIssue {
    pub origin: String,
    /// The line in the translation, or in the template for `Untranslated`.
    pub line: usize,
    pub context: Option<String>,
    pub msgid: String,
    pub kind: LintKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LintKind {
    /// The translation (or plural form `form`) does not use the same placeholders as
    /// the msgid. Extra placeholders make the runtime show English; missing ones drop
    /// a value from the message.
    PlaceholderMismatch {
        form: Option<usize>,
        missing: Vec<String>,
        extra: Vec<String>,
    },
    /// The translation has a format spec such as `{x:.1}`, which the runtime cannot
    /// apply; specs belong only in the source code.
    FormatSpec {
        form: Option<usize>,
        placeholder: String,
    },
    /// A plural entry has a different number of forms than the header's `nplurals`.
    PluralFormCount {
        expected: usize,
        found: usize,
    },
    /// A plural msgid is translated without plural forms, or the other way round.
    PluralShape,
    /// The header has no usable `Plural-Forms` while the file has plural entries.
    BrokenPluralForms(String),
    Duplicate {
        first_line: usize,
    },
    /// The template no longer has this msgid.
    Obsolete,
    /// The template has this msgid, but the translation lacks it, leaves it empty or
    /// marks it fuzzy.
    Untranslated,
}

impl fmt::Display for LintIssue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}:{}: ", self.origin, self.line)?;
        match &self.context {
            Some(context) => write!(formatter, "[{context}] {:?}: ", self.msgid)?,
            None => write!(formatter, "{:?}: ", self.msgid)?,
        }
        let form = |form: &Option<usize>| match form {
            Some(index) => format!(" (msgstr[{index}])"),
            None => String::new(),
        };
        match &self.kind {
            LintKind::PlaceholderMismatch {
                form: index,
                missing,
                extra,
            } => write!(
                formatter,
                "placeholders differ{}: missing {missing:?}, extra {extra:?}",
                form(index)
            ),
            LintKind::FormatSpec {
                form: index,
                placeholder,
            } => write!(
                formatter,
                "format spec {placeholder} in the translation{}",
                form(index)
            ),
            LintKind::PluralFormCount { expected, found } => {
                write!(formatter, "{found} plural forms, nplurals is {expected}")
            }
            LintKind::PluralShape => write!(formatter, "plural and non-plural entries differ"),
            LintKind::BrokenPluralForms(message) => write!(formatter, "Plural-Forms: {message}"),
            LintKind::Duplicate { first_line } => {
                write!(formatter, "duplicate of the entry on line {first_line}")
            }
            LintKind::Obsolete => write!(formatter, "not in the template"),
            LintKind::Untranslated => write!(formatter, "untranslated"),
        }
    }
}

pub fn lint(template: &PoFile, translation: &PoFile) -> Vec<LintIssue> {
    let mut issues = Vec::new();
    let issue = |entry: &PoEntry, origin: &str, line: usize, kind: LintKind| LintIssue {
        origin: origin.to_string(),
        line,
        context: entry.context.clone(),
        msgid: entry.msgid.clone(),
        kind,
    };

    let has_plural_entries = translation
        .entries
        .iter()
        .any(|entry| entry.msgid_plural.is_some() && entry.is_translated());
    let plural_rule = match translation
        .header
        .plural_forms
        .as_deref()
        .map(PluralRule::parse)
    {
        Some(Ok(rule)) => Some(rule),
        Some(Err(error)) => {
            if let Some(entry) = translation.entries.first() {
                issues.push(issue(
                    entry,
                    &translation.origin,
                    0,
                    LintKind::BrokenPluralForms(format!("{error:#}")),
                ));
            }
            None
        }
        None => {
            if has_plural_entries && let Some(entry) = translation.entries.first() {
                issues.push(issue(
                    entry,
                    &translation.origin,
                    0,
                    LintKind::BrokenPluralForms("missing".to_string()),
                ));
            }
            None
        }
    };

    let template_entries: HashMap<(Option<&str>, &str), &PoEntry> = template
        .entries
        .iter()
        .map(|entry| ((entry.context.as_deref(), entry.msgid.as_str()), entry))
        .collect();
    let mut seen: HashMap<(Option<&str>, &str), usize> = HashMap::new();

    for entry in &translation.entries {
        let key = (entry.context.as_deref(), entry.msgid.as_str());
        if let Some(first_line) = seen.get(&key) {
            issues.push(issue(
                entry,
                &translation.origin,
                entry.line,
                LintKind::Duplicate {
                    first_line: *first_line,
                },
            ));
            continue;
        }
        seen.insert(key, entry.line);

        let Some(template_entry) = template_entries.get(&key) else {
            issues.push(issue(
                entry,
                &translation.origin,
                entry.line,
                LintKind::Obsolete,
            ));
            continue;
        };
        if !entry.is_translated() {
            continue;
        }
        if template_entry.msgid_plural.is_some() != entry.msgid_plural.is_some() {
            issues.push(issue(
                entry,
                &translation.origin,
                entry.line,
                LintKind::PluralShape,
            ));
            continue;
        }

        let is_plural = entry.msgid_plural.is_some();
        if is_plural && let Some(rule) = &plural_rule {
            if entry.msgstr.len() != rule.plural_count() {
                issues.push(issue(
                    entry,
                    &translation.origin,
                    entry.line,
                    LintKind::PluralFormCount {
                        expected: rule.plural_count(),
                        found: entry.msgstr.len(),
                    },
                ));
            }
        }

        let mut expected = Placeholders::of(&entry.msgid);
        if let Some(plural) = &entry.msgid_plural {
            expected.extend(Placeholders::of(plural));
        }
        for (index, form) in entry.msgstr.iter().enumerate() {
            if form.is_empty() {
                continue;
            }
            let form_index = is_plural.then_some(index);
            let found = Placeholders::of(form);
            for placeholder in &found.with_format_spec {
                issues.push(issue(
                    entry,
                    &translation.origin,
                    entry.line,
                    LintKind::FormatSpec {
                        form: form_index,
                        placeholder: placeholder.clone(),
                    },
                ));
            }
            let extra: Vec<String> = found.names.difference(&expected.names).cloned().collect();
            // Languages without a separate singular may drop `{count}` from one form,
            // so plural forms are only checked for placeholders that cannot be filled.
            let missing: Vec<String> = if is_plural {
                Vec::new()
            } else {
                expected.names.difference(&found.names).cloned().collect()
            };
            if !extra.is_empty() || !missing.is_empty() {
                issues.push(issue(
                    entry,
                    &translation.origin,
                    entry.line,
                    LintKind::PlaceholderMismatch {
                        form: form_index,
                        missing,
                        extra,
                    },
                ));
            }
        }
    }

    for entry in &template.entries {
        let key = (entry.context.as_deref(), entry.msgid.as_str());
        let translated = translation
            .entries
            .iter()
            .find(|candidate| (candidate.context.as_deref(), candidate.msgid.as_str()) == key)
            .is_some_and(PoEntry::is_translated);
        if !translated {
            issues.push(issue(
                entry,
                &template.origin,
                entry.line,
                LintKind::Untranslated,
            ));
        }
    }
    issues
}

/// The placeholders a template refers to. Positional ones are normalized to their
/// index (`{}` counts up from 0 in order of appearance) so that a translation may
/// reorder them as `{1} {0}`.
#[derive(Default)]
struct Placeholders {
    names: BTreeSet<String>,
    with_format_spec: Vec<String>,
}

impl Placeholders {
    fn of(text: &str) -> Self {
        let mut placeholders = Placeholders::default();
        let mut next_index = 0usize;
        let mut rest = text;
        while let Some(position) = rest.find(['{', '}']) {
            let tail = &rest[position..];
            if let Some(after) = tail.strip_prefix("{{").or_else(|| tail.strip_prefix("}}")) {
                rest = after;
                continue;
            }
            if let Some(after) = tail.strip_prefix('}') {
                rest = after;
                continue;
            }
            let Some(close) = tail.find('}') else {
                break;
            };
            let inner = &tail[1..close];
            let (key, spec) = match inner.split_once(':') {
                Some((key, spec)) => (key, Some(spec)),
                None => (inner, None),
            };
            let name = if key.is_empty() {
                let index = next_index;
                next_index += 1;
                index.to_string()
            } else {
                key.to_string()
            };
            if spec.is_some() {
                placeholders
                    .with_format_spec
                    .push(tail[..=close].to_string());
            }
            placeholders.names.insert(name);
            rest = &tail[close + 1..];
        }
        placeholders
    }

    fn extend(&mut self, other: Placeholders) {
        self.names.extend(other.names);
        self.with_format_spec.extend(other.with_format_spec);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::po::parse_po;

    fn file(origin: &str, text: &str) -> PoFile {
        let (file, errors) = parse_po(origin, text);
        assert!(errors.is_empty(), "{errors:?}");
        file
    }

    fn kinds(issues: &[LintIssue]) -> Vec<(String, LintKind)> {
        issues
            .iter()
            .map(|issue| (issue.msgid.clone(), issue.kind.clone()))
            .collect()
    }

    const TEMPLATE: &str = concat!(
        "msgid \"Open\"\nmsgstr \"\"\n\n",
        "msgid \"Select a {} for {}\"\nmsgstr \"\"\n\n",
        "msgid \"Open {path}\"\nmsgstr \"\"\n\n",
        "msgid \"{count} Comment\"\nmsgid_plural \"{count} Comments\"\nmsgstr[0] \"\"\nmsgstr[1] \"\"\n\n",
        "msgid \"Untouched\"\nmsgstr \"\"\n\n",
        "msgid \"Fuzzy\"\nmsgstr \"\"\n\n",
        "msgctxt \"menu\"\nmsgid \"Open\"\nmsgstr \"\"\n",
    );

    #[test]
    fn clean_translation_has_no_issues() {
        let translation = file(
            "ja.po",
            concat!(
                "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=1; plural=0;\\n\"\n\n",
                "msgid \"Open\"\nmsgstr \"開く\"\n\n",
                "msgid \"Select a {} for {}\"\nmsgstr \"{1} の {0} を選択\"\n\n",
                "msgid \"Open {path}\"\nmsgstr \"{path} を開く\"\n\n",
                "msgid \"{count} Comment\"\nmsgid_plural \"{count} Comments\"\nmsgstr[0] \"コメント {count} 件\"\n\n",
                "msgid \"Untouched\"\nmsgstr \"そのまま\"\n\n",
                "msgid \"Fuzzy\"\nmsgstr \"あいまい\"\n\n",
                "msgctxt \"menu\"\nmsgid \"Open\"\nmsgstr \"開く…\"\n",
            ),
        );
        let issues = lint(&file("template.pot", TEMPLATE), &translation);
        assert_eq!(kinds(&issues), Vec::new());
    }

    #[test]
    fn reports_every_kind_of_problem() {
        let translation = file(
            "ja.po",
            concat!(
                "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=(n != 1);\\n\"\n\n",
                "msgid \"Open\"\nmsgstr \"開く {extra}\"\n\n",
                "msgid \"Select a {} for {}\"\nmsgstr \"{0} を選択\"\n\n",
                "msgid \"Open {path}\"\nmsgstr \"{path:?} を開く\"\n\n",
                "msgid \"{count} Comment\"\nmsgid_plural \"{count} Comments\"\nmsgstr[0] \"コメント {count} 件\"\n\n",
                "msgid \"Open\"\nmsgstr \"もう一度\"\n\n",
                "msgid \"Gone\"\nmsgstr \"廃止\"\n\n",
                "#, fuzzy\nmsgid \"Fuzzy\"\nmsgstr \"あいまい\"\n\n",
                "msgctxt \"menu\"\nmsgid \"Open\"\nmsgid_plural \"Opens\"\nmsgstr[0] \"開く\"\nmsgstr[1] \"開く\"\n",
            ),
        );
        let issues = lint(&file("template.pot", TEMPLATE), &translation);
        assert_eq!(
            kinds(&issues),
            vec![
                (
                    "Open".to_string(),
                    LintKind::PlaceholderMismatch {
                        form: None,
                        missing: vec![],
                        extra: vec!["extra".to_string()],
                    }
                ),
                (
                    "Select a {} for {}".to_string(),
                    LintKind::PlaceholderMismatch {
                        form: None,
                        missing: vec!["1".to_string()],
                        extra: vec![],
                    }
                ),
                (
                    "Open {path}".to_string(),
                    LintKind::FormatSpec {
                        form: None,
                        placeholder: "{path:?}".to_string(),
                    }
                ),
                (
                    "{count} Comment".to_string(),
                    LintKind::PluralFormCount {
                        expected: 2,
                        found: 1,
                    }
                ),
                ("Open".to_string(), LintKind::Duplicate { first_line: 4 }),
                ("Gone".to_string(), LintKind::Obsolete),
                ("Open".to_string(), LintKind::PluralShape),
                ("Untouched".to_string(), LintKind::Untranslated),
                ("Fuzzy".to_string(), LintKind::Untranslated),
            ]
        );
        assert_eq!(issues[4].line, 17);
        assert_eq!(issues[6].context.as_deref(), Some("menu"));
        assert_eq!(
            issues[0].to_string(),
            "ja.po:4: \"Open\": placeholders differ: missing [], extra [\"extra\"]"
        );
    }

    #[test]
    fn plural_forms_may_drop_count_but_not_invent_placeholders() {
        let translation = file(
            "fr.po",
            concat!(
                "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=(n > 1);\\n\"\n\n",
                "msgid \"{count} Comment\"\nmsgid_plural \"{count} Comments\"\n",
                "msgstr[0] \"un commentaire\"\nmsgstr[1] \"{count} commentaires {other}\"\n",
            ),
        );
        let issues = lint(&file("template.pot", TEMPLATE), &translation);
        let placeholder_issues: Vec<_> = issues
            .iter()
            .filter(|issue| matches!(issue.kind, LintKind::PlaceholderMismatch { .. }))
            .map(|issue| issue.kind.clone())
            .collect();
        assert_eq!(
            placeholder_issues,
            vec![LintKind::PlaceholderMismatch {
                form: Some(1),
                missing: vec![],
                extra: vec!["other".to_string()],
            }]
        );
    }

    #[test]
    fn broken_plural_forms_header() {
        let translation = file(
            "x.po",
            "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=2; plural=n +;\\n\"\n\nmsgid \"Open\"\nmsgstr \"x\"\n",
        );
        let issues = lint(&file("template.pot", TEMPLATE), &translation);
        assert!(
            issues
                .iter()
                .any(|issue| matches!(issue.kind, LintKind::BrokenPluralForms(_)))
        );
    }

    #[test]
    fn escaped_braces_are_not_placeholders() {
        let placeholders = Placeholders::of("{{literal}} {} {name} {0}}} {");
        let names: Vec<_> = placeholders.names.into_iter().collect();
        assert_eq!(names, ["0", "name"]);
    }
}
