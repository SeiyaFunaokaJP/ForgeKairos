//! Compares the messages the sources mark with the PO files of every language.

use crate::po_document::{compare_entries, describe_key, entry_key};
use crate::sources::{Extraction, Message};
use anyhow::{Context as _, Result};
use i18n_catalog::{LintKind, PoEntry, PoError, PoFile, lint, parse_po};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::path::Path;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Finding {
    /// (a) The source uses the message, the crate's PO file lacks it (or leaves it empty or fuzzy).
    Untranslated {
        file: String,
        message: String,
        location: String,
        translated_in: Vec<String>,
    },
    /// (b) The PO file has a message its crate no longer uses.
    Obsolete {
        file: String,
        line: usize,
        message: String,
    },
    /// (c) Placeholders, format specs and plural forms, as `i18n_catalog::lint` reports them.
    Lint(String),
    /// (d) Two files of a language translate the same key differently.
    Disagreement {
        message: String,
        translations: Vec<String>,
    },
    /// (e) An entry sorts before the one above it.
    Order {
        file: String,
        line: usize,
        message: String,
    },
    /// (f) The file has CR line endings.
    CarriageReturn {
        file: String,
        lines: usize,
    },
    Syntax(String),
}

impl Finding {
    fn fails(&self, strict: bool) -> bool {
        match self {
            Finding::Untranslated { .. } | Finding::Obsolete { .. } => strict,
            _ => true,
        }
    }
}

pub struct CheckOptions<'a> {
    pub languages: &'a [String],
    pub crates: &'a [String],
}

struct LanguageFile {
    /// Relative to the repository, like `locales/ja/editor.po`.
    path: String,
    /// The file name without `.po`.
    stem: String,
    text: String,
    file: PoFile,
    errors: Vec<PoError>,
}

pub fn check(
    repository: &Path,
    extraction: &Extraction,
    options: &CheckOptions,
) -> Result<BTreeMap<String, Vec<Finding>>> {
    let locales = repository.join("locales");
    let mut languages = Vec::new();
    for entry in fs::read_dir(&locales).with_context(|| format!("reading {}", locales.display()))? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            languages.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    languages.sort();
    for requested in options.languages {
        if !languages.contains(requested) {
            anyhow::bail!("there is no directory locales/{requested}");
        }
    }
    let mut results = BTreeMap::new();
    for language in languages {
        if !options.languages.is_empty() && !options.languages.contains(&language) {
            continue;
        }
        let files = read_language(repository, &language)?;
        results.insert(
            language.clone(),
            check_language(&language, &files, extraction, options.crates),
        );
    }
    Ok(results)
}

fn read_language(repository: &Path, language: &str) -> Result<Vec<LanguageFile>> {
    let directory = repository.join("locales").join(language);
    let mut files = Vec::new();
    for entry in fs::read_dir(&directory)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(stem) = name.strip_suffix(".po") else {
            continue;
        };
        let path = format!("locales/{language}/{name}");
        let bytes = fs::read(entry.path()).with_context(|| format!("reading {path}"))?;
        let text = String::from_utf8(bytes).with_context(|| format!("{path} is not UTF-8"))?;
        let (file, errors) = parse_po(&path, &text);
        files.push(LanguageFile {
            path,
            stem: stem.to_string(),
            text,
            file,
            errors,
        });
    }
    files.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(files)
}

fn template(origin: &str, entries: impl IntoIterator<Item = PoEntry>) -> PoFile {
    PoFile {
        origin: origin.to_string(),
        entries: entries.into_iter().collect(),
        ..PoFile::default()
    }
}

fn message_entry(message: &Message) -> PoEntry {
    PoEntry {
        context: message.key.context.clone(),
        msgid: message.key.msgid.clone(),
        msgid_plural: message.msgid_plural.clone(),
        msgstr: Vec::new(),
        fuzzy: false,
        line: message
            .locations
            .first()
            .map_or(0, |location| location.line),
    }
}

/// Files whose entries are fixtures that the catalog's own tests look up by key, not messages of
/// the crate's sources.
const FIXTURE_FILES: [&str; 1] = ["i18n_catalog"];

/// Messages under the contexts of the menus, the settings table and the command palette go into
/// `_menus.po`, `_settings.po` and `_actions.po`, not into the crate's file.
pub fn is_area_message(message: &Message) -> bool {
    message.key.context.as_deref().is_some_and(|context| {
        [
            i18n::context::ACTION,
            i18n::context::MENU,
            i18n::context::SETTINGS,
        ]
        .contains(&context)
    })
}

fn first_production_location(message: &Message) -> String {
    message
        .locations
        .iter()
        .find(|location| !location.in_test)
        .map(|location| format!("{}:{}", location.path, location.line))
        .unwrap_or_default()
}

