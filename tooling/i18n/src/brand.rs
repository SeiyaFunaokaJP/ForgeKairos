//! Puts the product's own name in place of upstream's in what the UI shows, without touching
//! upstream's English in the sources: the msgids stay as they are, `locales/en/_brand.po` gives
//! the English UI the renamed text, and the translations of every other language are rewritten.
//!
//! Not every "Zed" is the application. Zed's online services (Zed AI, Zed Pro, the Zed team),
//! statements about upstream (the official Zed, upstream Zed) and names that are stored or typed
//! (the base keymap "Zed") stay. `locales/_brand_keep.txt` lists them, each with its reason, and
//! everything else that names upstream's product is renamed. The rewrite is idempotent, so it is
//! run again after translations are added.

use crate::doc_catalogs::{ACTION_DOCS_STEM, english_original};
use crate::po_document::{DocumentEntry, PoDocument, entry_key, format_document, read_document};
use anyhow::{Context as _, Result, bail};
use i18n_catalog::PoEntry;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::fs;
use std::ops::Range;
use std::path::Path;

/// Relative to the repository.
pub const KEEP_LIST_PATH: &str = "locales/_brand_keep.txt";
/// Relative to the repository.
pub const BRAND_FILE_PATH: &str = "locales/en/_brand.po";
/// The language whose directory holds the brand file instead of translations.
pub const ENGLISH: &str = i18n_catalog::ENGLISH;

/// The name upstream's English uses for its application.
pub const UPSTREAM_NAME: &str = "Zed";

const BRAND_FILE_HEADER_COMMENTS: [&str; 4] = [
    "# The product's own name in place of upstream's, for the English UI and for messages a",
    "# translation lacks. Generated from the other languages' files and locales/_brand_keep.txt",
    "# by `cargo run -p i18n_tool -- brand --write`; edit those instead.",
    "#",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum KeepKind {
    /// Text that stays wherever it appears, in any language (`Zed Pro`, `Zed チーム`).
    Phrase,
    /// A message whose every mention of upstream's name stays.
    Message,
    /// A message whose English does not name the application but whose translations do (an
    /// action named `zed::About` that Japanese calls "Zed について"): the translations are renamed.
    Translation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reason {
    /// One of Zed's online services, its account or its plans.
    Service,
    /// A statement about upstream Zed, or a link to its own pages.
    Fact,
    /// A value that is stored or typed, which renaming would break.
    Identifier,
    /// The application itself (only for `Translation`).
    Application,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeepEntry {
    pub kind: KeepKind,
    pub reason: Reason,
    pub context: Option<String>,
    /// For `Phrase` the text itself; otherwise the msgid (an action name in `_action_docs.po`).
    pub text: String,
    pub line: usize,
}

pub struct KeepList {
    pub entries: Vec<KeepEntry>,
}

impl KeepList {
    pub fn parse(text: &str) -> Result<Self> {
        let mut entries = Vec::new();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            let [kind, reason, context, text] = fields[..] else {
                bail!(
                    "{KEEP_LIST_PATH}:{line_number}: expected four tab-separated fields (kind, reason, context, text)"
                );
            };
            let kind = match kind {
                "phrase" => KeepKind::Phrase,
                "message" => KeepKind::Message,
                "translation" => KeepKind::Translation,
                other => bail!("{KEEP_LIST_PATH}:{line_number}: unknown kind {other:?}"),
            };
            let reason = match reason {
                "service" => Reason::Service,
                "fact" => Reason::Fact,
                "identifier" => Reason::Identifier,
                "application" => Reason::Application,
                other => bail!("{KEEP_LIST_PATH}:{line_number}: unknown reason {other:?}"),
            };
            if (kind == KeepKind::Translation) != (reason == Reason::Application) {
                bail!(
                    "{KEEP_LIST_PATH}:{line_number}: `translation` lines, and only they, have the reason `application`"
                );
            }
            let context = match context {
                "-" => None,
                context => Some(unescape(context)),
            };
            if kind == KeepKind::Phrase && context.is_some() {
                bail!("{KEEP_LIST_PATH}:{line_number}: a phrase has no context (`-`)");
            }
            let text = unescape(text);
            if text.is_empty() {
                bail!("{KEEP_LIST_PATH}:{line_number}: empty text");
            }
            entries.push(KeepEntry {
                kind,
                reason,
                context,
                text,
                line: line_number,
            });
        }
        Ok(Self { entries })
    }

    fn phrases(&self) -> Vec<&str> {
        self.entries
            .iter()
            .filter(|entry| entry.kind == KeepKind::Phrase)
            .map(|entry| entry.text.as_str())
            .collect()
    }

    fn find(&self, kind: KeepKind, context: Option<&str>, msgid: &str) -> Option<&KeepEntry> {
        self.entries.iter().find(|entry| {
            entry.kind == kind && entry.context.as_deref() == context && entry.text == msgid
        })
    }
}

/// The PO escapes, so that a msgid with a line break or a tab fits on one line of the list.
fn unescape(text: &str) -> String {
    let mut unescaped = String::with_capacity(text.len());
    let mut characters = text.chars();
    while let Some(character) = characters.next() {
        if character != '\\' {
            unescaped.push(character);
            continue;
        }
        match characters.next() {
            Some('n') => unescaped.push('\n'),
            Some('t') => unescaped.push('\t'),
            Some('r') => unescaped.push('\r'),
            Some(other) => unescaped.push(other),
            None => unescaped.push('\\'),
        }
    }
    unescaped
}

/// Renames one name to another in text, leaving the protected phrases alone.
pub struct Renamer<'a> {
    pub from: &'a str,
    pub to: &'a str,
    pub phrases: Vec<&'a str>,
    /// Whether `from` joined to a word by a hyphen is a mention. Upstream's English and the
    /// sources write other names that way (`Zed-JP-Enhance`), while a translation may write
    /// the application's name into a compound (German `Zed-Einstellungen`).
    pub hyphen_compounds: bool,
}

fn is_name_character(character: char) -> bool {
    character.is_ascii_alphanumeric() || character == '_'
}

impl Renamer<'_> {
    /// Where `from` stands as a word of its own, protected phrases included. Unless
    /// `hyphen_compounds` is set, a hyphen joins words into one name (`Zed-Editor`), so a
    /// mention may not continue with one.
    fn words(&self, text: &str) -> Vec<Range<usize>> {
        text.match_indices(self.from)
            .map(|(start, matched)| start..start + matched.len())
            .filter(|range| {
                let before = text[..range.start].chars().next_back();
                let mut after = text[range.end..].chars();
                let next = after.next();
                let joined_by_hyphen = !self.hyphen_compounds
                    && next == Some('-')
                    && after.next().is_some_and(is_name_character);
                !before.is_some_and(is_name_character)
                    && !next.is_some_and(is_name_character)
                    && !joined_by_hyphen
            })
            .collect()
    }

    /// Where `from` stands as a word of its own outside every protected phrase.
    pub fn mentions(&self, text: &str) -> Vec<Range<usize>> {
        let protected: Vec<Range<usize>> = self
            .phrases
            .iter()
            .flat_map(|phrase| {
                text.match_indices(phrase)
                    .map(|(start, matched)| start..start + matched.len())
            })
            .collect();
        self.words(text)
            .into_iter()
            .filter(|range| {
                !protected
                    .iter()
                    .any(|phrase| phrase.start <= range.start && range.end <= phrase.end)
            })
            .collect()
    }

    /// How many of the mentions of `from` in `text` a protected phrase keeps.
    pub fn kept_count(&self, text: &str) -> usize {
        self.words(text)
            .len()
            .saturating_sub(self.mentions(text).len())
    }

    pub fn rename(&self, text: &str) -> String {
        let mut renamed = String::with_capacity(text.len());
        let mut copied = 0;
        for range in self.mentions(text) {
            renamed.push_str(&text[copied..range.start]);
            renamed.push_str(self.to);
            copied = range.end;
        }
        renamed.push_str(&text[copied..]);
        renamed
    }
}

