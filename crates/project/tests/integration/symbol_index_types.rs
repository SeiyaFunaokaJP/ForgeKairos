//! ADR-F68: the type engine of the symbol index, asked through the project's store.

use std::path::Path;

use fs::FakeFs;
use gpui::{AppContext as _, Entity, TestAppContext, UpdateGlobal as _};
use language::Buffer;
use project::{
    Project, ProjectPath, SymbolIndexStore, WorktreeId,
    symbol_index_store::{
        Confidence, MAX_TYPE_MEMBERS, MemberExpansionTarget, MemberSelector, MemberSummaryKind,
        ResolvedTypes, SourceLanguage, TagKind, TypeHover, TypeHoverSubject, TypeSummary,
        TypeSummaryKind, TypeTreeOptions, TypeTreeSubject, build_index_for_tests,
    },
};
use serde_json::json;
use settings::SettingsStore;
use text::Point;
use util::{path, rel_path::rel_path};

use crate::init_test;

const TYPES_H: &str = "\
#define MAX_ITEMS 16
struct inner { int depth; };
typedef struct outer {
    struct inner in; /* the inner part */
    struct { int a; int b; } anon_named;
    union { int u1; float u2; };
    int (*callback)(int);
    unsigned flags : 3;
} outer_t;
typedef outer_t outer_alias_t;
struct dup { int in_header; };
typedef struct secret_only secret_alias_t;
";

const MAIN_C: &str = "\
#include \"../inc/types.h\"
struct dup { int in_main; };
int use(outer_alias_t *value)
{
    struct outer local;
    return value->in.depth + local.flags + MAX_ITEMS;
}
";

const OTHER_DUP_H: &str = "struct dup { int in_other; };\n";
const SECRET_H: &str = "struct dup { int secret; };\nstruct secret_only { int hidden_member; };\n";

fn files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("inc/types.h", TYPES_H),
        ("src/main.c", MAIN_C),
        ("other/dup.h", OTHER_DUP_H),
        ("secret/hidden.h", SECRET_H),
    ]
}

struct Setup {
    project: Entity<Project>,
    worktree_id: WorktreeId,
    _index_directory: tempfile::TempDir,
}

/// A project whose index was built before `secret/` became private, as an index that has not
/// caught up with the settings yet would be.
async fn setup(files: &[(&str, &str)], cx: &mut TestAppContext) -> Setup {
    init_test(cx);
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files =
                    Some(vec!["**/secret/**".to_string()].into());
            });
        });
    });
    cx.executor().allow_parking();
    let fs = FakeFs::new(cx.executor());
    let mut tree = serde_json::Map::new();
    for (path, contents) in files {
        insert_json(&mut tree, path, contents);
    }
    fs.insert_tree(path!("/root"), serde_json::Value::Object(tree))
        .await;
    let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    let index_directory = tempfile::tempdir().expect("a temporary directory");
    let index = build_index_for_tests(index_directory.path(), files).expect("building the index");
    let store = store(&project, cx);
    store.update(cx, |store, cx| {
        store.insert_index_for_tests(worktree_id, index, cx)
    });
    cx.run_until_parked();
    Setup {
        project,
        worktree_id,
        _index_directory: index_directory,
    }
}

fn insert_json(tree: &mut serde_json::Map<String, serde_json::Value>, path: &str, contents: &str) {
    match path.split_once('/') {
        Some((directory, rest)) => {
            let entry = tree
                .entry(directory.to_string())
                .or_insert_with(|| json!({}));
            if let serde_json::Value::Object(children) = entry {
                insert_json(children, rest, contents);
            }
        }
        None => {
            tree.insert(path.to_string(), json!(contents));
        }
    }
}

fn store(project: &Entity<Project>, cx: &mut TestAppContext) -> Entity<SymbolIndexStore> {
    project.read_with(cx, |project, _| project.symbol_index_store().clone())
}

fn project_path(worktree_id: WorktreeId, path: &str) -> ProjectPath {
    ProjectPath {
        worktree_id,
        path: rel_path(path).into_arc(),
    }
}

/// The row and column of the `occurrence`-th `needle` in `text`.
fn point_of(text: &str, needle: &str, occurrence: usize) -> Point {
    let offset = text
        .match_indices(needle)
        .nth(occurrence)
        .map(|(offset, _)| offset)
        .unwrap_or_else(|| panic!("{needle:?} #{occurrence} not in the text"));
    let before = &text[..offset];
    let row = before.matches('\n').count() as u32;
    let column = (offset - before.rfind('\n').map_or(0, |newline| newline + 1)) as u32;
    Point::new(row, column)
}

