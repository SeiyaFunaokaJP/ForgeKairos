use super::*;
use editor::test::editor_test_context::EditorTestContext;
use gpui::TestAppContext;
use indoc::indoc;
use mermaid_render::architecture_model::{emit_architecture, parse_architecture};
use mermaid_render::mermaid_model::parse_document;
use std::sync::Arc;

fn init_test(cx: &mut TestAppContext) -> Arc<workspace::AppState> {
    cx.update(|cx| {
        let state = workspace::AppState::test(cx);
        editor::init(cx);
        crate::init(cx);
        state
    })
}

/// An editor holding `state` with the modal opened on the fence the cursor
/// marker sits in.
async fn open_on(
    state: &str,
    cx: &mut TestAppContext,
) -> (
    EditorTestContext,
    Entity<MermaidEditor>,
    Arc<workspace::AppState>,
) {
    let app_state = init_test(cx);
    let mut cx = EditorTestContext::new(cx).await;
    cx.update_buffer(|buffer, cx| {
        buffer.set_language(
            Some(Arc::new(language::Language::new(
                language::LanguageConfig {
                    name: "Markdown".into(),
                    ..Default::default()
                },
                None,
            ))),
            cx,
        )
    });
    cx.set_state(state);

    let editor = cx.editor.clone();
    let target = cx
        .update_editor(|editor, _window, cx| editor.mermaid_fence_target(cx))
        .expect("the cursor is inside a mermaid fence");
    let document = parse_document(&target.source)
        .unwrap_or_else(|error| panic!("the fence should open: {error}"));
    let modal = cx.update(|window, cx| {
        cx.new(|cx| MermaidEditor::new(editor.downgrade(), target, Some(document), window, cx))
    });
    (cx, modal, app_state)
}

fn set_field(field: &Entity<Editor>, text: &str, window: &mut Window, cx: &mut App) {
    field.update(cx, |field, cx| field.set_text(text, window, cx));
}

fn group_row(modal: &MermaidEditor, index: usize) -> &ItemRow {
    modal
        .architecture()
        .and_then(|form| form.groups.get(index))
        .unwrap_or_else(|| panic!("group {index} should have a row"))
}

fn node_row(modal: &MermaidEditor, index: usize) -> &ItemRow {
    modal
        .architecture()
        .and_then(|form| form.nodes.get(index))
        .unwrap_or_else(|| panic!("node {index} should have a row"))
}

const TEMPLATE_FENCE: &str = indoc! {"
    ```mermaid
    architecture-betaˇ
        %% group はまとまり、service は部品。:L :R :T :B で線をつなぐ辺を決める
        group app(cloud)[\"アプリ\"]
        service web(server)[\"Web サーバー\"] in app
        service db(database)[\"データベース\"] in app
        service disk(disk)[\"ストレージ\"] in app
        web:R -- L:db
        web:B -- T:disk
    ```
"};

/// The template `markdown::InsertMermaidDiagram` writes is read from the action
/// rather than copied here, so that a template which drifts away from what the
/// model emits turns this red.
#[test]
fn the_insert_template_is_the_source_the_model_writes_back() {
    let check = |template: &str| {
        let document = parse_architecture(template)
            .unwrap_or_else(|error| panic!("the insert template should open: {error}"));
        assert_eq!(emit_architecture(&document), template);
    };
    check(editor::actions::MermaidDiagramKind::Architecture.template());

    // The fences these tests open are the Japanese template, which is what a
    // user with the Japanese catalog inserts.
    let _japanese = i18n::test_support::LocaleOverride::new(Some(
        i18n_catalog::test_support::bundled_catalog("ja"),
    ));
    let template = editor::actions::MermaidDiagramKind::Architecture.template();
    check(template);
    assert!(
        TEMPLATE_FENCE.replace('\u{2c7}', "").contains(template),
        "the fence these tests open should hold the insert template"
    );
}

#[gpui::test]
async fn confirming_the_template_unchanged_leaves_the_fence_alone(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.confirm(&menu::SecondaryConfirm, window, cx)
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before,
        "an unedited architecture diagram must be written back exactly as it was"
    );
    assert_eq!(
        cx.update(|_window, cx| modal.read(cx).message.clone()),
        None
    );
}

