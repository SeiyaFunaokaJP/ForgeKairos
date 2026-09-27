// Calls taken from upstream `format!` sites (crates/ui, workspace, editor, git_ui,
// agent_ui, ...) with only the macro name swapped, to check that `t!` accepts the same
// input and, without a catalog, produces the same text.

#[macro_use]
extern crate i18n;

use std::cell::RefCell;
use std::fmt;
use std::path::PathBuf;

macro_rules! assert_same_as_format {
    ($($arguments:tt)*) => {{
        let translated: &str = &t!($($arguments)*);
        assert_eq!(translated, format!($($arguments)*));
    }};
}

struct Skill {
    name: &'static str,
    source: &'static str,
}

struct Entry {
    name: String,
}

struct Model(&'static str);

struct Selected {
    model: Model,
}

struct Position;

impl Position {
    fn label(&self) -> &'static str {
        "left"
    }
}

struct Character;

impl Character {
    fn description(&self) -> String {
        "space".to_string()
    }
}

struct Uri;

impl Uri {
    fn as_link(&self) -> impl fmt::Display {
        "[@a](file:///a)"
    }
}

struct AlternateError;

impl fmt::Display for AlternateError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if formatter.alternate() {
            write!(formatter, "outer: inner")
        } else {
            write!(formatter, "outer")
        }
    }
}

fn property_title(name: &str, property: u32) -> String {
    format!("{name}#{property}")
}

#[test]
fn upstream_format_calls_produce_the_same_text() {
    let worktree_paths = vec![PathBuf::from("a"), PathBuf::from("b")];
    assert_same_as_format!("{:?}", worktree_paths);
    let uri = Uri;
    assert_same_as_format!("{} ", uri.as_link());
    let line_number = 12;
    assert_same_as_format!(" L{}", line_number);
    let (name, property) = ("field", 3);
    assert_same_as_format!("{} is required", property_title(name, property));
    let title = "Pattern";
    assert_same_as_format!("{title} has a validation pattern that is too complex");
    let provider = "Ollama";
    assert_same_as_format!("No credentials configured for {provider}.");
    let skill = Skill {
        name: "review",
        source: "user",
    };
    assert_same_as_format!("{} ({})", skill.name, skill.source);
    let character = Character;
    assert_same_as_format!("• {}", character.description());
    let selected_model = Selected {
        model: Model("llama"),
    };
    assert_same_as_format!("Model {} was not found", selected_model.model.0);
    let i = 7;
    assert_same_as_format!("Legacy Thread {i}");
    let (simple_brackets_highlights, footer) = ("[]", "--");
    assert_same_as_format!("{simple_brackets_highlights}\n{footer}");
    let err = AlternateError;
    assert_same_as_format!("Failed to open permalink: {err}");
    assert_same_as_format!("{:#}", err);
    assert_same_as_format!("{err:#}");
    let struct_label = "Point";
    assert_same_as_format!("A tooltip for `{struct_label}`");
    assert_same_as_format!("fn func_{i}() {{}}\n");
    let id = Some(4_u32);
    assert_same_as_format!("switch_group_{:?}", id);
    assert_same_as_format!("{:#?}", id);
    let (entry, ix) = (
        Entry {
            name: "src".to_string(),
        },
        2,
    );
    assert_same_as_format!("dir_{}_{}", entry.name, ix);
    assert_same_as_format!("Clipboard ↔ {} @ L1:1-3", concat!("test/", "text.txt"));
    let display_path = "\\\\wsl$\\Ubuntu";
    assert_same_as_format!(
        "{display_path} is inside a WSL filesystem, some features may not work unless you open it with WSL remote"
    );
    let (flags, trailing_flags) = ("-rf", "");
    assert_same_as_format!(r"\brm\s+{flags}(--\s+)?(\$HOME|\$\{{HOME\}})/?(\*)?{trailing_flags}$");
    assert_same_as_format!("fn main() {{\n{}\n}}", "let i = 5;\n".repeat(3));
    let message = "Updating";
    let progress = 0.456_f64;
    assert_same_as_format!(
        "{message} ({:.0}% downloaded)",
        progress.clamp(0.0, 1.0) * 100.0
    );
    let (data, expected_len) = (vec![1, 2], 3);
    assert_same_as_format!(
        "Row length {} does not match expected {}",
        data.len(),
        expected_len
    );
    let position = Position;
    assert_same_as_format!(
        "Couldn't find any enabled panel for the {} dock.",
        position.label()
    );
    let file_names = ["a", "b", "c", "d", "e", "f", "g"];
    assert_same_as_format!(
        "{}\n.. and {} more",
        file_names
            .iter()
            .take(5)
            .copied()
            .collect::<Vec<_>>()
            .join("\n"),
        file_names.len() - 5
    );
    let codepoint = 'é';
    assert_same_as_format!("U+{:04X}", codepoint as u32);
    assert_same_as_format!("Unicode character U+{:02X}", codepoint as u32);
    // Upstream writes `({i})` here; one argument with two specs is not supported.
    assert_same_as_format!("    let var_{i:04} = ({});\n", i);
    let pointee = 5;
    let reference = &pointee;
    assert_same_as_format!("{:p} {reference:p}", reference);
    let buffer_row = 8;
    assert_same_as_format!("{:>3} ", buffer_row + 1);
    let (diff_marker, line_bytes, cumulative) = ("+", 12, 340);
    assert_same_as_format!("{}{:>3}({:>4}) ", diff_marker, line_bytes, cumulative);
    let workspace_id = Some(9_u64);
    assert_same_as_format!("persisting file folds for workspace {workspace_id:?}");
    let filename = "image.png";
    assert_same_as_format!("![$1]({filename})$0");
    let (lens_label, lens_text) = ("refs", "3");
    assert_same_as_format!(
        "{lens_label:<8}: {lens_text}\n\
         {line_number:<8}: next"
    );
    assert_same_as_format!("{0} {1} {0}", "left", "right");
    assert_same_as_format!("{} {0} {}", "first", "second");
    assert_same_as_format!("{name}", name = "named");
    assert_same_as_format!(
        "{:+} {:e} {:#x} {:08.3} {:^9}",
        5,
        1500.0_f64,
        255,
        -1.5_f64,
        "mid"
    );
    assert_same_as_format!("{:?} {:?}", "quoted \"text\"", '\n');
    assert_same_as_format!("{name :?}, {1 }, { }", 1, 2);
    assert_same_as_format!("{name }, {name\n}");
    let 名前 = "unicode";
    assert_same_as_format!("{名前}");
    assert_same_as_format!("trailing comma {}", 1,);
    assert_same_as_format!("{{{name}}} {{}}");
    assert_same_as_format!("");
    assert_same_as_format!("   ");
}

