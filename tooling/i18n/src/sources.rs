//! Reads the Rust sources of a working tree's `crates/` for `scan` (Japanese literals left in the
//! code) and `extract` (the messages marked with the `i18n` macros).

use crate::restore_pairs::{SourceFile, contains_japanese};
use anyhow::{Context as _, Result};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

pub struct RustFile {
    /// Relative to the repository, with `/` separators.
    pub path: String,
    pub crate_name: String,
    pub source: SourceFile,
    whole_file_is_test: bool,
    test_ranges: Vec<(usize, usize)>,
}

impl RustFile {
    pub fn is_test_token(&self, index: usize) -> bool {
        self.whole_file_is_test
            || self
                .test_ranges
                .iter()
                .any(|(low, high)| (*low..=*high).contains(&index))
    }
}

/// Every `.rs` file under `crates/<name>/` for the given crates (all crates when `crates` is
/// empty), with test code marked.
pub fn load_crates(repository: &Path, crates: &[String]) -> Result<Vec<RustFile>> {
    let crates_directory = repository.join("crates");
    let mut crate_names = Vec::new();
    for entry in fs::read_dir(&crates_directory)
        .with_context(|| format!("reading {}", crates_directory.display()))?
    {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if crates.is_empty() || crates.contains(&name) {
            crate_names.push(name);
        }
    }
    crate_names.sort();
    for requested in crates {
        if !crate_names.contains(requested) {
            anyhow::bail!("there is no crate directory crates/{requested}");
        }
    }
    let mut files = Vec::new();
    for crate_name in crate_names {
        files.extend(load_crate(repository, &crate_name)?);
    }
    Ok(files)
}

fn load_crate(repository: &Path, crate_name: &str) -> Result<Vec<RustFile>> {
    let crate_prefix = format!("crates/{crate_name}");
    let mut paths = Vec::new();
    collect_rust_files(repository, &crate_prefix, &mut paths)?;
    paths.sort();
    let roots = crate_roots(repository, &crate_prefix, &paths);
    let mut files = Vec::new();
    for path in paths {
        let text = fs::read(repository.join(&path))
            .with_context(|| format!("reading {path}"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())?;
        let source = SourceFile::parse(&text);
        let test_ranges = source.test_ranges();
        let whole_file_is_test = crate::is_test_path(&path, &HashSet::new());
        files.push(RustFile {
            path,
            crate_name: crate_name.to_string(),
            source,
            whole_file_is_test,
            test_ranges,
        });
    }
    mark_out_of_line_test_modules(&mut files, &roots);
    Ok(files)
}

fn collect_rust_files(repository: &Path, relative: &str, output: &mut Vec<String>) -> Result<()> {
    let directory = repository.join(relative);
    for entry in
        fs::read_dir(&directory).with_context(|| format!("reading {}", directory.display()))?
    {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = format!("{relative}/{name}");
        let file_type = entry.file_type()?;
        if file_type.is_dir() {
            if name.starts_with('.') || name == "target" {
                continue;
            }
            collect_rust_files(repository, &path, output)?;
        } else if file_type.is_file() && name.ends_with(".rs") {
            output.push(path);
        }
    }
    Ok(())
}

/// Files whose `mod name;` declarations resolve next to themselves rather than in a directory
/// named after them.
fn crate_roots(repository: &Path, crate_prefix: &str, paths: &[String]) -> HashSet<String> {
    let mut roots: HashSet<String> = paths
        .iter()
        .filter(|path| {
            let relative = path
                .strip_prefix(crate_prefix)
                .and_then(|rest| rest.strip_prefix('/'))
                .unwrap_or(path);
            let depth = relative.matches('/').count();
            matches!(relative, "src/lib.rs" | "src/main.rs" | "build.rs")
                || relative.starts_with("src/bin/") && depth == 2
                || ["tests/", "examples/", "benches/"]
                    .iter()
                    .any(|directory| relative.starts_with(directory) && depth == 1)
        })
        .cloned()
        .collect();
    let manifest = repository.join(crate_prefix).join("Cargo.toml");
    if let Ok(manifest) = fs::read_to_string(&manifest) {
        for line in manifest.lines() {
            let line = line.trim();
            let Some(value) = line
                .strip_prefix("path")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix('='))
            else {
                continue;
            };
            let value = value.trim().trim_matches('"');
            if value.ends_with(".rs") {
                roots.insert(format!("{crate_prefix}/{value}"));
            }
        }
    }
    roots
}