async fn describe(
    setup: &Setup,
    path: &str,
    position: Point,
    contents: Option<language::BufferSnapshot>,
    cx: &mut TestAppContext,
) -> Option<TypeHover> {
    let store = store(&setup.project, cx);
    let path = project_path(setup.worktree_id, path);
    store
        .update(cx, |store, cx| {
            store.describe_type_at(path, position, contents, Some(SourceLanguage::C), 1, cx)
        })
        .await
        .expect("describe_type_at")
}

async fn resolve(
    setup: &Setup,
    name: &str,
    tag: Option<TagKind>,
    near: Option<&str>,
    limit: usize,
    cx: &mut TestAppContext,
) -> ResolvedTypes {
    let store = store(&setup.project, cx);
    let near = near.map(|near| project_path(setup.worktree_id, near));
    store
        .update(cx, |store, cx| {
            store.resolve_type(name, tag, near, limit, cx)
        })
        .await
        .expect("resolve_type")
}

fn member_names(summary: &TypeSummary) -> Vec<Option<&str>> {
    summary
        .members
        .iter()
        .map(|member| member.name.as_deref())
        .collect()
}

fn located(types: &ResolvedTypes) -> Vec<(&str, bool)> {
    types
        .types
        .iter()
        .map(|summary| (summary.path.path.as_unix_str(), summary.members_read))
        .collect()
}

#[gpui::test]
async fn test_a_type_name_is_described_through_its_typedefs(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;
    let hover = describe(
        &setup,
        "src/main.c",
        point_of(MAIN_C, "outer_alias_t", 0),
        None,
        cx,
    )
    .await
    .expect("a hover");
    assert_eq!(
        hover.subject,
        TypeHoverSubject::TypeName {
            name: "outer_alias_t".to_string(),
            tag: None
        }
    );
    assert_eq!(hover.range.start, point_of(MAIN_C, "outer_alias_t", 0));
    let outer = hover.types.types.first().expect("the definition");
    assert_eq!(outer.name, "struct outer");
    assert_eq!(outer.kind, TypeSummaryKind::Struct);
    assert_eq!(outer.path, project_path(setup.worktree_id, "inc/types.h"));
    assert_eq!(outer.range.start.row, 2);
    assert_eq!(
        outer
            .typedef_chain
            .iter()
            .map(|step| (step.name.as_str(), step.target.as_str()))
            .collect::<Vec<_>>(),
        vec![("outer_alias_t", "outer_t"), ("outer_t", "struct outer")],
        "`outer_t` is declared by the definition itself, and still a step"
    );
    assert!(outer.members_read);
    assert_eq!(
        member_names(outer),
        vec![
            Some("in"),
            Some("anon_named"),
            None,
            Some("callback"),
            Some("flags")
        ]
    );
    let member = |index: usize| outer.members.get(index).expect("a member");
    assert_eq!(member(0).comment.as_deref(), Some("the inner part"));
    assert_eq!(member(0).core_name.as_deref(), Some("inner"));
    assert_eq!(member(0).tag, Some(TagKind::Struct));
    assert!(member(0).expandable);
    let anon_named = member(1).inline.as_ref().expect("a body defined in place");
    assert_eq!(member_names(anon_named), vec![Some("a"), Some("b")]);
    assert_eq!(anon_named.members[1].index_path, vec![1, 1]);
    assert_eq!(member(2).kind, MemberSummaryKind::AnonymousAggregate);
    assert_eq!(
        member_names(member(2).inline.as_ref().expect("the union's body")),
        vec![Some("u1"), Some("u2")]
    );
    assert!(member(3).is_function_pointer);
    assert_eq!(member(3).signature.as_deref(), Some("int (*)(int)"));
    assert!(!member(3).expandable);
    assert_eq!(member(4).bit_width.as_deref(), Some("3"));
    assert_eq!(hover.types.total, 1);
    assert!(!hover.types.truncated);
}

