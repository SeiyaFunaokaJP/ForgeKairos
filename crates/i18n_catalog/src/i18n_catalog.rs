//! Supplies `i18n` with catalogs read from gettext PO files: the ones bundled from the
//! repository's `locales/` directory, overlaid by the user's own
//! `<config_dir>/locales/<tag>/*.po`, which are read without rebuilding Zed.

mod lint;
mod plural;
mod po;

pub use lint::{LintIssue, LintKind, lint};
pub use plural::PluralRule;
pub use po::{PoEntry, PoError, PoFile, PoHeader, parse_po};

use anyhow::{Result, bail};
use parking_lot::Mutex;
use rust_embed::RustEmbed;
use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

// The user's directory is read on the UI thread when the language is chosen, and
// anything can be put there, so a runaway file or directory is skipped with a
// problem instead of stalling Zed. The bundled Japanese is well under 1 MiB.
const MAX_USER_FILE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_USER_FILES_PER_LANGUAGE: usize = 256;
/// Enough for any PO header, which comes first in the file.
const MAX_HEADER_BYTES: u64 = 64 * 1024;
/// BCP 47 tags in use are far shorter; a longer one would only make every
/// fallback step scan the user's directory again.
const MAX_TAG_LENGTH: usize = 64;

// `allow_missing` keeps the build working before any translation is checked in: git
// does not track an empty `locales/` directory.
#[derive(RustEmbed)]
#[folder = "../../locales"]
#[include = "*/*.po"]
#[allow_missing = true]
struct EmbeddedLocales;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocaleInfo {
    /// The BCP 47 tag, spelled as the directory is named.
    pub tag: String,
    /// `X-Language-Name` from the PO header, or the tag when no file has one.
    pub display_name: String,
    pub source: LocaleSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocaleSource {
    Embedded,
    User(PathBuf),
    Both,
}

enum Translation {
    Single(&'static str),
    Plural {
        forms: Box<[&'static str]>,
        rule: Arc<PluralRule>,
    },
}

/// The translations of one language, merged from every layer of its fallback chain.
///
/// Lookups are hash map reads with borrowed keys: they run on every frame, so they must
/// not allocate.
pub struct PoCatalog {
    locale: String,
    plain: HashMap<Box<str>, Translation>,
    contextual: HashMap<Box<str>, HashMap<Box<str>, Translation>>,
    problems: Vec<PoError>,
    layers: Vec<String>,
}

/// The PO files of one language directory.
pub struct Layer {
    /// Shown in messages, such as `user/pt-BR` or `embedded/pt`.
    pub name: String,
    /// `(origin, text)` of every `*.po` file in the directory.
    pub files: Vec<(String, String)>,
}

impl PoCatalog {
    /// Builds a catalog from layers ordered from the highest priority down. A message
    /// translated in an earlier layer hides the same message in later ones; untranslated
    /// and fuzzy entries never hide anything.
    pub fn from_layers(locale: &str, layers: Vec<Layer>, mut problems: Vec<PoError>) -> Self {
        let mut catalog = PoCatalog {
            locale: locale.to_string(),
            plain: HashMap::new(),
            contextual: HashMap::new(),
            problems: Vec::new(),
            layers: layers.iter().map(|layer| layer.name.clone()).collect(),
        };
        for layer in layers {
            for (origin, text) in layer.files {
                let (file, errors) = parse_po(&origin, &text);
                problems.extend(errors);
                catalog.add_file(file, &mut problems);
            }
        }
        catalog.problems = problems;
        catalog
    }

    fn add_file(&mut self, file: PoFile, problems: &mut Vec<PoError>) {
        let rule = match file.header.plural_forms.as_deref().map(PluralRule::parse) {
            Some(Ok(rule)) => rule,
            Some(Err(error)) => {
                problems.push(PoError {
                    origin: file.origin.clone(),
                    line: 0,
                    message: format!("Plural-Forms: {error:#}; using the English rule"),
                });
                PluralRule::english()
            }
            None => PluralRule::english(),
        };
        let rule = Arc::new(rule);
        for entry in file.entries {
            if !entry.is_translated() {
                continue;
            }
            let translation = if entry.msgid_plural.is_some() {
                Translation::Plural {
                    forms: entry.msgstr.into_iter().map(leak).collect(),
                    rule: rule.clone(),
                }
            } else {
                match entry.msgstr.into_iter().next() {
                    Some(string) => Translation::Single(leak(string)),
                    None => continue,
                }
            };
            let map = match entry.context {
                Some(context) => self.contextual.entry(context.into_boxed_str()).or_default(),
                None => &mut self.plain,
            };
            map.entry(entry.msgid.into_boxed_str())
                .or_insert(translation);
        }
    }

    /// Problems found while reading the files; the readable entries are still used.
    pub fn problems(&self) -> &[PoError] {
        &self.problems
    }

    /// The layers the catalog was built from, highest priority first.
    pub fn layers(&self) -> &[String] {
        &self.layers
    }

    pub fn len(&self) -> usize {
        self.plain.len() + self.contextual.values().map(HashMap::len).sum::<usize>()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn find(&self, context: Option<&str>, msgid: &str) -> Option<&Translation> {
        match context {
            Some(context) => self.contextual.get(context)?.get(msgid),
            None => self.plain.get(msgid),
        }
    }
}

// The catalog is leaked for the rest of the process (see `load`), so leaking its strings
// as well costs nothing more and lets lookups hand out `&'static str`.
fn leak(string: String) -> &'static str {
    Box::leak(string.into_boxed_str())
}

fn non_empty(string: &'static str) -> Option<&'static str> {
    (!string.is_empty()).then_some(string)
}

impl i18n::Catalog for PoCatalog {
    fn locale(&self) -> &str {
        &self.locale
    }

    fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str> {
        match self.find(context, msgid)? {
            Translation::Single(string) => Some(string),
            Translation::Plural { forms, .. } => forms.first().copied().and_then(non_empty),
        }
    }

    fn lookup_plural(
        &self,
        context: Option<&str>,
        msgid: &str,
        _msgid_plural: &str,
        count: u64,
    ) -> Option<&'static str> {
        match self.find(context, msgid)? {
            Translation::Single(string) => Some(string),
            Translation::Plural { forms, rule } => {
                let index = rule.form_index(count);
                forms
                    .get(index)
                    .or_else(|| forms.first())
                    .copied()
                    .and_then(non_empty)
            }
        }
    }
}

/// Normalizes the spellings platforms use (`pt_BR.UTF-8`, `de_DE@euro`) to BCP 47.
pub fn normalize_tag(tag: &str) -> String {
    let tag = tag.trim();
    let tag = tag.split(['.', '@']).next().unwrap_or(tag);
    tag.replace('_', "-")
}

/// Whether `tag` looks like a language tag. Tags come from settings and directory
/// names, so this also keeps them from being anything path-like.
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= MAX_TAG_LENGTH
        && tag.split('-').all(|subtag| {
            !subtag.is_empty()
                && subtag.len() <= 8
                && subtag
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric())
        })
        && tag
            .chars()
            .next()
            .is_some_and(|first| first.is_ascii_alphabetic())
}

/// The primary language of a tag: `pt` for `pt-BR`.
pub fn base_language(tag: &str) -> &str {
    tag.split('-').next().unwrap_or(tag)
}

/// `zh-Hant-TW`, `zh-Hant`, `zh`: the tags to try, most specific first.
fn fallback_chain(tag: &str) -> Vec<&str> {
    let mut chain = vec![tag];
    let mut rest = tag;
    while let Some(position) = rest.rfind('-') {
        rest = &rest[..position];
        chain.push(rest);
    }
    chain
}

fn embedded_files(tag: &str) -> Vec<(String, String)> {
    let mut names: Vec<_> = EmbeddedLocales::iter()
        .filter(|name| {
            matches!(name.split_once('/'), Some((directory, file))
                if directory.eq_ignore_ascii_case(tag) && is_po_file_name(file))
        })
        .collect();
    names.sort();
    names
        .into_iter()
        .filter_map(|name| {
            let file = EmbeddedLocales::get(&name)?;
            let text = String::from_utf8_lossy(&file.data).into_owned();
            Some((format!("embedded/{name}"), text))
        })
        .collect()
}

fn embedded_tags() -> Vec<String> {
    let mut tags: Vec<String> = Vec::new();
    for name in EmbeddedLocales::iter() {
        if let Some((directory, file)) = name.split_once('/')
            && is_po_file_name(file)
            && is_valid_tag(directory)
            && !tags.iter().any(|tag| tag.eq_ignore_ascii_case(directory))
        {
            tags.push(directory.to_string());
        }
    }
    tags
}

fn is_po_file_name(name: &str) -> bool {
    !name.contains('/') && name.len() > 3 && name.ends_with(".po")
}

/// Finds `<user_locales_directory>/<tag>` regardless of case, since tags are
/// case-insensitive but some file systems are not.
fn user_language_directory(user_locales_directory: &Path, tag: &str) -> Option<PathBuf> {
    let exact = user_locales_directory.join(tag);
    if exact.is_dir() {
        return Some(exact);
    }
    std::fs::read_dir(user_locales_directory)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| normalize_tag(name).eq_ignore_ascii_case(tag))
                && entry.path().is_dir()
        })
        .map(|entry| entry.path())
}