fn parse_number(text: &str) -> Result<String, std::num::ParseIntError> {
    Ok(t!("Parsed {}", text.parse::<u32>()?))
}

#[test]
fn arguments_may_use_question_mark() {
    assert_eq!(parse_number("42"), Ok("Parsed 42".to_string()));
    assert!(parse_number("x").is_err());
}

#[test]
fn nested_macros_and_closures() {
    let inner = t!("{}", t!("x"));
    assert_eq!(inner, "x");
    let nested = t!(
        "{} and {}",
        t!("{} {}", 1, 2),
        tn!(2, "{count} item", "{count} items")
    );
    assert_eq!(nested, "1 2 and 2 items");
    let labels: Vec<String> = (1..=2).map(|index| t!("Item {index}")).collect();
    assert_eq!(labels, vec!["Item 1", "Item 2"]);
    let prefix = String::from("p");
    let make = move |index: usize| t!("{prefix}-{index}");
    assert_eq!(make(3), "p-3");
}

#[test]
fn literal_messages_work_in_branches() {
    for flag in [true, false] {
        let label: &'static str = if flag { t!("On") } else { t!("Off") };
        let other: &'static str = match flag {
            true => t!("Yes"),
            false => tc!("answer", "No"),
        };
        assert_eq!(
            (label, other),
            if flag { ("On", "Yes") } else { ("Off", "No") }
        );
    }
}

struct DropProbe<'a> {
    log: &'a RefCell<Vec<&'static str>>,
}

impl fmt::Display for DropProbe<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("probe")
    }
}

impl Drop for DropProbe<'_> {
    fn drop(&mut self) {
        self.log.borrow_mut().push("dropped");
    }
}

fn observe(text: String, log: &RefCell<Vec<&'static str>>) -> (String, Vec<&'static str>) {
    (text, log.borrow().clone())
}

// `format!` drops the temporaries of its arguments before the surrounding expression
// continues. Keeping them until the end of the statement would hold guards such as
// `RefCell` borrows across `.await` and make futures `!Send`.
#[test]
fn argument_temporaries_end_with_the_macro() {
    let log = RefCell::new(Vec::new());
    let (text, seen) = observe(t!("{}", DropProbe { log: &log }), &log);
    assert_eq!(text, "probe");
    assert_eq!(seen, vec!["dropped"]);

    let log = RefCell::new(Vec::new());
    let (text, seen) = observe(
        tn!(1, "{count} {}", "{count} {}s", DropProbe { log: &log }),
        &log,
    );
    assert_eq!(text, "1 probe");
    assert_eq!(seen, vec!["dropped"]);
}

fn assert_send<T: Send>(value: T) -> T {
    value
}

async fn consume(text: String) -> usize {
    text.len()
}

#[test]
fn futures_stay_send_when_arguments_are_not() {
    let future = assert_send(async {
        let first = consume(t!("{}", std::rc::Rc::new(1))).await;
        let second = consume(tn!(2, "{count} {}", "{count} {}s", std::rc::Rc::new(2))).await;
        first + second
    });
    drop(future);
}

struct Tab {
    title: &'static str,
}

impl fmt::Display for Tab {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if formatter.alternate() {
            write!(formatter, "tab {}", self.title)
        } else {
            formatter.write_str(self.title)
        }
    }
}

impl Tab {
    fn describe(&self) -> (String, String) {
        (t!("Close {self}"), t!("Close {self:#} and {}", self.title))
    }
}

#[test]
fn self_can_be_captured() {
    let tab = Tab { title: "main.rs" };
    assert_eq!(
        tab.describe(),
        (
            "Close main.rs".to_string(),
            "Close tab main.rs and main.rs".to_string()
        )
    );
}
