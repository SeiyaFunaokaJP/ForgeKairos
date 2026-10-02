use std::path::Path;

use gpui::{TestAppContext, UpdateGlobal as _};
use project::{FakeFs, symbol_index_store::build_index_for_tests};
use serde_json::json;
use settings::SettingsStore;
use util::path;

use super::*;

const TYPES_H: &str = "\
#define MAX_ITEMS 16
typedef unsigned int u32;
struct inner {
\tu32 id; /* the id */
\tchar name[8];
};
struct outer {
\tstruct inner first;
\tstruct outer *parent;
#ifdef CONFIG_DEBUG
\tint debug;
#endif
};
struct dup { int in_types; };
";

const OTHER_DUP_H: &str = "struct dup { int in_other; };\n";

const MAIN_C: &str = "\
#include \"../inc/types.h\"
int use(struct outer *value)
{
\tint count = 0;
\treturn value->first.id + count + MAX_ITEMS;
}
";

const SECRET_H: &str = "struct secret_type { int password_member; };\n";

struct Setup {
    project: Entity<Project>,
    _index_directory: Option<tempfile::TempDir>,
}

async fn setup(with_index: bool, cx: &mut TestAppContext) -> Setup {
    cx.update(|cx| {
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files =
                    Some(vec!["**/secret/**".to_string()].into());
            });
        });
    });
    cx.executor().allow_parking();
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "inc": { "types.h": TYPES_H },
            "other": { "dup.h": OTHER_DUP_H },
            "src": { "main.c": MAIN_C, "main.rs": "fn main() {}\n" },
            "secret": { "secret.h": SECRET_H },
        }),
    )
    .await;
    let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;
    cx.run_until_parked();
    let mut index_directory = None;
    if with_index {
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id()
        });
        let directory = tempfile::tempdir().expect("a temporary directory");
        let index = build_index_for_tests(
            directory.path(),
            &[
                ("inc/types.h", TYPES_H),
                ("other/dup.h", OTHER_DUP_H),
                ("src/main.c", MAIN_C),
                ("secret/secret.h", SECRET_H),
            ],
        )
        .expect("the index");
        let store = project.read_with(cx, |project, _| project.symbol_index_store().clone());
        store.update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        });
        cx.run_until_parked();
        index_directory = Some(directory);
    }
    Setup {
        project,
        _index_directory: index_directory,
    }
}

fn by_name(name: &str) -> TypeLayoutToolInput {
    TypeLayoutToolInput {
        name: Some(name.to_string()),
        near_path: None,
        path: None,
        line: None,
        symbol: None,
        depth: None,
        max_chars: None,
    }
}

fn at(path: &str, line: u32, symbol: &str) -> TypeLayoutToolInput {
    TypeLayoutToolInput {
        name: None,
        path: Some(path.to_string()),
        line: Some(line),
        symbol: Some(symbol.to_string()),
        ..by_name("")
    }
}

async fn run(
    setup: &Setup,
    input: TypeLayoutToolInput,
    cx: &mut TestAppContext,
) -> (Result<String, String>, Vec<acp::ToolCallUpdateFields>) {
    let tool = Arc::new(TypeLayoutTool::new(setup.project.clone()));
    let (event_stream, mut receiver) = ToolCallEventStream::test();
    // Paths are shown with the platform's separators; the expected texts use `/`.
    let output = cx
        .update(|cx| tool.run(ToolInput::resolved(input), event_stream, cx))
        .await
        .map(|output| output.replace('\\', "/"));
    let mut updates = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let Ok(crate::ThreadEvent::ToolCallUpdate(acp_thread::ToolCallUpdate::UpdateFields(
            update,
        ))) = event
        {
            updates.push(update.fields);
        }
    }
    (output, updates)
}

