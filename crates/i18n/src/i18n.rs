//! The translation contract shared by every crate that shows text.
//!
//! Messages are keyed by their English source text (gettext style). With no catalog
//! installed every lookup returns the English text unchanged.
//!
//! Wrong arguments are compile errors:
//!
//! ```compile_fail
//! let _ = i18n::t!("Select a {} for {}", "file");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("Open", "unused");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("Open {path}", path = "a", other = "b");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("Open {0} {1}", "only one");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("Open {undefined_local_variable}");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("unbalanced { brace");
//! ```
//!
//! ```compile_fail
//! let _ = i18n::t!("{value:width$}", value = 1, width = 4);
//! ```
//!
//! ```compile_fail
//! let items = 2;
//! let _ = i18n::tn!(items, "{count} item", "{count} items", count = 3);
//! ```
//!
//! ```compile_fail
//! let value = 1.5;
//! let _ = i18n::t!("{value} {value:.1}");
//! ```
//!
//! The same shapes compile when the arguments line up:
//!
//! ```
//! let path = "a";
//! let _ = i18n::t!("Select a {} for {}", "file", "b");
//! let _ = i18n::t!("Open {path}");
//! let _ = i18n::t!("Open {0} {1}", "one", "two");
//! let _ = i18n::t!("{value:4}", value = 1);
//! let items = 2;
//! let _ = i18n::tn!(items, "{count} item", "{count} items");
//! ```

extern crate self as i18n;

use parking_lot::RwLock;
use std::cell::Cell;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

pub use i18n_macros::{msg, t, tc, tcn, tn};

/// A loaded set of translations for one language, implemented by the catalog provider.
///
/// Returned strings are `'static`: providers keep their catalogs alive by leaking them.
pub trait Catalog: Send + Sync + 'static {
    /// The BCP 47 tag of the language, such as "ja" or "pt-BR".
    fn locale(&self) -> &str;
    fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str>;
    fn lookup_plural(
        &self,
        context: Option<&str>,
        msgid: &str,
        msgid_plural: &str,
        count: u64,
    ) -> Option<&'static str>;
}

/// The msgctxt of the strings Zed translates where it draws them instead of through
/// the macros. Each area has its own context because every PO file of a language
/// goes into one catalog: without one, a short label of one area ("Go", "Selection")
/// would also translate the same English wherever else `tr` meets it, including
/// data such as a language named "Go".
pub mod context {
    /// Command palette names, keyed by the action name (`editor::GoToDefinition`).
    pub const ACTION: &str = "action";
    /// Application menu names and items.
    pub const MENU: &str = "menu";
    /// Titles, descriptions and headers of the settings window's table.
    pub const SETTINGS: &str = "settings";
    /// The description of an action (its doc comment), keyed by the action name like
    /// `ACTION`, so that a reworded doc comment upstream keeps its translation until
    /// someone updates it instead of losing it.
    pub const ACTION_DOCUMENTATION: &str = "action documentation";
    /// The `description` of a JSON schema Zed serves for its settings files, keyed by
    /// the English description itself.
    pub const SETTINGS_SCHEMA: &str = "settings schema";
}

/// The description of the action named `action_name` in the display language, or
/// `english` (the action's doc comment) when there is no translation.
pub fn action_documentation<'a>(action_name: &str, english: &'a str) -> &'a str {
    lookup_in_current(Some(context::ACTION_DOCUMENTATION), action_name).unwrap_or(english)
}

static CURRENT_CATALOG: RwLock<Option<&'static dyn Catalog>> = RwLock::new(None);
static GENERATION: AtomicU64 = AtomicU64::new(0);

