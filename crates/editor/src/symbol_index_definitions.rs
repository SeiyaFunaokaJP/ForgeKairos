//! ADR-F68: "go to definition" in C and C++ falls back on the project's
//! name-based symbol index when the language server has no answer (none
//! running, an empty answer, or no answer in time), which is common in large
//! C projects without a complete `compile_commands.json`.

use std::{ops::Range, time::Duration};

use anyhow::Result;
use collections::HashSet;
use futures::FutureExt as _;
use gpui::{App, AsyncApp, Context, Entity, EntityId, Global, Task, WeakEntity};
use language::{Bias, Buffer, BufferSnapshot, CharKind, Point, ToOffset as _};
use project::{Location, LocationLink, Project, ProjectPath, project_settings::ProjectSettings};
use settings::Settings as _;
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

/// More candidates than this are not opened; a name this common is better
/// searched for.
const MAX_CANDIDATES: usize = 64;

/// The targets of the most recent answer that came from the index, so that
/// the list they are shown in can say they are name matches.
#[derive(Default)]
struct NameMatchTargets(HashSet<(EntityId, Range<Point>)>);

impl Global for NameMatchTargets {}

struct IndexQuery {
    name: String,
    near: Option<ProjectPath>,
    origin: Location,
}

fn is_c_family(buffer: &Buffer) -> bool {
    buffer
        .language()
        .is_some_and(|language| matches!(language.name().as_ref(), "C" | "C++"))
}

/// The identifier at `position`, for C and C++ buffers only.
fn index_query(buffer: &Entity<Buffer>, position: text::Anchor, cx: &App) -> Option<IndexQuery> {
    let buffer_ref = buffer.read(cx);
    if !is_c_family(buffer_ref) {
        return None;
    }
    let snapshot = buffer_ref.snapshot();
    let offset = position.to_offset(&snapshot);
    let (range, kind) = snapshot.surrounding_word(offset, None);
    if kind != Some(CharKind::Word) || range.is_empty() {
        return None;
    }
    let name = snapshot.text_for_range(range.clone()).collect::<String>();
    if name.starts_with(|character: char| character.is_ascii_digit()) {
        return None;
    }
    if is_in_comment_or_string(&snapshot, range.clone()) {
        return None;
    }
    let near = buffer_ref.file().map(|file| ProjectPath {
        worktree_id: file.worktree_id(cx),
        path: file.path().clone(),
    });
    Some(IndexQuery {
        name,
        near,
        origin: Location {
            buffer: buffer.clone(),
            range: snapshot.anchor_after(range.start)..snapshot.anchor_before(range.end),
        },
    })
}

/// Whether `range` is in a comment, or in a string literal other than an `#include`'s path:
/// a word there is prose, and a definition of the same name found by name alone would be a
/// jump to something unrelated. An `#include`'s path keeps the fallback it always had.
fn is_in_comment_or_string(snapshot: &BufferSnapshot, range: Range<usize>) -> bool {
    for layer in snapshot.syntax_layers_for_range(range.clone(), false) {
        let Some(mut node) = layer
            .node()
            .descendant_for_byte_range(range.start, range.end)
        else {
            continue;
        };
        let mut in_string = false;
        loop {
            match node.kind() {
                "comment" => return true,
                "string_literal"
                | "raw_string_literal"
                | "char_literal"
                | "concatenated_string" => in_string = true,
                "preproc_include" => {
                    in_string = false;
                    break;
                }
                _ => {}
            }
            match node.parent() {
                Some(parent) => node = parent,
                None => break,
            }
        }
        if in_string {
            return true;
        }
    }
    false
}

/// `Project::definitions`, completed from the symbol index in C and C++ when
/// the language server gives nothing. A language server's answer is used as it
/// is.
pub(crate) fn definitions_with_index_fallback(
    project: &mut Project,
    buffer: &Entity<Buffer>,
    position: text::Anchor,
    cx: &mut Context<Project>,
) -> Task<Result<Option<Vec<LocationLink>>>> {
    let from_language_server = project.definitions(buffer, position, cx);
    let Some(query) = index_query(buffer, position, cx) else {
        return from_language_server;
    };
    let timeout = query
        .near
        .as_ref()
        .map(|near| {
            ProjectSettings::get(Some(near.into()), cx)
                .symbol_index
                .language_server_timeout
        })
        .unwrap_or_else(|| {
            ProjectSettings::get_global(cx)
                .symbol_index
                .language_server_timeout
        });
    cx.spawn(async move |project, cx| {
        let timer = cx.background_executor().timer(timeout);
        let mut from_language_server = from_language_server.fuse();
        let answered = futures::select_biased! {
            answer = from_language_server => Some(answer),
            _ = timer.fuse() => None,
        };
        match answered {
            Some(Ok(Some(links))) if !links.is_empty() => {
                forget_name_matches(cx);
                Ok(Some(links))
            }
            Some(answer) => {
                let from_index = links_from_index(&project, query, timeout, cx).await;
                if from_index.is_empty() {
                    answer
                } else {
                    Ok(Some(from_index))
                }
            }
            None => {
                let from_index = links_from_index(&project, query, timeout, cx).await;
                if from_index.is_empty() {
                    let answer = from_language_server.await;
                    if matches!(&answer, Ok(Some(links)) if !links.is_empty()) {
                        forget_name_matches(cx);
                    }
                    answer
                } else {
                    Ok(Some(from_index))
                }
            }
        }
    })
}

