//! ADR-F68: the project's name-based symbol index answers the symbol picker at
//! once, while the language servers' `workspace/symbol` answers join when they
//! arrive. Where both know a symbol, the language server's entry is kept.

use editor::{Bias, Editor, SelectionEffects, scroll::Autoscroll};
use gpui::{App, Entity, Task, WeakEntity, Window};
use picker::PreviewUpdate;
use project::{
    Project, Symbol,
    lsp_store::SymbolLocation,
    symbol_index_store::{IndexedDefinition, IndexedSymbolMatch, SearchMode},
};
use util::ResultExt as _;
use workspace::{
    Workspace,
    ui::{ListItem, ListItemSpacing, prelude::*},
};

/// How many index matches the picker shows.
pub(crate) const INDEX_MATCH_LIMIT: usize = 50;

/// Prefix matches first; when there are few, fuzzy ones fill up the list.
pub(crate) fn search_index(
    project: &Entity<Project>,
    query: &str,
    cx: &mut App,
) -> Task<Vec<IndexedSymbolMatch>> {
    if query.is_empty() {
        return Task::ready(Vec::new());
    }
    let store = project.read(cx).symbol_index_store().clone();
    if !store.read(cx).has_index() {
        return Task::ready(Vec::new());
    }
    let prefix = store.update(cx, |store, cx| {
        store.search(query, SearchMode::Prefix, INDEX_MATCH_LIMIT, cx)
    });
    let query = query.to_string();
    cx.spawn(async move |cx| {
        let mut found = prefix.await.log_err().unwrap_or_default();
        if found.len() < INDEX_MATCH_LIMIT {
            let fuzzy = store.update(cx, |store, cx| {
                store.search(&query, SearchMode::Fuzzy, INDEX_MATCH_LIMIT, cx)
            });
            for candidate in fuzzy.await.log_err().unwrap_or_default() {
                if found.len() >= INDEX_MATCH_LIMIT {
                    break;
                }
                if !found
                    .iter()
                    .any(|known| known.definition == candidate.definition)
                {
                    found.push(candidate);
                }
            }
        }
        found
    })
}

/// Whether a language server's symbol is the same one as the index found.
fn is_same_symbol(symbol: &Symbol, definition: &IndexedDefinition) -> bool {
    let SymbolLocation::InProject(path) = &symbol.path else {
        return false;
    };
    let row = symbol.range.start.0.row;
    *path == definition.path
        && symbol.name == definition.name
        && (row == definition.name_range.start.row
            || (definition.item_range.start.row..=definition.item_range.end.row).contains(&row))
}

/// The index matches that no language server symbol already stands for.
pub(crate) fn without_language_server_duplicates(
    matches: &[IndexedSymbolMatch],
    symbols: &[Symbol],
) -> Vec<IndexedSymbolMatch> {
    matches
        .iter()
        .filter(|found| {
            !symbols
                .iter()
                .any(|symbol| is_same_symbol(symbol, &found.definition))
        })
        .cloned()
        .collect()
}

pub(crate) fn render_index_match(
    ix: usize,
    found: &IndexedSymbolMatch,
    selected: bool,
    show_worktree_root_name: bool,
    project: &Entity<Project>,
    cx: &App,
) -> ListItem {
    let project = project.read(cx);
    let path_style = project.path_style(cx);
    let definition = &found.definition;
    let mut path = definition.path.path.to_rel_path_buf();
    if show_worktree_root_name
        && let Some(worktree) = project.worktree_for_id(definition.path.worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }
    let path = path.display(path_style).into_owned();
    let line_number = definition.name_range.start.row + 1;
    ListItem::new(ix)
        .inset(true)
        .spacing(ListItemSpacing::Sparse)
        .toggle_state(selected)
        .child(
            v_flex()
                .child(Label::new(definition.name.clone()).buffer_font(cx))
                .child(
                    h_flex()
                        .gap_1()
                        .child(Label::new(path).size(LabelSize::Small).color(Color::Muted))
                        .child(
                            Label::new(format!(":{line_number}"))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        )
                        .child(
                            Label::new(i18n::t!("name match"))
                                .size(LabelSize::Small)
                                .color(Color::Placeholder),
                        ),
                ),
        )
}

/// The preview of an index match: its buffer scrolled to the match once `loaded`, and until
/// then the file from its start.
pub(crate) fn preview_for_index_match(
    found: &IndexedSymbolMatch,
    loaded: Option<&Entity<language::Buffer>>,
    project: &Entity<Project>,
    cx: &App,
) -> Option<PreviewUpdate> {
    if let Some(buffer) = loaded {
        let snapshot = buffer.read(cx).text_snapshot();
        let start = snapshot.clip_point(found.definition.name_range.start, Bias::Left);
        let end = snapshot.clip_point(found.definition.name_range.end, Bias::Left);
        return Some(PreviewUpdate::from_buffer(
            buffer.clone(),
            picker::MatchLocation {
                anchor_range: snapshot.anchor_before(start)..snapshot.anchor_after(end),
                range: snapshot.point_to_offset(start)..snapshot.point_to_offset(end),
            },
        ));
    }
    let worktree = project
        .read(cx)
        .worktree_for_id(found.definition.path.worktree_id, cx)?;
    let abs_path = worktree.read(cx).absolutize(&found.definition.path.path);
    Some(PreviewUpdate::from_path(abs_path))
}

