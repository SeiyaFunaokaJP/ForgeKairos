//! `getTypeLayout` (ADR-F56 の型): 一覧・事前許可に載り、呼べて、サンドボックスから見えない定義は
//! 名前・中身・場所を出さずに件数だけにする。

use project::symbol_index_store::build_index_for_tests;

use super::*;

/// `/test/alias` は `/test/inc` を指すリンク (ワークツリーの中を指すので索引は読む)。IDE のツールは
/// リンクを通るファイルを見せない。`/other` はセッションのルートに入っていない、見えている
/// ワークツリー。
const TYPES_H: &str = "\
struct outer {
\tstruct inner first; /* the first part */
\tstruct nested_part part;
};
struct inner { int depth; };
";
const MAIN_C: &str = "\
#include \"types.h\"
int use(struct outer *value)
{
\treturn value->first.depth;
}
";
const NESTED_H: &str = "struct nested_part { int nested_hunter2; };\n";
const OTHER_H: &str = "struct other_type { int other_hunter2; };\n";

/// 型のファイルを足して索引を作る。戻り値の一時フォルダーは索引の置き場 (テストの間は保つ)。
async fn add_type_files(
    fixture: &SandboxFixture,
    cx: &mut TestAppContext,
) -> Vec<tempfile::TempDir> {
    cx.executor().allow_parking();
    fixture
        .fs
        .insert_tree(
            path!("/test/src"),
            json!({ "types.h": TYPES_H, "main.c": MAIN_C }),
        )
        .await;
    fixture
        .fs
        .insert_tree(path!("/test/inc"), json!({ "nested.h": NESTED_H }))
        .await;
    fixture
        .fs
        .insert_symlink(path!("/test/alias"), PathBuf::from(path!("/test/inc")))
        .await;
    fixture
        .fs
        .insert_file(path!("/test/.env"), b"SECRET=hunter2\n".to_vec())
        .await;
    fixture
        .fs
        .insert_tree(path!("/other"), json!({ "other.h": OTHER_H }))
        .await;
    fixture
        .project
        .update(cx, |project, cx| {
            project.find_or_create_worktree(path!("/other"), true, cx)
        })
        .await
        .expect("a second visible worktree");
    cx.run_until_parked();
    let worktree_ids = fixture.project.read_with(cx, |project, cx| {
        project
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).id())
            .collect::<Vec<_>>()
    });
    let [test_worktree, other_worktree] = worktree_ids.as_slice() else {
        panic!("two worktrees: {worktree_ids:?}");
    };
    let store = fixture
        .project
        .read_with(cx, |project, _| project.symbol_index_store().clone());
    let mut directories = Vec::new();
    for (worktree_id, files) in [
        (
            *test_worktree,
            vec![
                ("src/types.h", TYPES_H),
                ("src/main.c", MAIN_C),
                ("alias/nested.h", NESTED_H),
            ],
        ),
        (*other_worktree, vec![("other.h", OTHER_H)]),
    ] {
        let directory = tempfile::tempdir().expect("a temporary directory");
        let index = build_index_for_tests(directory.path(), &files).expect("the index");
        store.update(cx, |store, cx| {
            store.insert_index_for_tests(worktree_id, index, cx)
        });
        directories.push(directory);
    }
    cx.run_until_parked();
    directories
}

const PRUNED_TREE: &str = "\
struct outer  (src/types.h:1)
  struct inner first  (src/types.h:5)  // the first part
    int depth
  struct nested_part part  (withheld)
";

#[gpui::test]
async fn the_type_layout_tool_is_listed_and_preapproved(_cx: &mut TestAppContext) {
    assert!(crate::ASSIST_TOOL_NAMES.contains(&"getTypeLayout"));
    let tool = crate::assist_tools::tools()
        .into_iter()
        .find(|tool| tool.name == "getTypeLayout")
        .expect("the tool is listed");
    let properties = tool.input_schema["properties"]
        .as_object()
        .expect("the schema has properties");
    for field in [
        "name", "nearPath", "path", "line", "symbol", "column", "depth", "maxChars",
    ] {
        assert!(properties.contains_key(field), "{field}");
    }
    assert_eq!(tool.input_schema["required"], json!([]));
    assert!(
        tool.description
            .as_deref()
            .is_some_and(|description| description.contains("name match")),
        "{:?}",
        tool.description
    );
    let arguments = crate::cli_assist_arguments("http://127.0.0.1:4000/mcp/zed");
    let allowed = arguments.last().expect("the allowed tools");
    assert!(
        allowed
            .split(',')
            .any(|name| name == "mcp__zed__getTypeLayout"),
        "{allowed}"
    );
    assert!(crate::assist_tools::INSTRUCTIONS.contains("getTypeLayout"));
}