/// A finding that makes `brand --check` (and `check`) fail.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum BrandFinding {
    /// The file differs from what `brand --write` writes.
    OutOfDate { file: String },
    /// A translation names upstream's product where its English does not, and the keep list
    /// does not say whether that is the application.
    Unclassified { file: String, message: String },
    /// The English keeps upstream's name in a phrase and renames it elsewhere, and the
    /// translation's own mentions do not show which of them is which.
    Ambiguous { file: String, message: String },
    /// A keep list entry that no message has (any more).
    UnusedKeep { line: usize, text: String },
}

impl std::fmt::Display for BrandFinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BrandFinding::OutOfDate { file } => write!(
                formatter,
                "{file} does not match the keep list (run `cargo run -p i18n_tool -- brand --write`)"
            ),
            BrandFinding::Unclassified { file, message } => write!(
                formatter,
                "{file}: {message}: the translation names {UPSTREAM_NAME} but the English does not; add a `translation` or `message` line to {KEEP_LIST_PATH}"
            ),
            BrandFinding::Ambiguous { file, message } => write!(
                formatter,
                "{file}: {message}: the English keeps {UPSTREAM_NAME} in a phrase and renames it elsewhere; add the translation's wording of the kept phrase as a `phrase` line to {KEEP_LIST_PATH}"
            ),
            BrandFinding::UnusedKeep { line, text } => write!(
                formatter,
                "{KEEP_LIST_PATH}:{line}: {text:?} is not in any message; remove the line"
            ),
        }
    }
}

#[derive(Default)]
pub struct BrandOutcome {
    /// Relative path → the text `brand --write` writes, for the files whose text changes.
    pub changes: BTreeMap<String, String>,
    pub findings: Vec<BrandFinding>,
    /// Messages renamed (in English, from their msgid), and messages kept by the keep list.
    pub renamed: usize,
    pub kept: usize,
}

struct LanguageDocument {
    path: String,
    stem: String,
    text: String,
    document: PoDocument,
}