#[gpui::test]
async fn test_member_chains_variables_and_macros_are_described(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;

    let depth = describe(&setup, "src/main.c", point_of(MAIN_C, "depth", 0), None, cx)
        .await
        .expect("a hover");
    let TypeHoverSubject::Member {
        steps, complete, ..
    } = &depth.subject
    else {
        panic!("{:#?}", depth.subject);
    };
    assert!(complete);
    assert_eq!(
        steps
            .iter()
            .map(|step| (
                step.name.as_str(),
                step.owner
                    .as_ref()
                    .map(|owner| owner.display_name.as_str())
                    .unwrap_or_default()
            ))
            .collect::<Vec<_>>(),
        vec![("in", "struct outer"), ("depth", "struct inner")]
    );
    assert!(depth.types.types.is_empty(), "an int has no definition");

    let inner = describe(&setup, "src/main.c", point_of(MAIN_C, "in.", 0), None, cx)
        .await
        .expect("a hover");
    assert_eq!(
        inner
            .types
            .types
            .first()
            .map(|summary| summary.name.as_str()),
        Some("struct inner")
    );

    let local = describe(
        &setup,
        "src/main.c",
        point_of(MAIN_C, "local.", 0),
        None,
        cx,
    )
    .await
    .expect("a hover");
    match &local.subject {
        TypeHoverSubject::Variable {
            core_name,
            confidence,
            declaration,
            ..
        } => {
            assert_eq!(core_name.as_deref(), Some("outer"));
            assert_eq!(*confidence, Confidence::Local);
            assert_eq!(
                declaration.as_ref().map(|range| range.start),
                Some(point_of(MAIN_C, "local;", 0))
            );
        }
        other => panic!("{other:#?}"),
    }
    assert_eq!(
        local
            .types
            .types
            .first()
            .map(|summary| summary.name.as_str()),
        Some("struct outer")
    );

    let macro_hover = describe(
        &setup,
        "src/main.c",
        point_of(MAIN_C, "MAX_ITEMS", 0),
        None,
        cx,
    )
    .await
    .expect("a hover");
    match &macro_hover.subject {
        TypeHoverSubject::Macro { macros } => {
            assert_eq!(macros.len(), 1);
            assert_eq!(macros[0].text, "#define MAX_ITEMS 16");
            assert_eq!(
                macros[0].path,
                project_path(setup.worktree_id, "inc/types.h")
            );
        }
        other => panic!("{other:#?}"),
    }
}

#[gpui::test]
async fn test_same_name_types_are_ranked_counted_and_hidden_ones_left_out(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;
    let dup = resolve(
        &setup,
        "dup",
        Some(TagKind::Struct),
        Some("src/main.c"),
        1,
        cx,
    )
    .await;
    assert_eq!(
        located(&dup),
        vec![
            ("src/main.c", true),
            ("inc/types.h", false),
            ("other/dup.h", false)
        ],
        "the closest is read, the rest are listed; the private one is not among them"
    );
    assert_eq!(dup.total, 3, "the private definition is not counted");
    assert!(!dup.truncated);
    assert_eq!(
        member_names(&dup.types[0]),
        vec![Some("in_main")],
        "the members of the closest one"
    );
    assert!(dup.types[1].members.is_empty());

    let two = resolve(
        &setup,
        "dup",
        Some(TagKind::Struct),
        Some("src/main.c"),
        2,
        cx,
    )
    .await;
    assert_eq!(
        located(&two),
        vec![
            ("src/main.c", true),
            ("inc/types.h", true),
            ("other/dup.h", false)
        ]
    );

    // Found only in the private file.
    let secret = resolve(&setup, "secret_only", Some(TagKind::Struct), None, 1, cx).await;
    assert!(secret.types.is_empty(), "{secret:#?}");
    assert_eq!(secret.total, 0);
    // A typedef that names a private type stays an unresolved typedef.
    let alias = resolve(&setup, "secret_alias_t", None, None, 1, cx).await;
    assert_eq!(alias.types.len(), 1);
    assert_eq!(alias.types[0].kind, TypeSummaryKind::Typedef);
    assert!(alias.types[0].members.is_empty());
    assert!(alias.types[0].typedef_chain.is_empty());
    // A file that is private itself is not described.
    assert!(
        describe(&setup, "secret/hidden.h", Point::new(1, 8), None, cx)
            .await
            .is_none()
    );

    // Once the file is no longer private, it is found and counted.
    cx.update(|cx| {
        SettingsStore::update_global(cx, |store, cx| {
            store.update_user_settings(cx, |settings| {
                settings.project.worktree.private_files = Some(Vec::new().into());
            });
        });
    });
    cx.run_until_parked();
    let dup = resolve(
        &setup,
        "dup",
        Some(TagKind::Struct),
        Some("src/main.c"),
        1,
        cx,
    )
    .await;
    assert_eq!(dup.total, 4);
    let secret = resolve(&setup, "secret_only", Some(TagKind::Struct), None, 1, cx).await;
    assert_eq!(
        secret.types.first().map(member_names),
        Some(vec![Some("hidden_member")])
    );
}