#[gpui::test]
async fn test_the_output_names_the_match_the_markers_and_the_other_definitions(
    cx: &mut TestAppContext,
) {
    let setup = setup(true, cx).await;

    let (output, updates) = run(&setup, by_name("struct outer"), cx).await;
    assert_eq!(
        output.expect("the layout"),
        "struct outer — defined at root/inc/types.h:7 (name match)\n\
         \n\
         struct outer  (inc/types.h:7)\n\
         \x20 struct inner first  (inc/types.h:3)\n\
         \x20   u32 id  (= unsigned int)  // the id\n\
         \x20   char name[8]\n\
         \x20 struct outer *parent  (recursive)\n\
         \x20 int debug  [#if CONFIG_DEBUG]\n\
         \n\
         Paths above are relative to the `root` project folder.\n\
         Markers: (recursive): the type contains itself; [#if: the member exists only under that preprocessor condition.\n\
         This is a name match, not a compiler's view: members under every `#if` branch are listed and macros are not expanded.\n"
    );
    let locations = updates
        .iter()
        .find_map(|update| update.locations.clone())
        .expect("the card lists the definitions");
    assert_eq!(
        locations
            .iter()
            .map(|location| (location.path.clone(), location.line))
            .collect::<Vec<_>>(),
        vec![
            (Path::new(path!("/root/inc/types.h")).to_path_buf(), Some(6)),
            (Path::new(path!("/root/inc/types.h")).to_path_buf(), Some(2)),
            (Path::new(path!("/root/inc/types.h")).to_path_buf(), Some(1)),
        ]
    );

    let (output, _) = run(
        &setup,
        TypeLayoutToolInput {
            near_path: Some("root/other/dup.h".to_string()),
            ..by_name("dup")
        },
        cx,
    )
    .await;
    assert_eq!(
        output.expect("the layout"),
        "struct dup — defined at root/other/dup.h:1 (name match; 2 definitions with this name, showing the nearest)\n\
         \n\
         struct dup  (other/dup.h:1)\n\
         \x20 int in_other\n\
         \n\
         Paths above are relative to the `root` project folder.\n\
         This is a name match, not a compiler's view: members under every `#if` branch are listed and macros are not expanded.\n\
         Other definitions with this name: root/inc/types.h:14. Ask with `near_path` set to the file that uses the type to prefer another one.\n"
    );
}

#[gpui::test]
async fn test_positions_macros_and_missing_names(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let (output, _) = run(&setup, at("root/src/main.c", 5, "value"), cx).await;
    let output = output.expect("the layout");
    assert!(
        output.starts_with(
            "`value` is declared as `struct outer *` (a parameter).\n\
             struct outer — defined at root/inc/types.h:7 (name match)\n"
        ),
        "{output}"
    );

    let (output, _) = run(&setup, at("root/src/main.c", 5, "first"), cx).await;
    let output = output.expect("the layout");
    assert!(
        output.starts_with(
            "`first` is a member of `struct outer` declared as `struct inner`.\n\
             struct inner — defined at root/inc/types.h:3 (name match)\n"
        ),
        "{output}"
    );

    let (output, _) = run(&setup, at("root/src/main.c", 5, "count"), cx).await;
    assert_eq!(
        output.expect("an answer"),
        "`count` is declared as `int` (a local variable).\n\
         That type is not a struct, union, enum or typedef defined in the project's C or C++ files (a builtin type, or one the index doesn't have).\n"
    );

    let (output, _) = run(&setup, by_name("MAX_ITEMS"), cx).await;
    assert_eq!(
        output.expect("the macro"),
        "Macro `MAX_ITEMS` — defined at root/inc/types.h:1 (name match; macros are not expanded).\n\
         \n\
         inc/types.h:1\n\
         \x20 #define MAX_ITEMS 16\n\
         \n\
         Paths above are relative to the `root` project folder.\n"
    );

    let (output, _) = run(&setup, by_name("no_such_type"), cx).await;
    assert_eq!(
        output.expect("an answer"),
        "No struct, union, enum, typedef or macro named `no_such_type` was found in Zed's symbol index. It may be declared through a macro, in a file the index skips, or under another name: try `search_symbols` for `no_such_type`, or `grep` for `no_such_type`.\n"
    );
}

