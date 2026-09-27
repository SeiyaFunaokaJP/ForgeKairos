// `install` changes process-wide state, so it lives in its own test binary where no
// other test reads the global catalog concurrently.

use i18n::test_support::LocaleOverride;
use i18n::{Catalog, current_locale, generation, install, t, tr};

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

#[test]
fn install_switches_every_thread_and_bumps_the_generation() {
    let initial_generation = generation();
    assert_eq!(current_locale(), "en");

    install(Some(&CATALOG));
    assert_eq!(generation(), initial_generation + 1);
    assert_eq!(current_locale(), "ja");
    assert_eq!(t!("Open"), "開く");
    assert_eq!(tr("Open"), "開く");
    let other_thread = std::thread::spawn(|| t!("Open"))
        .join()
        .map_err(|_| "the other thread panicked");
    assert_eq!(other_thread, Ok("開く"));

    {
        let _override = LocaleOverride::new(None);
        assert_eq!(current_locale(), "en");
        assert_eq!(t!("Open"), "Open");
    }
    assert_eq!(t!("Open"), "開く");

    install(None);
    assert_eq!(generation(), initial_generation + 2);
    assert_eq!(current_locale(), "en");
    assert_eq!(t!("Open"), "Open");

    readers_see_either_catalog_while_it_is_replaced();
    assert_eq!(generation(), initial_generation + 4002);
}

// Called from the test above rather than being its own test: both change the
// process-wide catalog and would race with each other's assertions.
fn readers_see_either_catalog_while_it_is_replaced() {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let stop = stop.clone();
            std::thread::spawn(move || {
                let mut unexpected = Vec::new();
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    let label = t!("Open");
                    if label != "Open" && label != "開く" {
                        unexpected.push(label);
                    }
                }
                unexpected
            })
        })
        .collect();
    for _ in 0..2000 {
        install(Some(&CATALOG));
        install(None);
    }
    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    for reader in readers {
        assert_eq!(
            reader.join().map_err(|_| "a reader panicked"),
            Ok(Vec::new())
        );
    }
}