fn user_po_paths(directory: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        let is_po = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(is_po_file_name);
        if is_po && path.is_file() {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn user_files(directory: &Path, problems: &mut Vec<PoError>) -> Vec<(String, String)> {
    let paths = match user_po_paths(directory) {
        Ok(paths) => paths,
        Err(error) => {
            problems.push(PoError {
                origin: directory.display().to_string(),
                line: 0,
                message: format!("cannot list the directory: {error}"),
            });
            return Vec::new();
        }
    };
    let mut files = Vec::new();
    if paths.len() > MAX_USER_FILES_PER_LANGUAGE {
        problems.push(PoError {
            origin: directory.display().to_string(),
            line: 0,
            message: format!(
                "more than {MAX_USER_FILES_PER_LANGUAGE} PO files; only the first {MAX_USER_FILES_PER_LANGUAGE} are read"
            ),
        });
    }
    for path in paths.into_iter().take(MAX_USER_FILES_PER_LANGUAGE) {
        let origin = path.display().to_string();
        match read_user_file(&path, MAX_USER_FILE_BYTES) {
            Ok(None) => problems.push(PoError {
                origin,
                line: 0,
                message: format!(
                    "the file is larger than {} MiB; skipped",
                    MAX_USER_FILE_BYTES / 1024 / 1024
                ),
            }),
            Ok(Some(bytes)) => match String::from_utf8(bytes) {
                Ok(text) => files.push((origin, text)),
                Err(_) => problems.push(PoError {
                    origin,
                    line: 0,
                    message: "the file is not UTF-8; skipped".to_string(),
                }),
            },
            Err(error) => problems.push(PoError {
                origin,
                line: 0,
                message: format!("cannot read the file: {error}"),
            }),
        }
    }
    files
}

/// Reads at most `limit` bytes of `path`; `None` when the file is longer.
fn read_user_file(path: &Path, limit: u64) -> std::io::Result<Option<Vec<u8>>> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(limit + 1)
        .read_to_end(&mut bytes)?;
    Ok((bytes.len() as u64 <= limit).then_some(bytes))
}

/// The header of a user's PO file, read from its first bytes only.
fn user_file_header(path: &Path) -> Option<PoHeader> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_HEADER_BYTES)
        .read_to_end(&mut bytes)
        .ok()?;
    let text = String::from_utf8_lossy(&bytes);
    Some(po::parse_header_only(&path.display().to_string(), &text))
}