fn check_language(
    language: &str,
    files: &[LanguageFile],
    extraction: &Extraction,
    crates: &[String],
) -> Vec<Finding> {
    let selected = |stem: &str| {
        if crates.is_empty() {
            true
        } else {
            crates.iter().any(|name| name == stem)
        }
    };
    let mut findings = Vec::new();

    for language_file in files.iter().filter(|file| selected(&file.stem)) {
        let LanguageFile {
            path, text, file, ..
        } = language_file;
        findings.extend(
            language_file
                .errors
                .iter()
                .map(|error| Finding::Syntax(error.to_string())),
        );
        let carriage_returns = text.matches('\r').count();
        if carriage_returns > 0 {
            findings.push(Finding::CarriageReturn {
                file: path.clone(),
                lines: carriage_returns,
            });
        }
        for pair in file.entries.windows(2) {
            if compare_entries(&pair[0], &pair[1]).is_gt() {
                findings.push(Finding::Order {
                    file: path.clone(),
                    line: pair[1].line,
                    message: describe_key(&pair[1]),
                });
            }
        }

        // Every entry checked against itself: placeholders, specs, plural counts, duplicates.
        let itself = template(path, file.entries.iter().cloned());
        for issue in lint(&itself, file) {
            if matches!(
                issue.kind,
                LintKind::PlaceholderMismatch { .. }
                    | LintKind::FormatSpec { .. }
                    | LintKind::PluralFormCount { .. }
                    | LintKind::BrokenPluralForms(_)
                    | LintKind::Duplicate { .. }
            ) {
                findings.push(Finding::Lint(issue.to_string()));
            }
        }
    }

    // (a), (b): a crate's file against the messages of that crate.
    let mut stems: BTreeSet<&str> = files
        .iter()
        .map(|file| file.stem.as_str())
        .filter(|stem| !stem.starts_with('_'))
        .collect();
    stems.extend(extraction.crates.keys().map(String::as_str));
    for stem in stems {
        if !selected(stem) || FIXTURE_FILES.contains(&stem) {
            continue;
        }
        let (area_messages, messages): (Vec<&Message>, Vec<&Message>) = extraction
            .crates
            .get(stem)
            .map(|messages| {
                messages
                    .iter()
                    .partition(|message| is_area_message(message))
            })
            .unwrap_or_default();
        for message in area_messages {
            if message.only_in_tests() {
                continue;
            }
            let key = (message.key.context.as_deref(), message.key.msgid.as_str());
            let translated = files
                .iter()
                .filter(|file| file.stem.starts_with('_'))
                .flat_map(|file| &file.file.entries)
                .any(|entry| entry_key(entry) == key && entry.is_translated());
            if !translated {
                findings.push(Finding::Untranslated {
                    file: format!(
                        "locales/{language}/{}",
                        match message.key.context.as_deref() {
                            Some(i18n::context::ACTION) => "_actions.po",
                            Some(i18n::context::MENU) => "_menus.po",
                            _ => "_settings.po",
                        }
                    ),
                    message: format!(
                        "[{}] {:?}",
                        message.key.context.as_deref().unwrap_or_default(),
                        message.key.msgid
                    ),
                    location: first_production_location(message),
                    translated_in: Vec::new(),
                });
            }
        }
        let language_file = files.iter().find(|file| file.stem == stem);
        let path = language_file.map_or_else(
            || format!("locales/{language}/{stem}.po (missing)"),
            |file| file.path.clone(),
        );
        let empty = PoFile::default();
        let translation = language_file.map_or(&empty, |file| &file.file);
        let source = template(
            &format!("crates/{stem}"),
            messages.iter().copied().map(message_entry),
        );
        let by_key: BTreeMap<(Option<&str>, &str), &Message> = messages
            .iter()
            .copied()
            .map(|message| {
                (
                    (message.key.context.as_deref(), message.key.msgid.as_str()),
                    message,
                )
            })
            .collect();
        for issue in lint(&source, translation) {
            let key = (issue.context.as_deref(), issue.msgid.as_str());
            let described = match &issue.context {
                Some(context) => format!("[{context}] {:?}", issue.msgid),
                None => format!("{:?}", issue.msgid),
            };
            match issue.kind {
                LintKind::Untranslated => {
                    let Some(message) = by_key.get(&key) else {
                        continue;
                    };
                    // Tests run in English, so a message only tests use needs no translation.
                    if message.only_in_tests() {
                        continue;
                    }
                    let location = first_production_location(message);
                    let translated_in = files
                        .iter()
                        .filter(|other| other.stem != stem)
                        .filter(|other| {
                            other
                                .file
                                .entries
                                .iter()
                                .any(|entry| entry_key(entry) == key && entry.is_translated())
                        })
                        .map(|other| other.path.clone())
                        .collect();
                    findings.push(Finding::Untranslated {
                        file: path.clone(),
                        message: described,
                        location,
                        translated_in,
                    });
                }
                LintKind::Obsolete => findings.push(Finding::Obsolete {
                    file: path.clone(),
                    line: issue.line,
                    message: described,
                }),
                LintKind::PluralShape => findings.push(Finding::Lint(format!(
                    "{}:{}: {described}: the source and the translation differ in plural forms",
                    path, issue.line
                ))),
                _ => {}
            }
        }
    }

    // (d): every file of a language goes into one catalog.
    let mut translations: BTreeMap<(Option<&str>, &str), Vec<(&str, &PoEntry)>> = BTreeMap::new();
    for language_file in files {
        for entry in &language_file.file.entries {
            if entry.is_translated() {
                translations
                    .entry(entry_key(entry))
                    .or_default()
                    .push((language_file.path.as_str(), entry));
            }
        }
    }
    for found in translations.values() {
        let Some((_, first)) = found.first() else {
            continue;
        };
        let disagrees = found.iter().any(|(_, entry)| entry.msgstr != first.msgstr);
        let involves_selection = found.iter().any(|(path, _)| {
            let stem = path
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".po"))
                .unwrap_or_default();
            selected(stem)
        });
        if disagrees && involves_selection {
            findings.push(Finding::Disagreement {
                message: describe_key(first),
                translations: found
                    .iter()
                    .map(|(path, entry)| format!("{path}:{} {:?}", entry.line, entry.msgstr))
                    .collect(),
            });
        }
    }
    findings.sort();
    findings
}