#[gpui::test]
async fn test_members_are_expanded_one_level(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;
    let outer = resolve(&setup, "outer", Some(TagKind::Struct), None, 1, cx).await;
    let owner = outer.types[0].type_ref.clone().expect("a reference");
    let store = store(&setup.project, cx);
    let mut expand = async |selector: MemberSelector| {
        let owner = owner.clone();
        store
            .update(cx, |store, cx| store.expand_member(owner, selector, 1, cx))
            .await
            .expect("expand_member")
    };

    let inner = expand(MemberSelector::Name("in".to_string())).await;
    assert_eq!(
        inner
            .member
            .as_ref()
            .and_then(|member| member.name.as_deref()),
        Some("in")
    );
    let MemberExpansionTarget::Types(types) = &inner.target else {
        panic!("{inner:#?}");
    };
    assert_eq!(types.types[0].name, "struct inner");
    assert_eq!(member_names(&types.types[0]), vec![Some("depth")]);

    let anon = expand(MemberSelector::Path(vec![1])).await;
    let MemberExpansionTarget::Inline(body) = &anon.target else {
        panic!("{anon:#?}");
    };
    assert_eq!(member_names(body), vec![Some("a"), Some("b")]);
    // Through the anonymous union, the way C looks members up.
    let u2 = expand(MemberSelector::Name("u2".to_string())).await;
    assert_eq!(u2.target, MemberExpansionTarget::NotExpandable);
    assert_eq!(
        expand(MemberSelector::Path(vec![1, 0]))
            .await
            .member
            .map(|member| member.text),
        Some("int a".to_string())
    );
    let flags = expand(MemberSelector::Name("flags".to_string())).await;
    assert_eq!(flags.target, MemberExpansionTarget::NotExpandable);
    let missing = expand(MemberSelector::Name("missing".to_string())).await;
    assert_eq!(missing.target, MemberExpansionTarget::NotFound);
    assert_eq!(
        expand(MemberSelector::Path(vec![99])).await.target,
        MemberExpansionTarget::NotFound
    );
}

/// An edited buffer is read only where its file would be: one whose name is a link to a private
/// file is not, though the link's own name is not private.
#[gpui::test]
async fn test_edited_buffers_of_links_to_hidden_files_are_not_read(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;
    let fs = setup
        .project
        .read_with(cx, |project, _| project.fs().clone());
    let fs = fs.as_fake();
    fs.insert_file(path!("/root/src/plain.h"), b"\n".to_vec())
        .await;
    fs.insert_symlink(
        path!("/root/src/alias.h"),
        std::path::PathBuf::from(path!("/root/secret/hidden.h")),
    )
    .await;
    cx.run_until_parked();

    let mut buffers = Vec::new();
    for (path, added) in [
        (
            "src/plain.h",
            "struct plain_edited { int shown_member; };\n",
        ),
        (
            "src/alias.h",
            "struct alias_edited { int hidden_member; };\n",
        ),
    ] {
        let buffer = setup
            .project
            .update(cx, |project, cx| {
                project.open_buffer(project_path(setup.worktree_id, path), cx)
            })
            .await
            .expect("opening the file");
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, added)], None, cx));
        assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
        buffers.push(buffer);
    }
    cx.run_until_parked();

    let plain = resolve(
        &setup,
        "plain_edited",
        Some(TagKind::Struct),
        Some("src/plain.h"),
        1,
        cx,
    )
    .await;
    assert_eq!(located(&plain), [("src/plain.h", true)], "{plain:#?}");
    let alias = resolve(
        &setup,
        "alias_edited",
        Some(TagKind::Struct),
        Some("src/alias.h"),
        1,
        cx,
    )
    .await;
    assert!(alias.types.is_empty(), "{alias:#?}");
}

