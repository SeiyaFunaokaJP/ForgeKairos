use std::path::Path;

use gpui::{TestAppContext, UpdateGlobal as _};
use project::{FakeFs, WorktreeId, symbol_index_store::build_index_for_tests};
use serde_json::json;
use settings::SettingsStore;
use util::{path, rel_path::rel_path};

use super::*;

const TYPES_H: &str = "\
#define MAX_ITEMS 16
typedef unsigned int u32;
struct inner {
\tu32 id;
\tchar name[8];
};
struct outer {
\tstruct inner first; /* embedded */
\tstruct inner *second;
\tstruct nested_part part;
\tunion {
\t\tint i;
\t\tfloat f;
\t} u;
#ifdef CONFIG_DEBUG
\tint debug;
#endif
\tstruct outer *parent;
\tstruct missing *gone;
};
typedef struct outer outer_t;
enum state { IDLE, BUSY = 4 };
struct dup { int in_types; };
";

const NESTED_H: &str = "\
struct nested_part {
\tint nested_member; /* nested comment */
\tstruct inner back;
};
";

const OTHER_DUP_H: &str = "struct dup { int in_other; };\n";

const MAIN_C: &str = "\
#include \"../inc/types.h\"
int use(outer_t *value)
{
\tstruct outer local;
\treturn value->first.id + local.u.i + MAX_ITEMS;
}
";

const SECRET_C: &str = "struct secret_type { int password_member; };\n";

/// A typedef in one file naming a struct in another.
const ALIAS_H: &str = "typedef struct inner alias_t;\n";
/// A global declared in another file than the one using it: its type is read from the
/// declaration found by name.
const GLOBALS_C: &str = "struct inner g_config;\n";
const USER_C: &str = "\
int read_config(void)
{
\treturn g_config.id;
}
";
/// The roots of member access chains whose types are read from declarations in another file:
/// a global, and a function's return type.
const CHAIN_GLOBALS_C: &str = "\
struct outer *g_outer;
struct outer *make_outer(void) { return 0; }
struct outer *g_shadowed;
struct outer *make_shadowed(void) { return 0; }
";
/// `static` namesakes of globals and functions of `CHAIN_GLOBALS_C`, next to `CHAIN_USER_C`.
const CHAIN_STATIC_C: &str = "\
static struct outer *g_shadowed;
static struct outer *make_shadowed(void) { return 0; }
";
const CHAIN_USER_C: &str = "\
int read_param(struct outer *param)
{
\treturn param->first.id;
}
int read_global(void)
{
\treturn g_outer->first.id;
}
int read_call(void)
{
\treturn make_outer()->second->id;
}
int read_shadowed_global(void)
{
\treturn g_shadowed->first.id;
}
int read_shadowed_call(void)
{
\treturn make_shadowed()->second->id;
}
";

/// In a folder that is open but not part of the project (an invisible worktree): the store
/// shows its definitions, the agent must not see them.
const OUTSIDE_H: &str = "\
struct dup { int outside_member; };
struct only_outside { int outside_only_member; };
";

/// A type with more member types than one walk may ask about.
fn big_h() -> String {
    let mut text = String::new();
    for index in 0..BIG_MEMBERS {
        text.push_str(&format!("struct t{index} {{ int v{index}; }};\n"));
    }
    text.push_str("struct big {\n\tstruct hidden_t h;\n");
    for index in 0..BIG_MEMBERS {
        text.push_str(&format!("\tstruct t{index} m{index};\n"));
    }
    text.push_str("};\n");
    text
}

const BIG_MEMBERS: usize = MAX_TYPE_LAYOUT_REQUESTS + 10;
const HIDDEN_H: &str = "struct hidden_t { int hidden_member; };\n";

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("inc/types.h", TYPES_H),
        ("inc/nested.h", NESTED_H),
        ("other/dup.h", OTHER_DUP_H),
        ("src/main.c", MAIN_C),
        ("secret/hidden.c", SECRET_C),
        ("alias/alias.h", ALIAS_H),
        ("globals/globals.c", GLOBALS_C),
        ("src/user.c", USER_C),
        ("globals/chain_globals.c", CHAIN_GLOBALS_C),
        ("src/chain_user.c", CHAIN_USER_C),
        ("src/chain_static.c", CHAIN_STATIC_C),
    ]
    .into_iter()
    .chain(MACRO_FILES)
    .chain(MANY_FILES)
    .chain([("chain/chain.h", CHAIN_H)])
    .collect()
}

/// Types nested deeper than any depth, and through two pointers, each next to a member of a
/// type in `big/hidden.h` so that withholding that file makes the tree be walked.
const CHAIN_H: &str = "\
struct c6 { int leaf6; };
struct c5 { struct c6 next5; };
struct c4 { struct c5 next4; };
struct c3 { struct c4 next3; };
struct c2 { struct c3 next2; };
struct c1 { struct c2 next1; };
struct c0 { struct c1 next0; struct hidden_t h; };
struct p2 { int leaf2; };
struct p1 { struct p2 *b; };
struct p0 { struct p1 *a; struct hidden_t h; };
";