/// A file declared with `#[cfg(test)] mod name;`, or declared anywhere inside test code, is
/// compiled only for tests, and so is everything it declares in turn.
fn mark_out_of_line_test_modules(files: &mut [RustFile], roots: &HashSet<String>) {
    let index_by_path: BTreeMap<String, usize> = files
        .iter()
        .enumerate()
        .map(|(index, file)| (file.path.clone(), index))
        .collect();
    loop {
        let mut newly_test = BTreeSet::new();
        for file in files.iter() {
            for (module_index, name) in module_declarations(&file.source) {
                if !file.is_test_token(module_index) {
                    continue;
                }
                for candidate in
                    module_file_candidates(&file.path, &name, roots.contains(&file.path))
                {
                    if let Some(index) = index_by_path.get(&candidate)
                        && !files[*index].whole_file_is_test
                    {
                        newly_test.insert(*index);
                    }
                }
            }
        }
        if newly_test.is_empty() {
            return;
        }
        for index in newly_test {
            files[index].whole_file_is_test = true;
        }
    }
}

/// `mod name;` items: the index of the `mod` token and the module name.
fn module_declarations(source: &SourceFile) -> Vec<(usize, String)> {
    let mut declarations = Vec::new();
    for index in 0..source.code.len() {
        if source.is_ident(index)
            && source.token_text(index) == "mod"
            && source.is_ident(index + 1)
            && source.is_punct(index + 2, ";")
        {
            declarations.push((index, source.token_text(index + 1).to_string()));
        }
    }
    declarations
}

fn module_file_candidates(declaring_path: &str, module: &str, is_root: bool) -> Vec<String> {
    let (directory, file_name) = declaring_path
        .rsplit_once('/')
        .unwrap_or(("", declaring_path));
    let stem = file_name.strip_suffix(".rs").unwrap_or(file_name);
    let module_directory = if is_root || file_name == "mod.rs" {
        directory.to_string()
    } else {
        format!("{directory}/{stem}")
    };
    vec![
        format!("{module_directory}/{module}.rs"),
        format!("{module_directory}/{module}/mod.rs"),
    ]
}

pub struct JapaneseLiteral {
    pub crate_name: String,
    pub path: String,
    pub line: usize,
    pub value: String,
    pub in_test: bool,
}

/// Kana and Han characters (Chinese uses them too), Hangul, and also the CJK punctuation and
/// full-width forms that UI text uses without them: `"、"` joining a list, `"「{}」"` quoting a
/// name, `"{reason}。"`. Languages written in the Latin script cannot be told from English this
/// way.
fn contains_japanese_text(text: &str) -> bool {
    contains_japanese(text)
        || text.chars().any(|character| {
            matches!(character as u32,
                0x3000..=0x303F // CJK symbols and punctuation
                | 0xFF01..=0xFF65 // Full-width forms and half-width CJK punctuation
                | 0xFFE0..=0xFFEE
                | 0x1100..=0x11FF // Hangul jamo
                | 0x3130..=0x318F // Hangul compatibility jamo
                | 0xAC00..=0xD7AF // Hangul syllables
            )
        })
}

pub fn japanese_literals(files: &[RustFile]) -> Vec<JapaneseLiteral> {
    literals_where(files, contains_japanese_text)
}