/// A request takes at most 32 edited buffers on the foreground; the files of the others are read
/// from disk.
#[gpui::test]
async fn test_the_edited_buffers_one_request_reads_are_capped(cx: &mut TestAppContext) {
    const EDITED: usize = 33;
    let setup = setup(&files(), cx).await;
    let fs = setup
        .project
        .read_with(cx, |project, _| project.fs().clone());
    let fs = fs.as_fake();
    let edited_files = (0..EDITED)
        .map(|index| (format!("e{index}.h"), json!("\n")))
        .collect::<serde_json::Map<_, _>>();
    fs.insert_tree(
        path!("/root/edited"),
        serde_json::Value::Object(edited_files),
    )
    .await;
    cx.run_until_parked();
    let mut buffers = Vec::new();
    for index in 0..EDITED {
        let buffer = setup
            .project
            .update(cx, |project, cx| {
                project.open_buffer(
                    project_path(setup.worktree_id, &format!("edited/e{index}.h")),
                    cx,
                )
            })
            .await
            .expect("opening the file");
        let added = format!("struct e{index}_edited {{ int member; }};\n");
        buffer.update(cx, |buffer, cx| buffer.edit([(0..0, added)], None, cx));
        buffers.push(buffer);
    }
    cx.run_until_parked();

    let mut found = 0;
    for index in 0..EDITED {
        let resolved = resolve(
            &setup,
            &format!("e{index}_edited"),
            Some(TagKind::Struct),
            Some(&format!("edited/e{index}.h")),
            1,
            cx,
        )
        .await;
        found += resolved.types.len();
    }
    assert_eq!(found, 32);
}

/// Past the limit, a request reads the most recently edited buffers.
#[gpui::test]
async fn test_the_most_recently_edited_buffers_are_read_first(cx: &mut TestAppContext) {
    const EDITED: usize = 33;
    let setup = setup(&files(), cx).await;
    let fs = setup
        .project
        .read_with(cx, |project, _| project.fs().clone());
    let fs = fs.as_fake();
    let edited_files = (0..EDITED)
        .map(|index| (format!("e{index}.h"), json!("\n")))
        .collect::<serde_json::Map<_, _>>();
    fs.insert_tree(
        path!("/root/edited"),
        serde_json::Value::Object(edited_files),
    )
    .await;
    cx.run_until_parked();
    let mut buffers = Vec::new();
    for index in 0..EDITED {
        let buffer = setup
            .project
            .update(cx, |project, cx| {
                project.open_buffer(
                    project_path(setup.worktree_id, &format!("edited/e{index}.h")),
                    cx,
                )
            })
            .await
            .expect("opening the file");
        buffers.push(buffer);
    }
    // The first buffer is edited again last, which leaves the second as the least recently
    // edited.
    for index in (0..EDITED).chain([0]) {
        let added = format!("struct e{index}_recent {{ int member; }};\n");
        buffers[index].update(cx, |buffer, cx| buffer.edit([(0..0, added)], None, cx));
    }
    cx.run_until_parked();

    let mut found = Vec::new();
    for index in 0..EDITED {
        let resolved = resolve(
            &setup,
            &format!("e{index}_recent"),
            Some(TagKind::Struct),
            Some(&format!("edited/e{index}.h")),
            1,
            cx,
        )
        .await;
        if !resolved.types.is_empty() {
            found.push(index);
        }
    }
    let expected: Vec<usize> = (0..EDITED).filter(|&index| index != 1).collect();
    assert_eq!(found, expected);
}

