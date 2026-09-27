// Written the way consuming crates are expected to import the macros.
#[macro_use]
extern crate i18n;

use i18n::__private::format_message;
use i18n::test_support::LocaleOverride;
use i18n::{Catalog, current_locale, tr, tr_ctx, tr_ctx_opt, tr_plural};
use parking_lot::Mutex;

struct TestCatalog {
    locale: &'static str,
    entries: Vec<(Option<&'static str>, &'static str, &'static str)>,
    plural_entries: Vec<(Option<&'static str>, &'static str, Vec<&'static str>)>,
    plural_rule: fn(u64) -> usize,
    requested: Mutex<Vec<String>>,
}

impl Catalog for TestCatalog {
    fn locale(&self) -> &str {
        self.locale
    }

    fn lookup(&self, context: Option<&str>, msgid: &str) -> Option<&'static str> {
        self.requested.lock().push(msgid.to_string());
        self.entries
            .iter()
            .find(|(entry_context, entry_msgid, _)| {
                *entry_context == context && *entry_msgid == msgid
            })
            .map(|(_, _, translation)| *translation)
    }

    fn lookup_plural(
        &self,
        context: Option<&str>,
        msgid: &str,
        msgid_plural: &str,
        count: u64,
    ) -> Option<&'static str> {
        self.requested
            .lock()
            .push(format!("{msgid} | {msgid_plural}"));
        let (_, _, forms) =
            self.plural_entries
                .iter()
                .find(|(entry_context, entry_msgid, _)| {
                    *entry_context == context && *entry_msgid == msgid
                })?;
        forms.get((self.plural_rule)(count)).copied()
    }
}

impl TestCatalog {
    fn take_requested(&self) -> Vec<String> {
        std::mem::take(&mut *self.requested.lock())
    }
}

fn japanese() -> &'static TestCatalog {
    Box::leak(Box::new(TestCatalog {
        locale: "ja",
        entries: vec![
            (
                None,
                "",
                "Language: ja
",
            ),
            (
                Some("menu"),
                "",
                "Language: ja
",
            ),
            (None, "Found {} in {path}", "{path}で{}件"),
            (None, "Go to Definition", "定義へ移動"),
            (None, "Open", "開く"),
            (Some("menu"), "Open", "開く (メニュー)"),
            (None, "Close", "閉じる"),
            (None, "Select a {} for {}", "{1} の {0} を選択"),
            (None, "{} then {}", "{} の次に {}"),
            (None, "{0} or {1}", "{1} または {0}"),
            (None, "Open {path}", "{path} を開く"),
            (None, "{name} opened {file}", "{file} を {name} が開いた"),
            (None, "Progress {value}", "進捗 {value}"),
            (None, "Ratio {}", "比率 {}"),
            (None, "Debug {}", "デバッグ {}"),
            (None, "Hello {{name}}", "こんにちは {{name}}"),
            (None, "Broken {name}", "壊れた {missing}"),
            (None, "Broken {}", "壊れた {} {}"),
            (None, "Unbalanced {}", "不均衡 { {}"),
            (None, "Stray {}", "迷子 } {}"),
            (
                None,
                "Shadow {__i18n_formatted_0}",
                "影 {__i18n_formatted_0}",
            ),
        ],
        plural_entries: vec![
            (
                None,
                "",
                vec![
                    "Language: ja
",
                ],
            ),
            (None, "{count} Comment", vec!["コメント {count} 件"]),
            (None, "One file in {}", vec!["{} に {count} 個"]),
            (
                Some("files"),
                "{count} file in {}",
                vec!["{} に {count} 個"],
            ),
        ],
        plural_rule: |_| 0,
        requested: Mutex::new(Vec::new()),
    }))
}

fn french() -> &'static TestCatalog {
    Box::leak(Box::new(TestCatalog {
        locale: "fr",
        entries: vec![(None, "Open", "Ouvrir")],
        plural_entries: vec![(
            None,
            "{count} Comment",
            vec!["{count} commentaire", "{count} commentaires"],
        )],
        plural_rule: |count| usize::from(count > 1),
        requested: Mutex::new(Vec::new()),
    }))
}