/// More same-name types than are listed by location.
const MANY_FILES: [(&str, &str); MAX_OTHER_TYPE_CANDIDATES + 2] = [
    ("many/d0.h", "struct many { int f0; };\n"),
    ("many/d1.h", "struct many { int f1; };\n"),
    ("many/d2.h", "struct many { int f2; };\n"),
    ("many/d3.h", "struct many { int f3; };\n"),
    ("many/d4.h", "struct many { int f4; };\n"),
    ("many/d5.h", "struct many { int f5; };\n"),
    ("many/d6.h", "struct many { int f6; };\n"),
    ("many/d7.h", "struct many { int f7; };\n"),
    ("many/d8.h", "struct many { int f8; };\n"),
    ("many/d9.h", "struct many { int f9; };\n"),
];

/// One more same-name macro than an answer reads.
const MACRO_FILES: [(&str, &str); MAX_TYPE_READ_LIMIT + 1] = [
    ("macros/m1.h", "#define DUP_MACRO 101\n"),
    ("macros/m2.h", "#define DUP_MACRO 102\n"),
    ("macros/m3.h", "#define DUP_MACRO 103\n"),
    ("macros/m4.h", "#define DUP_MACRO 104\n"),
    ("macros/m5.h", "#define DUP_MACRO 105\n"),
];

struct Setup {
    project: Entity<Project>,
    worktree_id: WorktreeId,
    _index_directories: Vec<tempfile::TempDir>,
}

fn init_settings(cx: &mut TestAppContext) {
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
}

async fn setup(with_index: bool, cx: &mut TestAppContext) -> Setup {
    init_settings(cx);
    cx.executor().allow_parking();
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(
        path!("/root"),
        json!({
            "inc": { "types.h": TYPES_H, "nested.h": NESTED_H },
            "other": { "dup.h": OTHER_DUP_H },
            "src": {
                "main.c": MAIN_C,
                "user.c": USER_C,
                "chain_user.c": CHAIN_USER_C,
                "chain_static.c": CHAIN_STATIC_C,
                "readme.md": "# struct outer\n",
            },
            "secret": { "hidden.c": SECRET_C },
            "alias": { "alias.h": ALIAS_H },
            "globals": { "globals.c": GLOBALS_C, "chain_globals.c": CHAIN_GLOBALS_C },
            "macros": MACRO_FILES
                .iter()
                .map(|(path, contents)| {
                    let name = path.trim_start_matches("macros/").to_string();
                    (name, json!(contents))
                })
                .collect::<serde_json::Map<_, _>>(),
            "many": MANY_FILES
                .iter()
                .map(|(path, contents)| {
                    let name = path.trim_start_matches("many/").to_string();
                    (name, json!(contents))
                })
                .collect::<serde_json::Map<_, _>>(),
            "chain": { "chain.h": CHAIN_H },
            "big": { "big.h": big_h(), "hidden.h": HIDDEN_H },
        }),
    )
    .await;
    fs.insert_tree(path!("/outside"), json!({ "outside.h": OUTSIDE_H }))
        .await;
    let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;
    let (outside, _) = project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/outside"), false, cx)
        })
        .await
        .expect("the invisible worktree");
    cx.run_until_parked();
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    let outside_worktree_id = outside.read_with(cx, |worktree, _| worktree.id());
    let mut directories = Vec::new();
    if with_index {
        let store = project.read_with(cx, |project, _| project.symbol_index_store().clone());
        let big = big_h();
        let mut all_files = files();
        all_files.push(("big/big.h", big.as_str()));
        all_files.push(("big/hidden.h", HIDDEN_H));
        for (id, files) in [
            (worktree_id, all_files),
            (outside_worktree_id, vec![("outside.h", OUTSIDE_H)]),
        ] {
            let directory = tempfile::tempdir().expect("a temporary directory");
            let index = build_index_for_tests(directory.path(), &files).expect("the index");
            store.update(cx, |store, cx| store.insert_index_for_tests(id, index, cx));
            directories.push(directory);
        }
        cx.run_until_parked();
    }
    Setup {
        project,
        worktree_id,
        _index_directories: directories,
    }
}

impl Setup {
    fn path(&self, path: &str) -> ProjectPath {
        ProjectPath {
            worktree_id: self.worktree_id,
            path: rel_path(path).into_arc(),
        }
    }
}

fn by_name(name: &str) -> TypeLayoutRequest {
    TypeLayoutRequest {
        subject: TypeLayoutSubject::Name {
            name: name.to_string(),
            near: None,
        },
        depth: 0,
        max_chars: 0,
    }
}

fn at(setup: &Setup, path: &str, line: u32, symbol: &str) -> TypeLayoutRequest {
    TypeLayoutRequest {
        subject: TypeLayoutSubject::Position {
            path: setup.path(path),
            position: LinePosition {
                line,
                symbol: Some(symbol.to_string()),
                column: None,
            },
        },
        depth: 0,
        max_chars: 0,
    }
}

async fn layout(
    setup: &Setup,
    request: TypeLayoutRequest,
    cx: &mut TestAppContext,
) -> Result<TypeLayoutReport> {
    cx.update(|cx| type_layout(&setup.project, request, cx))
        .await
}