/// The string literals of `files` (outside comments) whose text satisfies `predicate`.
pub fn literals_where(
    files: &[RustFile],
    predicate: impl Fn(&str) -> bool,
) -> Vec<JapaneseLiteral> {
    let mut literals = Vec::new();
    for file in files {
        for index in 0..file.source.code.len() {
            let Some(literal) = file.source.literal(index) else {
                continue;
            };
            if !predicate(&literal.value) {
                continue;
            }
            literals.push(JapaneseLiteral {
                crate_name: file.crate_name.clone(),
                path: file.path.clone(),
                line: file.source.line_of_token(index),
                value: literal.value.clone(),
                in_test: file.is_test_token(index),
            });
        }
    }
    literals
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MacroKind {
    Translate,
    TranslateWithContext,
    TranslatePlural,
    TranslateWithContextPlural,
    Mark,
    /// `i18n::tr("…")`, which looks up its argument unchanged.
    Lookup,
    /// `i18n::tr_ctx("context", "…")` and `i18n::tr_ctx_opt`.
    LookupWithContext,
}

const MACROS: [(&str, MacroKind); 5] = [
    ("t", MacroKind::Translate),
    ("tc", MacroKind::TranslateWithContext),
    ("tn", MacroKind::TranslatePlural),
    ("tcn", MacroKind::TranslateWithContextPlural),
    ("msg", MacroKind::Mark),
];

/// The lookup functions, whose calls with string literals are messages like the macros'. Calls
/// with anything else look up text known only at run time and are not reported.
const FUNCTIONS: [(&str, MacroKind); 3] = [
    ("tr", MacroKind::Lookup),
    ("tr_ctx", MacroKind::LookupWithContext),
    ("tr_ctx_opt", MacroKind::LookupWithContext),
];

impl MacroKind {
    /// Argument positions of the context, the msgid and the plural msgid.
    fn positions(self) -> (Option<usize>, usize, Option<usize>) {
        match self {
            MacroKind::Translate | MacroKind::Mark | MacroKind::Lookup => (None, 0, None),
            MacroKind::TranslateWithContext | MacroKind::LookupWithContext => (Some(0), 1, None),
            MacroKind::TranslatePlural => (None, 1, Some(2)),
            MacroKind::TranslateWithContextPlural => (Some(0), 2, Some(3)),
        }
    }

    fn is_function(self) -> bool {
        matches!(self, MacroKind::Lookup | MacroKind::LookupWithContext)
    }

    /// Whether the msgid is the literal as written rather than a `format!` template.
    fn looks_up_unchanged(self) -> bool {
        matches!(
            self,
            MacroKind::Mark | MacroKind::Lookup | MacroKind::LookupWithContext
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MessageKey {
    pub context: Option<String>,
    pub msgid: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Location {
    pub path: String,
    pub line: usize,
    pub in_test: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    pub key: MessageKey,
    pub msgid_plural: Option<String>,
    pub locations: Vec<Location>,
}

impl Message {
    pub fn only_in_tests(&self) -> bool {
        self.locations.iter().all(|location| location.in_test)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct ExtractWarning {
    pub path: String,
    pub line: usize,
    pub message: String,
}

#[derive(Default)]
pub struct Extraction {
    /// Crate name → messages sorted by key.
    pub crates: BTreeMap<String, Vec<Message>>,
    pub warnings: Vec<ExtractWarning>,
}

pub fn extract(files: &[RustFile]) -> Extraction {
    let mut by_crate: BTreeMap<String, BTreeMap<MessageKey, Message>> = BTreeMap::new();
    let mut warnings = Vec::new();
    // `#[macro_use] extern crate i18n;` in a crate's library or binary puts the macros in scope
    // in every file of it. Files under `tests/` and the like are crates of their own.
    let crates_with_macro_use: BTreeSet<&str> = files
        .iter()
        .filter(|file| !is_separate_target(&file.path) && has_macro_use(&file.source))
        .map(|file| file.crate_name.as_str())
        .collect();
    for file in files {
        let crate_names = i18n_crate_names(&file.source);
        let mut imported = imported_macros(&file.source, &crate_names);
        if has_macro_use(&file.source)
            || (!is_separate_target(&file.path)
                && crates_with_macro_use.contains(file.crate_name.as_str()))
        {
            for (name, kind) in MACROS {
                imported.entry(name.to_string()).or_insert(kind);
            }
        }
        let source = &file.source;
        for index in 0..source.code.len() {
            if !source.is_ident(index) {
                continue;
            }
            let is_macro_call = source.is_punct(index + 1, "!")
                && (source.is_punct(index + 2, "(")
                    || source.is_punct(index + 2, "[")
                    || source.is_punct(index + 2, "{"));
            let is_function_call = source.is_punct(index + 1, "(");
            if !is_macro_call && !is_function_call {
                continue;
            }
            let name = source.token_text(index);
            let qualified = index >= 3
                && source.is_punct(index - 1, ":")
                && source.is_punct(index - 2, ":")
                && source.is_ident(index - 3)
                && crate_names.contains(source.token_text(index - 3));
            let kind = if qualified {
                MACROS
                    .iter()
                    .chain(FUNCTIONS.iter())
                    .find(|(item_name, kind)| {
                        *item_name == name && kind.is_function() == is_function_call
                    })
                    .map(|(_, kind)| *kind)
            } else if index >= 1 && source.is_punct(index - 1, ":") {
                None
            } else {
                imported
                    .get(name)
                    .copied()
                    .filter(|kind| kind.is_function() == is_function_call)
            };
            let Some(kind) = kind else {
                continue;
            };
            // `fn tr(` declares a function of the same name, and `.tr(` calls a method.
            if kind.is_function()
                && index >= 1
                && (source.is_punct(index - 1, ".")
                    || (source.is_ident(index - 1) && source.token_text(index - 1) == "fn"))
            {
                continue;
            }
            let line = source.line_of_token(index);
            let arguments = source.arguments(if kind.is_function() {
                index + 1
            } else {
                index + 2
            });
            let literal_at = |position: usize| -> Option<String> {
                let (low, high) = *arguments.get(position)?;
                if low != high {
                    return None;
                }
                let literal = source.literal(low)?;
                (literal.prefix == crate::lexer::StringPrefix::None).then(|| literal.value.clone())
            };
            let (context_position, msgid_position, plural_position) = kind.positions();
            // `msg!` expands to the literal itself, which `i18n::tr` looks up unchanged.
            let template_at = |position: usize| -> Option<String> {
                let value = literal_at(position)?;
                Some(if kind.looks_up_unchanged() {
                    value
                } else {
                    runtime_msgid(&value)
                })
            };
            let context = context_position.map(literal_at);
            let msgid = template_at(msgid_position);
            let msgid_plural = plural_position.map(template_at);
            let (Some(msgid), Some(context), Some(msgid_plural)) = (
                msgid,
                context.map_or(Some(None), |context| context.map(Some)),
                msgid_plural.map_or(Some(None), |plural| plural.map(Some)),
            ) else {
                if !kind.is_function() {
                    warnings.push(ExtractWarning {
                        path: file.path.clone(),
                        line,
                        message: format!("`{name}!` without string literals where messages go"),
                    });
                }
                continue;
            };
            let key = MessageKey { context, msgid };
            let location = Location {
                path: file.path.clone(),
                line,
                in_test: file.is_test_token(index),
            };
            let messages = by_crate.entry(file.crate_name.clone()).or_default();
            match messages.get_mut(&key) {
                Some(existing) => {
                    if existing.msgid_plural != msgid_plural {
                        warnings.push(ExtractWarning {
                            path: file.path.clone(),
                            line,
                            message: format!(
                                "{:?} is used with different plural forms ({:?} and {:?})",
                                key.msgid, existing.msgid_plural, msgid_plural
                            ),
                        });
                    }
                    existing.locations.push(location);
                }
                None => {
                    messages.insert(
                        key.clone(),
                        Message {
                            key,
                            msgid_plural,
                            locations: vec![location],
                        },
                    );
                }
            }
        }
    }
    Extraction {
        crates: by_crate
            .into_iter()
            .map(|(crate_name, messages)| (crate_name, messages.into_values().collect()))
            .collect(),
        warnings,
    }
}

/// The msgid the `t!` family looks up for a template, as `normalized_msgid` in `i18n_macros`
/// builds it: format specs and trailing spaces are dropped from placeholders (`{size:.1}` →
/// `{size}`, `{0:?}` → `{0}`), and argument indices lose leading zeros. A template the macros
/// would reject does not compile, so it is returned as written.
fn runtime_msgid(template: &str) -> String {
    let mut msgid = String::with_capacity(template.len());
    let mut characters = template.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' if characters.peek() == Some(&'{') => {
                characters.next();
                msgid.push_str("{{");
            }
            '}' if characters.peek() == Some(&'}') => {
                characters.next();
                msgid.push_str("}}");
            }
            '{' => {
                let mut inner = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    match next {
                        '}' => {
                            closed = true;
                            break;
                        }
                        '{' => return template.to_string(),
                        other => inner.push(other),
                    }
                }
                if !closed {
                    return template.to_string();
                }
                let argument = inner
                    .split_once(':')
                    .map_or(inner.as_str(), |(argument, _)| argument)
                    .trim_end();
                msgid.push('{');
                if !argument.is_empty() && argument.chars().all(|digit| digit.is_ascii_digit()) {
                    match argument.parse::<usize>() {
                        Ok(index) => msgid.push_str(&index.to_string()),
                        Err(_) => return template.to_string(),
                    }
                } else {
                    msgid.push_str(argument);
                }
                msgid.push('}');
            }
            '}' => return template.to_string(),
            other => msgid.push(other),
        }
    }
    msgid
}

fn is_separate_target(path: &str) -> bool {
    ["/tests/", "/examples/", "/benches/"]
        .iter()
        .any(|directory| path.contains(directory))
}

fn has_macro_use(source: &SourceFile) -> bool {
    (0..source.code.len()).any(|index| {
        source.is_ident(index)
            && source.token_text(index) == "extern"
            && source.token_text(index + 1) == "crate"
            && source.token_text(index + 2) == "i18n"
            && source.is_punct(index + 3, ";")
            && (index.saturating_sub(6)..index)
                .any(|before| source.is_ident(before) && source.token_text(before) == "macro_use")
    })
}

/// The names a file can reach the macros' crate by: `i18n`, `i18n_macros`, and aliases from
/// `use i18n as name;` or `extern crate i18n as name;`.
fn i18n_crate_names(source: &SourceFile) -> BTreeSet<String> {
    let mut names = BTreeSet::from(["i18n".to_string(), "i18n_macros".to_string()]);
    for index in 0..source.code.len() {
        if source.is_ident(index)
            && matches!(source.token_text(index), "i18n" | "i18n_macros")
            && source.is_ident(index + 1)
            && source.token_text(index + 1) == "as"
            && source.is_ident(index + 2)
            && source.is_punct(index + 3, ";")
        {
            names.insert(source.token_text(index + 2).to_string());
        }
    }
    names
}

/// The macros and lookup functions a file brings into scope with `use i18n::…` (`use i18n::t;`,
/// `use i18n::{t, tn as plural};`, `use i18n::*;`), by the name they are called with.
fn imported_macros(
    source: &SourceFile,
    crate_names: &BTreeSet<String>,
) -> BTreeMap<String, MacroKind> {
    let mut imported = BTreeMap::new();
    let mut index = 0;
    while index < source.code.len() {
        if !(source.is_ident(index) && source.token_text(index) == "use") {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        if source.is_punct(cursor, ":") && source.is_punct(cursor + 1, ":") {
            cursor += 2;
        }
        let is_i18n = source.is_ident(cursor)
            && crate_names.contains(source.token_text(cursor))
            && source.is_punct(cursor + 1, ":")
            && source.is_punct(cursor + 2, ":");
        let mut end = cursor;
        while end < source.code.len() && !source.is_punct(end, ";") {
            end += 1;
        }
        if is_i18n {
            let path_start = cursor + 3;
            if source.is_punct(path_start, "*") {
                for (name, kind) in MACROS.into_iter().chain(FUNCTIONS) {
                    imported.insert(name.to_string(), kind);
                }
            }
            for position in path_start..end {
                if !source.is_ident(position) {
                    continue;
                }
                let name = source.token_text(position);
                let Some((_, kind)) = MACROS
                    .iter()
                    .chain(FUNCTIONS.iter())
                    .find(|(item_name, _)| *item_name == name)
                else {
                    continue;
                };
                // `context::t` would be a different item.
                if source.is_punct(position + 1, ":")
                    || (position != path_start && source.is_punct(position - 1, ":"))
                {
                    continue;
                }
                let alias = if source.is_ident(position + 1)
                    && source.token_text(position + 1) == "as"
                    && source.is_ident(position + 2)
                {
                    source.token_text(position + 2)
                } else {
                    name
                };
                if alias != "_" {
                    imported.insert(alias.to_string(), *kind);
                }
            }
        }
        index = end + 1;
    }
    imported
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn file(path: &str, text: &str) -> RustFile {
        let source = SourceFile::parse(text);
        let test_ranges = source.test_ranges();
        RustFile {
            path: path.to_string(),
            crate_name: "c".to_string(),
            whole_file_is_test: crate::is_test_path(path, &HashSet::new()),
            source,
            test_ranges,
        }
    }

    fn keys(extraction: &Extraction) -> Vec<(Option<String>, String, Option<String>, bool)> {
        extraction
            .crates
            .values()
            .flatten()
            .map(|message| {
                (
                    message.key.context.clone(),
                    message.key.msgid.clone(),
                    message.msgid_plural.clone(),
                    message.only_in_tests(),
                )
            })
            .collect()
    }

    #[test]
    fn japanese_literals_skip_comments_and_split_tests() {
        let files = [
            file(
                "crates/c/src/c.rs",
                r#"
// 日本語のコメント
/// 文書の「コメント」
fn ui() { show("表示"); show("English"); let c = '字'; }
#[cfg(test)]
mod tests {
    #[test]
    fn t() { assert_eq!(value(), "試験"); }
}
#[gpui::test]
async fn standalone() { check(r"単独"); }
"#,
            ),
            file("crates/c/tests/integration.rs", "fn f() { g(\"統合\"); }"),
            file("crates/c/src/c_tests.rs", "fn f() { g(\"別\"); }"),
        ];
        let found: Vec<(String, usize, bool)> = japanese_literals(&files)
            .into_iter()
            .map(|literal| (literal.value, literal.line, literal.in_test))
            .collect();
        assert_eq!(
            found,
            vec![
                ("表示".to_string(), 4, false),
                ("試験".to_string(), 8, true),
                ("単独".to_string(), 11, true),
                ("統合".to_string(), 1, true),
                ("別".to_string(), 1, true),
            ]
        );
    }

    #[test]
    fn chinese_and_korean_literals_are_found_too() {
        let files = [file(
            "crates/c/src/c.rs",
            "fn ui() { show(\"取消\"); show(\"취소\"); show(\"ㅋ\"); show(\"Abbrechen\"); }",
        )];
        let found: Vec<String> = japanese_literals(&files)
            .into_iter()
            .map(|literal| literal.value)
            .collect();
        assert_eq!(found, ["取消", "취소", "ㅋ"]);
    }

    #[test]
    fn only_cfgs_that_require_tests_are_tests() {
        let files = [file(
            "crates/c/src/c.rs",
            r#"
#[cfg(any(unix, test))]
const NOTICE: &str = "本体一";
#[cfg(any(target_os = "windows", test))]
fn plan() { fail("本体二"); }
#[cfg(any(test, debug_assertions))]
fn debug() { fail("本体三"); }
#[cfg(not(test))]
fn release() { fail("本体四"); }
#[cfg(all(test, unix))]
fn unix_test() { check("試験一"); }
#[cfg(any(test, feature = "test-support"))]
fn support() { check("試験二"); }
#[cfg(all(target_os = "macos", any(test, feature = "test-support")))]
fn mac_support() { check("試験三"); }
#[cfg(feature = "test-support")]
fn only_support() { check("試験四"); }
#[cfg_attr(test, derive(Debug))]
struct Shown { label: &'static str }
fn shown() -> Shown { Shown { label: "本体五" } }
"#,
        )];
        let found: Vec<(String, bool)> = japanese_literals(&files)
            .into_iter()
            .map(|literal| (literal.value, literal.in_test))
            .collect();
        assert_eq!(
            found,
            vec![
                ("本体一".to_string(), false),
                ("本体二".to_string(), false),
                ("本体三".to_string(), false),
                ("本体四".to_string(), false),
                ("試験一".to_string(), true),
                ("試験二".to_string(), true),
                ("試験三".to_string(), true),
                ("試験四".to_string(), true),
                ("本体五".to_string(), false),
            ]
        );
    }

    #[test]
    fn test_only_fields_variants_and_arms_end_at_their_comma() {
        let files = [file(
            "crates/c/src/c.rs",
            r#"
struct Panel {
    #[cfg(test)]
    probe: usize,
    title: String,
}
impl Panel { fn render(&self) { show("本体一"); } }
enum Kind {
    #[cfg(test)]
    Fake,
    Real,
}
fn label(kind: Kind) -> &'static str {
    match kind {
        #[cfg(test)]
        Kind::Fake => "試験一",
        Kind::Real => "本体二",
    }
}
fn build() -> Panel {
    Panel {
        #[cfg(test)]
        probe: count("試験二"),
        title: "本体三".into(),
    }
}
fn generic() {
    #[cfg(test)]
    let pair: HashMap<String, String> = make("試験三");
    show("本体四");
}
#[cfg(test)]
impl<A, B> Fake<A, B> { fn f() { check("試験四"); } }
fn after() { show("本体五"); }
struct Last {
    title: String,
    #[cfg(test)]
    probe: usize,
}
impl Last { fn render(&self) { show("本体六"); } }
"#,
        )];
        let found: Vec<(String, bool)> = japanese_literals(&files)
            .into_iter()
            .map(|literal| (literal.value, literal.in_test))
            .collect();
        assert_eq!(
            found,
            vec![
                ("本体一".to_string(), false),
                ("試験一".to_string(), true),
                ("本体二".to_string(), false),
                ("試験二".to_string(), true),
                ("本体三".to_string(), false),
                ("試験三".to_string(), true),
                ("本体四".to_string(), false),
                ("試験四".to_string(), true),
                ("本体五".to_string(), false),
                ("本体六".to_string(), false),
            ]
        );
    }

    #[test]
    fn literal_forms_are_all_scanned() {
        let files = [file(
            "crates/c/src/c.rs",
            "fn f() {\n    show(r#\"生\"#);\n    show(concat!(\"連\", \"結\"));\n    show(\"\\u{65E5}\\u{672C}\");\n    show(\"複数\n行\");\n    show(c\"C文字列\");\n    /* 外 /* 内 \"注釈\" */ まだ \"注釈\" */\n    show(format!(\"{}件\", n));\n    names.join(\"、\");\n    show(format!(\"「{}」: {error}\"));\n    show(\"Ｆｕｌｌ\");\n    show(\"English — “quoted” … ‹x›\");\n}\n",
        )];
        let found: Vec<(String, usize)> = japanese_literals(&files)
            .into_iter()
            .map(|literal| (literal.value, literal.line))
            .collect();
        assert_eq!(
            found,
            vec![
                ("生".to_string(), 2),
                ("連".to_string(), 3),
                ("結".to_string(), 3),
                ("日本".to_string(), 4),
                ("複数\n行".to_string(), 5),
                ("C文字列".to_string(), 7),
                ("{}件".to_string(), 9),
                ("、".to_string(), 10),
                ("「{}」: {error}".to_string(), 11),
                ("Ｆｕｌｌ".to_string(), 12),
            ]
        );
    }

    #[test]
    fn msgids_are_the_ones_the_macros_look_up() {
        let files = [file(
            "crates/c/src/c.rs",
            r##"
use i18n as l10n;
const MARKED: &str = i18n::msg!("Keep {spec:?} as written");
fn f(size: f64, count: usize) {
    i18n::t!("Size {size:.1} of {0:>4} and {:?} {{literal}} {name } {01}", a, b, name = c);
    i18n::tn!(count, "{count:>3} file", "{count:>3} files");
    l10n::tc!("aliased", "Via {alias:?}", alias = x);
    i18n_macros::t!("Direct");
    i18n::t!("Escaped \u{3042} \x41 \"q\" \\ tab\tend");
    i18n::t!("Continued \
              line");
    i18n::t!(r#"Raw "quoted" \n"#);
}
"##,
        )];
        let extraction = extract(&files);
        assert_eq!(extraction.warnings, Vec::new());
        assert_eq!(
            keys(&extraction),
            vec![
                (None, "Continued line".into(), None, false),
                (None, "Direct".into(), None, false),
                (
                    None,
                    "Escaped \u{3042} A \"q\" \\ tab\tend".into(),
                    None,
                    false
                ),
                (None, "Keep {spec:?} as written".into(), None, false),
                (None, "Raw \"quoted\" \\n".into(), None, false),
                (
                    None,
                    "Size {size} of {0} and {} {{literal}} {name} {1}".into(),
                    None,
                    false
                ),
                (
                    None,
                    "{count} file".into(),
                    Some("{count} files".into()),
                    false
                ),
                (Some("aliased".into()), "Via {alias}".into(), None, false),
            ]
        );
    }

    #[test]
    fn out_of_line_test_modules_and_their_children_are_tests() {
        let mut files = vec![
            file(
                "crates/c/src/c.rs",
                "mod panel;\n#[cfg(test)]\nmod fixtures;\n#[cfg(any(test, feature = \"test-support\"))]\npub mod support;",
            ),
            file(
                "crates/c/src/panel.rs",
                "#[cfg(test)] mod panel_checks;\nmod row;",
            ),
            file("crates/c/src/panel/panel_checks.rs", "mod deeper;"),
            file("crates/c/src/panel/panel_checks/deeper.rs", "fn f() {}"),
            file("crates/c/src/panel/row.rs", "fn f() {}"),
            file("crates/c/src/fixtures.rs", "fn f() {}"),
            file("crates/c/src/support/mod.rs", "fn f() {}"),
            file("crates/c/src/panel_checks.rs", "fn not_the_module() {}"),
        ];
        let roots = HashSet::from(["crates/c/src/c.rs".to_string()]);
        mark_out_of_line_test_modules(&mut files, &roots);
        let flags: Vec<(&str, bool)> = files
            .iter()
            .map(|file| (file.path.as_str(), file.whole_file_is_test))
            .collect();
        assert_eq!(
            flags,
            vec![
                ("crates/c/src/c.rs", false),
                ("crates/c/src/panel.rs", false),
                ("crates/c/src/panel/panel_checks.rs", true),
                ("crates/c/src/panel/panel_checks/deeper.rs", true),
                ("crates/c/src/panel/row.rs", false),
                ("crates/c/src/fixtures.rs", true),
                ("crates/c/src/support/mod.rs", true),
                ("crates/c/src/panel_checks.rs", false),
            ]
        );
    }

    #[test]
    fn extracts_every_macro_form() {
        let files = [file(
            "crates/c/src/c.rs",
            r#"
use i18n::{t, tn as plural};
const LABEL: &str = i18n::msg!("Marked");
fn ui(count: usize) {
    t!("Imported");
    i18n::t!("Open {path}", path = p);
    ::i18n::tc!("tab", "Close");
    plural!(count, "{count} file", "{count} files");
    i18n::tcn!("tab", items.len(), "{count} tab", "{count} tabs");
    i18n::t!(r"Raw \n");
    i18n::t!("Line\nbreak \"quoted\"");
    t!("Imported");
    tc!("not imported", "Skipped");
    other::t!("Skipped");
    tn!(1, "Skipped", "Skipped");
}
#[cfg(test)]
mod tests {
    fn f() { i18n::t!("Only in tests"); i18n::t!("Imported"); }
}
"#,
        )];
        let extraction = extract(&files);
        assert_eq!(extraction.warnings, Vec::new());
        assert_eq!(
            keys(&extraction),
            vec![
                (None, "Imported".into(), None, false),
                (None, "Line\nbreak \"quoted\"".into(), None, false),
                (None, "Marked".into(), None, false),
                (None, "Only in tests".into(), None, true),
                (None, "Open {path}".into(), None, false),
                (None, "Raw \\n".into(), None, false),
                (
                    None,
                    "{count} file".into(),
                    Some("{count} files".into()),
                    false
                ),
                (Some("tab".into()), "Close".into(), None, false),
                (
                    Some("tab".into()),
                    "{count} tab".into(),
                    Some("{count} tabs".into()),
                    false
                ),
            ]
        );
        let imported = &extraction.crates["c"][0];
        let lines: Vec<(usize, bool)> = imported
            .locations
            .iter()
            .map(|location| (location.line, location.in_test))
            .collect();
        assert_eq!(lines, vec![(5, false), (12, false), (19, true)]);
    }

    #[test]
    fn macro_use_applies_to_the_whole_crate_but_not_to_integration_tests() {
        let files = [
            file(
                "crates/c/src/c.rs",
                "#[macro_use]\nextern crate i18n;\nmod other;\nfn f() { t!(\"Root\"); }",
            ),
            file(
                "crates/c/src/other.rs",
                "fn f() { tc!(\"ctx\", \"Other\"); }",
            ),
            file("crates/c/tests/separate.rs", "fn f() { t!(\"Separate\"); }"),
        ];
        let extraction = extract(&files);
        assert_eq!(
            keys(&extraction),
            vec![
                (None, "Root".into(), None, false),
                (Some("ctx".into()), "Other".into(), None, false),
            ]
        );
    }

    #[test]
    fn lookups_of_literals_are_messages() {
        let files = [file(
            "crates/c/src/c.rs",
            r#"
use i18n::tr_ctx;
fn f(name: &str) {
    i18n::tr("Looked {up} as written");
    tr_ctx("panel", "Context");
    i18n::tr_ctx_opt("panel", "Optional");
    i18n::tr(name);
    tr("Not imported");
    other::tr("Not ours");
    value.tr_ctx("panel", "A method");
    i18n::tr!("Not a function");
}
fn tr(text: &str) -> &str { text }
"#,
        )];
        let extraction = extract(&files);
        assert_eq!(extraction.warnings, Vec::new());
        assert_eq!(
            keys(&extraction),
            vec![
                (None, "Looked {up} as written".into(), None, false),
                (Some("panel".into()), "Context".into(), None, false),
                (Some("panel".into()), "Optional".into(), None, false),
            ]
        );
    }

    #[test]
    fn glob_imports_and_non_literal_arguments() {
        let files = [file(
            "crates/c/src/c.rs",
            "use i18n::*;\nfn f() { tc!(\"ctx\", \"Glob\"); t!(concat!(\"a\", \"b\")); msg!(NAME); }",
        )];
        let extraction = extract(&files);
        assert_eq!(
            keys(&extraction),
            vec![(Some("ctx".into()), "Glob".into(), None, false)]
        );
        let warning_lines: Vec<usize> = extraction
            .warnings
            .iter()
            .map(|warning| warning.line)
            .collect();
        assert_eq!(warning_lines, vec![2, 2]);
    }
}
