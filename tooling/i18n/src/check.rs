//! Compares the messages the sources mark with the PO files of every language.

use crate::doc_catalogs::{self, ACTION_DOCS_STEM, SETTINGS_SCHEMA_STEM};
use crate::po_document::{compare_entries, describe_key, entry_key, read_document};
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
    /// (b') The English a `_action_docs.po` or `_settings_schema.po` entry translates has changed.
    SourceChanged {
        file: String,
        line: usize,
        message: String,
        now: String,
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
    /// An untranslated message fails without `--strict`, so that a check of one crate can
    /// gate its changes; an entry nothing uses any more shows nothing wrong on screen.
    fn fails(&self, strict: bool) -> bool {
        match self {
            Finding::Obsolete { .. } | Finding::SourceChanged { .. } => strict,
            _ => true,
        }
    }
}

pub struct CheckOptions<'a> {
    pub languages: &'a [String],
    pub crates: &'a [String],
    /// Action name → doc comment, to compare `_action_docs.po` with.
    pub action_documentation: Option<&'a BTreeMap<String, String>>,
    /// The descriptions of the settings schema, to compare `_settings_schema.po` with.
    pub settings_schema_descriptions: Option<&'a BTreeSet<String>>,
    /// The descriptions of the actions' arguments, which `_settings_schema.po` may also
    /// translate (for the keymap file) without them being in the settings schema.
    pub action_argument_descriptions: Option<&'a BTreeSet<String>>,
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
        let name = entry.file_name().to_string_lossy().into_owned();
        // English holds no translation, only the product name that `brand` writes and checks.
        if entry.file_type()?.is_dir() && !name.eq_ignore_ascii_case(crate::brand::ENGLISH) {
            languages.push(name);
        }
    }
    languages.sort();
    // `--lang zh_hans` names `locales/zh-Hans` as a language tag does, regardless of case.
    let is_requested = |language: &str, requested: &str| {
        i18n_catalog::normalize_tag(requested).eq_ignore_ascii_case(language)
    };
    for requested in options.languages {
        if !languages
            .iter()
            .any(|language| is_requested(language, requested))
        {
            anyhow::bail!("there is no directory locales/{requested}");
        }
    }
    let mut results = BTreeMap::new();
    for language in languages {
        if !options.languages.is_empty()
            && !options
                .languages
                .iter()
                .any(|requested| is_requested(&language, requested))
        {
            continue;
        }
        let files = read_language(repository, &language)?;
        results.insert(
            language.clone(),
            check_language(&language, &files, extraction, options),
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

/// Messages under the contexts of the menus, the settings table, the command palette, the action
/// descriptions and the settings schema go into `_menus.po`, `_settings.po`, `_actions.po`,
/// `_action_docs.po` and `_settings_schema.po`, not into the crate's file.
pub fn is_area_message(message: &Message) -> bool {
    message.key.context.as_deref().is_some_and(|context| {
        [
            i18n::context::ACTION,
            i18n::context::MENU,
            i18n::context::SETTINGS,
            i18n::context::ACTION_DOCUMENTATION,
            i18n::context::SETTINGS_SCHEMA,
        ]
        .contains(&context)
    })
}

/// Files of plain text that Zed shows as it is: braces in them are not placeholders.
fn is_unformatted(stem: &str) -> bool {
    [ACTION_DOCS_STEM, SETTINGS_SCHEMA_STEM].contains(&stem)
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
    options: &CheckOptions,
) -> Vec<Finding> {
    let crates = options.crates;
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
            path,
            text,
            file,
            stem,
            ..
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
            let is_placeholder_issue = matches!(
                issue.kind,
                LintKind::PlaceholderMismatch { .. } | LintKind::FormatSpec { .. }
            );
            if is_placeholder_issue && is_unformatted(stem) {
                continue;
            }
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
                            Some(i18n::context::ACTION_DOCUMENTATION) => "_action_docs.po",
                            Some(i18n::context::SETTINGS_SCHEMA) => "_settings_schema.po",
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

    if let Some(action_documentation) = options.action_documentation
        && selected(ACTION_DOCS_STEM)
    {
        findings.extend(check_action_docs(language, files, action_documentation));
    }
    if let Some(descriptions) = options.settings_schema_descriptions
        && selected(SETTINGS_SCHEMA_STEM)
    {
        findings.extend(check_settings_schema(
            language,
            files,
            descriptions,
            options.action_argument_descriptions,
        ));
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
    findings.extend(cancel_collisions(files, selected));
    findings.sort();
    findings
}

const AREA_CONTEXTS: [&str; 5] = [
    i18n::context::ACTION,
    i18n::context::MENU,
    i18n::context::SETTINGS,
    i18n::context::ACTION_DOCUMENTATION,
    i18n::context::SETTINGS_SCHEMA,
];

/// (c) A dialog takes an answer whose label is the translation of "Cancel" (without a
/// msgctxt) for its cancel button, which Escape and the close box answer with, so no
/// other message may be translated the same: in a dialog that offers it, Escape would
/// pick it. The names the command palette, the menus and the settings show are never
/// dialog answers.
fn cancel_collisions(files: &[LanguageFile], selected: impl Fn(&str) -> bool) -> Vec<Finding> {
    let Some(cancel) = files
        .iter()
        .flat_map(|file| &file.file.entries)
        .find(|entry| entry.context.is_none() && entry.msgid == "Cancel" && entry.is_translated())
        .and_then(|entry| entry.msgstr.first())
    else {
        return Vec::new();
    };
    let cancel = cancel.to_lowercase();
    let mut findings = Vec::new();
    for language_file in files.iter().filter(|file| selected(&file.stem)) {
        for entry in &language_file.file.entries {
            let is_area = entry
                .context
                .as_deref()
                .is_some_and(|context| AREA_CONTEXTS.contains(&context));
            if entry.msgid == "Cancel" || is_area || !entry.is_translated() {
                continue;
            }
            if entry
                .msgstr
                .iter()
                .any(|form| form.to_lowercase() == cancel)
            {
                findings.push(Finding::Lint(format!(
                    "{}:{}: {}: translated as the translation of \"Cancel\", so Escape would answer a dialog with it",
                    language_file.path,
                    entry.line,
                    describe_key(entry)
                )));
            }
        }
    }
    findings
}

fn doc_catalog<'a>(files: &'a [LanguageFile], stem: &str) -> Option<&'a LanguageFile> {
    files.iter().find(|file| file.stem == stem)
}

/// (a), (b) and (b') for `_action_docs.po`: actions without a description, descriptions of
/// actions that are gone or have none now, and descriptions whose doc comment has changed since
/// they were translated (the English is kept in the entry's `#.` comments).
fn check_action_docs(
    language: &str,
    files: &[LanguageFile],
    action_documentation: &BTreeMap<String, String>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let language_file = doc_catalog(files, ACTION_DOCS_STEM);
    let path = language_file.map_or_else(
        || format!("locales/{language}/{ACTION_DOCS_STEM}.po (missing)"),
        |file| file.path.clone(),
    );
    let document = language_file.and_then(|file| read_document(&file.path, &file.text).ok());
    let mut translated = BTreeSet::new();
    for document_entry in document.iter().flat_map(|document| &document.entries) {
        let entry = &document_entry.entry;
        if entry.context.as_deref() != Some(i18n::context::ACTION_DOCUMENTATION) {
            continue;
        }
        let message = describe_key(entry);
        match action_documentation.get(&entry.msgid) {
            None => findings.push(Finding::Obsolete {
                file: path.clone(),
                line: entry.line,
                message,
            }),
            Some(current) => {
                if entry.is_translated() {
                    translated.insert(entry.msgid.as_str());
                }
                if doc_catalogs::english_original(&document_entry.comments).as_ref()
                    != Some(current)
                {
                    findings.push(Finding::SourceChanged {
                        file: path.clone(),
                        line: entry.line,
                        message,
                        now: current.clone(),
                    });
                }
            }
        }
    }
    for action_name in action_documentation.keys() {
        if !translated.contains(action_name.as_str()) {
            findings.push(Finding::Untranslated {
                file: path.clone(),
                message: format!("[{}] {action_name:?}", i18n::context::ACTION_DOCUMENTATION),
                location: "zed --dump-all-actions".to_string(),
                translated_in: Vec::new(),
            });
        }
    }
    findings
}

/// (a), (b) and (b') for `_settings_schema.po`, whose msgids are the English descriptions: a
/// translated description the schema no longer has is reported as changed when a description
/// the file lacks shares most of its words, and as obsolete otherwise. An entry for an action's
/// argument is in use, but only the settings schema's descriptions are required.
fn check_settings_schema(
    language: &str,
    files: &[LanguageFile],
    descriptions: &BTreeSet<String>,
    action_argument_descriptions: Option<&BTreeSet<String>>,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let language_file = doc_catalog(files, SETTINGS_SCHEMA_STEM);
    let path = language_file.map_or_else(
        || format!("locales/{language}/{SETTINGS_SCHEMA_STEM}.po (missing)"),
        |file| file.path.clone(),
    );
    let entries: Vec<&PoEntry> = language_file
        .map(|file| {
            file.file
                .entries
                .iter()
                .filter(|entry| entry.context.as_deref() == Some(i18n::context::SETTINGS_SCHEMA))
                .collect()
        })
        .unwrap_or_default();
    let translated: BTreeSet<&str> = entries
        .iter()
        .filter(|entry| entry.is_translated())
        .map(|entry| entry.msgid.as_str())
        .collect();
    let untranslated: Vec<&String> = descriptions
        .iter()
        .filter(|description| !translated.contains(description.as_str()))
        .collect();
    for entry in &entries {
        if descriptions.contains(&entry.msgid)
            || action_argument_descriptions
                .is_some_and(|arguments| arguments.contains(&entry.msgid))
        {
            continue;
        }
        let message = describe_key(entry);
        match doc_catalogs::closest_replacement(&entry.msgid, untranslated.iter().copied()) {
            Some(now) => findings.push(Finding::SourceChanged {
                file: path.clone(),
                line: entry.line,
                message,
                now: now.clone(),
            }),
            None => findings.push(Finding::Obsolete {
                file: path.clone(),
                line: entry.line,
                message,
            }),
        }
    }
    for description in untranslated {
        findings.push(Finding::Untranslated {
            file: path.clone(),
            message: format!("[{}] {description:?}", i18n::context::SETTINGS_SCHEMA),
            location: "the settings schema".to_string(),
            translated_in: Vec::new(),
        });
    }
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
        let changed = count(|finding| matches!(finding, Finding::SourceChanged { .. }));
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
                Finding::SourceChanged {
                    file,
                    line,
                    message,
                    now,
                } => format!("(b') source changed {file}:{line}: {message}, now {now:?}"),
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
            "  summary: (a) untranslated {untranslated}, (b) obsolete {obsolete}, (b') source changed {changed}, (c) lint {lint_count}, (d) disagreements {disagreements}, (e) order {order}, (f) CR {carriage_returns}, syntax errors {syntax}"
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
                action_documentation: None,
                settings_schema_descriptions: None,
                action_argument_descriptions: None,
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
                "  summary: (a) untranslated 2, (b) obsolete 1, (b') source changed 0, (c) lint 3, (d) disagreements 1, (e) order 2, (f) CR 1, syntax errors 0",
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
        assert!(
            failed,
            "an untranslated message fails without --strict: {output}"
        );
        assert!(
            output
                .contains("(a) untranslated 1, (b) obsolete 1, (b') source changed 0, (c) lint 0"),
            "{output}"
        );
        assert!(!output.contains("beta.po has"), "{output}");
        assert!(!output.contains("_menus.po"), "{output}");
        let (_, failed) = run(root, &alpha, true);
        assert!(failed);

        write(
            root,
            "crates/alpha/src/alpha.rs",
            "fn ui(count: usize) { i18n::t!(\"Open {path}\", path = p); i18n::t!(\"Save\"); i18n::tn!(count, \"{count} file\", \"{count} files\"); }",
        );
        let (output, failed) = run(root, &alpha, false);
        assert!(
            !failed,
            "an obsolete entry fails only with --strict: {output}"
        );
        assert!(
            output.contains("(a) untranslated 0, (b) obsolete 1"),
            "{output}"
        );
        let (_, failed) = run(root, &alpha, true);
        assert!(failed);
    }

    #[test]
    fn action_docs_and_settings_schema_against_what_zed_describes() {
        let directory = repository();
        let root = directory.path();
        write(
            root,
            "locales/ja/_action_docs.po",
            &format!(
                "{HEADER}#. Copy the selection.\nmsgctxt \"action documentation\"\nmsgid \"editor::Copy\"\nmsgstr \"選択範囲をコピーします。\"\n\n#. Paste.\nmsgctxt \"action documentation\"\nmsgid \"editor::Paste\"\nmsgstr \"{{ と }} の間に貼り付けます。\"\n\n#. Gone.\nmsgctxt \"action documentation\"\nmsgid \"editor::Removed\"\nmsgstr \"削除しました。\"\n"
            ),
        );
        write(
            root,
            "locales/ja/_settings_schema.po",
            &format!(
                "{HEADER}msgctxt \"settings schema\"\nmsgid \"The SHA.\"\nmsgstr \"SHA です。\"\n\nmsgctxt \"settings schema\"\nmsgid \"The tab size.\"\nmsgstr \"タブの幅。\"\n\nmsgctxt \"settings schema\"\nmsgid \"Unrelated old text.\"\nmsgstr \"古い文。\"\n\nmsgctxt \"settings schema\"\nmsgid \"Whether to show the tab bar.\"\nmsgstr \"タブバーを表示するかどうか。\"\n"
            ),
        );
        let action_documentation = BTreeMap::from([
            (
                "editor::Copy".to_string(),
                "Copy the selection.".to_string(),
            ),
            (
                "editor::Paste".to_string(),
                "Paste between {\nand }.".to_string(),
            ),
            ("editor::Cut".to_string(), "Cut the selection.".to_string()),
        ]);
        let descriptions = BTreeSet::from([
            "The tab size.".to_string(),
            "Whether to show the tab bar in the editor.".to_string(),
            "Brand new.".to_string(),
        ]);
        let action_arguments =
            BTreeSet::from(["The tab size.".to_string(), "The SHA.".to_string()]);
        let files = crate::sources::load_crates(root, &[]).expect("load");
        let extraction = crate::sources::extract(&files);
        let results = check(
            root,
            &extraction,
            &CheckOptions {
                languages: &[],
                crates: &[],
                action_documentation: Some(&action_documentation),
                settings_schema_descriptions: Some(&descriptions),
                action_argument_descriptions: Some(&action_arguments),
            },
        )
        .expect("check");
        let (output, _) = report(&results, false);
        let lines: Vec<&str> = output
            .lines()
            .filter(|line| line.contains("_action_docs") || line.contains("_settings_schema"))
            .collect();
        assert_eq!(
            lines,
            vec![
                "  (a) untranslated locales/ja/_action_docs.po: [action documentation] \"editor::Cut\" used at zed --dump-all-actions",
                "  (a) untranslated locales/ja/_settings_schema.po: [settings schema] \"Brand new.\" used at the settings schema",
                "  (a) untranslated locales/ja/_settings_schema.po: [settings schema] \"Whether to show the tab bar in the editor.\" used at the settings schema",
                "  (b) obsolete locales/ja/_action_docs.po:17: [action documentation] \"editor::Removed\"",
                "  (b) obsolete locales/ja/_settings_schema.po:14: [settings schema] \"Unrelated old text.\"",
                "  (b') source changed locales/ja/_action_docs.po:12: [action documentation] \"editor::Paste\", now \"Paste between {\\nand }.\"",
                "  (b') source changed locales/ja/_settings_schema.po:18: [settings schema] \"Whether to show the tab bar.\", now \"Whether to show the tab bar in the editor.\"",
            ]
        );
        let (_, failed_strictly) = report(&results, true);
        assert!(failed_strictly);
        let only_doc_findings = results.values().flatten().all(
            |finding| !matches!(finding, Finding::Lint(text) if text.contains("_action_docs")),
        );
        assert!(only_doc_findings, "{output}");
    }

    /// Escape answers a dialog with the button labelled as the translation of "Cancel", so
    /// no other answer may share that label, in any language.
    #[test]
    fn a_translation_equal_to_cancel_is_reported_in_every_language() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let root = directory.path();
        write(
            root,
            "crates/alpha/src/alpha.rs",
            "fn ui() { i18n::t!(\"Cancel\"); i18n::t!(\"Abort\"); i18n::t!(\"Discard\"); i18n::tc!(\"bookmark\", \"Cancel\"); }",
        );
        let header = |language: &str| {
            format!(
                "msgid \"\"\nmsgstr \"\"\n\"Language: {language}\n\"\n\"Plural-Forms: nplurals=2; plural=(n != 1);\n\"\n\n"
            )
        };
        write(
            root,
            "locales/de/alpha.po",
            &format!(
                "{}msgid \"Abort\"\nmsgstr \"Abbrechen\"\n\nmsgid \"Cancel\"\nmsgstr \"Abbrechen\"\n\nmsgid \"Discard\"\nmsgstr \"Verwerfen\"\n\nmsgctxt \"bookmark\"\nmsgid \"Cancel\"\nmsgstr \"Verwerfen\"\n",
                header("de")
            ),
        );
        write(
            root,
            "locales/zh-Hans/alpha.po",
            &format!(
                "{}msgid \"Abort\"\nmsgstr \"中止\"\n\nmsgid \"Cancel\"\nmsgstr \"取消\"\n\nmsgid \"Discard\"\nmsgstr \"放弃\"\n\nmsgctxt \"bookmark\"\nmsgid \"Cancel\"\nmsgstr \"取消\"\n",
                header("zh-Hans")
            ),
        );
        write(
            root,
            "locales/zh-Hans/_actions.po",
            &format!(
                "{}msgctxt \"action\"\nmsgid \"editor::Cancel\"\nmsgstr \"取消\"\n",
                header("zh-Hans")
            ),
        );
        let files = crate::sources::load_crates(root, &[]).expect("load");
        let extraction = crate::sources::extract(&files);
        let check_languages = |languages: &[String]| {
            check(
                root,
                &extraction,
                &CheckOptions {
                    languages,
                    crates: &[],
                    action_documentation: None,
                    settings_schema_descriptions: None,
                    action_argument_descriptions: None,
                },
            )
            .expect("check")
        };
        let results = check_languages(&[]);
        let collisions = |language: &str| -> Vec<String> {
            results[language]
                .iter()
                .filter_map(|finding| match finding {
                    Finding::Lint(text) if text.contains("translation of \"Cancel\"") => {
                        Some(text.clone())
                    }
                    _ => None,
                })
                .collect()
        };
        let german = collisions("de");
        assert_eq!(german.len(), 1, "{german:?}");
        assert!(
            german[0].starts_with("locales/de/alpha.po:") && german[0].contains(": \"Abort\": "),
            "{german:?}"
        );
        assert_eq!(collisions("zh-Hans"), Vec::<String>::new());

        let only_chinese = check_languages(&["ZH_hans".to_string()]);
        assert_eq!(
            only_chinese.keys().collect::<Vec<_>>(),
            ["zh-Hans"],
            "--lang names a directory as a language tag does"
        );
    }
}