/// Withholds the listed files on top of what Zed's agent may not read, as a sandbox that
/// only sees part of a worktree does.
struct Withholding {
    inner: AgentTypeLayoutPaths,
    withheld: Vec<&'static str>,
    asked: Vec<ProjectPath>,
}

impl TypeLayoutPaths for Withholding {
    fn allows<'a>(
        &'a mut self,
        path: &'a ProjectPath,
        cx: &'a mut AsyncApp,
    ) -> LocalBoxFuture<'a, bool> {
        async move {
            self.asked.push(path.clone());
            if self.withheld.contains(&path.path.as_unix_str()) {
                return false;
            }
            self.inner.allows(path, cx).await
        }
        .boxed_local()
    }
}

async fn layout_withholding(
    setup: &Setup,
    request: TypeLayoutRequest,
    withheld: Vec<&'static str>,
    cx: &mut TestAppContext,
) -> (Result<TypeLayoutReport>, Vec<ProjectPath>) {
    let project = setup.project.clone();
    cx.update(|cx| {
        cx.spawn(async move |cx| {
            let mut paths = Withholding {
                inner: AgentTypeLayoutPaths::new(project.clone()),
                withheld,
                asked: Vec::new(),
            };
            let result = type_layout_with(&project, request, &mut paths, cx).await;
            (result, paths.asked)
        })
    })
    .await
}

fn location(setup: &Setup, path: &str, line: u32) -> TypeLayoutLocation {
    TypeLayoutLocation {
        path: setup.path(path),
        line,
    }
}