/// Renaming a group and a service carries into the `in` of every member and
/// into the edges, deleting a service takes its edges with it, and the menus'
/// choices — a parent, an end, a side, a known icon — are written back.
#[gpui::test]
async fn renames_removals_and_menu_choices_are_written_back(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let group_id = group_row(modal, 0).id.clone();
            set_field(&group_id, "system", window, cx);
            let web_id = node_row(modal, 0).id.clone();
            set_field(&web_id, "frontend", window, cx);

            modal.remove_architecture_item(ItemList::Nodes, 1, cx);
            modal.add_architecture_node(ArchitectureNodeKind::Junction, window, cx);
            modal.set_architecture_parent(ItemList::Nodes, 2, Some("system".to_string()), cx);
            modal.set_architecture_icon(ItemList::Nodes, 1, "database", window, cx);

            modal.add_architecture_edge(window, cx);
            // `junction` is a keyword merman 0.8 refuses as an id, so the new
            // junction is offered as `junction2`.
            modal.update_architecture_edge(1, cx, |edge| {
                edge.to.id = "junction2".to_string();
                edge.from.side = ArchitectureSide::Bottom;
            });
            let title = modal
                .architecture()
                .and_then(|form| form.edges.first())
                .map(|row| row.title.clone())
                .expect("the edge to the disk is kept");
            set_field(&title, "保存", window, cx);

            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            architecture-beta
                %% group はまとまり、service は部品。:L :R :T :B で線をつなぐ辺を決める
                group system(cloud)[\"アプリ\"]
                service frontend(server)[\"Web サーバー\"] in system
                service disk(database)[\"ストレージ\"] in system
                junction junction2 in system
                frontend:B -[\"保存\"]- T:disk
                frontend:B --> L:junction2
            ```
        "},
    );
}

/// Removing a group moves what was in it out one level, and removing the last
/// of everything leaves a diagram the form will not write.
#[gpui::test]
async fn removing_a_group_keeps_its_members(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.remove_architecture_item(ItemList::Groups, 0, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        indoc! {"
            ```mermaid
            architecture-beta
                %% group はまとまり、service は部品。:L :R :T :B で線をつなぐ辺を決める
                service web(server)[\"Web サーバー\"]
                service db(database)[\"データベース\"]
                service disk(disk)[\"ストレージ\"]
                web:R -- L:db
                web:B -- T:disk
            ```
        "},
    );
}

/// An id used twice, a group end on an edge that stays inside one group, a
/// group placed inside itself and an empty diagram are each reported, and
/// nothing is written.
#[gpui::test]
async fn what_merman_would_refuse_is_not_written(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    let attempt = |cx: &mut EditorTestContext,
                   edit: &dyn Fn(&mut MermaidEditor, &mut Window, &mut Context<MermaidEditor>)|
     -> Option<SharedString> {
        cx.update(|window, cx| {
            modal.update(cx, |modal, cx| {
                edit(modal, window, cx);
                modal.confirm(&menu::SecondaryConfirm, window, cx);
                modal.message.clone()
            })
        })
    };

    let message = attempt(&mut cx, &|modal, window, cx| {
        let db_id = node_row(modal, 1).id.clone();
        set_field(&db_id, "web", window, cx);
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("twice")),
        "a duplicate id should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    let message = attempt(&mut cx, &|modal, window, cx| {
        let db_id = node_row(modal, 1).id.clone();
        set_field(&db_id, "db", window, cx);
        modal.update_architecture_edge(0, cx, |edge| edge.to.group_boundary = true);
    });
    assert!(
        message.as_deref().is_some_and(
            |message| message.contains("stays inside one group, so it cannot use “Group Edge”")
        ),
        "a group end inside one group should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    let message = attempt(&mut cx, &|modal, _window, cx| {
        modal.update_architecture_edge(0, cx, |edge| edge.to.group_boundary = false);
        modal.set_architecture_parent(ItemList::Groups, 0, Some("app".to_string()), cx);
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("inside itself")),
        "a group inside itself should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    let message = attempt(&mut cx, &|modal, _window, cx| {
        modal.remove_architecture_item(ItemList::Groups, 0, cx);
        for _ in 0..3 {
            modal.remove_architecture_item(ItemList::Nodes, 0, cx);
        }
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("at least one")),
        "an empty diagram should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );
}

/// `merman` cannot draw an edge that ends on a group: the edge menus offer
/// only services and junctions, adding an edge with none of them does nothing,
/// and an edge pointed at a group anyway is not written.
#[gpui::test]
async fn an_edge_is_never_written_to_a_group(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    let message = cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            modal.update_architecture_edge(0, cx, |edge| edge.to.id = "app".to_string());
            modal.confirm(&menu::SecondaryConfirm, window, cx);
            modal.message.clone()
        })
    });
    assert!(
        message
            .as_deref()
            .is_some_and(|message| message.contains("is a group")),
        "an edge to a group should be refused, got {message:?}"
    );
    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
    );

    let edges = cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            for _ in 0..3 {
                modal.remove_architecture_item(ItemList::Nodes, 0, cx);
            }
            modal.add_architecture_edge(window, cx);
            modal
                .architecture()
                .map(|form| (form.edges.len(), form.document.edges.len()))
        })
    });
    assert_eq!(edges, Some((0, 0)));
}

/// An id typed over another row's on the way to a new one must not hand that
/// row's edges over when a button is pressed in between: the references wait
/// until the id is unique again and then follow the rename.
#[gpui::test]
async fn a_passing_duplicate_id_moves_no_edge(cx: &mut TestAppContext) {
    let (mut cx, modal, _app_state) = open_on(TEMPLATE_FENCE, cx).await;
    let before = cx.update_editor(|editor, _window, cx| editor.text(cx));

    cx.update(|window, cx| {
        modal.update(cx, |modal, cx| {
            let id = node_row(modal, 0).id.clone();
            set_field(&id, "db", window, cx);
            modal.move_architecture_item(ItemList::Nodes, 2, false, cx);
            modal.move_architecture_item(ItemList::Nodes, 1, true, cx);
            set_field(&id, "front", window, cx);
            modal.confirm(&menu::SecondaryConfirm, window, cx);
        })
    });

    assert_eq!(
        cx.update_editor(|editor, _window, cx| editor.text(cx)),
        before
            .replace("service web(", "service front(")
            .replace("web:", "front:")
    );
}