fn assert_static(_: &'static str) {}

#[test]
fn every_macro_returns_english_without_a_catalog() {
    assert_eq!(current_locale(), "en");
    assert_eq!(t!("Go to Definition"), "Go to Definition");
    assert_eq!(
        t!("Select a {} for {}", "language", "buffer"),
        "Select a language for buffer"
    );
    let path = "a.rs";
    assert_eq!(t!("Open {path}"), "Open a.rs");
    assert_eq!(tn!(1, "{count} Comment", "{count} Comments"), "1 Comment");
    assert_eq!(tn!(2, "{count} Comment", "{count} Comments"), "2 Comments");
    assert_eq!(tn!(0, "{count} Comment", "{count} Comments"), "0 Comments");
    assert_eq!(tc!("menu", "Open"), "Open");
    assert_eq!(tc!("menu", "Open {path}"), "Open a.rs");
    assert_eq!(
        tcn!(
            "files",
            1,
            "{count} file in {}",
            "{count} files in {}",
            "src"
        ),
        "1 file in src"
    );
    assert_eq!(
        tcn!(
            "files",
            3,
            "{count} file in {}",
            "{count} files in {}",
            "src"
        ),
        "3 files in src"
    );
    assert_eq!(msg!("Go to Definition"), "Go to Definition");
    assert_eq!(tr("Go to Definition"), "Go to Definition");
    assert_eq!(tr_ctx("menu", "Open"), "Open");
    assert_eq!(tr_ctx_opt("menu", "Open"), None);
    assert_eq!(
        tr_plural(None, "{count} Comment", "{count} Comments", 1),
        "{count} Comment"
    );
    assert_eq!(
        tr_plural(None, "{count} Comment", "{count} Comments", 5),
        "{count} Comments"
    );
}

#[test]
fn an_override_catalog_translates_every_macro() {
    let catalog = japanese();
    let _override = LocaleOverride::new(Some(catalog));
    assert_eq!(current_locale(), "ja");
    assert_eq!(t!("Go to Definition"), "定義へ移動");
    assert_eq!(tc!("menu", "Open"), "開く (メニュー)");
    assert_eq!(
        tn!(1, "{count} Comment", "{count} Comments"),
        "コメント 1 件"
    );
    assert_eq!(
        tcn!(
            "files",
            4,
            "{count} file in {}",
            "{count} files in {}",
            "src"
        ),
        "src に 4 個"
    );
    const LABEL: &str = msg!("Go to Definition");
    assert_eq!(LABEL, "Go to Definition");
    assert_eq!(tr(LABEL), "定義へ移動");
    assert_eq!(tr_ctx("menu", "Open"), "開く (メニュー)");
    assert_eq!(tr_ctx_opt("menu", "Open"), Some("開く (メニュー)"));
    assert_eq!(tr_ctx_opt("menu", "Close"), None);
    assert_eq!(
        tr_plural(None, "{count} Comment", "{count} Comments", 7),
        "コメント {count} 件"
    );
    assert_eq!(t!("Not in the catalog"), "Not in the catalog");
}

#[test]
fn translations_reorder_positional_arguments() {
    let _override = LocaleOverride::new(Some(japanese()));
    assert_eq!(
        t!("Select a {} for {}", "language", "buffer"),
        "buffer の language を選択"
    );
    assert_eq!(t!("{} then {}", "a", "b"), "a の次に b");
    assert_eq!(t!("{0} or {1}", "left", "right"), "right または left");
}

#[test]
fn named_arguments_implicit_captures_and_specs_apply_to_translations() {
    let catalog = japanese();
    let _override = LocaleOverride::new(Some(catalog));

    let path = "main.rs";
    assert_eq!(t!("Open {path}"), "main.rs を開く");
    assert_eq!(t!("Open {path}", path = "lib.rs"), "lib.rs を開く");
    assert_eq!(
        t!("{name} opened {file}", name = "Alice", file = path),
        "main.rs を Alice が開いた"
    );

    let value = 2.3456_f64;
    assert_eq!(t!("Progress {value:.1}"), "進捗 2.3");
    assert_eq!(t!("Ratio {:.1}", 0.26_f64), "比率 0.3");
    assert_eq!(t!("Debug {:?}", "quoted"), "デバッグ \"quoted\"");

    catalog.take_requested();
    assert_eq!(t!("Progress {value:>8.2}"), "進捗     2.35");
    assert_eq!(
        catalog.take_requested(),
        vec!["Progress {value}".to_string()]
    );
}

#[test]
fn msgids_are_normalized() {
    let catalog = japanese();
    let _override = LocaleOverride::new(Some(catalog));
    let value = 1.5_f64;
    let name = "x";
    catalog.take_requested();
    let _formatted = t!("A {value:.1} {:?} {1} {name:>4} {{b}} {}", 7, 8);
    let _formatted = tn!(2, "{count:>3} Item {name:?}", "{count:>3} Items {name:?}");
    let _formatted = tc!("menu", "Plain");
    assert_eq!(
        catalog.take_requested(),
        vec![
            "A {value} {} {1} {name} {{b}} {}".to_string(),
            "{count} Item {name} | {count} Items {name}".to_string(),
            "Plain".to_string(),
        ]
    );
}

#[test]
fn plural_forms_follow_the_catalog_rule_and_english_without_one() {
    assert_eq!(
        tn!(1_usize, "{count} Comment", "{count} Comments"),
        "1 Comment"
    );
    assert_eq!(
        tn!(2_u32, "{count} Comment", "{count} Comments"),
        "2 Comments"
    );
    let negative = -1_i32;
    assert_eq!(
        tn!(negative, "{count} Comment", "{count} Comments"),
        "-1 Comment"
    );
    let borrowed = &2_usize;
    assert_eq!(
        tn!(borrowed, "{count} Comment", "{count} Comments"),
        "2 Comments"
    );

    {
        let _override = LocaleOverride::new(Some(japanese()));
        assert_eq!(
            tn!(1, "{count} Comment", "{count} Comments"),
            "コメント 1 件"
        );
        assert_eq!(
            tn!(2, "{count} Comment", "{count} Comments"),
            "コメント 2 件"
        );
    }
    {
        let _override = LocaleOverride::new(Some(french()));
        assert_eq!(
            tn!(0, "{count} Comment", "{count} Comments"),
            "0 commentaire"
        );
        assert_eq!(
            tn!(1, "{count} Comment", "{count} Comments"),
            "1 commentaire"
        );
        assert_eq!(
            tn!(2, "{count} Comment", "{count} Comments"),
            "2 commentaires"
        );
    }
}

#[test]
fn context_and_no_context_are_separate_messages() {
    let _override = LocaleOverride::new(Some(japanese()));
    assert_eq!(t!("Open"), "開く");
    assert_eq!(tc!("menu", "Open"), "開く (メニュー)");
    assert_eq!(tc!("toolbar", "Open"), "Open");
    assert_eq!(tr("Open"), "開く");
    assert_eq!(tr_ctx("menu", "Open"), "開く (メニュー)");
    assert_eq!(tr_ctx("toolbar", "Open"), "Open");
    assert_eq!(tc!("menu", "Close"), "Close");
}

#[test]
fn literal_messages_are_static_str() {
    let _override = LocaleOverride::new(Some(japanese()));
    let static_label: &'static str = t!("Close");
    assert_static(static_label);
    assert_static(tc!("menu", "Open"));
    let labels: &[&str] = &[t!("Open"), t!("Close"), tc!("menu", "Open")];
    assert_eq!(labels, &["開く", "閉じる", "開く (メニュー)"]);

    const CONSTANT_LABEL: &str = msg!("Close");
    assert_eq!(CONSTANT_LABEL, "Close");
    static STATIC_LABELS: [&str; 2] = [msg!("Open"), msg!("Close")];
    assert_static(tr(CONSTANT_LABEL));
    assert_eq!(tr(CONSTANT_LABEL), "閉じる");
    assert_eq!(STATIC_LABELS.map(tr), ["開く", "閉じる"]);

    let runtime_text = String::from("Open");
    let borrowed: &str = tr(&runtime_text);
    assert_eq!(borrowed, "開く");
}

#[test]
fn broken_translation_placeholders_fall_back_to_english() {
    let _override = LocaleOverride::new(Some(japanese()));
    let name = "Alice";
    assert_eq!(t!("Broken {name}"), "Broken Alice");
    assert_eq!(t!("Broken {}", "one"), "Broken one");
    assert_eq!(t!("Unbalanced {}", "one"), "Unbalanced one");
    assert_eq!(t!("Stray {}", "one"), "Stray one");

    assert_eq!(
        format_message("a {missing} {} {0}", &[]),
        "a {missing} {} {0}"
    );
    assert_eq!(format_message("a {b", &[("b", "x")]), "a {b");
    assert_eq!(format_message("} {x} {{ }}", &[("x", "1")]), "} 1 { }");
    assert_eq!(
        format_message("{} {} {0} {1}", &[("0", "a"), ("1", "b")]),
        "a b a b"
    );
    assert_eq!(format_message("{a{b}", &[("b", "x")]), "{a{b}");
}

#[test]
fn escaped_braces_stay_literal() {
    assert_eq!(t!("Hello {{name}}"), "Hello {name}");
    let _override = LocaleOverride::new(Some(japanese()));
    assert_eq!(t!("Hello {{name}}"), "こんにちは {name}");
}

#[test]
fn arguments_are_evaluated_once_in_order() {
    let mut calls = Vec::new();
    let mut record = |value: &'static str| {
        calls.push(value);
        value
    };
    let formatted = t!(
        "{1} {0} {0} {name}",
        record("a"),
        record("b"),
        name = record("c")
    );
    assert_eq!(formatted, "b a a c");
    assert_eq!(calls, vec!["a", "b", "c"]);

    let mut count_calls = 0;
    let mut next_count = || {
        count_calls += 1;
        2
    };
    assert_eq!(
        tn!(next_count(), "{count} Comment", "{count} Comments"),
        "2 Comments"
    );
    assert_eq!(count_calls, 1);
}