fn forget_name_matches(cx: &mut AsyncApp) {
    cx.update(|cx| cx.set_global(NameMatchTargets::default()));
}

async fn links_from_index(
    project: &WeakEntity<Project>,
    query: IndexQuery,
    timeout: Duration,
    cx: &mut AsyncApp,
) -> Vec<LocationLink> {
    let Ok(definitions) = project.update(cx, |project, cx| {
        project.symbol_index_store().update(cx, |store, cx| {
            store.definitions(&query.name, query.near.clone(), cx)
        })
    }) else {
        return Vec::new();
    };
    // A remote index that never answers must not hold the jump up for long.
    let timer = cx.background_executor().timer(timeout);
    let mut definitions = futures::select_biased! {
        definitions = definitions.fuse() => definitions.log_err().unwrap_or_default(),
        _ = timer.fuse() => Vec::new(),
    };
    // Like a language server, go to the function's body rather than also
    // listing its prototype in a header; declarations only stand in for a
    // definition the index does not have.
    if definitions
        .iter()
        .any(|definition| !definition.is_declaration)
    {
        definitions.retain(|definition| !definition.is_declaration);
    }
    let mut links = Vec::new();
    let mut targets = HashSet::default();
    for definition in definitions.into_iter().take(MAX_CANDIDATES) {
        let Ok(open) = project.update(cx, |project, cx| {
            project.open_buffer(definition.path.clone(), cx)
        }) else {
            break;
        };
        let Some(buffer) = open.await.log_err() else {
            continue;
        };
        let (range, point_range) = buffer.read_with(cx, |buffer, _| {
            let start = buffer.clip_point(definition.name_range.start, Bias::Left);
            let end = buffer.clip_point(definition.name_range.end, Bias::Left);
            (
                buffer.anchor_after(start)..buffer.anchor_before(end),
                start..end,
            )
        });
        targets.insert((buffer.entity_id(), point_range));
        links.push(LocationLink {
            origin: Some(query.origin.clone()),
            target: Location { buffer, range },
        });
    }
    // A single target left behind cannot label a later list, which has more
    // than one location; a language server's answer forgets it.
    cx.update(|cx| cx.set_global(NameMatchTargets(targets)));
    links
}

struct NameMatchJump;

/// Says that a jump went to the index's only name match: unlike a list, a jump
/// has no title to tell a guess from a language server's answer.
pub(crate) fn note_name_match_jump(workspace: &Entity<Workspace>, cx: &mut App) {
    workspace.update(cx, |workspace, cx| {
        workspace.show_toast(
            Toast::new(
                NotificationId::unique::<NameMatchJump>(),
                i18n::t!("Jumped by name match (no result from the language server)"),
            )
            .autohide(),
            cx,
        );
    });
}