thread_local! {
    // The outer `Option` says whether this thread overrides the process-wide catalog;
    // the inner one is the override itself, where `None` means English.
    static THREAD_OVERRIDE: Cell<Option<Option<&'static dyn Catalog>>> = const { Cell::new(None) };
    // Set by `test_support::ThreadLocalInstalls`.
    static INSTALLS_ARE_THREAD_LOCAL: Cell<bool> = const { Cell::new(false) };
    // Installs made while they were thread-local; added to the process-wide
    // generation and never reset, so this thread's generation only grows.
    static THREAD_GENERATION: Cell<u64> = const { Cell::new(0) };
    // Set by `in_english`. Kept apart from `THREAD_OVERRIDE` so that `Bilingual`
    // can step out of it to the language the user reads.
    static FORCED_ENGLISH: Cell<bool> = const { Cell::new(false) };
}

fn current_catalog() -> Option<&'static dyn Catalog> {
    if FORCED_ENGLISH.with(Cell::get) {
        return None;
    }
    match THREAD_OVERRIDE.with(Cell::get) {
        Some(overridden) => overridden,
        None => *CURRENT_CATALOG.read(),
    }
}

/// Replaces the process-wide catalog; `None` goes back to English.
pub fn install(catalog: Option<&'static dyn Catalog>) {
    if INSTALLS_ARE_THREAD_LOCAL.with(Cell::get) {
        THREAD_OVERRIDE.with(|cell| cell.set(Some(catalog)));
        THREAD_GENERATION.with(|cell| cell.set(cell.get() + 1));
    } else {
        *CURRENT_CATALOG.write() = catalog;
        GENERATION.fetch_add(1, Ordering::SeqCst);
    }
}

/// The locale of the current catalog, or "en" when none is installed.
pub fn current_locale() -> &'static str {
    match current_catalog() {
        Some(catalog) => catalog.locale(),
        None => "en",
    }
}

/// Increases every time `install` is called, so caches of translated text can tell
/// when they are stale.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst) + THREAD_GENERATION.with(Cell::get)
}

fn lookup_in_current(context: Option<&str>, msgid: &str) -> Option<&'static str> {
    // In a PO file the empty msgid is the header, so `tr("")` on an empty run-time
    // string would otherwise show the catalog's metadata.
    if msgid.is_empty() {
        return None;
    }
    let catalog = current_catalog()?;
    let translation = catalog.lookup(context, msgid);
    if translation.is_none() {
        diagnostics::report_missing(context, msgid);
    }
    translation
}

fn lookup_plural_in_current(
    context: Option<&str>,
    singular: &str,
    plural: &str,
    count: u64,
) -> Option<&'static str> {
    if singular.is_empty() {
        return None;
    }
    let catalog = current_catalog()?;
    let translation = catalog.lookup_plural(context, singular, plural, count);
    if translation.is_none() {
        diagnostics::report_missing(context, singular);
    }
    translation
}

/// Translates a string known only at run time, such as a title from a table.
/// Returns the input unchanged when there is no translation.
pub fn tr(msgid: &str) -> &str {
    lookup_in_current(None, msgid).unwrap_or(msgid)
}

pub fn tr_ctx<'a>(context: &str, msgid: &'a str) -> &'a str {
    lookup_in_current(Some(context), msgid).unwrap_or(msgid)
}

/// Like `tr_ctx`, but returns `None` when there is no translation so the caller can
/// fall back to something other than the msgid.
pub fn tr_ctx_opt(context: &str, msgid: &str) -> Option<&'static str> {
    lookup_in_current(Some(context), msgid)
}

pub fn tr_plural<'a>(
    context: Option<&str>,
    singular: &'a str,
    plural: &'a str,
    count: u64,
) -> &'a str {
    lookup_plural_in_current(context, singular, plural, count)
        .unwrap_or_else(|| __private::english_plural(singular, plural, count))
}

/// Restores `FORCED_ENGLISH` when dropped, also when the closure panics.
struct ForcedEnglish {
    previous: bool,
}

impl ForcedEnglish {
    fn set(forced: bool) -> Self {
        Self {
            previous: FORCED_ENGLISH.with(|cell| cell.replace(forced)),
        }
    }
}

impl Drop for ForcedEnglish {
    fn drop(&mut self) {
        FORCED_ENGLISH.with(|cell| cell.set(self.previous));
    }
}