fn read_languages(repository: &Path) -> Result<Vec<LanguageDocument>> {
    let locales = repository.join("locales");
    let mut languages = Vec::new();
    for entry in fs::read_dir(&locales).with_context(|| format!("reading {}", locales.display()))? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if entry.file_type()?.is_dir() && !name.eq_ignore_ascii_case(ENGLISH) {
            languages.push(name);
        }
    }
    languages.sort();
    let mut documents = Vec::new();
    for language in languages {
        let mut names = Vec::new();
        for entry in fs::read_dir(locales.join(&language))? {
            let name = entry?.file_name().to_string_lossy().into_owned();
            if name.ends_with(".po") {
                names.push(name);
            }
        }
        names.sort();
        for name in names {
            let path = format!("locales/{language}/{name}");
            let text = fs::read_to_string(repository.join(&path))
                .with_context(|| format!("reading {path}"))?;
            let document = read_document(&path, &text).map_err(|errors| {
                let messages: Vec<String> = errors.iter().map(ToString::to_string).collect();
                anyhow::anyhow!("{}", messages.join("\n"))
            })?;
            documents.push(LanguageDocument {
                stem: name.trim_end_matches(".po").to_string(),
                path,
                text,
                document,
            });
        }
    }
    Ok(documents)
}

/// The English an entry shows, singular and plural: its msgid, or for an action description
/// the doc comment it was translated from.
fn english_of(stem: &str, entry: &DocumentEntry) -> Option<(String, Option<String>)> {
    if stem == ACTION_DOCS_STEM {
        english_original(&entry.comments).map(|english| (english, None))
    } else {
        Some((entry.entry.msgid.clone(), entry.entry.msgid_plural.clone()))
    }
}

fn describe(context: Option<&str>, msgid: &str) -> String {
    match context {
        Some(context) => format!("[{context}] {msgid:?}"),
        None => format!("{msgid:?}"),
    }
}

/// Works out every file `brand --write` writes, from the files in `repository`.
pub fn plan(repository: &Path, product_name: &str) -> Result<BrandOutcome> {
    let keep_text = fs::read_to_string(repository.join(KEEP_LIST_PATH))
        .with_context(|| format!("reading {KEEP_LIST_PATH}"))?;
    let keep = KeepList::parse(&keep_text)?;
    let renamer = Renamer {
        from: UPSTREAM_NAME,
        to: product_name,
        phrases: keep.phrases(),
        hyphen_compounds: false,
    };
    let translation_renamer = Renamer {
        hyphen_compounds: true,
        phrases: keep.phrases(),
        ..renamer
    };
    let mut documents = read_languages(repository)?;
    let mut outcome = BrandOutcome::default();
    let mut used_keep_lines = BTreeSet::new();
    let mut brand_entries: BTreeMap<(Option<String>, String), PoEntry> = BTreeMap::new();

    for language_document in &mut documents {
        let stem = language_document.stem.clone();
        for document_entry in &mut language_document.document.entries {
            let english = english_of(&stem, document_entry);
            let entry = &mut document_entry.entry;
            let (context, msgid) = entry_key(entry);
            let (context, msgid) = (context.map(str::to_string), msgid.to_string());
            // An action description without the English it was translated from cannot be
            // classified, so a translation naming upstream's product there is reported
            // rather than skipped.
            let Some((english, english_plural)) = english else {
                if entry
                    .msgstr
                    .iter()
                    .any(|form| !translation_renamer.mentions(form).is_empty())
                {
                    outcome.findings.push(BrandFinding::Unclassified {
                        file: language_document.path.clone(),
                        message: describe(context.as_deref(), &msgid),
                    });
                }
                continue;
            };
            if let Some(kept) = keep.find(KeepKind::Message, context.as_deref(), &msgid) {
                used_keep_lines.insert(kept.line);
                outcome.kept += 1;
                continue;
            }
            let english_forms: Vec<&str> = std::iter::once(english.as_str())
                .chain(english_plural.as_deref())
                .collect();
            let names_the_application = english_forms
                .iter()
                .any(|form| !renamer.mentions(form).is_empty());
            // A phrase of the keep list is matched in the English, so that it holds for every
            // language without each of them listing its own wording of it.
            let keeps_upstream = english_forms
                .iter()
                .any(|form| renamer.kept_count(form) > 0);
            if names_the_application {
                let msgstr = english_forms
                    .iter()
                    .map(|form| renamer.rename(form))
                    .collect();
                brand_entries
                    .entry((context.clone(), msgid.clone()))
                    .or_insert_with(|| PoEntry {
                        context: context.clone(),
                        msgid: msgid.clone(),
                        msgid_plural: entry.msgid_plural.clone(),
                        msgstr,
                        fuzzy: false,
                        line: 0,
                    });
                // Which of the translation's mentions are the kept ones cannot be told from the
                // English, so the translation has to protect them with a phrase of its own,
                // which leaves as many mentions to rename as the English renames (counting
                // those an earlier run renamed already).
                let lines_up = !keeps_upstream
                    || entry.msgstr.iter().enumerate().all(|(index, form)| {
                        let english_form = english_forms
                            .get(index)
                            .or(english_forms.last())
                            .copied()
                            .unwrap_or_default();
                        translation_renamer.mentions(form).len()
                            + form.matches(product_name).count()
                            == renamer.mentions(english_form).len()
                    });
                if !lines_up {
                    outcome.findings.push(BrandFinding::Ambiguous {
                        file: language_document.path.clone(),
                        message: describe(context.as_deref(), &msgid),
                    });
                    continue;
                }
            } else if keeps_upstream {
                continue;
            } else if let Some(listed) =
                keep.find(KeepKind::Translation, context.as_deref(), &msgid)
            {
                used_keep_lines.insert(listed.line);
            } else {
                if entry
                    .msgstr
                    .iter()
                    .any(|form| !translation_renamer.mentions(form).is_empty())
                {
                    outcome.findings.push(BrandFinding::Unclassified {
                        file: language_document.path.clone(),
                        message: describe(context.as_deref(), &msgid),
                    });
                }
                continue;
            }
            for form in &mut entry.msgstr {
                *form = translation_renamer.rename(form);
            }
        }
    }

    let used_phrases: BTreeSet<&str> = renamer
        .phrases
        .iter()
        .copied()
        .filter(|phrase| {
            documents.iter().any(|document| {
                document.document.entries.iter().any(|entry| {
                    entry.entry.msgid.contains(phrase)
                        || entry.entry.msgstr.iter().any(|form| form.contains(phrase))
                        || entry
                            .comments
                            .iter()
                            .any(|comment| comment.contains(phrase))
                })
            })
        })
        .collect();

    for language_document in documents {
        let rendered = format_document(&language_document.path, language_document.document)?;
        if rendered != language_document.text {
            outcome.changes.insert(language_document.path, rendered);
        }
    }

    outcome.renamed = brand_entries.len();
    let brand_document = PoDocument {
        header_comments: BRAND_FILE_HEADER_COMMENTS
            .iter()
            .map(|line| line.to_string())
            .collect(),
        header: Some(vec![
            ("Language".to_string(), ENGLISH.to_string()),
            (
                "Content-Type".to_string(),
                "text/plain; charset=UTF-8".to_string(),
            ),
            (
                "Plural-Forms".to_string(),
                "nplurals=2; plural=(n != 1);".to_string(),
            ),
        ]),
        entries: brand_entries
            .into_values()
            .map(|entry| DocumentEntry {
                comments: Vec::new(),
                entry,
            })
            .collect(),
        trailing_comments: Vec::new(),
    };
    let brand_text = format_document(BRAND_FILE_PATH, brand_document)?;
    let current_brand_text = fs::read_to_string(repository.join(BRAND_FILE_PATH)).ok();
    if current_brand_text.as_deref() != Some(brand_text.as_str()) {
        outcome
            .changes
            .insert(BRAND_FILE_PATH.to_string(), brand_text);
    }

    for entry in &keep.entries {
        let used = match entry.kind {
            KeepKind::Phrase => used_phrases.contains(entry.text.as_str()),
            KeepKind::Message | KeepKind::Translation => used_keep_lines.contains(&entry.line),
        };
        if !used {
            outcome.findings.push(BrandFinding::UnusedKeep {
                line: entry.line,
                text: entry.text.clone(),
            });
        }
    }
    Ok(outcome)
}