/// 木の途中でリンクを通る定義は、その枝だけを `(withheld)` にする。セッションのルートの外の
/// ワークツリーの型は、サンドボックスには件数だけ、ホストのクライアントには中身まで。
#[gpui::test]
async fn type_layouts_cut_what_the_client_cannot_see(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;
    let _index = add_type_files(&fixture, cx).await;

    let response =
        call_assist_tool(&fixture, "getTypeLayout", json!({ "name": "struct outer" })).await;
    assert!(!response.to_string().contains("hunter2"), "{response}");
    assert!(!response.to_string().contains("nested.h"), "{response}");
    let result = first_text_as_json(&response);
    assert_eq!(result["kind"], json!("type"));
    assert_eq!(result["source"], json!("nameIndex"));
    assert_eq!(result["text"], json!(PRUNED_TREE), "{result}");
    assert_eq!(result["pruned"], json!(true));
    assert_eq!(result["withheldDefinitions"], json!(1));
    assert_eq!(
        result["definition"],
        json!({ "path": format!("{CLIENT_ROOT}/src/types.h"), "line": 1 })
    );
    assert_eq!(
        result["definitions"],
        json!([
            { "path": format!("{CLIENT_ROOT}/src/types.h"), "line": 1 },
            { "path": format!("{CLIENT_ROOT}/src/types.h"), "line": 5 },
        ])
    );

    // 位置の形。
    let response = call_assist_tool(
        &fixture,
        "getTypeLayout",
        json!({ "path": "src/main.c", "line": 4, "symbol": "first" }),
    )
    .await;
    let result = first_text_as_json(&response);
    assert_eq!(result["title"], json!("struct inner"), "{result}");
    assert_eq!(result["inferred"]["memberOf"], json!("struct outer"));
    assert_eq!(result["inferred"]["typeText"], json!("struct inner"));

    // 見えない定義そのものを名前で引いても、件数だけ。
    for name in ["struct nested_part", "struct other_type"] {
        let response = call_assist_tool(&fixture, "getTypeLayout", json!({ "name": name })).await;
        assert!(!response.to_string().contains("hunter2"), "{response}");
        assert!(!response.to_string().contains("other.h"), "{response}");
        let result = first_text_as_json(&response);
        assert_eq!(result["kind"], json!("notFound"), "{result}");
        assert_eq!(result["withheldDefinitions"], json!(1), "{result}");
        assert_eq!(result["definitions"], json!([]));
    }

    // ホストのクライアントにはセッションのルートは関係ない (リンクを通るものは伏せたまま)。
    let source = AssistSource {
        id: "host".into(),
        label: "Claude Code".into(),
    };
    let run = |arguments: Value,
               paths: crate::assist_tools::AssistPaths<'static>,
               cx: &mut TestAppContext| {
        let project = fixture.project.clone();
        let source = source.clone();
        let mut async_cx = cx.to_async();
        async move {
            let content = crate::assist_tools::run_assist_tool_with(
                "getTypeLayout",
                arguments,
                project,
                paths,
                &source,
                &mut async_cx,
            )
            .await
            .expect("an answer");
            let text = content[0]["text"]
                .as_str()
                .expect("a text block")
                .to_string();
            serde_json::from_str::<Value>(&text).expect("JSON")
        }
    };
    let result = run(
        json!({ "name": "struct other_type" }),
        crate::assist_tools::AssistPaths::Host,
        cx,
    )
    .await;
    assert_eq!(
        result["text"],
        json!("struct other_type  (other.h:1)\n  int other_hunter2\n"),
        "{result}"
    );
    let result = run(
        json!({ "name": "struct outer" }),
        crate::assist_tools::AssistPaths::Host,
        cx,
    )
    .await;
    assert_eq!(result["text"], json!(PRUNED_TREE), "{result}");

    // 接続先に確かめさせる形 (SSH) でも同じ。
    let paths = session_paths();
    let request = remote_request(&paths, &fixture.project, cx).await;
    let project = fixture.project.clone();
    let mut async_cx = cx.to_async();
    let content = crate::assist_tools::run_assist_tool_with(
        "getTypeLayout",
        json!({ "name": "struct other_type" }),
        project,
        crate::assist_tools::AssistPaths::Sandbox(request),
        &source,
        &mut async_cx,
    )
    .await
    .expect("an answer");
    let text = content[0]["text"].as_str().expect("a text block");
    assert!(
        !text.contains("hunter2") && !text.contains("other.h"),
        "{text}"
    );
    let result: Value = serde_json::from_str(text).expect("JSON");
    assert_eq!(result["kind"], json!("notFound"));
    assert_eq!(result["withheldDefinitions"], json!(1));
}

#[gpui::test]
async fn type_layout_arguments_are_checked(cx: &mut TestAppContext) {
    let fixture = start_sandbox_fixture(cx).await;

    // 索引の無いプロジェクト: 次の一手を添えて断る。
    let response =
        call_assist_tool(&fixture, "getTypeLayout", json!({ "name": "struct outer" })).await;
    let text = tool_error_text(&response);
    assert!(text.contains("Name index not available"), "{text}");
    assert!(text.contains("searchSymbols"), "{text}");

    let _index = add_type_files(&fixture, cx).await;
    for (arguments, reason) in [
        (json!({}), "Give name"),
        (json!({ "name": "  " }), "Give name"),
        (
            json!({ "name": "struct outer", "depth": 0 }),
            "depth must be",
        ),
        (
            json!({ "name": "struct outer", "maxChars": "many" }),
            "maxChars must be",
        ),
        (json!({ "name": "struct a b" }), "isn't a type name"),
        (json!({ "path": "main.rs" }), "line must be"),
        (json!({ "path": ".env", "line": 1 }), "private_files"),
        (
            json!({ "path": "alias/nested.h", "line": 1 }),
            "goes through a link",
        ),
        (
            json!({ "name": "struct outer", "nearPath": ".env" }),
            "private_files",
        ),
        (
            json!({ "path": "main.rs", "line": 1, "symbol": "main" }),
            "isn't a C or C++ file",
        ),
    ] {
        let response = call_assist_tool(&fixture, "getTypeLayout", arguments.clone()).await;
        let text = tool_error_text(&response);
        assert!(text.contains(reason), "{arguments}: {text}");
        assert!(!text.contains("hunter2"), "{arguments}: {text}");
    }

    let response = call_assist_tool(
        &fixture,
        "getTypeLayout",
        json!({ "name": "struct outer", "depth": 9, "maxChars": 100000 }),
    )
    .await;
    let result = first_text_as_json(&response);
    assert_eq!(result["text"], json!(PRUNED_TREE), "{result}");
}