/// Every language that has at least one PO file, bundled or in the user's directory.
pub fn available_locales(user_locales_directory: Option<&Path>) -> Vec<LocaleInfo> {
    let mut locales: Vec<LocaleInfo> = Vec::new();

    if let Some(directory) = user_locales_directory
        && let Ok(entries) = std::fs::read_dir(directory)
    {
        for entry in entries.filter_map(|entry| entry.ok()) {
            let path = entry.path();
            let Some(tag) = entry.file_name().to_str().map(normalize_tag) else {
                continue;
            };
            if !is_valid_tag(&tag) || !path.is_dir() {
                continue;
            }
            let Ok(paths) = user_po_paths(&path) else {
                continue;
            };
            if paths.is_empty() {
                continue;
            }
            let display_name = paths
                .iter()
                .take(MAX_USER_FILES_PER_LANGUAGE)
                .find_map(|po_path| user_file_header(po_path)?.language_name);
            locales.push(LocaleInfo {
                display_name: display_name.unwrap_or_else(|| tag.clone()),
                tag,
                source: LocaleSource::User(path),
            });
        }
    }

    for tag in embedded_tags() {
        let display_name = || {
            embedded_files(&tag)
                .iter()
                .find_map(|(origin, text)| po::parse_header_only(origin, text).language_name)
        };
        match locales
            .iter_mut()
            .find(|locale| locale.tag.eq_ignore_ascii_case(&tag))
        {
            Some(locale) => {
                locale.source = LocaleSource::Both;
                if locale.display_name == locale.tag
                    && let Some(name) = display_name()
                {
                    locale.display_name = name;
                }
            }
            None => locales.push(LocaleInfo {
                display_name: display_name().unwrap_or_else(|| tag.clone()),
                tag,
                source: LocaleSource::Embedded,
            }),
        }
    }

    locales.sort_by_key(|locale| locale.tag.to_ascii_lowercase());
    locales
}

/// Picks the language to show from `requested` (most preferred first): the first one
/// that is available, trying `pt` when `pt-BR` is not. English is always available
/// without a catalog, so a request for English that comes before any available
/// language returns `None`, as does a list with nothing available.
pub fn negotiate(requested: &[String], available: &[LocaleInfo]) -> Option<String> {
    for request in requested {
        let request = normalize_tag(request);
        if !is_valid_tag(&request) {
            continue;
        }
        for candidate in fallback_chain(&request) {
            if let Some(locale) = available
                .iter()
                .find(|locale| locale.tag.eq_ignore_ascii_case(candidate))
            {
                return Some(locale.tag.clone());
            }
        }
        if base_language(&request).eq_ignore_ascii_case("en") {
            return None;
        }
    }
    None
}

/// The user's preferred languages as the operating system reports them, most
/// preferred first, normalized to BCP 47.
pub fn system_locales() -> Vec<String> {
    let mut locales: Vec<String> = Vec::new();
    for locale in sys_locale::get_locales() {
        let locale = normalize_tag(&locale);
        if is_valid_tag(&locale)
            && !locales
                .iter()
                .any(|known| known.eq_ignore_ascii_case(&locale))
        {
            locales.push(locale);
        }
    }
    locales
}

