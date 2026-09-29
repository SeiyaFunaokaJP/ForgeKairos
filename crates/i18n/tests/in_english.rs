use i18n::test_support::LocaleOverride;
use i18n::{Bilingual, Catalog, current_locale, in_english, in_english_while_polling, tr};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

struct JapaneseCatalog;

impl Catalog for JapaneseCatalog {
    fn locale(&self) -> &str {
        "ja"
    }

    fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str> {
        match (context, msgid) {
            (None, "Open") => Some("開く"),
            (None, "Open {path}") => Some("{path} を開く"),
            _ => None,
        }
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

static CATALOG: JapaneseCatalog = JapaneseCatalog;

fn open_path(path: &str) -> String {
    i18n::t!("Open {path}")
}

#[test]
fn lookups_are_english_only_inside_the_closure() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    assert_eq!(tr("Open"), "開く");

    let (locale, label, message) =
        in_english(|| (current_locale(), i18n::t!("Open"), open_path("a.txt")));
    assert_eq!(locale, "en");
    assert_eq!(label, "Open");
    assert_eq!(message, "Open a.txt");

    assert_eq!(current_locale(), "ja");
    assert_eq!(tr("Open"), "開く");
    assert_eq!(open_path("a.txt"), "a.txt を開く");
}

#[test]
fn nested_calls_leave_the_outer_one_in_english() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    in_english(|| {
        assert_eq!(in_english(|| tr("Open")), "Open");
        assert_eq!(
            tr("Open"),
            "Open",
            "the inner call must not end the outer one"
        );

        // A locale override made inside does not bring the display language back.
        let _inner = LocaleOverride::new(Some(&CATALOG));
        assert_eq!(tr("Open"), "Open");
    });
    assert_eq!(tr("Open"), "開く");
}

#[test]
fn a_panic_inside_does_not_leave_the_thread_in_english() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    let result =
        std::panic::catch_unwind(|| in_english(|| -> () { panic!("building the message failed") }));
    assert!(result.is_err());
    assert_eq!(tr("Open"), "開く");
}

#[test]
fn other_threads_keep_their_language() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    in_english(|| {
        let other = std::thread::spawn(|| {
            let _japanese = LocaleOverride::new(Some(&CATALOG));
            tr("Open")
        })
        .join()
        .unwrap_or("the other thread panicked");
        assert_eq!(other, "開く");
    });
}

#[test]
fn a_future_is_in_english_only_while_it_is_polled() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    let mut seen = Vec::new();
    let mut yielded = false;
    let future = std::future::poll_fn(|cx| {
        seen.push(tr("Open"));
        if yielded {
            Poll::Ready(open_path("b.txt"))
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    });
    let mut future = in_english_while_polling(future);
    let mut context = Context::from_waker(Waker::noop());

    assert!(Pin::new(&mut future).poll(&mut context).is_pending());
    assert_eq!(
        tr("Open"),
        "開く",
        "between polls the thread is back in the display language"
    );
    assert_eq!(
        Pin::new(&mut future).poll(&mut context),
        Poll::Ready("Open b.txt".to_string())
    );
    assert_eq!(tr("Open"), "開く");
    drop(future);
    assert_eq!(seen, ["Open", "Open"]);
}

#[test]
fn a_panic_while_polling_does_not_leave_the_thread_in_english() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    let mut future = in_english_while_polling(std::future::poll_fn(|_| -> Poll<()> {
        panic!("polling failed")
    }));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        Pin::new(&mut future).poll(&mut Context::from_waker(Waker::noop()))
    }));
    assert!(result.is_err());
    assert_eq!(tr("Open"), "開く");
}

/// An executor may resume a task on another of its threads: English follows the
/// future there and stays on neither thread once the poll returns.
#[test]
fn a_future_resumed_on_another_thread_is_english_only_while_polled_there() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    let mut yielded = false;
    let mut future = in_english_while_polling(std::future::poll_fn(move |_| {
        if yielded {
            Poll::Ready(tr("Open"))
        } else {
            yielded = true;
            Poll::Pending
        }
    }));
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
    assert_eq!(tr("Open"), "開く");

    let (polled, after) = std::thread::spawn(move || {
        let _japanese = LocaleOverride::new(Some(&CATALOG));
        let polled = Pin::new(&mut future).poll(&mut Context::from_waker(Waker::noop()));
        (polled, tr("Open"))
    })
    .join()
    .unwrap_or((Poll::Pending, "the other thread panicked"));
    assert_eq!(polled, Poll::Ready("Open"));
    assert_eq!(after, "開く");
    assert_eq!(tr("Open"), "開く");
}

#[test]
fn a_bilingual_message_is_read_in_the_readers_language() {
    let _japanese = LocaleOverride::new(Some(&CATALOG));
    let message = Bilingual::new(|| open_path("c.txt"));
    assert_eq!(message.english(), "Open c.txt");
    assert_eq!(message.display(), "c.txt を開く");
    assert_eq!(message.to_string(), "c.txt を開く");
    assert_eq!(in_english(|| message.to_string()), "Open c.txt");

    // Built where a model's text is being built, it still keeps the user's language.
    let built_inside = in_english(|| Bilingual::new(|| open_path("d.txt")));
    assert_eq!(built_inside.english(), "Open d.txt");
    assert_eq!(built_inside.display(), "d.txt を開く");
    assert_eq!(built_inside.current(), "d.txt を開く");
    assert_eq!(
        in_english(|| built_inside.current().to_string()),
        "Open d.txt"
    );
    assert_eq!(tr("Open"), "開く");
}