/// Writes the planned files, or with `check_only` lists what is out of date; returns whether
/// everything was in order.
pub fn run(repository: &Path, product_name: &str, check_only: bool) -> Result<bool> {
    let mut outcome = plan(repository, product_name)?;
    let mut output = String::new();
    if check_only {
        for file in outcome.changes.keys() {
            outcome
                .findings
                .push(BrandFinding::OutOfDate { file: file.clone() });
        }
    } else {
        for (file, text) in &outcome.changes {
            let path = repository.join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, text).with_context(|| format!("writing {file}"))?;
            writeln!(output, "wrote {file}").ok();
        }
    }
    outcome.findings.sort();
    for finding in &outcome.findings {
        writeln!(output, "{finding}").ok();
    }
    writeln!(
        output,
        "{} messages name the product in English, {} keep upstream's name; {} problems",
        outcome.renamed,
        outcome.kept,
        outcome.findings.len()
    )
    .ok();
    print!("{output}");
    Ok(outcome.findings.is_empty())
}

/// Relative to the repository: the string literals outside tests that keep upstream's name.
pub const SOURCE_KEEP_LIST_PATH: &str = "tooling/i18n/brand_sources.txt";

/// The reasons `tooling/i18n/brand_sources.txt` gives, explained at its top.
const SOURCE_REASONS: [&str; 8] = [
    "service",
    "fact",
    "identifier",
    "internal",
    "log",
    "dev",
    "table",
    "pending",
];

/// `{error:#}` → `{error}`: the extraction keys a message by its msgid without format specs.
fn without_format_specs(text: &str) -> String {
    let mut stripped = String::with_capacity(text.len());
    let mut in_placeholder = false;
    let mut in_spec = false;
    for character in text.chars() {
        match character {
            '{' => {
                in_placeholder = !in_placeholder;
                in_spec = false;
            }
            '}' => {
                in_placeholder = false;
                in_spec = false;
            }
            ':' if in_placeholder => in_spec = true,
            _ => {}
        }
        if !in_spec || character == '}' {
            stripped.push(character);
        }
    }
    stripped
}