#[gpui::test]
async fn test_what_the_agent_may_not_read_is_refused_or_hidden(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let (output, _) = run(&setup, by_name("secret_type"), cx).await;
    let output = output.expect("an answer");
    assert!(output.starts_with("No struct"), "{output}");
    assert!(!output.contains("password_member"), "{output}");

    for input in [
        at("root/secret/secret.h", 1, "secret_type"),
        TypeLayoutToolInput {
            near_path: Some("root/secret/secret.h".to_string()),
            ..by_name("secret_type")
        },
    ] {
        let (output, updates) = run(&setup, input, cx).await;
        let error = output.expect_err("a private file is refused");
        assert!(error.contains("`private_files`"), "{error}");
        assert!(!error.contains("password_member"), "{error}");
        assert!(updates.iter().all(|update| update.locations.is_none()));
    }

    let (output, _) = run(&setup, by_name(""), cx).await;
    assert!(output.expect_err("no subject").contains("Give `name`"));
}

#[gpui::test]
async fn test_without_an_index_the_tool_says_what_to_do_instead(cx: &mut TestAppContext) {
    let setup = setup(false, cx).await;

    let (output, updates) = run(&setup, by_name("struct outer"), cx).await;
    let output = output.expect("an explanation");
    assert!(output.starts_with("Name index not available"), "{output}");
    assert!(
        output.contains("`search_symbols` for `outer`")
            && output.contains("`grep` for `struct outer {`"),
        "{output}"
    );
    assert!(
        updates.iter().any(|update| update.title.as_deref()
            == Some("Type layout of `struct outer` (no name index)")),
        "{updates:?}"
    );

    let setup = self::setup(true, cx).await;
    let (output, _) = run(&setup, at("root/src/main.rs", 1, "main"), cx).await;
    let output = output.expect("an explanation");
    assert!(output.contains("isn't a C or C++ file"), "{output}");
    assert!(output.contains("`hover`"), "{output}");
}

#[test]
fn test_the_input_schema_explains_both_forms() {
    let schema = serde_json::to_value(TypeLayoutTool::input_schema()).expect("a schema");
    let properties = schema["properties"]
        .as_object()
        .expect("the schema has properties");
    for field in [
        "name",
        "near_path",
        "path",
        "line",
        "symbol",
        "depth",
        "max_chars",
    ] {
        let description = properties[field]["description"]
            .as_str()
            .unwrap_or_default();
        assert!(!description.is_empty(), "{field} has no description");
    }
    assert!(
        TypeLayoutTool::description().contains("before looking for its header"),
        "{}",
        TypeLayoutTool::description()
    );
    assert_eq!(TypeLayoutTool::kind(), acp::ToolKind::Search);
}

/// A member an old remote host can't place is withheld, and the model is told why instead of
/// being left to think its file is restricted.
#[gpui::test]
async fn test_a_member_an_old_host_cannot_place_says_why(cx: &mut TestAppContext) {
    let setup = setup(false, cx).await;
    let mut report = TypeLayoutReport {
        kind: TypeLayoutKind::NotFound,
        requested: "id".to_string(),
        title: String::new(),
        definition: None,
        text: String::new(),
        definitions: Vec::new(),
        candidates: 0,
        candidates_truncated: false,
        other_candidates: Vec::new(),
        withheld: 1,
        withheld_note: Some(ide_query::HOST_TOO_OLD_TO_JUDGE_MEMBER),
        inferred: None,
        pruned: false,
        truncated: false,
        index_note: None,
    };
    let output = setup
        .project
        .read_with(cx, |project, cx| format_type_layout(project, &report, cx));
    assert_eq!(
        output,
        format!(
            "1 definition of `id` can't be shown (it could not be checked): {}\n",
            ide_query::HOST_TOO_OLD_TO_JUDGE_MEMBER
        )
    );

    report.withheld_note = None;
    let output = setup
        .project
        .read_with(cx, |project, cx| format_type_layout(project, &report, cx));
    assert_eq!(
        output,
        "1 definition of `id` can't be shown (in files with restricted access).\n"
    );
}