/// A member added in an open buffer and not saved is listed, expanded and drawn: the store reads
/// the project's edited buffers rather than their files on disk.
#[gpui::test]
async fn test_edited_buffers_are_read_instead_of_their_files(cx: &mut TestAppContext) {
    let setup = setup(&files(), cx).await;
    let outer = resolve(&setup, "outer", Some(TagKind::Struct), None, 1, cx).await;
    let owner = outer.types[0].type_ref.clone().expect("a reference");
    let store = store(&setup.project, cx);
    let types_h = project_path(setup.worktree_id, "inc/types.h");
    let buffer = setup
        .project
        .update(cx, |project, cx| project.open_buffer(types_h, cx))
        .await
        .expect("opening the header");
    cx.run_until_parked();

    let expand_added = async |cx: &mut TestAppContext| {
        let owner = owner.clone();
        store
            .update(cx, |store, cx| {
                store.expand_member(owner, MemberSelector::Name("added".to_string()), 1, cx)
            })
            .await
            .expect("expand_member")
    };
    let outer_members = async |cx: &mut TestAppContext| {
        let hover = describe(
            &setup,
            "src/main.c",
            point_of(MAIN_C, "outer_alias_t", 0),
            None,
            cx,
        )
        .await
        .expect("a hover");
        let outer = hover.types.types.first().expect("the definition").clone();
        member_names(&outer)
            .into_iter()
            .map(|name| name.map(str::to_string))
            .collect::<Vec<_>>()
    };

    // Open but not edited: the file on disk, which has no such member.
    assert_eq!(
        expand_added(cx).await.target,
        MemberExpansionTarget::NotFound
    );

    let anchor = "    struct inner in; /* the inner part */\n";
    let offset = TYPES_H.find(anchor).expect("the member") + anchor.len();
    buffer.update(cx, |buffer, cx| {
        buffer.edit([(offset..offset, "    struct inner added;\n")], None, cx)
    });
    assert!(buffer.read_with(cx, |buffer, _| buffer.is_dirty()));

    assert!(
        outer_members(cx).await.contains(&Some("added".to_string())),
        "the definition is read from the buffer"
    );
    let added = expand_added(cx).await;
    let MemberExpansionTarget::Types(types) = &added.target else {
        panic!("{added:#?}");
    };
    assert_eq!(types.types[0].name, "struct inner");
    let tree = store
        .update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Ref(owner.clone()),
                TypeTreeOptions::default(),
                cx,
            )
        })
        .await
        .expect("type_tree_text")
        .expect("a tree");
    assert!(tree.text.contains("struct inner added"), "{}", tree.text);

    // The edit undone, the buffer is no longer edited, and the file on disk is read again.
    buffer.update(cx, |buffer, cx| buffer.undo(cx));
    assert!(!buffer.read_with(cx, |buffer, _| buffer.is_dirty()));
    assert_eq!(
        expand_added(cx).await.target,
        MemberExpansionTarget::NotFound
    );
    assert!(!outer_members(cx).await.contains(&Some("added".to_string())));
}

#[gpui::test]
async fn test_unsaved_edits_and_crlf_files_are_described_at_the_right_place(
    cx: &mut TestAppContext,
) {
    let crlf = "\
#include \"../inc/types.h\"\r
\r
\r
struct dup { int in_crlf; };\r
int f(struct dup *d) { return d->in_crlf; }\r
";
    let mut files = files();
    files.push(("src/crlf.c", crlf));
    let setup = setup(&files, cx).await;

    let lf = crlf.replace("\r\n", "\n");
    let buffer = cx.new(|cx| Buffer::local(crlf, cx));
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    assert_eq!(snapshot.line_ending(), text::LineEnding::Windows);
    let position = point_of(&lf, "in_crlf;", 1);
    let hover = describe(&setup, "src/crlf.c", position, Some(snapshot), cx)
        .await
        .expect("a hover");
    assert_eq!(hover.name, "in_crlf");
    assert_eq!(hover.range.start, position, "rows and columns do not shift");
    let TypeHoverSubject::Member { steps, .. } = &hover.subject else {
        panic!("{:#?}", hover.subject);
    };
    let owner = steps[0].owner.as_ref().expect("the owner");
    assert_eq!(
        (owner.path.clone(), owner.range.start.row),
        (Some(project_path(setup.worktree_id, "src/crlf.c")), 3),
        "the definition in the same file, read at the index's offsets"
    );

    // An edit not saved yet: a new type only the buffer has.
    let edited = format!("{lf}struct fresh {{ int new_field; }};\nstruct fresh *pointer;\n");
    buffer.update(cx, |buffer, cx| buffer.set_text(edited.clone(), cx));
    let snapshot = buffer.read_with(cx, |buffer, _| buffer.snapshot());
    let hover = describe(
        &setup,
        "src/crlf.c",
        point_of(&edited, "fresh", 1),
        Some(snapshot),
        cx,
    )
    .await
    .expect("a hover");
    let fresh = hover.types.types.first().expect("the new type");
    assert_eq!(fresh.name, "struct fresh");
    assert_eq!(member_names(fresh), vec![Some("new_field")]);
    // Without the buffer, the file on disk does not have it.
    let from_disk = describe(&setup, "src/crlf.c", Point::new(0, 0), None, cx).await;
    assert!(from_disk.is_none_or(|hover| hover.types.types.is_empty()));
}