/// Reads the layers for `tag` from the disk and the bundle without caching.
pub fn read_layers(
    tag: &str,
    user_locales_directory: Option<&Path>,
) -> Result<(Vec<Layer>, Vec<PoError>)> {
    let tag = normalize_tag(tag);
    if !is_valid_tag(&tag) {
        bail!("{tag:?} is not a language tag");
    }
    let mut problems = Vec::new();
    let mut layers = Vec::new();
    for candidate in fallback_chain(&tag) {
        if let Some(user_directory) = user_locales_directory
            .and_then(|directory| user_language_directory(directory, candidate))
        {
            let files = user_files(&user_directory, &mut problems);
            if !files.is_empty() {
                layers.push(Layer {
                    name: format!("user/{candidate}"),
                    files,
                });
            }
        }
        let files = embedded_files(candidate);
        if !files.is_empty() {
            layers.push(Layer {
                name: format!("embedded/{candidate}"),
                files,
            });
        }
    }
    Ok((layers, problems))
}

type CacheKey = (String, Option<PathBuf>);

static CACHE: LazyLock<Mutex<HashMap<CacheKey, &'static PoCatalog>>> =
    LazyLock::new(Default::default);

/// Loads the catalog for `tag`, or `None` when no layer of its fallback chain has a PO
/// file (the caller then shows English).
///
/// Catalogs are leaked so that lookups can return `&'static str`; the same tag and
/// directory are therefore read only once per process and later calls return the
/// cached catalog. Unreadable files and syntax errors do not fail the load: they are
/// reported by [`PoCatalog::problems`] and the readable entries are used.
pub fn load(
    tag: &str,
    user_locales_directory: Option<&Path>,
) -> Result<Option<&'static PoCatalog>> {
    let tag = normalize_tag(tag);
    let key = (
        tag.to_ascii_lowercase(),
        user_locales_directory.map(Path::to_path_buf),
    );
    if let Some(catalog) = CACHE.lock().get(&key) {
        return Ok(Some(*catalog));
    }
    let (layers, problems) = read_layers(&tag, user_locales_directory)?;
    if layers.is_empty() {
        return Ok(None);
    }
    let catalog: &'static PoCatalog =
        Box::leak(Box::new(PoCatalog::from_layers(&tag, layers, problems)));
    for problem in catalog.problems() {
        log::warn!(target: "i18n", "{problem}");
    }
    Ok(Some(*CACHE.lock().entry(key).or_insert(catalog)))
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use super::{PoCatalog, load};

    /// The catalog Zed ships for `tag`, built from every bundled `locales/<tag>/*.po`
    /// exactly as the running app builds it, for tests to install with
    /// `i18n::test_support::LocaleOverride`.
    pub fn bundled_catalog(tag: &str) -> &'static PoCatalog {
        match load(tag, None) {
            Ok(Some(catalog)) => catalog,
            Ok(None) => panic!("no PO file is bundled for {tag:?}"),
            Err(error) => panic!("cannot load the bundled {tag:?} catalog: {error:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use i18n::Catalog as _;
    use pretty_assertions::assert_eq;
    use std::time::Instant;

    fn write(path: &Path, text: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create the directory");
        }
        std::fs::write(path, text).expect("write the file");
    }

    fn layer(name: &str, text: &str) -> Layer {
        Layer {
            name: name.to_string(),
            files: vec![(format!("{name}/x.po"), text.to_string())],
        }
    }

    const PLURAL_HEADER_RU: &str = "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=3; plural=(n%10==1 && n%100!=11 ? 0 : n%10>=2 && n%10<=4 && (n%100<10 || n%100>=20) ? 1 : 2);\\n\"\n\n";

    #[test]
    fn lookups_with_context_plurals_and_untranslated_entries() {
        let text = format!(
            "{PLURAL_HEADER_RU}{}",
            concat!(
                "msgid \"Open\"\nmsgstr \"Открыть\"\n\n",
                "msgctxt \"menu\"\nmsgid \"Open\"\nmsgstr \"Открыть…\"\n\n",
                "msgid \"{count} file\"\nmsgid_plural \"{count} files\"\n",
                "msgstr[0] \"{count} файл\"\nmsgstr[1] \"{count} файла\"\nmsgstr[2] \"{count} файлов\"\n\n",
                "msgid \"{count} line\"\nmsgid_plural \"{count} lines\"\n",
                "msgstr[0] \"{count} строка\"\nmsgstr[1] \"\"\n\n",
                "msgid \"Empty\"\nmsgstr \"\"\n\n",
                "#, fuzzy\nmsgid \"Fuzzy\"\nmsgstr \"Нечёткий\"\n",
            )
        );
        let catalog = PoCatalog::from_layers("ru", vec![layer("embedded/ru", &text)], Vec::new());
        assert_eq!(catalog.problems(), &[]);
        assert_eq!(catalog.locale(), "ru");
        assert_eq!(catalog.lookup(None, "Open"), Some("Открыть"));
        assert_eq!(catalog.lookup(Some("menu"), "Open"), Some("Открыть…"));
        assert_eq!(catalog.lookup(Some("other"), "Open"), None);
        assert_eq!(catalog.lookup(None, "Missing"), None);
        assert_eq!(catalog.lookup(None, "Empty"), None);
        assert_eq!(catalog.lookup(None, "Fuzzy"), None);
        let files = |count| catalog.lookup_plural(None, "{count} file", "{count} files", count);
        assert_eq!(files(1), Some("{count} файл"));
        assert_eq!(files(3), Some("{count} файла"));
        assert_eq!(files(5), Some("{count} файлов"));
        assert_eq!(files(21), Some("{count} файл"));
        assert_eq!(catalog.lookup(None, "{count} file"), Some("{count} файл"));
        let lines = |count| catalog.lookup_plural(None, "{count} line", "{count} lines", count);
        assert_eq!(lines(1), Some("{count} строка"));
        // An empty form is untranslated, and form 2 is missing altogether.
        assert_eq!(lines(2), None);
        assert_eq!(lines(5), Some("{count} строка"));
        assert_eq!(catalog.len(), 4);
    }

    #[test]
    fn earlier_layers_win_and_untranslated_entries_do_not_hide_later_ones() {
        let catalog = PoCatalog::from_layers(
            "pt-BR",
            vec![
                layer(
                    "user/pt-BR",
                    "msgid \"A\"\nmsgstr \"A do usuário\"\n\nmsgid \"B\"\nmsgstr \"\"\n",
                ),
                layer(
                    "embedded/pt-BR",
                    "msgid \"A\"\nmsgstr \"A embutido\"\n\nmsgid \"B\"\nmsgstr \"B embutido\"\n\nmsgid \"C\"\nmsgstr \"C (BR)\"\n",
                ),
                layer(
                    "embedded/pt",
                    "msgid \"C\"\nmsgstr \"C (PT)\"\n\nmsgid \"D\"\nmsgstr \"D (PT)\"\n",
                ),
            ],
            Vec::new(),
        );
        assert_eq!(catalog.lookup(None, "A"), Some("A do usuário"));
        assert_eq!(catalog.lookup(None, "B"), Some("B embutido"));
        assert_eq!(catalog.lookup(None, "C"), Some("C (BR)"));
        assert_eq!(catalog.lookup(None, "D"), Some("D (PT)"));
    }

    #[test]
    fn broken_files_keep_their_readable_entries() {
        let catalog = PoCatalog::from_layers(
            "ja",
            vec![layer(
                "user/ja",
                "msgid \"\"\nmsgstr \"Plural-Forms: nplurals=oops\\n\"\n\nmsgid \"Good\"\nmsgstr \"良い\"\n\nmsgid \"Bad\nmsgstr \"x\"\n\nmsgid \"{count} item\"\nmsgid_plural \"{count} items\"\nmsgstr[0] \"one\"\nmsgstr[1] \"other\"\n",
            )],
            Vec::new(),
        );
        assert_eq!(catalog.lookup(None, "Good"), Some("良い"));
        assert_eq!(catalog.lookup(None, "Bad"), None);
        // The broken Plural-Forms falls back to the English rule.
        assert_eq!(
            catalog.lookup_plural(None, "{count} item", "{count} items", 1),
            Some("one")
        );
        assert_eq!(
            catalog.lookup_plural(None, "{count} item", "{count} items", 2),
            Some("other")
        );
        let lines: Vec<_> = catalog
            .problems()
            .iter()
            .map(|problem| problem.line)
            .collect();
        assert_eq!(lines, [7, 0]);
    }

    #[test]
    fn tags() {
        assert_eq!(normalize_tag("pt_BR.UTF-8"), "pt-BR");
        assert_eq!(normalize_tag(" de_DE@euro "), "de-DE");
        assert!(is_valid_tag("zh-Hant-TW"));
        for invalid in [
            "",
            "../ja",
            "ja/x",
            "-ja",
            "ja-",
            "1ja",
            "ja--JP",
            "C:",
            "toolongsubtag",
        ] {
            assert!(!is_valid_tag(invalid), "{invalid:?}");
        }
        assert_eq!(
            fallback_chain("zh-Hant-TW"),
            ["zh-Hant-TW", "zh-Hant", "zh"]
        );
        assert_eq!(fallback_chain("ja"), ["ja"]);
    }

    fn info(tag: &str) -> LocaleInfo {
        LocaleInfo {
            tag: tag.to_string(),
            display_name: tag.to_string(),
            source: LocaleSource::Embedded,
        }
    }

    fn requested(tags: &[&str]) -> Vec<String> {
        tags.iter().map(|tag| tag.to_string()).collect()
    }

    #[test]
    fn negotiation() {
        let available = [info("ja"), info("pt"), info("zh-Hant")];
        assert_eq!(
            negotiate(&requested(&["ja-JP"]), &available).as_deref(),
            Some("ja")
        );
        assert_eq!(
            negotiate(&requested(&["pt-BR"]), &available).as_deref(),
            Some("pt")
        );
        assert_eq!(
            negotiate(&requested(&["PT_br"]), &available).as_deref(),
            Some("pt")
        );
        assert_eq!(
            negotiate(&requested(&["zh-Hant-TW"]), &available).as_deref(),
            Some("zh-Hant")
        );
        assert_eq!(negotiate(&requested(&["zh-Hans-CN"]), &available), None);
        assert_eq!(
            negotiate(&requested(&["fr-FR", "ja"]), &available).as_deref(),
            Some("ja")
        );
        assert_eq!(negotiate(&requested(&["en-US", "ja"]), &available), None);
        assert_eq!(
            negotiate(&requested(&["../x", "ja"]), &available).as_deref(),
            Some("ja")
        );
        assert_eq!(negotiate(&requested(&["fr"]), &available), None);
        assert_eq!(negotiate(&[], &available), None);

        let with_user_english = [info("en-GB"), info("ja")];
        assert_eq!(
            negotiate(&requested(&["en-GB", "ja"]), &with_user_english).as_deref(),
            Some("en-GB")
        );
        let exact_wins = [info("pt"), info("pt-BR")];
        assert_eq!(
            negotiate(&requested(&["pt-BR"]), &exact_wins).as_deref(),
            Some("pt-BR")
        );
    }

    #[test]
    fn user_directory_is_listed_loaded_and_overrides_the_bundle() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(
            &root.join("fr/x.po"),
            "msgid \"\"\nmsgstr \"\"\n\"Language: fr\\n\"\n\"X-Language-Name: Français\\n\"\n\"Plural-Forms: nplurals=2; plural=(n > 1);\\n\"\n\nmsgid \"Open\"\nmsgstr \"Ouvrir\"\n\nmsgid \"{count} file\"\nmsgid_plural \"{count} files\"\nmsgstr[0] \"{count} fichier\"\nmsgstr[1] \"{count} fichiers\"\n",
        );
        write(
            &root.join("fr/y.po"),
            "msgid \"Close\"\nmsgstr \"Fermer\"\n",
        );
        write(&root.join("fr/notes.txt"), "not a PO file");
        write(
            &root.join("ja/override.po"),
            "msgid \"i18n_catalog test: Hello\"\nmsgstr \"ユーザーのこんにちは\"\n",
        );
        std::fs::create_dir_all(root.join("de")).expect("create an empty language directory");
        std::fs::create_dir_all(root.join("..bad")).expect("create a directory");

        let locales = available_locales(Some(root));
        let french = locales
            .iter()
            .find(|locale| locale.tag == "fr")
            .expect("fr should be listed");
        assert_eq!(french.display_name, "Français");
        assert_eq!(french.source, LocaleSource::User(root.join("fr")));
        let japanese = locales
            .iter()
            .find(|locale| locale.tag == "ja")
            .expect("ja should be listed");
        assert_eq!(japanese.source, LocaleSource::Both);
        assert_eq!(japanese.display_name, "日本語");
        assert!(locales.iter().all(|locale| locale.tag != "de"));
        assert!(locales.iter().all(|locale| !locale.tag.contains("bad")));

        let catalog = load("fr", Some(root))
            .expect("loading should not fail")
            .expect("fr has files");
        assert_eq!(catalog.lookup(None, "Open"), Some("Ouvrir"));
        assert_eq!(catalog.lookup(None, "Close"), Some("Fermer"));
        assert_eq!(
            catalog.lookup_plural(None, "{count} file", "{count} files", 1),
            Some("{count} fichier")
        );
        assert_eq!(
            catalog.lookup_plural(None, "{count} file", "{count} files", 0),
            Some("{count} fichier")
        );
        assert_eq!(
            catalog.lookup_plural(None, "{count} file", "{count} files", 2),
            Some("{count} fichiers")
        );
        let again = load("FR", Some(root)).expect("load").expect("cached");
        assert!(std::ptr::eq(catalog, again));

        let japanese = load("ja-JP", Some(root)).expect("load").expect("ja exists");
        assert_eq!(japanese.locale(), "ja-JP");
        assert_eq!(japanese.layers(), ["user/ja", "embedded/ja"]);
        assert_eq!(
            japanese.lookup(None, "i18n_catalog test: Hello"),
            Some("ユーザーのこんにちは")
        );
        assert_eq!(
            japanese.lookup(None, "i18n_catalog test: Goodbye"),
            Some("さようなら (同梱)")
        );

        assert!(load("de", Some(root)).expect("load").is_none());
        assert!(load("../ja", Some(root)).is_err());
    }

    #[test]
    fn regional_user_translations_fall_back_to_the_base_language() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(
            &root.join("pt_BR/a.po"),
            "msgid \"Open\"\nmsgstr \"Abrir (BR)\"\n",
        );
        write(
            &root.join("pt/a.po"),
            "msgid \"Open\"\nmsgstr \"Abrir (PT)\"\n\nmsgid \"Close\"\nmsgstr \"Fechar (PT)\"\n",
        );
        let catalog = load("pt-BR", Some(root))
            .expect("load")
            .expect("pt-BR exists");
        assert_eq!(catalog.layers(), ["user/pt-BR", "user/pt"]);
        assert_eq!(catalog.lookup(None, "Open"), Some("Abrir (BR)"));
        assert_eq!(catalog.lookup(None, "Close"), Some("Fechar (PT)"));

        let tags: Vec<_> = available_locales(Some(root))
            .into_iter()
            .map(|locale| locale.tag)
            .filter(|tag| tag.starts_with("pt"))
            .collect();
        assert_eq!(tags, ["pt", "pt-BR"]);
    }

    #[test]
    fn unreadable_files_are_problems_not_failures() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        write(&root.join("xx/good.po"), "msgid \"Open\"\nmsgstr \"Ope\"\n");
        std::fs::write(
            root.join("xx/latin1.po"),
            b"msgid \"Caf\xe9\"\nmsgstr \"x\"\n",
        )
        .expect("write the file");
        let catalog = load("xx", Some(root)).expect("load").expect("xx exists");
        assert_eq!(catalog.lookup(None, "Open"), Some("Ope"));
        assert_eq!(catalog.problems().len(), 1);
        assert!(catalog.problems()[0].message.contains("UTF-8"));
    }

    #[test]
    fn oversized_and_excess_user_files_are_skipped_with_a_problem() {
        let directory = tempfile::tempdir().expect("create a temporary directory");
        let root = directory.path();
        let path = root.join("big.po");
        write(&path, "0123456789");
        assert_eq!(
            read_user_file(&path, 10).expect("read"),
            Some(b"0123456789".to_vec())
        );
        assert_eq!(read_user_file(&path, 9).expect("read"), None);

        for index in 0..MAX_USER_FILES_PER_LANGUAGE + 3 {
            write(
                &root.join(format!("many/{index:04}.po")),
                &format!("msgid \"Message {index}\"
msgstr \"Translated {index}\"
"),
            );
        }
        let catalog = load("many", Some(root)).expect("load").expect("many exists");
        assert_eq!(catalog.len(), MAX_USER_FILES_PER_LANGUAGE);
        assert_eq!(catalog.problems().len(), 1, "{:?}", catalog.problems());
        assert!(
            catalog.lookup(None, "Message 0").is_some()
                && catalog
                    .lookup(None, &format!("Message {MAX_USER_FILES_PER_LANGUAGE}"))
                    .is_none()
        );

        let long_tag = ["ab"; 40].join("-");
        assert!(!is_valid_tag(&long_tag));
        assert!(load(&long_tag, Some(root)).is_err());
    }

    #[test]
    fn bundled_japanese() {
        let catalog = load("ja", None).expect("load").expect("ja is bundled");
        assert_eq!(
            catalog.lookup(None, "i18n_catalog test: Hello"),
            Some("こんにちは (同梱)")
        );
        assert_eq!(
            catalog.lookup_plural(
                None,
                "i18n_catalog test: {count} file",
                "i18n_catalog test: {count} files",
                7
            ),
            Some("ファイル {count} 個")
        );
        assert_eq!(
            catalog.lookup(Some("i18n_catalog test"), "Open"),
            Some("開く (文脈つき)")
        );
        let locales = available_locales(None);
        let japanese = locales
            .iter()
            .find(|locale| locale.tag == "ja")
            .expect("ja is bundled");
        assert_eq!(japanese.display_name, "日本語");
        assert_eq!(japanese.source, LocaleSource::Embedded);
        assert!(catalog.problems().is_empty(), "{:?}", catalog.problems());
    }

    /// Every bundled PO file must parse cleanly, so mistakes are caught before release.
    #[test]
    fn bundled_files_have_no_syntax_errors() {
        for tag in embedded_tags() {
            for (origin, text) in embedded_files(&tag) {
                let (_, errors) = parse_po(&origin, &text);
                assert!(errors.is_empty(), "{errors:?}");
            }
        }
    }

    /// The files of a language directory are merged into one catalog in which a message
    /// without a context is a single key across all of them, so two files translating
    /// it differently would make one of them lose silently, depending on file order.
    #[test]
    fn bundled_files_of_a_language_agree_on_every_message() {
        for tag in embedded_tags() {
            let mut translations: HashMap<(Option<String>, String), Vec<(String, Vec<String>)>> =
                HashMap::new();
            for (origin, text) in embedded_files(&tag) {
                let (file, _) = parse_po(&origin, &text);
                for entry in file.entries {
                    if !entry.is_translated() {
                        continue;
                    }
                    translations
                        .entry((entry.context, entry.msgid))
                        .or_default()
                        .push((origin.clone(), entry.msgstr));
                }
            }
            let mut conflicts: Vec<String> = translations
                .into_iter()
                .filter(|(_, found)| found.iter().any(|(_, msgstr)| *msgstr != found[0].1))
                .map(|((context, msgid), found)| format!("{context:?} {msgid:?}: {found:?}"))
                .collect();
            conflicts.sort();
            assert!(
                conflicts.is_empty(),
                "locales/{tag} translates these messages differently in different files:\n{}",
                conflicts.join("\n")
            );
        }
    }

    #[test]
    fn test_support_returns_the_bundled_catalog() {
        let catalog = test_support::bundled_catalog("ja");
        assert!(std::ptr::eq(
            catalog,
            load("ja", None).expect("load").expect("ja is bundled")
        ));
        assert_eq!(
            catalog.lookup(Some("action"), "zed::OpenKeymap"),
            Some("環境設定: キーマップを開く")
        );
        assert_eq!(catalog.lookup(Some("menu"), "File"), Some("ファイル"));
        assert_eq!(catalog.lookup(Some("settings"), "General"), Some("全般"));
    }

    /// The files translated where Zed draws them (`_actions.po`, `_menus.po`,
    /// `_settings.po`) keep every entry under a context, so their short labels
    /// never translate the same English met elsewhere (a language named "Go",
    /// another crate's "No"); the crates' files leave those contexts to them.
    #[test]
    fn bundled_area_files_keep_to_their_contexts() {
        let area_contexts = [
            ("_actions.po", i18n::context::ACTION),
            ("_menus.po", i18n::context::MENU),
            ("_settings.po", i18n::context::SETTINGS),
        ];
        let mut misplaced = Vec::new();
        for tag in embedded_tags() {
            for (origin, text) in embedded_files(&tag) {
                let file_name = origin.rsplit('/').next().unwrap_or(&origin);
                let area_context = area_contexts
                    .iter()
                    .find(|(name, _)| *name == file_name)
                    .map(|(_, context)| *context);
                let (file, _) = parse_po(&origin, &text);
                for entry in file.entries {
                    let context = entry.context.as_deref();
                    let in_place = match area_context {
                        // The settings dropdowns translate their labels in the
                        // context of the enum's name.
                        Some(i18n::context::SETTINGS) => context.is_some(),
                        Some(area_context) => context == Some(area_context),
                        None => !area_contexts
                            .iter()
                            .any(|(_, reserved)| context == Some(*reserved)),
                    };
                    if !in_place {
                        misplaced.push(format!("{origin}: {context:?} {:?}", entry.msgid));
                    }
                }
            }
        }
        assert!(misplaced.is_empty(), "{misplaced:#?}");
    }

    #[test]
    fn installing_through_the_contract() {
        let catalog = load("ja", None).expect("load").expect("ja is bundled");
        let _override = i18n::test_support::LocaleOverride::new(Some(catalog));
        assert_eq!(i18n::current_locale(), "ja");
        assert_eq!(i18n::tr("i18n_catalog test: Hello"), "こんにちは (同梱)");
        assert_eq!(i18n::tr("Not translated"), "Not translated");
        assert_eq!(
            i18n::tr_plural(
                None,
                "i18n_catalog test: {count} file",
                "i18n_catalog test: {count} files",
                1
            ),
            "ファイル {count} 個"
        );
    }

    #[test]
    fn system_locales_are_valid_tags() {
        let locales = system_locales();
        println!("system locales: {locales:?}");
        assert!(
            locales.iter().all(|locale| is_valid_tag(locale)),
            "{locales:?}"
        );
    }

    fn synthetic_po(entry_count: usize) -> String {
        let mut text = String::from(
            "msgid \"\"\nmsgstr \"\"\n\"Language: ja\\n\"\n\"Plural-Forms: nplurals=1; plural=0;\\n\"\n\n",
        );
        for index in 0..entry_count {
            if index % 10 == 0 {
                text.push_str(&format!("msgctxt \"context {}\"\n", index % 7));
            }
            if index % 25 == 0 {
                text.push_str(&format!(
                    "msgid \"{index} item with a {{name}} placeholder\"\nmsgid_plural \"{index} items with a {{name}} placeholder\"\nmsgstr[0] \"{{name}} の項目 {index} 番の訳文です。少し長めの文章にしておく。\"\n\n"
                ));
            } else {
                text.push_str(&format!(
                    "msgid \"Message number {index}: an English source text of moderate length\"\nmsgstr \"\"\n\"メッセージ {index} 番: 中くらいの長さの日本語の訳文を\"\n\"複数行に分けて書いたもの\\n\"\n\n"
                ));
            }
        }
        text
    }

    /// Prints how long a large catalog takes to read; run with `--nocapture` to see it.
    #[test]
    fn large_catalog_load_time() {
        let text = synthetic_po(15_000);
        let started = Instant::now();
        let (file, errors) = parse_po("large.po", &text);
        let parsed = started.elapsed();
        assert!(errors.is_empty());
        assert_eq!(file.entries.len(), 15_000);
        drop(file);

        let started = Instant::now();
        let catalog = PoCatalog::from_layers("ja", vec![layer("large", &text)], Vec::new());
        let built = started.elapsed();
        assert_eq!(catalog.len(), 15_000);

        let keys: Vec<String> = (0..15_000)
            .filter(|index| index % 25 != 0 && index % 10 != 0)
            .map(|index| {
                format!("Message number {index}: an English source text of moderate length")
            })
            .collect();
        let started = Instant::now();
        let mut found = 0;
        for key in &keys {
            if catalog.lookup(None, key).is_some() {
                found += 1;
            }
        }
        let looked_up = started.elapsed();
        assert_eq!(found, keys.len());
        println!(
            "{} bytes, 15000 entries: parse {parsed:?}, parse and build {built:?}, {} lookups {looked_up:?}",
            text.len(),
            keys.len()
        );
        // Generous enough for an unoptimized build on a slow machine; the numbers above
        // are what matters.
        assert!(built.as_secs() < 10, "building took {built:?}");
    }
}