/// Whether every one of `locations` is a target of the index's most recent
/// answer, so that their list (or jump) is labelled as name matches. The
/// answer is used up by what it labels.
pub(crate) fn are_name_matches<'a>(
    locations: impl IntoIterator<Item = (&'a Entity<Buffer>, &'a Range<Point>)>,
    cx: &mut App,
) -> bool {
    let Some(targets) = cx.try_global::<NameMatchTargets>() else {
        return false;
    };
    let mut any = false;
    for (buffer, range) in locations {
        any = true;
        if !targets.0.contains(&(buffer.entity_id(), range.clone())) {
            return false;
        }
    }
    if any {
        cx.set_global(NameMatchTargets::default());
    }
    any
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::StreamExt as _;
    use gpui::TestAppContext;
    use language::{Language, LanguageConfig, LanguageMatcher};
    use project::symbol_index_store::build_index_for_tests;
    use workspace::notifications::NotificationId;

    use crate::{
        Editor, GoToDefinition, Navigated, editor_tests::init_test,
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    fn c_language() -> Language {
        Language::new(
            LanguageConfig {
                name: "C".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["c".to_string()],
                    ..LanguageMatcher::default()
                }
                .into(),
                ..LanguageConfig::default()
            },
            None,
        )
    }

    async fn c_context(cx: &mut TestAppContext) -> EditorLspTestContext {
        init_test(cx, |_| {});
        EditorLspTestContext::new(
            c_language(),
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await
    }

    /// Writes `others` next to the edited file and gives the project an index
    /// of them and of `edited`, the edited file's text.
    async fn install_index(
        cx: &mut EditorLspTestContext,
        edited: (&str, &str),
        others: &[(&str, &str)],
    ) -> tempfile::TempDir {
        let project = cx.update_workspace(|workspace, _, _| workspace.project().clone());
        let fs = cx.update_workspace(|workspace, _, cx| workspace.project().read(cx).fs().clone());
        for (path, text) in others {
            fs.as_fake()
                .insert_file(
                    EditorLspTestContext::root_path().join(path),
                    text.as_bytes().to_vec(),
                )
                .await;
        }
        cx.executor().run_until_parked();
        let directory = tempfile::tempdir().expect("a temporary directory");
        let mut files = vec![edited];
        files.extend_from_slice(others);
        let index = build_index_for_tests(directory.path(), &files).expect("building the index");
        cx.update_workspace(|_, _, cx| {
            let worktree_id = project
                .read(cx)
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id();
            project.update(cx, |project, cx| {
                project.symbol_index_store().update(cx, |store, cx| {
                    store.insert_index_for_tests(worktree_id, index, cx)
                })
            });
        });
        directory
    }

    fn active_editor(cx: &mut EditorLspTestContext) -> gpui::Entity<Editor> {
        cx.update_workspace(|workspace, _, cx| {
            workspace
                .active_item_as::<Editor>(cx)
                .expect("an active editor")
        })
    }

    fn active_path(cx: &mut EditorLspTestContext) -> String {
        let editor = active_editor(cx);
        cx.update_workspace(|_, _, cx| {
            let buffer = editor
                .read(cx)
                .buffer()
                .read(cx)
                .as_singleton()
                .expect("a singleton buffer");
            let path = buffer
                .read(cx)
                .file()
                .expect("a file")
                .path()
                .as_unix_str()
                .to_string();
            path
        })
    }

    fn active_selection_text(cx: &mut EditorLspTestContext) -> String {
        let editor = active_editor(cx);
        cx.update_workspace(|_, _, cx| {
            editor.update(cx, |editor, cx| {
                let snapshot = editor.display_snapshot(cx);
                let selection = editor
                    .selections
                    .newest::<language::Point>(&snapshot)
                    .range();
                editor
                    .buffer()
                    .read(cx)
                    .snapshot(cx)
                    .text_for_range(selection)
                    .collect::<String>()
            })
        })
    }

    async fn go_to_definition(cx: &mut EditorLspTestContext) -> Navigated {
        cx.update_editor(|editor, window, cx| {
            editor.go_to_definition(&GoToDefinition::default(), window, cx)
        })
        .await
        .expect("go to definition")
    }

    #[gpui::test]
    async fn test_empty_language_server_answer_falls_back_on_the_index(cx: &mut TestAppContext) {
        let mut cx = c_context(cx).await;
        cx.set_state("int main(void) { return ˇfoo(); }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.c", "int main(void) { return foo(); }\n"),
            &[("dir/other.c", "int foo(void) { return 1; }\n")],
        )
        .await;
        let mut requests = cx
            .lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );

        assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes);
        requests
            .next()
            .await
            .expect("the language server is asked first");
        assert_eq!(active_path(&mut cx), "dir/other.c");
        assert_eq!(active_selection_text(&mut cx), "foo");
        assert!(
            has_name_match_notice(&mut cx),
            "a jump by name match says so"
        );
    }

    fn has_name_match_notice(cx: &mut EditorLspTestContext) -> bool {
        cx.update_workspace(|workspace, _, _| {
            workspace.has_notification(&NotificationId::unique::<super::NameMatchJump>())
        })
    }

    fn c_language_with_grammar() -> Language {
        Language::new(
            LanguageConfig {
                name: "C".into(),
                matcher: LanguageMatcher {
                    path_suffixes: vec!["c".to_string()],
                    ..LanguageMatcher::default()
                }
                .into(),
                ..LanguageConfig::default()
            },
            Some(tree_sitter_c::LANGUAGE.into()),
        )
    }

    #[gpui::test]
    async fn test_words_in_comments_and_strings_are_not_looked_up_by_name(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorLspTestContext::new(
            c_language_with_grammar(),
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;
        let text = "#include \"foo.h\"\n#define CALL foo()\n/* see foo */\nconst char *s = \"foo\";\nchar c = 'f';\nint main(void) { return foo(); } // foo\n";
        cx.set_state(&format!("ˇ{text}"));
        let _index = install_index(
            &mut cx,
            ("dir/file.c", text),
            &[
                ("dir/other.c", "int foo(void) { return 1; }\n"),
                ("dir/defs.c", "int f(void) { return 2; }\n"),
            ],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );
        cx.executor().run_until_parked();

        let original = active_editor(&mut cx);
        // The cursor goes on the last character of `needle`.
        let place_cursor = |cx: &mut EditorLspTestContext, needle: &str| {
            let offset = text.find(needle).expect("the needle is in the text") + needle.len() - 1;
            cx.update_workspace(|workspace, window, cx| {
                workspace.activate_item(&original, true, true, window, cx);
            });
            cx.set_state(&format!("{}ˇ{}", &text[..offset], &text[offset..]));
            cx.executor().run_until_parked();
        };

        for (needle, why) in [
            ("see foo", "in a block comment"),
            ("s = \"foo", "in a string"),
            ("// foo", "in a line comment"),
            ("'f", "in a character literal"),
        ] {
            place_cursor(&mut cx, needle);
            assert_eq!(go_to_definition(&mut cx).await, Navigated::No, "{why}");
            assert_eq!(active_path(&mut cx), "dir/file.c", "{why}");
        }
        assert!(!has_name_match_notice(&mut cx));

        for (needle, why) in [
            ("CALL foo", "in a macro's body"),
            ("#include \"foo", "in an #include's path, as before"),
            ("return foo", "in code"),
        ] {
            place_cursor(&mut cx, needle);
            assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes, "{why}");
            assert_eq!(active_path(&mut cx), "dir/other.c", "{why}");
        }
    }

    #[gpui::test]
    async fn test_the_definition_is_gone_to_rather_than_its_prototypes(cx: &mut TestAppContext) {
        let mut cx = c_context(cx).await;
        cx.set_state("int main(void) { return ˇfoo(); }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.c", "int main(void) { return foo(); }\n"),
            &[
                ("dir/other.c", "int foo(void) { return 1; }\n"),
                ("dir/foo.h", "int foo(void);\n"),
                ("dir/again.h", "extern int foo(void);\n"),
            ],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );

        assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes);
        assert_eq!(active_path(&mut cx), "dir/other.c", "no list of prototypes");
        assert_eq!(active_selection_text(&mut cx), "foo");
    }

    #[gpui::test]
    async fn test_language_server_answer_is_used_as_it_is(cx: &mut TestAppContext) {
        let mut cx = c_context(cx).await;
        cx.set_state("int main(void) { return ˇfoo(); }\nint foo(void) { return 0; }\n");
        let _index = install_index(
            &mut cx,
            (
                "dir/file.c",
                "int main(void) { return foo(); }\nint foo(void) { return 0; }\n",
            ),
            &[("dir/other.c", "int foo(void) { return 1; }\n")],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(|params, _| async move {
                Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
                    uri: params.text_document_position_params.text_document.uri,
                    range: lsp::Range::new(lsp::Position::new(1, 4), lsp::Position::new(1, 7)),
                })))
            });

        assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes);
        cx.assert_editor_state(
            "int main(void) { return foo(); }\nint «fooˇ»(void) { return 0; }\n",
        );
        let editors =
            cx.update_workspace(|workspace, _, cx| workspace.items_of_type::<Editor>(cx).count());
        assert_eq!(editors, 1, "the index's candidate in other.c is not opened");
        assert!(!has_name_match_notice(&mut cx));
    }

    #[gpui::test]
    async fn test_static_definition_in_the_same_file_wins(cx: &mut TestAppContext) {
        let mut cx = c_context(cx).await;
        let text = "static int helper(void) { return 0; }\nint main(void) { return helper(); }\n";
        cx.set_state(
            "static int helper(void) { return 0; }\nint main(void) { return ˇhelper(); }\n",
        );
        let _index = install_index(
            &mut cx,
            ("dir/file.c", text),
            &[("dir/other.c", "static int helper(void) { return 1; }\n")],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );

        assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes);
        cx.assert_editor_state(
            "static int «helperˇ»(void) { return 0; }\nint main(void) { return helper(); }\n",
        );
    }

    #[gpui::test]
    async fn test_no_language_server_answer_in_time_falls_back_on_the_index(
        cx: &mut TestAppContext,
    ) {
        let mut cx = c_context(cx).await;
        cx.set_state("int main(void) { return ˇfoo(); }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.c", "int main(void) { return foo(); }\n"),
            &[("dir/other.c", "int foo(void) { return 1; }\n")],
        )
        .await;
        let (answer_now, answer_later) = futures::channel::oneshot::channel::<()>();
        let answer_later = futures::FutureExt::shared(answer_later);
        let _requests = cx
            .lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(move |params, _| {
                let answer_later = answer_later.clone();
                async move {
                    answer_later.await.ok();
                    Ok(Some(lsp::GotoDefinitionResponse::Scalar(lsp::Location {
                        uri: params.text_document_position_params.text_document.uri,
                        range: lsp::Range::new(lsp::Position::new(0, 4), lsp::Position::new(0, 8)),
                    })))
                }
            });
        // Shorter than the default, so that the index is seen to answer
        // before the test's language server request ends in any other way.
        cx.update(|_, cx| {
            use gpui::UpdateGlobal as _;
            settings::SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings
                        .project
                        .symbol_index
                        .get_or_insert_default()
                        .language_server_timeout_ms = Some(500);
                });
            });
        });

        let navigation = cx.update_editor(|editor, window, cx| {
            editor.go_to_definition(&GoToDefinition::default(), window, cx)
        });
        // Awaiting in a test lets the clock jump to the next timer, which could
        // be the request's own timeout, so the steps below only run what is
        // due and look at the result without awaiting.
        cx.executor().advance_clock(Duration::from_millis(400));
        cx.executor().run_until_parked();
        assert_eq!(
            active_path(&mut cx),
            "dir/file.c",
            "the language server still has time to answer"
        );
        cx.executor().advance_clock(Duration::from_millis(200));
        cx.executor().run_until_parked();
        assert_eq!(active_path(&mut cx), "dir/other.c");
        assert!(matches!(
            futures::FutureExt::now_or_never(navigation),
            Some(Ok(Navigated::Yes))
        ));

        // The language server's late answer does not move the cursor again.
        answer_now.send(()).ok();
        cx.executor().run_until_parked();
        assert_eq!(active_path(&mut cx), "dir/other.c");
        assert_eq!(active_selection_text(&mut cx), "foo");
    }

    #[gpui::test]
    async fn test_several_name_matches_are_listed_as_such(cx: &mut TestAppContext) {
        let mut cx = c_context(cx).await;
        cx.set_state("int main(void) { return ˇfoo(); }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.c", "int main(void) { return foo(); }\n"),
            &[
                ("dir/one.c", "int foo(void) { return 1; }\n"),
                ("dir/two.c", "int foo(void) { return 2; }\n"),
            ],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );

        for (catalog, expected) in [
            (None, "Definitions (name match) for foo"),
            (
                Some(i18n_catalog::test_support::bundled_catalog("ja")),
                "foo の定義 (名前一致)",
            ),
        ] {
            let _locale = i18n::test_support::LocaleOverride::new(
                catalog.map(|catalog| catalog as &'static dyn i18n::Catalog),
            );
            let original = active_editor(&mut cx);
            assert_eq!(go_to_definition(&mut cx).await, Navigated::Yes);
            let list = active_editor(&mut cx);
            assert_ne!(list, original, "the candidates open in a list");
            let title = cx
                .update_workspace(|_, _, cx| list.read(cx).buffer().read(cx).title(cx).to_string());
            assert_eq!(title, expected);
            cx.update_workspace(|workspace, window, cx| {
                workspace.activate_item(&original, true, true, window, cx);
            });
            cx.executor().run_until_parked();
        }
    }

    #[gpui::test]
    async fn test_other_languages_do_not_use_the_index(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                definition_provider: Some(lsp::OneOf::Left(true)),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;
        cx.set_state("fn main() { ˇfoo(); }\n");
        let _index = install_index(
            &mut cx,
            ("dir/file.rs", "fn main() { foo(); }\n"),
            &[("dir/other.c", "int foo(void) { return 1; }\n")],
        )
        .await;
        cx.lsp
            .set_request_handler::<lsp::request::GotoDefinition, _, _>(
                |_, _| async move { Ok(None) },
            );

        assert_eq!(go_to_definition(&mut cx).await, Navigated::No);
        let editors =
            cx.update_workspace(|workspace, _, cx| workspace.items_of_type::<Editor>(cx).count());
        assert_eq!(editors, 1);
    }
}