#[gpui::test]
async fn test_type_answers_are_limited(cx: &mut TestAppContext) {
    let members = (0..MAX_TYPE_MEMBERS + 100)
        .map(|index| format!("    int member_{index};\n"))
        .collect::<String>();
    let big = format!("struct big {{\n{members}}};\n");
    let mut files = files();
    files.push(("inc/big.h", big.as_str()));
    let setup = setup(&files, cx).await;
    let found = resolve(&setup, "big", Some(TagKind::Struct), None, 1, cx).await;
    let summary = found.types.first().expect("the type");
    assert_eq!(summary.members.len(), MAX_TYPE_MEMBERS);
    assert!(summary.truncated);

    let store = store(&setup.project, cx);
    let full = store
        .update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Name {
                    name: "outer_alias_t".to_string(),
                    tag: None,
                    near: None,
                },
                TypeTreeOptions::default(),
                cx,
            )
        })
        .await
        .expect("type_tree_text")
        .expect("a tree");
    assert!(!full.truncated, "{}", full.text);
    assert!(full.text.contains("struct outer"), "{}", full.text);
    assert!(
        full.text.contains("int depth"),
        "one level of `in` is expanded: {}",
        full.text
    );
    assert!(
        full.definitions
            .contains(&(project_path(setup.worktree_id, "inc/types.h"), 1)),
        "{:?}",
        full.definitions
    );
    let cut = store
        .update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Name {
                    name: "big".to_string(),
                    tag: Some(TagKind::Struct),
                    near: None,
                },
                TypeTreeOptions {
                    max_chars: 200,
                    ..TypeTreeOptions::default()
                },
                cx,
            )
        })
        .await
        .expect("type_tree_text")
        .expect("a tree");
    assert!(cut.truncated);
    assert!(cut.text.chars().count() <= 200, "{}", cut.text);
    // Asking for more than the host allows is held to its limits.
    let options = TypeTreeOptions {
        max_depth: 100,
        max_pointer_hops: 100,
        max_nodes: usize::MAX,
        max_chars: usize::MAX,
    }
    .clamped();
    assert_eq!(options.max_depth, 4);
    assert_eq!(options.max_nodes, 2000);

    let hidden = store
        .update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Name {
                    name: "secret_only".to_string(),
                    tag: Some(TagKind::Struct),
                    near: None,
                },
                TypeTreeOptions::default(),
                cx,
            )
        })
        .await
        .expect("type_tree_text");
    assert!(hidden.is_none(), "{hidden:#?}");
}

/// What a host answers a remote client with goes through the same functions, so their own
/// limits are what holds a client asking for more.
#[gpui::test]
async fn test_requests_for_more_than_the_limits_are_held_to_them(cx: &mut TestAppContext) {
    let many = (0..6)
        .map(|index| {
            (
                format!("many/m{index}.h"),
                "struct many { int field; };\n".to_string(),
            )
        })
        .collect::<Vec<_>>();
    let long_members = (0..3000)
        .map(|index| format!("    int a_member_with_a_rather_long_name_number_{index};\n"))
        .collect::<String>();
    let long = format!("struct long_one {{\n{long_members}}};\n");
    let mut files: Vec<(&str, &str)> = files();
    for (path, contents) in &many {
        files.push((path.as_str(), contents.as_str()));
    }
    files.push(("inc/long.h", long.as_str()));
    let setup = setup(&files, cx).await;

    let found = resolve(&setup, "many", Some(TagKind::Struct), None, usize::MAX, cx).await;
    assert_eq!(found.types.len(), 6, "{:?}", located(&found));
    assert_eq!(
        found
            .types
            .iter()
            .filter(|summary| summary.members_read)
            .count(),
        project::symbol_index_store::MAX_TYPE_READ_LIMIT
    );

    let store = store(&setup.project, cx);
    let tree = store
        .update(cx, |store, cx| {
            store.type_tree_text(
                TypeTreeSubject::Name {
                    name: "long_one".to_string(),
                    tag: Some(TagKind::Struct),
                    near: None,
                },
                TypeTreeOptions {
                    max_depth: usize::MAX,
                    max_pointer_hops: usize::MAX,
                    max_nodes: usize::MAX,
                    max_chars: usize::MAX,
                },
                cx,
            )
        })
        .await
        .expect("type_tree_text")
        .expect("a tree");
    assert!(tree.truncated);
    assert!(
        tree.text.chars().count() <= project::symbol_index_store::MAX_TYPE_TREE_CHARS,
        "{}",
        tree.text.chars().count()
    );
}

