// Its own test binary: it checks what other threads see, so nothing else may install.

use i18n::test_support::{LocaleOverride, ThreadLocalInstalls};
use i18n::{Catalog, current_locale, generation, install, tr};

struct SingleEntryCatalog;

impl Catalog for SingleEntryCatalog {
    fn locale(&self) -> &str {
        "ja"
    }

    fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str> {
        (context.is_none() && msgid == "Open").then_some("開く")
    }

    fn lookup_plural(
        &self,
        _context: Option<&str>,
        _msgid: &str,
        _msgid_plural: &str,
        _count: u64,
    ) -> Option<&'static str> {
        None
    }
}

static CATALOG: SingleEntryCatalog = SingleEntryCatalog;

fn on_another_thread() -> (&'static str, &'static str, u64) {
    std::thread::spawn(|| (current_locale(), tr("Open"), generation()))
        .join()
        .unwrap_or(("the other thread panicked", "", 0))
}

#[test]
fn installs_stay_on_the_thread_while_the_guard_lives() {
    let initial_generation = generation();
    {
        let _installs = ThreadLocalInstalls::new();
        assert_eq!(current_locale(), "en");

        install(Some(&CATALOG));
        assert_eq!(generation(), initial_generation + 1);
        assert_eq!(current_locale(), "ja");
        assert_eq!(tr("Open"), "開く");
        assert_eq!(on_another_thread(), ("en", "Open", initial_generation));

        {
            let _override = LocaleOverride::new(None);
            assert_eq!(tr("Open"), "Open");
        }
        assert_eq!(tr("Open"), "開く");

        install(None);
        assert_eq!(current_locale(), "en");
        install(Some(&CATALOG));
    }
    assert_eq!(current_locale(), "en");
    assert_eq!(tr("Open"), "Open");
    assert_eq!(generation(), initial_generation + 3);

    install(Some(&CATALOG));
    assert_eq!(generation(), initial_generation + 4);
    assert_eq!(on_another_thread(), ("ja", "開く", initial_generation + 1));
    install(None);
}