/// A string literal outside tests that names upstream's product and is not a message the
/// catalogs rename.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SourceMention {
    pub path: String,
    pub line: usize,
    pub value: String,
}

struct SourceKeepEntry {
    path: String,
    value: String,
    line: usize,
}

fn parse_source_keep_list(text: &str) -> Result<Vec<SourceKeepEntry>> {
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        let fields: Vec<&str> = line.split('\t').collect();
        let [path, reason, value] = fields[..] else {
            bail!(
                "{SOURCE_KEEP_LIST_PATH}:{line_number}: expected three tab-separated fields (path, reason, literal)"
            );
        };
        if !SOURCE_REASONS.contains(&reason) {
            bail!("{SOURCE_KEEP_LIST_PATH}:{line_number}: unknown reason {reason:?}");
        }
        entries.push(SourceKeepEntry {
            path: path.to_string(),
            value: unescape(value),
            line: line_number,
        });
    }
    Ok(entries)
}

/// Every literal outside tests in `crates/` that names upstream's application, leaving out
/// the kept phrases and the messages the catalogs rename: the msgids of the i18n macros, and
/// text that one of the `_` files has as a msgid (tables translated where they are drawn, such
/// as the menus and the settings window).
pub fn source_mentions(repository: &Path, crates: &[String]) -> Result<Vec<SourceMention>> {
    let keep_text = fs::read_to_string(repository.join(KEEP_LIST_PATH))
        .with_context(|| format!("reading {KEEP_LIST_PATH}"))?;
    let keep = KeepList::parse(&keep_text)?;
    let renamer = Renamer {
        from: UPSTREAM_NAME,
        to: UPSTREAM_NAME,
        phrases: keep.phrases(),
        hyphen_compounds: false,
    };
    let files = crate::sources::load_crates(repository, crates)?;
    let extraction = crate::sources::extract(&files);
    let documents = read_languages(repository)?;
    // `brand` renames only what the catalogs hold, so a message no language has yet (new
    // upstream text) still shows upstream's name and is reported like any other literal.
    let in_catalogs: BTreeSet<&str> = documents
        .iter()
        .flat_map(|document| &document.document.entries)
        .flat_map(|entry| {
            std::iter::once(entry.entry.msgid.as_str()).chain(entry.entry.msgid_plural.as_deref())
        })
        .collect();
    let mut messages: BTreeSet<(&str, &str)> = BTreeSet::new();
    for message in extraction.crates.values().flatten() {
        if !in_catalogs.contains(message.key.msgid.as_str()) {
            continue;
        }
        for location in &message.locations {
            messages.insert((location.path.as_str(), message.key.msgid.as_str()));
            if let Some(plural) = &message.msgid_plural {
                messages.insert((location.path.as_str(), plural.as_str()));
            }
        }
    }
    let translated: BTreeSet<&str> = documents
        .iter()
        .filter(|document| document.stem.starts_with('_'))
        .flat_map(|document| &document.document.entries)
        .map(|entry| entry.entry.msgid.as_str())
        .collect();
    let mut mentions: Vec<SourceMention> =
        crate::sources::literals_where(&files, |value| !renamer.mentions(value).is_empty())
            .into_iter()
            .filter(|literal| !literal.in_test)
            .filter(|literal| {
                !messages.contains(&(literal.path.as_str(), literal.value.as_str()))
                    && !messages.contains(&(
                        literal.path.as_str(),
                        without_format_specs(&literal.value).as_str(),
                    ))
            })
            // The bare name is also an identifier in many places (a provider, a keymap, a
            // client name), so a msgid "Zed" in one table must not hide it everywhere else.
            .filter(|literal| {
                literal.value == UPSTREAM_NAME || !translated.contains(literal.value.as_str())
            })
            .map(|literal| SourceMention {
                path: literal.path,
                line: literal.line,
                value: literal.value,
            })
            .collect();
    mentions.sort();
    Ok(mentions)
}