const OUTER_TREE: &str = "\
struct outer  (inc/types.h:7)
  struct inner first  (inc/types.h:3)  // embedded
    u32 id  (= unsigned int)
    char name[8]
  struct inner *second  (see above)
  struct nested_part part  (inc/nested.h:1)
    int nested_member  // nested comment
    struct inner back  (see above)
  union {...} u
    int i
    float f
  int debug  [#if CONFIG_DEBUG]
  struct outer *parent  (recursive)
  struct missing *gone  (not found)
";

#[gpui::test]
async fn test_a_type_is_described_by_name(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let report = layout(&setup, by_name("struct outer"), cx)
        .await
        .expect("the layout");
    assert_eq!(report.kind, TypeLayoutKind::Type);
    assert_eq!(report.requested, "struct outer");
    assert_eq!(report.title, "struct outer");
    assert_eq!(report.text, OUTER_TREE);
    assert_eq!(report.definition, Some(location(&setup, "inc/types.h", 7)));
    assert_eq!(
        report.definitions,
        vec![
            location(&setup, "inc/types.h", 7),
            location(&setup, "inc/types.h", 3),
            location(&setup, "inc/types.h", 2),
            location(&setup, "inc/nested.h", 1),
        ]
    );
    assert_eq!(report.candidates, 1);
    assert!(report.other_candidates.is_empty());
    assert_eq!(report.withheld, 0);
    assert!(!report.pruned);
    assert!(!report.truncated);
    assert_eq!(report.index_note, None);

    let typedef = layout(&setup, by_name("outer_t"), cx)
        .await
        .expect("the layout");
    assert_eq!(typedef.title, "struct outer");
    assert_eq!(typedef.text, format!("outer_t -> {OUTER_TREE}"));
    assert!(
        typedef
            .definitions
            .contains(&location(&setup, "inc/types.h", 21)),
        "{:?}",
        typedef.definitions
    );

    let enumeration = layout(&setup, by_name("enum state"), cx)
        .await
        .expect("the layout");
    assert_eq!(
        enumeration.text,
        "enum state  (inc/types.h:22)\n  IDLE  (= 0)\n  BUSY = 4\n"
    );
}

#[gpui::test]
async fn test_same_name_definitions_are_listed_by_location(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let report = layout(
        &setup,
        TypeLayoutRequest {
            subject: TypeLayoutSubject::Name {
                name: "dup".to_string(),
                near: Some(setup.path("other/dup.h")),
            },
            depth: 0,
            max_chars: 0,
        },
        cx,
    )
    .await
    .expect("the layout");
    assert_eq!(report.text, "struct dup  (other/dup.h:1)\n  int in_other\n");
    assert_eq!(report.candidates, 2);
    assert_eq!(
        report.other_candidates,
        vec![location(&setup, "inc/types.h", 23)]
    );
    assert_eq!(report.withheld, 0);
    // The same name in a folder that is open but not part of the project is never shown.
    assert!(!format!("{report:?}").contains("outside"), "{report:?}");

    // A candidate the agent may not read is counted, and neither shown nor listed.
    let (withheld, _) = layout_withholding(&setup, by_name("dup"), vec!["other/dup.h"], cx).await;
    let withheld = withheld.expect("the layout");
    assert_eq!(
        withheld.text,
        "struct dup  (inc/types.h:23)
  int in_types
"
    );
    assert_eq!(withheld.candidates, 1);
    assert!(withheld.other_candidates.is_empty());
    assert_eq!(withheld.withheld, 1);
    let shown = format!("{withheld:?}");
    assert!(
        !shown.contains("in_other") && !shown.contains("other/dup.h"),
        "{shown}"
    );
}

#[gpui::test]
async fn test_types_of_names_at_positions_are_described(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let type_name = layout(&setup, at(&setup, "src/main.c", 2, "outer_t"), cx)
        .await
        .expect("the layout");
    assert_eq!(type_name.title, "struct outer");
    assert_eq!(type_name.inferred, None);

    let variable = layout(&setup, at(&setup, "src/main.c", 5, "value"), cx)
        .await
        .expect("the layout");
    assert_eq!(variable.kind, TypeLayoutKind::Type);
    assert_eq!(variable.title, "struct outer");
    let inferred = variable.inferred.expect("the variable's type");
    assert_eq!(inferred.name, "value");
    assert_eq!(inferred.type_text, "outer_t *");
    assert_eq!(inferred.confidence, Some(Confidence::Parameter));

    let member = layout(&setup, at(&setup, "src/main.c", 5, "first"), cx)
        .await
        .expect("the layout");
    assert_eq!(member.title, "struct inner");
    assert_eq!(
        member.text,
        "struct inner  (inc/types.h:3)\n  u32 id  (= unsigned int)\n  char name[8]\n"
    );
    let inferred = member.inferred.expect("the member's type");
    assert_eq!(inferred.type_text, "struct inner");
    assert_eq!(inferred.member_of.as_deref(), Some("struct outer"));

    let inline = layout(&setup, at(&setup, "src/main.c", 5, "u."), cx)
        .await
        .expect("the layout");
    assert_eq!(inline.kind, TypeLayoutKind::Type);
    assert!(
        inline.text.contains("  int i\n  float f\n"),
        "{}",
        inline.text
    );

    let macro_at = layout(&setup, at(&setup, "src/main.c", 5, "MAX_ITEMS"), cx)
        .await
        .expect("the layout");
    assert_eq!(macro_at.kind, TypeLayoutKind::Macro);
    assert_eq!(macro_at.text, "inc/types.h:1\n  #define MAX_ITEMS 16\n");
    assert_eq!(
        macro_at.definition,
        Some(location(&setup, "inc/types.h", 1))
    );

    let macro_by_name = layout(&setup, by_name("MAX_ITEMS"), cx)
        .await
        .expect("the layout");
    assert_eq!(macro_by_name.kind, TypeLayoutKind::Macro);
    assert_eq!(macro_by_name.text, macro_at.text);
}

#[gpui::test]
async fn test_depth_and_size_limits(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let shallow = layout(
        &setup,
        TypeLayoutRequest {
            depth: 1,
            ..by_name("struct nested_part")
        },
        cx,
    )
    .await
    .expect("the layout");
    let deep = layout(
        &setup,
        TypeLayoutRequest {
            depth: 2,
            ..by_name("struct nested_part")
        },
        cx,
    )
    .await
    .expect("the layout");
    assert!(
        shallow.text.contains(
            "    u32 id
"
        ),
        "{}",
        shallow.text
    );
    assert!(
        !shallow.text.contains("(= unsigned int)"),
        "{}",
        shallow.text
    );
    assert!(
        deep.text.contains("u32 id  (= unsigned int)"),
        "{}",
        deep.text
    );

    let cut = layout(
        &setup,
        TypeLayoutRequest {
            max_chars: 1,
            ..by_name("struct outer")
        },
        cx,
    )
    .await
    .expect("the layout");
    assert!(cut.truncated);
    assert!(cut.text.ends_with("... (truncated)\n"), "{}", cut.text);
    assert!(
        cut.text.chars().count() <= MIN_TYPE_LAYOUT_CHARS
            && cut.text.chars().count() > MIN_TYPE_LAYOUT_CHARS / 2,
        "{}",
        cut.text
    );
    assert!(cut.text.len() < OUTER_TREE.len());
}

#[gpui::test]
async fn test_what_is_not_there_is_reported_as_such(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let missing = layout(&setup, by_name("no_such_type"), cx)
        .await
        .expect("an answer");
    assert_eq!(missing.kind, TypeLayoutKind::NotFound);
    assert_eq!(missing.withheld, 0);

    for (request, expected) in [
        (by_name(""), "is empty"),
        (by_name("struct a b"), "isn't a type name"),
        (by_name("a->b"), "isn't a type name"),
    ] {
        let error = layout(&setup, request.clone(), cx)
            .await
            .expect_err("not a name");
        assert!(error.to_string().contains(expected), "{request:?}: {error}");
    }

    let error = layout(&setup, at(&setup, "src/readme.md", 1, "outer"), cx)
        .await
        .expect_err("not C");
    assert_eq!(
        error
            .downcast_ref::<TypeIndexUnavailable>()
            .map(|error| error.reason.clone()),
        Some(TypeIndexUnavailableReason::NotCOrCpp),
        "{error}"
    );
}

/// An index still being built answers with what it has, and the answer says so, by name and at
/// a position alike.
#[gpui::test]
async fn test_an_index_still_being_built_adds_a_note(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let store = setup
        .project
        .read_with(cx, |project, _| project.symbol_index_store().clone());
    store.update(cx, |store, cx| {
        store.set_status_for_tests(
            setup.worktree_id,
            SymbolIndexStatus::Indexing { done: 2, total: 4 },
            cx,
        )
    });
    cx.run_until_parked();

    for request in [
        by_name("struct outer"),
        at(&setup, "src/main.c", 4, "local"),
    ] {
        let report = layout(&setup, request, cx).await.expect("the layout");
        assert_eq!(report.kind, TypeLayoutKind::Type, "{report:?}");
        assert_eq!(report.title, "struct outer");
        let note = report.index_note.expect("a note");
        assert!(note.contains("still being built (2 of 4 files)"), "{note}");
    }

    store.update(cx, |store, cx| {
        store.set_status_for_tests(
            setup.worktree_id,
            SymbolIndexStatus::Ready {
                stats: Default::default(),
            },
            cx,
        )
    });
    cx.run_until_parked();
    let report = layout(&setup, by_name("struct outer"), cx)
        .await
        .expect("the layout");
    assert_eq!(report.index_note, None);
}

#[gpui::test]
async fn test_a_project_without_an_index_says_so(cx: &mut TestAppContext) {
    let setup = setup(false, cx).await;
    for request in [
        by_name("struct outer"),
        at(&setup, "src/main.c", 5, "value"),
    ] {
        let error = layout(&setup, request, cx).await.expect_err("no index");
        assert!(
            error.downcast_ref::<TypeIndexUnavailable>().is_some(),
            "{error}"
        );
        assert!(
            error.to_string().contains("Name index not available"),
            "{error}"
        );
    }
}

/// Neither the definitions in a folder outside the project, nor those in a private file, nor
/// the position in a private file reach the agent: not their members, their names or paths.
#[gpui::test]
async fn test_definitions_the_agent_may_not_read_are_only_counted(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let outside = layout(&setup, by_name("only_outside"), cx)
        .await
        .expect("an answer");
    assert_eq!(outside.kind, TypeLayoutKind::NotFound);
    let shown = format!("{outside:?}");
    assert!(!shown.contains("outside_only_member"), "{shown}");
    assert!(!shown.contains("outside.h"), "{shown}");

    // The store itself hides a private file's definitions: not even counted.
    let private = layout(&setup, by_name("secret_type"), cx)
        .await
        .expect("an answer");
    assert_eq!(private.kind, TypeLayoutKind::NotFound);
    assert!(!format!("{private:?}").contains("password_member"));

    let error = layout(&setup, at(&setup, "secret/hidden.c", 1, "secret_type"), cx)
        .await
        .expect_err("a private file is refused");
    assert!(error.to_string().contains("`private_files`"), "{error}");
    assert!(!error.to_string().contains("password_member"), "{error}");
}

/// A tree that names a definition the agent may not read is walked member by member, and only
/// the branch at that definition is cut: every other line is what the index renders.
#[gpui::test]
async fn test_a_withheld_branch_is_cut_from_the_tree(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let (report, asked) =
        layout_withholding(&setup, by_name("struct outer"), vec!["inc/nested.h"], cx).await;
    let report = report.expect("the layout");
    assert!(report.pruned);
    assert_eq!(report.withheld, 1);
    assert_eq!(
        report.text,
        OUTER_TREE.replace(
            "  struct nested_part part  (inc/nested.h:1)\n    int nested_member  // nested comment\n    struct inner back  (see above)\n",
            "  struct nested_part part  (withheld)\n",
        )
    );
    assert!(!report.text.contains("nested_member"));
    assert!(!format!("{report:?}").contains("nested.h"), "{report:?}");
    assert!(
        asked.contains(&setup.path("inc/nested.h")),
        "the tree's definitions are all checked"
    );

    // Nothing withheld: the index's text is used as it is.
    let (report, _) =
        layout_withholding(&setup, by_name("struct outer"), vec!["other/dup.h"], cx).await;
    let report = report.expect("the layout");
    assert!(!report.pruned);
    assert_eq!(report.text, OUTER_TREE);

    // The type itself withheld: only counted.
    let (report, _) =
        layout_withholding(&setup, by_name("struct outer"), vec!["inc/types.h"], cx).await;
    let report = report.expect("an answer");
    assert_eq!(report.kind, TypeLayoutKind::NotFound);
    assert_eq!(report.withheld, 1);
    assert!(!format!("{report:?}").contains("first"), "{report:?}");

    // A member whose owner is withheld: the member's declaration is in the owner's file.
    let (report, _) = layout_withholding(
        &setup,
        at(&setup, "src/main.c", 5, "first"),
        vec!["inc/types.h"],
        cx,
    )
    .await;
    let report = report.expect("an answer");
    assert_eq!(report.kind, TypeLayoutKind::NotFound);
    assert!(report.inferred.is_none());
    assert!(
        !format!("{report:?}").contains("struct inner"),
        "{report:?}"
    );

    // A macro in a withheld file.
    let (report, _) =
        layout_withholding(&setup, by_name("MAX_ITEMS"), vec!["inc/types.h"], cx).await;
    let report = report.expect("an answer");
    assert_eq!(report.kind, TypeLayoutKind::NotFound);
    assert_eq!(report.withheld, 1);
    assert!(!format!("{report:?}").contains("16"), "{report:?}");
}

/// What the answer is read through, not only the definition it shows, has to pass: the
/// typedef leading to the struct, the declaration a global's type is read from, the macro at a
/// position, and the file asked about.
#[gpui::test]
async fn test_what_leads_to_a_definition_is_judged_too(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;

    let (shown, _) = layout_withholding(&setup, by_name("alias_t"), vec![], cx).await;
    let shown = shown.expect("the layout");
    assert!(
        shown
            .text
            .starts_with("alias_t -> struct inner  (inc/types.h:3)\n"),
        "{shown:?}"
    );
    assert!(
        shown
            .definitions
            .contains(&location(&setup, "alias/alias.h", 1)),
        "{shown:?}"
    );
    // The struct may be read, the typedef naming it may not.
    let (withheld, _) =
        layout_withholding(&setup, by_name("alias_t"), vec!["alias/alias.h"], cx).await;
    let withheld = withheld.expect("an answer");
    assert_eq!(withheld.kind, TypeLayoutKind::NotFound);
    assert_eq!(withheld.withheld, 1);
    let debug = format!("{withheld:?}");
    assert!(
        !debug.contains("inner") && !debug.contains("alias.h"),
        "{debug}"
    );

    let global = at(&setup, "src/user.c", 3, "g_config");
    let (shown, _) = layout_withholding(&setup, global.clone(), vec![], cx).await;
    let shown = shown.expect("the layout");
    assert_eq!(shown.title, "struct inner", "{shown:?}");
    let inferred = shown.inferred.expect("the global's type");
    assert_eq!(inferred.type_text, "struct inner");
    assert_eq!(inferred.confidence, Some(Confidence::NameOnly));
    // `struct inner` may be read, the file declaring the global as one may not.
    let (withheld, _) = layout_withholding(&setup, global, vec!["globals/globals.c"], cx).await;
    let withheld = withheld.expect("an answer");
    assert_eq!(withheld.kind, TypeLayoutKind::NotFound);
    assert_eq!(withheld.withheld, 1);
    assert_eq!(withheld.inferred, None);
    let debug = format!("{withheld:?}");
    assert!(
        !debug.contains("inner") && !debug.contains("globals"),
        "{debug}"
    );

    let macro_at = at(&setup, "src/main.c", 5, "MAX_ITEMS");
    let (withheld, _) = layout_withholding(&setup, macro_at, vec!["inc/types.h"], cx).await;
    let withheld = withheld.expect("an answer");
    assert_eq!(withheld.kind, TypeLayoutKind::NotFound);
    assert_eq!(withheld.withheld, 1);
    let debug = format!("{withheld:?}");
    assert!(
        !debug.contains("16") && !debug.contains("types.h"),
        "{debug}"
    );

    let (error, _) = layout_withholding(
        &setup,
        at(&setup, "src/main.c", 5, "value"),
        vec!["src/main.c"],
        cx,
    )
    .await;
    let error = error.expect_err("the file asked about is refused");
    assert!(error.to_string().contains("can't be read"), "{error}");
    assert!(!error.to_string().contains("outer"), "{error}");
}

/// The type of `a` in `a->b.c` may be read from a global's declaration or a function's in
/// another file; that file has to pass as the owners along the chain do. A root declared in
/// the file asked about needs nothing more.
#[gpui::test]
async fn test_the_root_of_a_member_chain_is_judged_too(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let withheld_files = vec!["globals/chain_globals.c"];

    let parameter = at(&setup, "src/chain_user.c", 3, "first");
    let global = at(&setup, "src/chain_user.c", 7, "first");
    let call = at(&setup, "src/chain_user.c", 11, "second");

    for (request, title) in [
        (parameter.clone(), "struct inner"),
        (global.clone(), "struct inner"),
        (call.clone(), "struct inner"),
    ] {
        let (shown, _) = layout_withholding(&setup, request, vec![], cx).await;
        let shown = shown.expect("the layout");
        assert_eq!(shown.kind, TypeLayoutKind::Type, "{shown:?}");
        assert_eq!(shown.title, title, "{shown:?}");
        assert_eq!(
            shown
                .inferred
                .as_ref()
                .and_then(|inferred| inferred.member_of.as_deref()),
            Some("struct outer"),
            "{shown:?}"
        );
    }

    // Declared in the file asked about: the withheld file plays no part.
    let (shown, _) = layout_withholding(&setup, parameter, withheld_files.clone(), cx).await;
    let shown = shown.expect("the layout");
    assert_eq!(shown.kind, TypeLayoutKind::Type, "{shown:?}");
    assert_eq!(shown.title, "struct inner", "{shown:?}");

    for request in [global, call] {
        let (withheld, asked) =
            layout_withholding(&setup, request, withheld_files.clone(), cx).await;
        let withheld = withheld.expect("an answer");
        assert_eq!(withheld.kind, TypeLayoutKind::NotFound, "{withheld:?}");
        assert_eq!(withheld.withheld, 1, "{withheld:?}");
        assert_eq!(withheld.inferred, None);
        let debug = format!("{withheld:?}");
        assert!(
            !debug.contains("inner") && !debug.contains("outer") && !debug.contains("chain_"),
            "{debug}"
        );
        assert!(
            asked.contains(&setup.path("globals/chain_globals.c")),
            "the root's declaration is checked: {asked:?}"
        );
    }
}

/// The type engine may read a chain's root from another file's `static` global or function
/// of the name (the nearest), which a lookup near the file asked about leaves out; every
/// file declaring the name is judged.
#[gpui::test]
async fn test_the_root_of_a_member_chain_is_judged_by_every_namesake(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let global = at(&setup, "src/chain_user.c", 15, "first");
    let call = at(&setup, "src/chain_user.c", 19, "second");

    for request in [global.clone(), call.clone()] {
        let (shown, _) = layout_withholding(&setup, request, vec![], cx).await;
        let shown = shown.expect("the layout");
        assert_eq!(shown.kind, TypeLayoutKind::Type, "{shown:?}");
        assert_eq!(shown.title, "struct inner", "{shown:?}");
    }

    for request in [global, call] {
        let (withheld, asked) =
            layout_withholding(&setup, request, vec!["src/chain_static.c"], cx).await;
        let withheld = withheld.expect("an answer");
        assert_eq!(withheld.kind, TypeLayoutKind::NotFound, "{withheld:?}");
        assert_eq!(withheld.withheld, 1, "{withheld:?}");
        assert!(
            asked.contains(&setup.path("src/chain_static.c")),
            "the static namesake is checked: {asked:?}"
        );
    }
}

/// A walk asks about one member at a time; it stops at [`MAX_TYPE_LAYOUT_REQUESTS`] and says
/// it was cut, rather than making hundreds of round trips.
#[gpui::test]
async fn test_a_walk_stops_at_its_request_limit(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let request = TypeLayoutRequest {
        max_chars: MAX_TYPE_LAYOUT_CHARS,
        ..by_name("struct big")
    };
    let expanded = |text: &str| text.matches("(big/big.h:").count();

    let (full, _) = layout_withholding(&setup, request.clone(), vec!["no/such/file.h"], cx).await;
    let full = full.expect("the layout");
    assert!(!full.pruned && !full.truncated, "{full:?}");
    assert!(
        full.text
            .contains("  struct hidden_t h  (big/hidden.h:1)\n"),
        "{full:?}"
    );
    assert_eq!(expanded(&full.text), BIG_MEMBERS + 1, "{}", full.text);

    let (walked, _) = layout_withholding(&setup, request, vec!["big/hidden.h"], cx).await;
    let walked = walked.expect("the layout");
    assert!(walked.pruned && walked.truncated, "{}", walked.text);
    assert!(
        walked.text.contains("  struct hidden_t h  (withheld)\n"),
        "{}",
        walked.text
    );
    assert!(!walked.text.contains("hidden_member"), "{}", walked.text);
    let walked_members = expanded(&walked.text);
    assert!(
        walked_members < MAX_TYPE_LAYOUT_REQUESTS,
        "{walked_members}: {}",
        walked.text
    );
    assert!(
        walked
            .text
            .contains(&format!("  struct t{0} m{0}\n", BIG_MEMBERS - 1))
    );
}

/// Takes a second for every judgement, as a slow remote host does, and withholds one file so
/// that the tree has to be walked.
#[derive(Default)]
struct SlowJudge {
    first_call: Option<Instant>,
    last_call: Option<Instant>,
}

impl TypeLayoutPaths for SlowJudge {
    fn allows<'a>(
        &'a mut self,
        path: &'a ProjectPath,
        cx: &'a mut AsyncApp,
    ) -> LocalBoxFuture<'a, bool> {
        async move {
            let executor = cx.background_executor().clone();
            self.first_call.get_or_insert(executor.now());
            executor.advance_clock(Duration::from_secs(1));
            self.last_call = Some(executor.now());
            path.path.as_unix_str() != "big/hidden.h"
        }
        .boxed_local()
    }
}