/// Runs `build` with every lookup on this thread returning the English source text,
/// for text a model reads: the display language must not change what it is told.
///
/// Only what `build` does before it returns is in English. A future it returns is
/// polled later, outside, and work it hands to another thread is not covered; use
/// [`in_english_while_polling`] for a future.
pub fn in_english<R>(build: impl FnOnce() -> R) -> R {
    let _forced = ForcedEnglish::set(true);
    build()
}

/// Polls `future` with every lookup on the polling thread returning English, as
/// [`in_english`] does for a closure.
///
/// English is switched on only for the duration of each poll of `future`, so other
/// tasks that run on the same thread between its polls keep the display language.
/// Work the future hands to another thread or task is not covered.
pub fn in_english_while_polling<F: Future>(future: F) -> InEnglish<F> {
    InEnglish {
        future: Box::pin(future),
    }
}

/// The future returned by [`in_english_while_polling`].
#[must_use = "futures do nothing unless polled"]
pub struct InEnglish<F> {
    future: Pin<Box<F>>,
}

impl<F: Future> Future for InEnglish<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<F::Output> {
        let _forced = ForcedEnglish::set(true);
        self.future.as_mut().poll(cx)
    }
}

/// A message built once in English and once in the display language, for text
/// that is both shown to the user and told to a model, where it is built before
/// anyone knows which of them will read it.
///
/// Its `Display` is the English text inside [`in_english`] and the display
/// language elsewhere, so formatting an error that carries it follows the reader.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Bilingual {
    english: String,
    display: String,
}

impl Bilingual {
    /// Calls `build` twice, so it must not have side effects. The display text is
    /// built in the language the user reads also inside [`in_english`], because the
    /// user may be shown it later.
    pub fn new(build: impl Fn() -> String) -> Self {
        let english = in_english(&build);
        let display = {
            let _outside_english = ForcedEnglish::set(false);
            build()
        };
        Self { english, display }
    }

    pub fn english(&self) -> &str {
        &self.english
    }

    pub fn display(&self) -> &str {
        &self.display
    }

    /// The text for whoever reads it now: English inside [`in_english`], and the
    /// display language elsewhere.
    pub fn current(&self) -> &str {
        if FORCED_ENGLISH.with(Cell::get) {
            &self.english
        } else {
            &self.display
        }
    }
}

impl std::fmt::Display for Bilingual {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.current())
    }
}

impl std::error::Error for Bilingual {}

pub mod test_support {
    use super::{Catalog, INSTALLS_ARE_THREAD_LOCAL, THREAD_OVERRIDE};
    use std::marker::PhantomData;

    /// Overrides the catalog for the current thread until dropped.
    ///
    /// Tests run in parallel on separate threads, so tests use this instead of
    /// `install`, which would leak into every other test.
    #[must_use = "the override ends as soon as this value is dropped"]
    pub struct LocaleOverride {
        previous: Option<Option<&'static dyn Catalog>>,
        // Restoring must happen on the thread whose override was changed.
        _not_send: PhantomData<*const ()>,
    }

    impl LocaleOverride {
        pub fn new(catalog: Option<&'static dyn Catalog>) -> Self {
            let previous = THREAD_OVERRIDE.with(|cell| cell.replace(Some(catalog)));
            Self {
                previous,
                _not_send: PhantomData,
            }
        }
    }

    impl Drop for LocaleOverride {
        fn drop(&mut self) {
            THREAD_OVERRIDE.with(|cell| cell.set(self.previous));
        }
    }

    /// Until dropped, `install` called on the current thread replaces only this
    /// thread's catalog, starting from English, and advances only the `generation`
    /// this thread sees.
    ///
    /// For tests of code that installs catalogs itself, such as a change of the
    /// `ui_language` setting: GPUI tests run the app on the test's own thread, so
    /// the catalog they install stays out of the tests running beside them.
    #[must_use = "installs go back to being process-wide as soon as this value is dropped"]
    pub struct ThreadLocalInstalls {
        previous_override: Option<Option<&'static dyn Catalog>>,
        previous_installs_are_thread_local: bool,
        _not_send: PhantomData<*const ()>,
    }