#[test]
fn generated_bindings_do_not_capture_caller_variables() {
    let _override = LocaleOverride::new(Some(japanese()));
    let __i18n_formatted_0 = "shadowed";
    assert_eq!(t!("Shadow {__i18n_formatted_0}"), "影 shadowed");
    let count = 9;
    assert_eq!(t!("Open {count}"), "Open 9");
}

#[test]
fn overrides_nest_and_restore_on_drop() {
    assert_eq!(current_locale(), "en");
    {
        let _outer = LocaleOverride::new(Some(japanese()));
        assert_eq!(t!("Open"), "開く");
        {
            let _middle = LocaleOverride::new(Some(french()));
            assert_eq!(current_locale(), "fr");
            assert_eq!(t!("Open"), "Ouvrir");
            {
                let _inner = LocaleOverride::new(None);
                assert_eq!(current_locale(), "en");
                assert_eq!(t!("Open"), "Open");
            }
            assert_eq!(current_locale(), "fr");
            assert_eq!(t!("Open"), "Ouvrir");
        }
        assert_eq!(current_locale(), "ja");
        assert_eq!(t!("Open"), "開く");
    }
    assert_eq!(current_locale(), "en");
    assert_eq!(t!("Open"), "Open");
}

#[test]
fn overrides_stay_on_their_thread() {
    let _override = LocaleOverride::new(Some(japanese()));
    assert_eq!(t!("Open"), "開く");
    let other_thread = std::thread::spawn(|| (current_locale(), t!("Open")))
        .join()
        .map_err(|_| "the other thread panicked");
    assert_eq!(other_thread, Ok(("en", "Open")));
}