/// The tree a walk builds is the index's tree with the withheld branch cut, at every depth
/// (including one above the most the index expands) and through pointers.
#[gpui::test]
async fn test_a_walk_draws_the_tree_the_index_draws(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    for (name, depth) in [
        ("struct c0", 1),
        ("struct c0", 3),
        ("struct c0", MAX_TYPE_LAYOUT_DEPTH),
        ("struct c0", MAX_TYPE_LAYOUT_DEPTH + 5),
        ("struct p0", 0),
        ("struct p0", MAX_TYPE_LAYOUT_DEPTH),
    ] {
        let request = TypeLayoutRequest {
            depth,
            ..by_name(name)
        };
        let (index, _) = layout_withholding(&setup, request.clone(), vec![], cx).await;
        let index = index.expect("the index's tree");
        assert!(!index.pruned, "{name} {depth}");
        let hidden = "  struct hidden_t h  (big/hidden.h:1)\n    int hidden_member\n";
        assert!(
            index.text.contains(hidden),
            "{name} {depth}: {}",
            index.text
        );
        let (walked, _) = layout_withholding(&setup, request, vec!["big/hidden.h"], cx).await;
        let walked = walked.expect("the walked tree");
        assert!(walked.pruned, "{name} {depth}");
        assert_eq!(
            walked.text,
            index
                .text
                .replace(hidden, "  struct hidden_t h  (withheld)\n"),
            "{name} {depth}"
        );
    }
}