    impl ThreadLocalInstalls {
        pub fn new() -> Self {
            Self {
                previous_override: THREAD_OVERRIDE.with(|cell| cell.replace(Some(None))),
                previous_installs_are_thread_local: INSTALLS_ARE_THREAD_LOCAL
                    .with(|cell| cell.replace(true)),
                _not_send: PhantomData,
            }
        }
    }

    impl Default for ThreadLocalInstalls {
        fn default() -> Self {
            Self::new()
        }
    }

    impl Drop for ThreadLocalInstalls {
        fn drop(&mut self) {
            INSTALLS_ARE_THREAD_LOCAL
                .with(|cell| cell.set(self.previous_installs_are_thread_local));
            THREAD_OVERRIDE.with(|cell| cell.set(self.previous_override));
        }
    }
}

mod diagnostics {
    #[cfg(debug_assertions)]
    use std::collections::HashSet;
    #[cfg(debug_assertions)]
    use std::hash::{DefaultHasher, Hash, Hasher};
    #[cfg(debug_assertions)]
    use std::sync::LazyLock;

    // `tr` is called with run-time strings, so the set of distinct messages is not
    // bounded by the source; past this many reports the rest stay quiet.
    #[cfg(debug_assertions)]
    pub(crate) const MAX_REPORTED: usize = 4096;

    // Hashes rather than the strings themselves, so a message that was already
    // reported costs no allocation on the next lookup.
    #[cfg(debug_assertions)]
    static REPORTED: LazyLock<parking_lot::Mutex<HashSet<u64>>> = LazyLock::new(Default::default);

    #[cfg(debug_assertions)]
    pub(crate) fn first_time(parts: &[&str]) -> bool {
        let mut hasher = DefaultHasher::new();
        parts.hash(&mut hasher);
        let key = hasher.finish();
        let mut reported = REPORTED.lock();
        if reported.len() >= MAX_REPORTED {
            return false;
        }
        reported.insert(key)
    }

    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    pub(crate) fn report_missing(context: Option<&str>, msgid: &str) {
        #[cfg(debug_assertions)]
        if log::log_enabled!(target: "i18n", log::Level::Debug)
            && first_time(&["missing", context.unwrap_or("\u{4}"), msgid])
        {
            log::debug!(target: "i18n", "no translation for {msgid:?} (context {context:?})");
        }
    }

    #[cfg_attr(not(debug_assertions), allow(unused_variables))]
    pub(crate) fn report_broken_translation(msgid: &str, translation: &str) {
        #[cfg(debug_assertions)]
        if log::log_enabled!(target: "i18n", log::Level::Warn)
            && first_time(&["broken", msgid, translation])
        {
            log::warn!(
                target: "i18n",
                "the translation {translation:?} of {msgid:?} has a placeholder that cannot be resolved; showing the English text"
            );
        }
    }
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::diagnostics::{MAX_REPORTED, first_time};

    #[test]
    fn diagnostics_report_each_message_once_and_stay_bounded() {
        assert!(first_time(&["test", "repeated"]));
        assert!(!first_time(&["test", "repeated"]));
        assert!(first_time(&["test", "a", "bc"]));
        assert!(first_time(&["test", "ab", "c"]));

        let mut first_reports = 3;
        for index in 0..MAX_REPORTED * 2 {
            if first_time(&["test", &index.to_string()]) {
                first_reports += 1;
            }
        }
        assert_eq!(first_reports, MAX_REPORTED);
        assert!(!first_time(&["test", "after the limit"]));
    }
}

#[doc(hidden)]
pub mod __private {
    use super::{diagnostics, lookup_in_current, lookup_plural_in_current};

    /// Substitutes already formatted arguments into a runtime template.
    ///
    /// Positional arguments are named "0", "1", ...; `{}` takes the next positional
    /// argument in order of appearance. Placeholders that cannot be resolved are kept
    /// as written instead of panicking.
    pub fn format_message(template: &str, arguments: &[(&str, &str)]) -> String {
        expand(template, arguments, false).unwrap_or_else(|| template.to_string())
    }