/// The report and whether the check failed.
pub fn report(results: &BTreeMap<String, Vec<Finding>>, strict: bool) -> (String, bool) {
    let mut output = String::new();
    let mut failed = false;
    for (language, findings) in results {
        failed |= findings.iter().any(|finding| finding.fails(strict));
        let count =
            |predicate: fn(&Finding) -> bool| findings.iter().filter(|f| predicate(f)).count();
        let untranslated = count(|finding| matches!(finding, Finding::Untranslated { .. }));
        let obsolete = count(|finding| matches!(finding, Finding::Obsolete { .. }));
        let lint_count = count(|finding| matches!(finding, Finding::Lint(_)));
        let disagreements = count(|finding| matches!(finding, Finding::Disagreement { .. }));
        let order = count(|finding| matches!(finding, Finding::Order { .. }));
        let carriage_returns = count(|finding| matches!(finding, Finding::CarriageReturn { .. }));
        let syntax = count(|finding| matches!(finding, Finding::Syntax(_)));
        writeln!(output, "== locales/{language} ==").ok();
        for finding in findings {
            let line = match finding {
                Finding::Untranslated {
                    file,
                    message,
                    location,
                    translated_in,
                } => {
                    let elsewhere = if translated_in.is_empty() {
                        String::new()
                    } else {
                        format!(" (translated in {})", translated_in.join(", "))
                    };
                    format!("(a) untranslated {file}: {message} used at {location}{elsewhere}")
                }
                Finding::Obsolete {
                    file,
                    line,
                    message,
                } => format!("(b) obsolete {file}:{line}: {message}"),
                Finding::Lint(text) => format!("(c) {text}"),
                Finding::Disagreement {
                    message,
                    translations,
                } => format!(
                    "(d) translated differently: {message}\n      {}",
                    translations.join("\n      ")
                ),
                Finding::Order {
                    file,
                    line,
                    message,
                } => format!("(e) out of order {file}:{line}: {message} (run po-format)"),
                Finding::CarriageReturn { file, lines } => {
                    format!("(f) {file} has {lines} CR characters (use LF line endings)")
                }
                Finding::Syntax(text) => format!("syntax error {text}"),
            };
            writeln!(output, "  {line}").ok();
        }
        writeln!(
            output,
            "  summary: (a) untranslated {untranslated}, (b) obsolete {obsolete}, (c) lint {lint_count}, (d) disagreements {disagreements}, (e) order {order}, (f) CR {carriage_returns}, syntax errors {syntax}"
        )
        .ok();
    }
    (output, failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use std::path::PathBuf;

    const HEADER: &str = "msgid \"\"\nmsgstr \"\"\n\"Language: ja\\n\"\n\"Plural-Forms: nplurals=1; plural=0;\\n\"\n\n";

    fn write(root: &Path, relative: &str, text: &str) {
        let path: PathBuf = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create directory");
        }
        fs::write(path, text).expect("write");
    }

    fn repository() -> tempfile::TempDir {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        write(
            root,
            "crates/alpha/src/alpha.rs",
            r#"
fn ui(count: usize) {
    i18n::t!("Open {path}", path = p);
    i18n::t!("Save");
    i18n::t!("Missing");
    i18n::tn!(count, "{count} file", "{count} files");
}
#[cfg(test)]
mod tests {
    fn f() { i18n::t!("Test only"); i18n::t!("Kept for tests"); }
}
"#,
        );
        write(
            root,
            "crates/beta/src/beta.rs",
            "fn ui() { i18n::t!(\"Save\"); i18n::tc!(\"menu\", \"Only here {x}\", x = 1); i18n::tc!(\"settings\", \"Nowhere\"); }",
        );
        write(
            root,
            "locales/ja/i18n_catalog.po",
            &format!("{HEADER}msgid \"Fixture\"\nmsgstr \"試験用\"\n"),
        );
        write(
            root,
            "locales/ja/alpha.po",
            &format!(
                "{HEADER}msgid \"Open {{path}}\"\nmsgstr \"{{name}} を開く\"\n\nmsgid \"Kept for tests\"\nmsgstr \"試験\"\n\nmsgid \"Gone\"\nmsgstr \"廃止\"\n\nmsgid \"Save\"\nmsgstr \"保存\"\n\nmsgid \"{{count}} file\"\nmsgstr \"{{count}} 個\"\n"
            ),
        );
        write(
            root,
            "locales/ja/beta.po",
            &format!("{HEADER}msgid \"Save\"\r\nmsgstr \"保存する\"\r\n"),
        );
        write(
            root,
            "locales/ja/_menus.po",
            &format!("{HEADER}msgctxt \"menu\"\nmsgid \"Only here {{x}}\"\nmsgstr \"ここだけ\"\n"),
        );
        directory
    }

    fn run(root: &Path, crates: &[String], strict: bool) -> (String, bool) {
        let files = crate::sources::load_crates(root, crates).expect("load");
        let extraction = crate::sources::extract(&files);
        let results = check(
            root,
            &extraction,
            &CheckOptions {
                languages: &[],
                crates,
            },
        )
        .expect("check");
        report(&results, strict)
    }

    #[test]
    fn reports_each_kind_of_finding() {
        let directory = repository();
        let (output, failed) = run(directory.path(), &[], false);
        assert!(failed);
        assert_eq!(
            output,
            [
                "== locales/ja ==",
                "  (a) untranslated locales/ja/_settings.po: [settings] \"Nowhere\" used at crates/beta/src/beta.rs:1",
                "  (a) untranslated locales/ja/alpha.po: \"Missing\" used at crates/alpha/src/alpha.rs:5",
                "  (b) obsolete locales/ja/alpha.po:12: \"Gone\"",
                "  (c) locales/ja/_menus.po:6: [menu] \"Only here {x}\": placeholders differ: missing [\"x\"], extra []",
                "  (c) locales/ja/alpha.po:18: \"{count} file\": the source and the translation differ in plural forms",
                "  (c) locales/ja/alpha.po:6: \"Open {path}\": placeholders differ: missing [\"path\"], extra [\"name\"]",
                "  (d) translated differently: \"Save\"",
                "      locales/ja/alpha.po:15 [\"保存\"]",
                "      locales/ja/beta.po:6 [\"保存する\"]",
                "  (e) out of order locales/ja/alpha.po:9: \"Kept for tests\" (run po-format)",
                "  (e) out of order locales/ja/alpha.po:12: \"Gone\" (run po-format)",
                "  (f) locales/ja/beta.po has 2 CR characters (use LF line endings)",
                "  summary: (a) untranslated 2, (b) obsolete 1, (c) lint 3, (d) disagreements 1, (e) order 2, (f) CR 1, syntax errors 0",
                "",
            ]
            .join("\n")
        );
    }

    #[test]
    fn crate_filter_and_strictness() {
        let directory = repository();
        let root = directory.path();
        write(
            root,
            "locales/ja/alpha.po",
            &format!(
                "{HEADER}msgid \"Gone\"\nmsgstr \"廃止\"\n\nmsgid \"Open {{path}}\"\nmsgstr \"{{path}} を開く\"\n\nmsgid \"Save\"\nmsgstr \"保存する\"\n\nmsgid \"{{count}} file\"\nmsgid_plural \"{{count}} files\"\nmsgstr[0] \"{{count}} 個\"\n"
            ),
        );
        let alpha = vec!["alpha".to_string()];
        let (output, failed) = run(root, &alpha, false);
        assert!(!failed, "{output}");
        assert!(
            output.contains("(a) untranslated 1, (b) obsolete 1, (c) lint 0"),
            "{output}"
        );
        assert!(!output.contains("beta.po has"), "{output}");
        assert!(!output.contains("_menus.po"), "{output}");
        let (_, failed) = run(root, &alpha, true);
        assert!(failed);
    }
}