#[gpui::test]
async fn test_other_same_name_definitions_are_capped(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let report = layout(&setup, by_name("struct many"), cx)
        .await
        .expect("the layout");
    assert_eq!(report.candidates, MANY_FILES.len());
    assert_eq!(report.other_candidates.len(), MAX_OTHER_TYPE_CANDIDATES);
}

/// The walk stops expanding before the whole answer's budget runs out, so what it found is
/// returned instead of a time-out.
#[gpui::test]
async fn test_a_slow_walk_returns_what_it_found(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let project = setup.project.clone();
    let request = TypeLayoutRequest {
        max_chars: MAX_TYPE_LAYOUT_CHARS,
        ..by_name("struct big")
    };
    let (report, judge) = cx
        .update(|cx| {
            cx.spawn(async move |cx| {
                let mut judge = SlowJudge::default();
                let report = type_layout_with(&project, request, &mut judge, cx).await;
                (report, judge)
            })
        })
        .await;
    let report = report.expect("an answer before the budget runs out");
    // No member is expanded once the margin is reached; the one being judged then may still
    // take its two judgements (its file, and the file its reference points into).
    let elapsed = judge
        .last_call
        .zip(judge.first_call)
        .map(|(last, first)| last - first)
        .expect("judged");
    assert!(
        elapsed <= TYPE_LAYOUT_TIME_BUDGET - WALK_TIME_MARGIN + Duration::from_secs(1),
        "{elapsed:?}"
    );
    assert!(report.pruned && report.truncated, "{}", report.text);
    assert!(
        report.text.contains("  struct hidden_t h  (withheld)\n"),
        "{}",
        report.text
    );
    let expanded = report.text.matches("(big/big.h:").count();
    let budget_seconds = (TYPE_LAYOUT_TIME_BUDGET - WALK_TIME_MARGIN).as_secs() as usize;
    assert!(
        expanded > 1 && expanded <= budget_seconds,
        "{expanded}: {}",
        report.text
    );
}