/// Compares the literals that name upstream's application with the list of those that keep
/// it; prints the ones the list lacks and the lines nothing matches, and returns whether there
/// were none.
pub fn check_sources(repository: &Path, crates: &[String], list: bool) -> Result<bool> {
    let mentions = source_mentions(repository, crates)?;
    let keep_text = fs::read_to_string(repository.join(SOURCE_KEEP_LIST_PATH))
        .with_context(|| format!("reading {SOURCE_KEEP_LIST_PATH}"))?;
    let keep = parse_source_keep_list(&keep_text)?;
    let in_scope = |path: &str| {
        crates.is_empty()
            || crates
                .iter()
                .any(|name| path.starts_with(&format!("crates/{name}/")))
    };
    let mut ok = true;
    for mention in &mentions {
        let kept = keep
            .iter()
            .any(|entry| entry.path == mention.path && entry.value == mention.value);
        if list || !kept {
            println!(
                "{}:{}: {}{:?}",
                mention.path,
                mention.line,
                if kept { "kept " } else { "" },
                mention.value
            );
        }
        ok &= kept;
    }
    for entry in keep.iter().filter(|entry| in_scope(&entry.path)) {
        let found = mentions
            .iter()
            .any(|mention| mention.path == entry.path && mention.value == entry.value);
        if !found {
            println!(
                "{SOURCE_KEEP_LIST_PATH}:{}: {} {:?} is no longer in the sources; remove the line",
                entry.line, entry.path, entry.value
            );
            ok = false;
        }
    }
    println!(
        "{} literals outside tests name {UPSTREAM_NAME}; {} of them are listed in {SOURCE_KEEP_LIST_PATH}",
        mentions.len(),
        mentions
            .iter()
            .filter(|mention| keep
                .iter()
                .any(|entry| entry.path == mention.path && entry.value == mention.value))
            .count()
    );
    Ok(ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn renamer<'a>(phrases: &[&'a str]) -> Renamer<'a> {
        Renamer {
            from: "Zed",
            to: "Forge",
            phrases: phrases.to_vec(),
            hyphen_compounds: false,
        }
    }

    #[test]
    fn only_the_name_as_a_word_outside_kept_phrases_is_renamed() {
        let renamer = renamer(&["Zed Pro", "公式版の Zed"]);
        assert_eq!(
            renamer.rename("About Zed. Zed's log, Zed!\nZed—Zed"),
            "About Forge. Forge's log, Forge!\nForge—Forge"
        );
        assert_eq!(
            renamer.rename("Zed の設定、Zed について"),
            "Forge の設定、Forge について"
        );
        assert_eq!(
            renamer.rename("Upgrade to Zed Pro in Zed"),
            "Upgrade to Zed Pro in Forge"
        );
        assert_eq!(
            renamer.rename("公式版の Zed とこの Zed"),
            "公式版の Zed とこの Forge"
        );
        for untouched in [
            "zed://agent",
            "ZedIndustries",
            "cloud.zed.dev",
            "OpenZedRepo",
            "Zed-JP-Enhance",
            "ZED_HOME",
            "_Zed",
        ] {
            assert_eq!(renamer.rename(untouched), untouched);
        }
        assert_eq!(renamer.rename("Zed-"), "Forge-");

        let in_translations = Renamer {
            phrases: Vec::new(),
            hyphen_compounds: true,
            ..renamer
        };
        assert_eq!(
            in_translations.rename("Zed-Einstellungen, Zed-Übersicht, Zed"),
            "Forge-Einstellungen, Forge-Übersicht, Forge"
        );
        assert_eq!(in_translations.rename("ZedIndustries"), "ZedIndustries");
    }

    /// Every language is renamed the same way, and a phrase the keep list gives in English
    /// keeps the name in every translation of the message without listing their wording.
    #[test]
    fn translations_in_any_language_follow_the_english_keep_list() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        let header =
            |language: &str| format!("msgid \"\"\nmsgstr \"\"\n\"Language: {language}\\n\"\n\n");
        write(root, KEEP_LIST_PATH, "phrase\tservice\t-\tZed team\n");
        for (language, restart, ask, mixed) in [
            (
                "de",
                "Zed-Einstellungen neu laden",
                "Das Zed-Team fragen",
                "Zed neu starten oder das Zed-Team fragen",
            ),
            (
                "es",
                "Reiniciar Zed",
                "Preguntar al equipo de Zed",
                "Reiniciar Zed o preguntar al equipo de Zed",
            ),
            (
                "ko",
                "Zed를 다시 시작",
                "Zed 팀에 문의",
                "Zed를 다시 시작하거나 Zed 팀에 문의",
            ),
            (
                "zh-Hans",
                "重新启动 Zed",
                "询问 Zed 团队",
                "重新启动 Zed 或询问 Zed 团队",
            ),
        ] {
            write(
                root,
                &format!("locales/{language}/a.po"),
                &format!(
                    "{}msgid \"Ask the Zed team\"\nmsgstr \"{ask}\"\n\nmsgid \"Reload Zed\"\nmsgstr \"{restart}\"\n\nmsgid \"Restart Zed or ask the Zed team\"\nmsgstr \"{mixed}\"\n",
                    header(language)
                ),
            );
        }

        let outcome = plan(root, "Forge").expect("plan");
        let mut ambiguous: Vec<&str> = outcome
            .findings
            .iter()
            .map(|finding| match finding {
                BrandFinding::Ambiguous { file, message } => {
                    assert_eq!(message, "\"Restart Zed or ask the Zed team\"");
                    file.as_str()
                }
                other => panic!("{other}"),
            })
            .collect();
        ambiguous.sort();
        assert_eq!(
            ambiguous,
            [
                "locales/de/a.po",
                "locales/es/a.po",
                "locales/ko/a.po",
                "locales/zh-Hans/a.po"
            ],
            "the translation has to say which mention is the kept one"
        );
        for (language, renamed, kept) in [
            ("de", "Forge-Einstellungen neu laden", "Das Zed-Team fragen"),
            ("es", "Reiniciar Forge", "Preguntar al equipo de Zed"),
            ("ko", "Forge를 다시 시작", "Zed 팀에 문의"),
            ("zh-Hans", "重新启动 Forge", "询问 Zed 团队"),
        ] {
            let text = &outcome.changes[&format!("locales/{language}/a.po")];
            assert!(text.contains(&format!("msgstr \"{renamed}\"")), "{text}");
            assert!(text.contains(&format!("msgstr \"{kept}\"")), "{text}");
        }

        write(
            root,
            KEEP_LIST_PATH,
            "phrase\tservice\t-\tZed team\nphrase\tservice\t-\tZed-Team\nphrase\tservice\t-\tequipo de Zed\nphrase\tservice\t-\tZed 팀\nphrase\tservice\t-\tZed 团队\n",
        );
        let outcome = plan(root, "Forge").expect("plan");
        assert_eq!(outcome.findings, Vec::new());
        for (language, mixed) in [
            ("de", "Forge neu starten oder das Zed-Team fragen"),
            ("es", "Reiniciar Forge o preguntar al equipo de Zed"),
            ("ko", "Forge를 다시 시작하거나 Zed 팀에 문의"),
            ("zh-Hans", "重新启动 Forge 或询问 Zed 团队"),
        ] {
            let text = &outcome.changes[&format!("locales/{language}/a.po")];
            assert!(text.contains(&format!("msgstr \"{mixed}\"")), "{text}");
        }
    }

    const HEADER: &str = "msgid \"\"\nmsgstr \"\"\n\"Language: ja\\n\"\n\n";

    fn write(root: &Path, path: &str, text: &str) {
        let path = root.join(path);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the directory");
        }
        fs::write(path, text).expect("write the file");
    }

    #[test]
    fn plan_renames_the_application_keeps_the_list_and_is_idempotent() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(
            root,
            KEEP_LIST_PATH,
            "# comment\nphrase\tservice\t-\tZed Pro\nmessage\tfact\tmenu\tZed Repository\ntranslation\tapplication\taction\tzed::Quit\n",
        );
        write(
            root,
            "locales/ja/a.po",
            &format!(
                "{HEADER}msgid \"Restart Zed\"\nmsgstr \"Zed を再起動\"\n\nmsgid \"Upgrade to Zed Pro\"\nmsgstr \"Zed Pro にアップグレード\"\n\nmsgid \"{{count}} Zed window\"\nmsgid_plural \"{{count}} Zed windows\"\nmsgstr[0] \"Zed のウィンドウ {{count}} 個\"\n"
            ),
        );
        write(
            root,
            "locales/ja/_actions.po",
            &format!("{HEADER}msgctxt \"action\"\nmsgid \"zed::Quit\"\nmsgstr \"Zed を終了\"\n"),
        );
        write(
            root,
            "locales/ja/_menus.po",
            &format!(
                "{HEADER}msgctxt \"menu\"\nmsgid \"Zed Repository\"\nmsgstr \"Zed のリポジトリ\"\n"
            ),
        );
        write(
            root,
            "locales/ja/_action_docs.po",
            &format!(
                "{HEADER}#. Shows information about Zed.\nmsgctxt \"action documentation\"\nmsgid \"zed::About\"\nmsgstr \"Zed についての情報を表示します。\"\n"
            ),
        );

        let outcome = plan(root, "Forge").expect("plan");
        assert_eq!(outcome.findings, Vec::new());
        assert_eq!((outcome.renamed, outcome.kept), (3, 1));
        let brand = &outcome.changes[BRAND_FILE_PATH];
        assert!(brand.starts_with("# The product's own name"), "{brand}");
        assert!(brand.contains("\"Language: en\\n\""), "{brand}");
        assert!(
            brand.contains("msgid \"Restart Zed\"\nmsgstr \"Restart Forge\"\n"),
            "{brand}"
        );
        assert!(
            brand.contains(
                "msgid \"{count} Zed window\"\nmsgid_plural \"{count} Zed windows\"\nmsgstr[0] \"{count} Forge window\"\nmsgstr[1] \"{count} Forge windows\"\n"
            ),
            "{brand}"
        );
        assert!(
            brand.contains(
                "msgctxt \"action documentation\"\nmsgid \"zed::About\"\nmsgstr \"Shows information about Forge.\"\n"
            ),
            "{brand}"
        );
        assert!(!brand.contains("Upgrade to Zed Pro"), "{brand}");
        assert!(!brand.contains("Zed Repository"), "{brand}");
        assert!(!brand.contains("zed::Quit"), "{brand}");

        let japanese = outcome.changes["locales/ja/a.po"].clone();
        assert!(japanese.contains("msgstr \"Forge を再起動\""), "{japanese}");
        assert!(
            japanese.contains("msgstr \"Zed Pro にアップグレード\""),
            "{japanese}"
        );
        assert!(
            japanese.contains("msgstr[0] \"Forge のウィンドウ {count} 個\""),
            "{japanese}"
        );
        assert!(outcome.changes["locales/ja/_actions.po"].contains("msgstr \"Forge を終了\""));
        assert!(
            outcome.changes["locales/ja/_action_docs.po"]
                .contains("#. Shows information about Zed.\n"),
            "the English the entry was translated from stays"
        );
        assert!(!outcome.changes.contains_key("locales/ja/_menus.po"));

        for (path, text) in &outcome.changes {
            write(root, path, text);
        }
        let again = plan(root, "Forge").expect("plan again");
        assert_eq!(
            again.changes,
            BTreeMap::new(),
            "a second run changes nothing"
        );
        assert_eq!(again.findings, Vec::new());

        write(
            root,
            "locales/ja/a.po",
            &japanese.replace("Forge を再起動", "Zed を再起動"),
        );
        let reverted = plan(root, "Forge").expect("plan");
        assert_eq!(
            reverted.changes.keys().collect::<Vec<_>>(),
            ["locales/ja/a.po"],
            "a translation that names upstream's application again is out of date"
        );
    }

    #[test]
    fn unclassified_translations_and_unused_keep_lines_are_problems() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(
            root,
            KEEP_LIST_PATH,
            "phrase\tservice\t-\tZed VIP\nmessage\tservice\t-\tGone message about Zed\n",
        );
        write(
            root,
            "locales/ja/a.po",
            &format!("{HEADER}msgid \"Coming from Vim?\"\nmsgstr \"Zed の Vim モード\"\n"),
        );
        write(
            root,
            "locales/ja/_action_docs.po",
            &format!(
                "{HEADER}msgctxt \"action documentation\"\nmsgid \"zed::Hide\"\nmsgstr \"Zed を隠します。\"\n"
            ),
        );
        let outcome = plan(root, "Forge").expect("plan");
        assert_eq!(
            outcome.findings,
            vec![
                BrandFinding::Unclassified {
                    file: "locales/ja/_action_docs.po".to_string(),
                    message: "[action documentation] \"zed::Hide\"".to_string(),
                },
                BrandFinding::Unclassified {
                    file: "locales/ja/a.po".to_string(),
                    message: "\"Coming from Vim?\"".to_string(),
                },
                BrandFinding::UnusedKeep {
                    line: 1,
                    text: "Zed VIP".to_string(),
                },
                BrandFinding::UnusedKeep {
                    line: 2,
                    text: "Gone message about Zed".to_string(),
                },
            ]
        );
    }

    #[test]
    fn keep_lists_with_mistakes_are_rejected() {
        for broken in [
            "phrase\tservice\tZed Pro\n",
            "phrases\tservice\t-\tZed Pro\n",
            "phrase\tbecause\t-\tZed Pro\n",
            "phrase\tservice\tmenu\tZed Pro\n",
            "message\tapplication\t-\tAbout Zed\n",
            "translation\tservice\taction\tzed::Quit\n",
            "message\tservice\t-\t\n",
        ] {
            assert!(KeepList::parse(broken).is_err(), "{broken:?}");
        }
        let list =
            KeepList::parse("message\tidentifier\tsettings schema\tDefault: Zed\\n\\\"x\\\"\n")
                .expect("parse");
        assert_eq!(list.entries[0].text, "Default: Zed\n\"x\"");
        assert_eq!(list.entries[0].context.as_deref(), Some("settings schema"));
    }

    #[test]
    fn literals_the_catalogs_do_not_rename_are_reported() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(root, KEEP_LIST_PATH, "phrase\tservice\t-\tZed Pro\n");
        write(
            root,
            "locales/ja/a.po",
            &format!("{HEADER}msgid \"Restart Zed\"\nmsgstr \"Zed を再起動\"\n"),
        );
        write(
            root,
            "locales/ja/_menus.po",
            &format!("{HEADER}msgctxt \"menu\"\nmsgid \"Zed\"\nmsgstr \"Zed\"\n"),
        );
        write(
            root,
            "crates/demo/src/demo.rs",
            "fn shown() {\n    i18n::t!(\"Restart Zed\");\n    i18n::t!(\"Quit Zed\");\n    provider(\"Zed\");\n    plan(\"Upgrade to Zed Pro\");\n}\n",
        );
        let mentions = source_mentions(root, &[]).expect("scan");
        let values: Vec<&str> = mentions
            .iter()
            .map(|mention| mention.value.as_str())
            .collect();
        assert_eq!(
            values,
            ["Quit Zed", "Zed"],
            "a message no translation has yet is not renamed, and the bare name is an identifier \
             even when a menu translates it"
        );
    }

    #[test]
    fn format_specs_are_left_out_like_the_extraction_does() {
        assert_eq!(
            without_format_specs("reconnect: {error:#} {count:>4} {{literal:x}} {}"),
            "reconnect: {error} {count} {{literal:x}} {}"
        );
    }

    /// Every literal outside tests that still says "Zed" is listed with its reason, and
    /// every listed one is still in the sources.
    #[test]
    fn every_literal_naming_upstream_is_listed() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        assert!(
            check_sources(&repository, &[], false).expect("scan"),
            "see the output above; run `cargo run -p i18n_tool -- scan --brand`"
        );
    }

    /// The repository's translations and `locales/en/_brand.po` are what `brand --write` makes
    /// of them, so no message shows upstream's name for the application.
    #[test]
    fn the_repository_is_branded() {
        let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let outcome = plan(&repository, branding::CURRENT.product_name).expect("plan");
        assert_eq!(
            outcome.changes.keys().collect::<Vec<_>>(),
            Vec::<&String>::new(),
            "run `cargo run -p i18n_tool -- brand --write`"
        );
        assert_eq!(outcome.findings, Vec::new());
        assert!(outcome.renamed > 0);
    }
}