    /// Formats `template` (usually a translation), falling back to the English
    /// `msgid` when the template refers to an argument that does not exist.
    pub fn format_translated(template: &str, msgid: &str, arguments: &[(&str, &str)]) -> String {
        if template == msgid {
            return format_message(msgid, arguments);
        }
        match expand(template, arguments, true) {
            Some(formatted) => formatted,
            None => {
                diagnostics::report_broken_translation(msgid, template);
                format_message(msgid, arguments)
            }
        }
    }

    pub fn lookup_literal(context: Option<&'static str>, msgid: &'static str) -> &'static str {
        lookup_in_current(context, msgid).unwrap_or(msgid)
    }

    pub fn lookup_plural_literal(
        context: Option<&'static str>,
        singular: &'static str,
        plural: &'static str,
        count: u64,
    ) -> &'static str {
        lookup_plural_in_current(context, singular, plural, count)
            .unwrap_or_else(|| english_plural(singular, plural, count))
    }

    /// The English plural rule, used when no catalog has the message.
    pub fn english_plural<'a>(singular: &'a str, plural: &'a str, count: u64) -> &'a str {
        if count == 1 { singular } else { plural }
    }

    /// Converts the count given to `tn!` and `tcn!` into the value plural rules take.
    pub trait PluralCount {
        fn plural_count(&self) -> u64;
    }

    impl<T: PluralCount + ?Sized> PluralCount for &T {
        fn plural_count(&self) -> u64 {
            (**self).plural_count()
        }
    }

    macro_rules! impl_plural_count_unsigned {
        ($($type:ty),*) => {$(
            impl PluralCount for $type {
                fn plural_count(&self) -> u64 {
                    u64::try_from(*self).unwrap_or(u64::MAX)
                }
            }
        )*};
    }

    macro_rules! impl_plural_count_signed {
        ($($type:ty),*) => {$(
            impl PluralCount for $type {
                fn plural_count(&self) -> u64 {
                    u64::try_from(self.unsigned_abs()).unwrap_or(u64::MAX)
                }
            }
        )*};
    }

    impl_plural_count_unsigned!(u8, u16, u32, u64, u128, usize);
    impl_plural_count_signed!(i8, i16, i32, i64, i128, isize);

    /// In strict mode any unresolvable placeholder or stray brace makes the whole
    /// expansion fail; otherwise such text is copied through unchanged.
    fn expand(template: &str, arguments: &[(&str, &str)], strict: bool) -> Option<String> {
        let mut output = String::with_capacity(template.len());
        let mut next_index = 0usize;
        let mut rest = template;
        while let Some(position) = rest.find(['{', '}']) {
            output.push_str(&rest[..position]);
            let tail = &rest[position..];
            if let Some(after) = tail.strip_prefix("{{") {
                output.push('{');
                rest = after;
            } else if let Some(after) = tail.strip_prefix("}}") {
                output.push('}');
                rest = after;
            } else if let Some(after) = tail.strip_prefix('}') {
                if strict {
                    return None;
                }
                output.push('}');
                rest = after;
            } else {
                let Some(close) = tail.find('}') else {
                    if strict {
                        return None;
                    }
                    output.push_str(tail);
                    rest = "";
                    break;
                };
                let placeholder = &tail[..=close];
                let key = &tail[1..close];
                let value = if key.is_empty() {
                    let index = next_index;
                    next_index += 1;
                    let key = index.to_string();
                    find_argument(arguments, &key)
                } else if key.contains('{') {
                    None
                } else {
                    find_argument(arguments, key)
                };
                match value {
                    Some(value) => output.push_str(value),
                    None if strict => return None,
                    None => output.push_str(placeholder),
                }
                rest = &tail[close + 1..];
            }
        }
        output.push_str(rest);
        Some(output)
    }

    fn find_argument<'a>(arguments: &[(&str, &'a str)], key: &str) -> Option<&'a str> {
        arguments
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    }
}