/// Same-name macros are read up to [`MAX_TYPE_READ_LIMIT`]; the rest are counted.
#[gpui::test]
async fn test_same_name_macros_are_read_up_to_the_limit(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let report = layout(&setup, by_name("DUP_MACRO"), cx)
        .await
        .expect("the macros");
    assert_eq!(report.kind, TypeLayoutKind::Macro);
    assert_eq!(
        report.definitions.len(),
        MAX_TYPE_READ_LIMIT,
        "{}",
        report.text
    );
    assert_eq!(
        report.text.matches("#define DUP_MACRO").count(),
        MAX_TYPE_READ_LIMIT
    );
    assert_eq!(report.candidates, MACRO_FILES.len());
    assert!(report.candidates_truncated);
}

struct NeverAnswers;

impl TypeLayoutPaths for NeverAnswers {
    fn allows<'a>(
        &'a mut self,
        _path: &'a ProjectPath,
        _cx: &'a mut AsyncApp,
    ) -> LocalBoxFuture<'a, bool> {
        futures::future::pending().boxed_local()
    }
}

#[gpui::test]
async fn test_an_answer_that_takes_too_long_is_given_up(cx: &mut TestAppContext) {
    let setup = setup(true, cx).await;
    let project = setup.project;
    let mut task = cx.update(|cx| {
        cx.spawn(async move |cx| {
            type_layout_with(&project, by_name("struct outer"), &mut NeverAnswers, cx).await
        })
    });
    cx.run_until_parked();
    assert!(
        (&mut task).now_or_never().is_none(),
        "still waiting at first"
    );
    cx.executor()
        .advance_clock(TYPE_LAYOUT_TIME_BUDGET + Duration::from_secs(1));
    cx.run_until_parked();
    let error = task
        .now_or_never()
        .expect("given up once the budget passes")
        .expect_err("no answer");
    assert!(
        error.downcast_ref::<TypeLayoutTimedOut>().is_some(),
        "{error}"
    );
}

#[test]
fn test_type_names_are_parsed() {
    for (text, expected) in [
        ("foo_t", ("foo_t", None)),
        ("  struct   foo ", ("foo", Some(TagKind::Struct))),
        ("union u", ("u", Some(TagKind::Union))),
        ("enum e", ("e", Some(TagKind::Enum))),
        ("class ns::c", ("ns::c", Some(TagKind::Class))),
    ] {
        let (name, tag) = parse_type_name(text).expect(text);
        assert_eq!((name.as_str(), tag), expected, "{text}");
    }
    for text in [
        "",
        "struct",
        "1abc",
        "foo bar",
        "struct foo *",
        "x".repeat(201).as_str(),
    ] {
        assert!(parse_type_name(text).is_err(), "{text:?}");
    }
}