#[gpui::test]
async fn test_a_project_without_an_index_answers_with_nothing(cx: &mut TestAppContext) {
    init_test(cx);
    let fs = FakeFs::new(cx.executor());
    fs.insert_tree(path!("/root"), json!({ "main.c": MAIN_C }))
        .await;
    let project = Project::test(fs, [Path::new(path!("/root"))], cx).await;
    let worktree_id = project.read_with(cx, |project, cx| {
        project
            .worktrees(cx)
            .next()
            .expect("the project has a worktree")
            .read(cx)
            .id()
    });
    let store = store(&project, cx);
    let hover = store
        .update(cx, |store, cx| {
            store.describe_type_at(
                project_path(worktree_id, "main.c"),
                Point::new(2, 10),
                None,
                None,
                1,
                cx,
            )
        })
        .await
        .expect("no error");
    assert!(hover.is_none());
    let types = store
        .update(cx, |store, cx| {
            store.resolve_type("outer", None, None, 1, cx)
        })
        .await
        .expect("no error");
    assert_eq!(types, ResolvedTypes::default());
}

/// `typedef struct device { ... } device_t;` is one definition, though the index records
/// `struct device` at its first row and `device_t` at the row of its name below the body: it
/// is counted once, next to a forward declaration that has no body to show.
#[gpui::test]
async fn test_a_typedef_around_its_body_and_forward_declarations_count_once(
    cx: &mut TestAppContext,
) {
    const DEVICE_H: &str = "\
struct device;
struct lonely;
typedef struct lonely lonely_t;
struct pair;
typedef struct device {
    int state;
    struct device *next;
} device_t;
";
    const PAIR_A_H: &str = "struct pair {\n    int from_a;\n};\n";
    const PAIR_B_H: &str = "struct pair {\n    int from_b;\n};\n";
    const DEVICE_C: &str = "\
#include \"../inc/device.h\"
int open_device(device_t *dev)
{
    return dev->state;
}
";
    let setup = setup(
        &[
            ("inc/device.h", DEVICE_H),
            ("inc/pair_a.h", PAIR_A_H),
            ("inc/pair_b.h", PAIR_B_H),
            ("src/device.c", DEVICE_C),
        ],
        cx,
    )
    .await;
    let described = |types: &ResolvedTypes| {
        types
            .types
            .iter()
            .map(|summary| {
                (
                    summary.name.clone(),
                    summary.path.path.as_unix_str().to_string(),
                    summary.range.start.row,
                    summary.is_forward_declaration,
                )
            })
            .collect::<Vec<_>>()
    };
    let device = vec![(
        "struct device".to_string(),
        "inc/device.h".to_string(),
        4,
        false,
    )];

    let hover = describe(
        &setup,
        "src/device.c",
        point_of(DEVICE_C, "device_t", 0),
        None,
        cx,
    )
    .await
    .expect("a hover");
    assert_eq!(described(&hover.types), device, "{hover:#?}");
    assert_eq!(hover.types.total, 1);
    let variable = describe(
        &setup,
        "src/device.c",
        point_of(DEVICE_C, "dev->state", 0),
        None,
        cx,
    )
    .await
    .expect("a hover");
    assert_eq!(described(&variable.types), device);
    assert_eq!(variable.types.total, 1);
    for (name, tag) in [("device_t", None), ("device", Some(TagKind::Struct))] {
        for limit in [1, 3] {
            let types = resolve(&setup, name, tag, Some("src/device.c"), limit, cx).await;
            assert_eq!(described(&types), device, "{name}, reading {limit}");
            assert_eq!(types.total, 1, "{name}, reading {limit}");
        }
    }

    // With no body anywhere, the forward declaration is what there is to show.
    let lonely = resolve(&setup, "lonely_t", None, Some("src/device.c"), 1, cx).await;
    assert_eq!(
        described(&lonely),
        vec![(
            "struct lonely".to_string(),
            "inc/device.h".to_string(),
            1,
            true
        )]
    );
    assert_eq!(lonely.total, 1);

    // Two bodies are two candidates; the forward declaration next to them is not a third.
    for limit in [1, 3] {
        let pair = resolve(
            &setup,
            "pair",
            Some(TagKind::Struct),
            Some("src/device.c"),
            limit,
            cx,
        )
        .await;
        assert_eq!(pair.total, 2, "reading {limit}: {pair:#?}");
        assert_eq!(pair.types.len(), 2);
        assert!(
            pair.types
                .iter()
                .all(|summary| !summary.is_forward_declaration),
            "{pair:#?}"
        );
    }
}