#[test]
fn the_empty_msgid_is_never_looked_up() {
    let catalog = japanese();
    let _override = LocaleOverride::new(Some(catalog));
    catalog.take_requested();
    assert_eq!(t!(""), "");
    assert_eq!(tc!("menu", ""), "");
    assert_eq!(tr(""), "");
    assert_eq!(tr_ctx("menu", ""), "");
    assert_eq!(tr_ctx_opt("menu", ""), None);
    assert_eq!(tr_plural(None, "", "", 1), "");
    assert_eq!(catalog.take_requested(), Vec::<String>::new());
}

#[test]
fn translations_may_use_arguments_of_either_plural_form() {
    assert_eq!(
        tn!(1, "One file in {}", "{count} files in {}", "src"),
        "One file in src"
    );
    assert_eq!(
        tn!(3, "One file in {}", "{count} files in {}", "src"),
        "3 files in src"
    );
    let _override = LocaleOverride::new(Some(japanese()));
    assert_eq!(
        tn!(1, "One file in {}", "{count} files in {}", "src"),
        "src に 1 個"
    );
}

#[test]
fn placeholders_next_to_multibyte_text_expand() {
    let _override = LocaleOverride::new(Some(japanese()));
    let path = "ルート";
    assert_eq!(t!("Found {} in {path}", 3), "ルートで3件");
    assert_eq!(
        format_message("件{}件{x}件", &[("0", "一"), ("x", "二")]),
        "件一件二件"
    );
    assert_eq!(
        format_message(
            "{file} {f} {1} {10}",
            &[("file", "A"), ("f", "B"), ("10", "J"), ("1", "I")]
        ),
        "A B I J"
    );
}

#[test]
fn caller_variables_with_common_names_are_not_shadowed() {
    let arguments = "a";
    let template = "t";
    let count = 5;
    let context = "c";
    let msgid = "m";
    let __i18n_message = "hidden";
    let __i18n_count = 7;
    assert_eq!(
        t!("{arguments} {template} {count} {context} {msgid} {__i18n_message} {__i18n_count}"),
        "a t 5 c m hidden 7"
    );
    assert_eq!(
        tn!(
            2,
            "{count} of {arguments}",
            "{count} of {arguments} ({__i18n_count})"
        ),
        "2 of a (7)"
    );
    assert_eq!(t!("{}", t!("{count} {}", t!("{arguments}"))), "5 a");
}