pub(crate) fn open_index_match(
    found: &IndexedSymbolMatch,
    secondary: bool,
    workspace: WeakEntity<Workspace>,
    project: &Entity<Project>,
    window: &mut Window,
    cx: &mut App,
) {
    let definition = found.definition.clone();
    let buffer = project.update(cx, |project, cx| {
        project.open_buffer(definition.path.clone(), cx)
    });
    window
        .spawn(cx, async move |cx| {
            let buffer = buffer.await?;
            workspace.update_in(cx, |workspace, window, cx| {
                let position = buffer
                    .read(cx)
                    .clip_point(definition.name_range.start, Bias::Left);
                let pane = if secondary {
                    workspace.adjacent_pane(window, cx)
                } else {
                    workspace.active_pane().clone()
                };
                let editor = workspace.open_project_item::<Editor>(
                    secondary.then_some(pane),
                    buffer,
                    true,
                    true,
                    true,
                    true,
                    window,
                    cx,
                );
                editor.update(cx, |editor, cx| {
                    let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                    let Some(buffer_snapshot) = multibuffer_snapshot.as_singleton() else {
                        return;
                    };
                    let text_anchor = buffer_snapshot.anchor_before(position);
                    let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(text_anchor) else {
                        return;
                    };
                    editor.change_selections(
                        SelectionEffects::scroll(Autoscroll::center()),
                        window,
                        cx,
                        |selections| selections.select_ranges([anchor..anchor]),
                    );
                });
            })?;
            anyhow::Ok(())
        })
        .detach_and_log_err(cx);
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{Arc, Mutex},
    };

    use futures::StreamExt as _;
    use gpui::{AnyElement, App, IntoElement as _, TestAppContext, VisualContext as _, Window};
    use language::{FakeLspAdapter, Language, LanguageConfig, LanguageMatcher, ToPoint as _};
    use lsp::OneOf;
    use picker::{Picker, PickerDelegate as _};
    use project::{FakeFs, Project, symbol_index_store::build_index_for_tests};
    use serde_json::json;
    use settings::SettingsStore;
    use util::path;
    use workspace::MultiWorkspace;

    use crate::ProjectSymbolsDelegate;

    const A_C: &str = "int foo(void) { return 0; }\nint fold(void) { return 1; }\n";
    const B_C: &str = "int bar(void) { return foo(); }\n";

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }

    fn lsp_symbol(name: &str, path: &Path, row: u32) -> lsp::SymbolInformation {
        #[allow(deprecated)]
        lsp::SymbolInformation {
            name: name.to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            container_name: None,
            location: lsp::Location::new(
                lsp::Uri::from_file_path(path).expect("an absolute path"),
                lsp::Range::new(lsp::Position::new(row, 4), lsp::Position::new(row, 7)),
            ),
        }
    }

    #[gpui::test]
    async fn test_index_matches_join_and_language_server_duplicates_go(cx: &mut TestAppContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "a.c": A_C, "b.c": B_C }))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(Arc::new(Language::new(
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
        )));
        let mut fake_servers = language_registry.register_fake_lsp(
            "C",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    workspace_symbol_provider: Some(OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        let _buffer = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/a.c"), cx)
            })
            .await
            .expect("opening a.c");
        let fake_server = fake_servers.next().await.expect("a language server");
        // The language server knows `foo` only.
        fake_server.set_request_handler::<lsp::WorkspaceSymbolRequest, _, _>(
            move |params: lsp::WorkspaceSymbolParams, _| async move {
                let symbols = if "foo".starts_with(&params.query) && !params.query.is_empty() {
                    vec![lsp_symbol("foo", Path::new(path!("/dir/a.c")), 0)]
                } else {
                    Vec::new()
                };
                Ok(Some(lsp::WorkspaceSymbolResponse::Flat(symbols)))
            },
        );

        let index_directory = tempfile::tempdir().expect("a temporary directory");
        let index = build_index_for_tests(index_directory.path(), &[("a.c", A_C), ("b.c", B_C)])
            .expect("building the index");
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id()
        });
        project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });

        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });

        let names = |symbols: &Picker<ProjectSymbolsDelegate>| {
            let delegate = &symbols.delegate;
            (
                delegate
                    .matches
                    .iter()
                    .map(|found| found.string.clone())
                    .collect::<Vec<_>>(),
                delegate
                    .index_matches
                    .iter()
                    .map(|found| found.definition.name.clone())
                    .collect::<Vec<_>>(),
            )
        };

        // `foo` comes from the language server; the index adds `fold` only.
        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("fo".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(
                names(symbols),
                (vec!["foo".to_string()], vec!["fold".to_string()])
            );
            assert_eq!(symbols.delegate.match_count(), 2);
        });

        // Only the index knows `bar`.
        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("ba".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(names(symbols), (Vec::new(), vec!["bar".to_string()]));
        });
        symbols.update_in(cx, |picker, window, cx| {
            assert!(
                picker.delegate.render_match(0, true, window, cx).is_some(),
                "an index match renders"
            );
        });

        // Keystrokes in a row leave the last one's answer.
        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("b".to_string(), window, cx);
            picker.update_matches("ba".to_string(), window, cx);
            picker.update_matches("zzz".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(names(symbols), (Vec::new(), Vec::new()));
        });
    }

    #[gpui::test]
    async fn test_the_preview_of_an_index_match_scrolls_to_it(cx: &mut TestAppContext) {
        init_test(cx);
        let padding = "\n".repeat(200);
        let far = format!("{padding}int far_away(void) {{ return 0; }}\n");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "far.c": far.clone() }))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let index_directory = tempfile::tempdir().expect("a temporary directory");
        let index = build_index_for_tests(index_directory.path(), &[("far.c", far.as_str())])
            .expect("building the index");
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id()
        });
        project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });
        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("far_aw".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, cx| {
            assert_eq!(symbols.delegate.index_matches.len(), 1);
            let update = symbols
                .delegate
                .try_get_preview_data_for_match(cx)
                .expect("a preview");
            assert!(
                matches!(update.source, picker::PreviewSource::Buffer(_)),
                "the loaded buffer is shown"
            );
            let location = update.match_location.expect("a place to scroll to");
            assert_eq!(location.range, 204..204 + "far_away".len(), "after `int `");
        });
    }

    /// What a picker asked its preview to show, one line per update.
    struct RecordingPreview(Arc<Mutex<Vec<String>>>);

    impl picker::PreviewBackend for RecordingPreview {
        fn update(&self, update: picker::PreviewUpdate, _: &mut Window, cx: &mut App) {
            let shown = match (&update.source, &update.match_location) {
                (picker::PreviewSource::Buffer(buffer), Some(location)) => {
                    let row = location
                        .anchor_range
                        .start
                        .to_point(&buffer.read(cx).text_snapshot())
                        .row;
                    format!("buffer at row {row}")
                }
                (picker::PreviewSource::Buffer(_), None) => "buffer".to_string(),
                (picker::PreviewSource::Path(_), _) => "path".to_string(),
                (picker::PreviewSource::Symbol(_), _) => "symbol".to_string(),
                (picker::PreviewSource::Message(_), _) => "message".to_string(),
            };
            self.0.lock().expect("not poisoned").push(shown);
        }

        fn render(&self, _: picker::PreviewLayout, _: &mut App) -> AnyElement {
            gpui::Empty.into_any_element()
        }

        fn adjust_to_new_size(&self, _: &mut Window, _: &mut App) {}

        fn clear(&self, _: &mut App) {
            self.0
                .lock()
                .expect("not poisoned")
                .push("cleared".to_string());
        }
    }

    /// The index answers after the picker has shown its matches, and the match's file loads
    /// later still: the preview is shown again each time, ending at the match's row.
    #[gpui::test]
    async fn test_the_picker_preview_follows_a_late_index_match(cx: &mut TestAppContext) {
        init_test(cx);
        let padding = "\n".repeat(200);
        let far = format!("{padding}int far_away(void) {{ return 0; }}\n");
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "far.c": far.clone() }))
            .await;
        let project = Project::test(fs.clone(), [path!("/dir").as_ref()], cx).await;
        let index_directory = tempfile::tempdir().expect("a temporary directory");
        let index = build_index_for_tests(index_directory.path(), &[("far.c", far.as_str())])
            .expect("building the index");
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .worktrees(cx)
                .next()
                .expect("a worktree")
                .read(cx)
                .id()
        });
        project.update(cx, |project, cx| {
            project.symbol_index_store().update(cx, |store, cx| {
                store.insert_index_for_tests(worktree_id, index, cx)
            })
        });
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        let shown = Arc::new(Mutex::new(Vec::new()));
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list_with_preview(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                Arc::new(RecordingPreview(shown.clone())),
                window,
                cx,
            )
        });
        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("far_aw".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert_eq!(symbols.delegate.index_matches.len(), 1);
        });
        let shown = shown.lock().expect("not poisoned").clone();
        assert_eq!(
            shown.last().map(String::as_str),
            Some("buffer at row 200"),
            "the preview ends at the match's row: {shown:?}"
        );
    }
}
