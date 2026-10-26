use std::collections::VecDeque;
use std::fmt;

use gpui::{App, AsyncApp, Entity};
use language::{Buffer, Location};
use project::{CodeAction, Project};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use text::ToPoint as _;
use text::{Anchor, Point};

use crate::ToolCallEventStream;

/// Identifies a specific symbol (declaration or usage) in the source code.
///
/// Use the file path, line number, and symbol name from file outlines, grep results, or other tool outputs to populate these fields.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct SymbolLocator {
    /// The relative path of the file containing the symbol (e.g. "crates/editor/src/editor.rs").
    pub file_path: String,

    /// The 1-based line number where the symbol appears. Use the line numbers from file outlines or grep results.
    pub line: u32,

    /// The name of the symbol (function name, type name, variable name, etc.)
    pub symbol_name: String,
}

pub struct PendingCodeActions {
    pub actions: Vec<CodeAction>,
    pub buffer: Entity<Buffer>,
}

/// Model-facing explanation for a language request that produced nothing
/// because no language server is running for the file.
///
/// "The language server found nothing" and "there is no language server" reach
/// the model as the same empty result, and small local models read the
/// ambiguity as a broken editor setup — the report that prompted this had the
/// model telling the user to install a VS Code extension instead of reporting
/// what actually happened. So state the fact, name the two causes this fork
/// actually produces (nothing installed, or the automatic download failed),
/// rule out the wrong fix, and give the next step. The egress allowlist is no
/// longer one of them: it stopped covering language servers in ADR-F38.
pub fn no_language_server_message(action: &str, file_path: &str) -> String {
    format!(
        "Couldn't {action}: no language server is running for `{file_path}` (no language \
         server for this language is installed, or its automatic download failed). Changing \
         editor or LSP settings won't fix this. First tell the user that a language server \
         needs to be installed. Then, to just find the symbol, use the `grep` tool. If you \
         need to rewrite it, find every occurrence with `grep` first, then use `edit_file`, \
         and tell the user plainly that this is a plain text replacement without semantic \
         analysis."
    )
}

/// Retitles a tool call's card to say that it found no language server.
///
/// The explanation itself lands in the tool's output, which the card keeps
/// collapsed, so without this the user sees a search that merely looks like it
/// found nothing — and this is a state only the user can fix (install the
/// language server, or allow its download).
pub fn mark_card_missing_language_server(event_stream: &ToolCallEventStream, title: &str) {
    event_stream.update_fields(|update| update.title(i18n::t!("{title} (no language server)")));
}

/// Whether any language server that could answer a request for `buffer` is
/// running. Callers use this only to explain an empty result, never to skip
/// the request: a server that exists but has not finished attaching to a
/// freshly opened buffer would otherwise be reported as missing.
pub fn has_language_server(
    project: &Entity<Project>,
    buffer: &Entity<Buffer>,
    cx: &AsyncApp,
) -> bool {
    project.read_with(cx, |project, cx| {
        project.has_language_server_for_buffer(buffer, cx)
    })
}

pub type CodeActionStore = Entity<Option<PendingCodeActions>>;

pub struct ResolvedSymbol {
    pub buffer: Entity<Buffer>,
    pub position: Anchor,
    pub line_text: String,
    pub truncated: bool,
}

pub const MAX_LINE_DISPLAY_LEN: usize = 200;

pub struct LocationDisplay {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    pub snippet: String,
    pub truncated: bool,
    /// Set when the file is one `read_file` refuses; `snippet` is then empty.
    pub withheld: Option<&'static str>,
}

impl LocationDisplay {
    pub fn from_location(location: &Location, project: &Entity<Project>, cx: &App) -> Self {
        let withheld = ide_query::agent_withheld_reason_for_buffer(
            project.read(cx),
            location.buffer.read(cx),
            cx,
        );
        let snapshot = location.buffer.read(cx).snapshot();
        let range =
            location.range.start.to_point(&snapshot)..location.range.end.to_point(&snapshot);
        let path = location
            .buffer
            .read(cx)
            .file()
            .map(|f| f.full_path(cx).display().to_string())
            .unwrap_or_else(|| "<untitled>".to_string());

        let start_line = range.start.row + 1;
        let end_line = range.end.row + 1;

        // The language server picks the file, so the line is held back for
        // the files the agent could not have read on its own.
        let line_len = if withheld.is_some() {
            0
        } else {
            snapshot.line_len(range.start.row)
        };
        let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
        let snippet: String = snapshot
            .text_for_range(Point::new(range.start.row, 0)..Point::new(range.start.row, line_len))
            .flat_map(|chunk| chunk.chars())
            .skip_while(|c| c.is_whitespace())
            .take(MAX_LINE_DISPLAY_LEN)
            .collect::<String>();
        let snippet = snippet.trim_end().to_string();

        Self {
            path,
            start_line,
            end_line,
            snippet,
            truncated,
            withheld,
        }
    }
}

impl fmt::Display for LocationDisplay {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let truncated_label = if self.truncated { " (truncated)" } else { "" };
        if self.start_line == self.end_line {
            writeln!(f, "{}#L{}{truncated_label}", self.path, self.start_line)?;
        } else {
            writeln!(
                f,
                "{}#L{}-{}{truncated_label}",
                self.path, self.start_line, self.end_line
            )?;
        }
        if let Some(reason) = self.withheld {
            return write!(f, "(source not shown: the file {reason})");
        }
        writeln!(f, "```")?;
        writeln!(f, "{}", self.snippet)?;
        write!(f, "```")
    }
}

/// Searches for `needle` in a char iterator, returning the byte offset of the
/// first occurrence without collecting the full iterator into a string.
///
/// Equivalent to [`str::find`]
fn find_in_char_iter(chars: impl Iterator<Item = char>, needle: &str) -> Option<usize> {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() {
        return Some(0);
    }

    let mut window: VecDeque<char> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offsets: VecDeque<usize> = VecDeque::with_capacity(needle_chars.len());
    let mut byte_offset = 0usize;

    for ch in chars {
        window.push_back(ch);
        byte_offsets.push_back(byte_offset);
        byte_offset += ch.len_utf8();

        if window.len() > needle_chars.len() {
            window.pop_front();
            byte_offsets.pop_front();
        }

        if window.len() == needle_chars.len()
            && window.iter().zip(needle_chars.iter()).all(|(a, b)| a == b)
        {
            return byte_offsets.front().copied();
        }
    }

    None
}

impl SymbolLocator {
    /// Resolves this locator into a concrete buffer and position.
    ///
    /// Opens the file at `file_path`, then searches for `symbol_name` on the
    /// specified `line`. Returns an error if the file can't be found, the line
    /// is out of range, or the symbol name doesn't appear on that line.
    /// If the symbol name appears multiple times on the line, uses the first
    /// occurrence.
    pub async fn resolve(
        &self,
        project: &Entity<Project>,
        cx: &mut AsyncApp,
    ) -> Result<ResolvedSymbol, String> {
        let Self {
            file_path,
            line,
            symbol_name,
        } = self;

        let open_buffer_task = project.update(cx, |project, cx| {
            let Some(project_path) = project.find_project_path(file_path, cx) else {
                return Err(format!("Could not find path '{file_path}' in project",));
            };
            // Resolving reads the line, and both the result and the "symbol
            // not found" error quote it.
            if let Some(reason) = ide_query::agent_withheld_reason(project, &project_path, cx) {
                return Err(format!("Cannot use '{file_path}' because it {reason}"));
            }
            Ok(project.open_buffer(project_path, cx))
        })?;

        let buffer = open_buffer_task
            .await
            .map_err(|e| format!("Failed to open '{}': {e}", self.file_path))?;

        // Directories that are ignored or behind a link are not scanned until
        // something in them is opened, so only now is it known where the
        // file really is.
        if let Some(reason) = project.read_with(cx, |project, cx| {
            ide_query::agent_withheld_reason_for_buffer(project, buffer.read(cx), cx)
        }) {
            return Err(format!("Cannot use '{file_path}' because it {reason}"));
        }

        let (position, line_text, truncated) = buffer.read_with(cx, |buffer, _cx| {
            let snapshot = buffer.snapshot();
            let row = line.saturating_sub(1);

            if row > snapshot.max_point().row {
                let line_count = snapshot.max_point().row + 1;
                return Err(format!(
                    "Line {line} is beyond the end of '{file_path}' (file has {line_count} lines)",
                ));
            }

            let line_len = snapshot.line_len(row);
            let truncated = line_len as usize > MAX_LINE_DISPLAY_LEN;
            let line_start = Point::new(row, 0);
            let line_end = Point::new(row, line_len);
            let line_chars = || {
                snapshot
                    .text_for_range(line_start..line_end)
                    .flat_map(|chunk| chunk.chars())
            };

            let byte_offset = find_in_char_iter(line_chars(), symbol_name).ok_or_else(|| {
                let preview: String = line_chars()
                    .skip_while(|c| c.is_whitespace())
                    .take(MAX_LINE_DISPLAY_LEN)
                    .collect();
                format!(
                    "Symbol '{symbol_name}' not found on line {line} of '{file_path}'. \
                     Line content: '{}'",
                    preview.trim_end()
                )
            })?;

            let position = snapshot.anchor_before(Point::new(row, byte_offset as u32));
            let display_text: String = line_chars()
                .skip_while(|c| c.is_whitespace())
                .take(MAX_LINE_DISPLAY_LEN)
                .collect::<String>();
            let display_text = display_text.trim_end().to_string();

            Ok((position, display_text, truncated))
        })?;

        Ok(ResolvedSymbol {
            buffer,
            position,
            line_text,
            truncated,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::proptest::prelude::*;

    #[gpui::property_test]
    fn find_in_char_iter_test(
        // limited character sets to increase odds of finding matches
        #[strategy = "[abcd]{100,1000}"] haystack: String,
        #[strategy = "[abcd]{1,5}"] needle: String,
    ) -> Result<(), TestCaseError> {
        let expected = haystack.find(&needle);
        let actual = find_in_char_iter(haystack.chars(), &needle);
        prop_assert_eq!(actual, expected);
        Ok::<_, TestCaseError>(())
    }

    use crate::tools::{
        DiagnosticsTool, DiagnosticsToolInput, FindReferencesTool, FindReferencesToolInput,
        GetCodeActionsTool, GetCodeActionsToolInput, GoToDefinitionTool, GoToDefinitionToolInput,
        RenameTool, RenameToolInput,
    };
    use crate::{AgentTool as _, ToolInput};
    use futures::StreamExt as _;
    use gpui::{AppContext as _, TestAppContext, UpdateGlobal as _};
    use language::{FakeLspAdapter, rust_lang};
    use project::{FakeFs, lsp_store::OpenLspBufferHandle};
    use serde_json::json;
    use settings::{SettingsStore, SplicingVec};
    use std::sync::Arc;
    use util::path;

    const VISIBLE: &str = "VISIBLE_MARK";
    /// One mark per reason a file's contents stay away from the model. Each
    /// appears only in the file it is named after, so finding one in a tool's
    /// output means that file's text got through.
    const WITHHELD: [&str; 4] = [
        "PRIVATE_MARK",
        "EXCLUDED_MARK",
        "DEPENDENCY_MARK",
        "LINKED_MARK",
    ];

    async fn project_with_withheld_files(cx: &mut TestAppContext) -> Entity<Project> {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
            SettingsStore::update_global(cx, |store, cx| {
                store.update_user_settings(cx, |settings| {
                    settings.project.worktree.file_scan_exclusions =
                        Some(SplicingVec::from(vec!["**/excluded".to_string()]));
                    settings.project.worktree.private_files =
                        Some(vec!["**/secret.rs".to_string()].into());
                });
            });
        });
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/root"),
            json!({
                "src": {
                    "lib.rs": "fn main() {\n    helper();\n}\n",
                    "other.rs": "fn helper() {} // VISIBLE_MARK\n",
                },
                "secret.rs": "fn helper() {} // PRIVATE_MARK\n",
                ".gitignore": "vendor\n",
                "vendor": {},
                "excluded": {"generated.rs": "fn helper() {} // EXCLUDED_MARK\n"},
            }),
        )
        .await;
        fs.insert_tree(
            path!("/deps"),
            json!({"dep.rs": "fn helper() {} // DEPENDENCY_MARK\n"}),
        )
        .await;
        fs.insert_tree(
            path!("/outside"),
            json!({"linked.rs": "fn helper() {} // LINKED_MARK\n"}),
        )
        .await;
        fs.insert_symlink(path!("/root/link"), path!("/outside").into())
            .await;
        fs.insert_symlink(path!("/root/vendor/keys"), path!("/outside").into())
            .await;
        let project = Project::test(fs, [path!("/root").as_ref()], cx).await;
        project.read_with(cx, |project, _| project.languages().add(rust_lang()));
        cx.executor().run_until_parked();
        project
    }

    async fn start_language_server(
        project: &Entity<Project>,
        cx: &mut TestAppContext,
    ) -> (lsp::FakeLanguageServer, OpenLspBufferHandle) {
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    definition_provider: Some(lsp::OneOf::Left(true)),
                    references_provider: Some(lsp::OneOf::Left(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        let (_buffer, registration) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/root/src/lib.rs"), cx)
            })
            .await
            .expect("the file opens");
        let server = fake_servers.next().await.expect("the server starts");
        cx.executor().run_until_parked();
        (server, registration)
    }

    fn every_location() -> Vec<lsp::Location> {
        [
            path!("/root/src/other.rs"),
            path!("/root/secret.rs"),
            path!("/root/excluded/generated.rs"),
            path!("/deps/dep.rs"),
            path!("/root/link/linked.rs"),
        ]
        .into_iter()
        .map(|abs_path| {
            lsp::Location::new(
                lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
                lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 9)),
            )
        })
        .collect()
    }

    fn locator(file_path: &str, symbol_name: &str) -> SymbolLocator {
        SymbolLocator {
            file_path: file_path.into(),
            line: 1,
            symbol_name: symbol_name.into(),
        }
    }

    async fn resolve(
        project: &Entity<Project>,
        file_path: &str,
        symbol_name: &str,
        cx: &mut TestAppContext,
    ) -> Result<ResolvedSymbol, String> {
        let project = project.clone();
        let locator = locator(file_path, symbol_name);
        cx.spawn(|mut cx| async move { locator.resolve(&project, &mut cx).await })
            .await
    }

    /// The language server decides where a definition or a reference is, so
    /// it can point at any file it can read. The place is reported for every
    /// answer; the source line only for files `read_file` would also hand out.
    #[gpui::test]
    async fn test_locations_the_language_server_returns_withhold_what_read_file_refuses(
        cx: &mut TestAppContext,
    ) {
        let project = project_with_withheld_files(cx).await;
        let (server, _registration) = start_language_server(&project, cx).await;
        server.set_request_handler::<lsp::request::References, _, _>(|_, _| async move {
            Ok(Some(every_location()))
        });
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async move {
            Ok(Some(lsp::GotoDefinitionResponse::Array(every_location())))
        });
        let symbol = SymbolLocator {
            file_path: "root/src/lib.rs".into(),
            line: 2,
            symbol_name: "helper".into(),
        };

        #[allow(clippy::arc_with_non_send_sync)]
        let find_references = Arc::new(FindReferencesTool::new(project.clone()));
        let (event_stream, _events) = ToolCallEventStream::test();
        let references = cx
            .update(|cx| {
                find_references.run(
                    ToolInput::resolved(FindReferencesToolInput {
                        symbol: symbol.clone(),
                    }),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the references are found");

        #[allow(clippy::arc_with_non_send_sync)]
        let go_to_definition = Arc::new(GoToDefinitionTool::new(project.clone()));
        let (event_stream, _events) = ToolCallEventStream::test();
        let definitions = cx
            .update(|cx| {
                go_to_definition.run(
                    ToolInput::resolved(GoToDefinitionToolInput { symbol }),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the definitions are found");

        for output in [&references, &definitions] {
            assert!(
                output.contains(VISIBLE),
                "a file of the project keeps its snippet: {output}"
            );
            for file_name in [
                "other.rs",
                "secret.rs",
                "generated.rs",
                "dep.rs",
                "linked.rs",
            ] {
                assert!(
                    output.contains(file_name),
                    "every place is reported, {file_name} is missing: {output}"
                );
            }
            for mark in WITHHELD {
                assert!(!output.contains(mark), "{mark} reached the model: {output}");
            }
        }
    }

    /// A reference the bound on what one response opens left out is not
    /// silently missing: the model is told how many there were.
    #[gpui::test]
    async fn test_references_left_out_by_the_response_bound_are_reported(cx: &mut TestAppContext) {
        let project = project_with_withheld_files(cx).await;
        let line = "fn helper() {}\n";
        let huge = line.repeat(
            project::lsp_store::result_buffers::MAX_BYTES_OPENED_PER_RESPONSE as usize / line.len()
                + 1,
        );
        let fs = project.read_with(cx, |project, _| project.fs().clone());
        fs.save(
            path!("/root/src/huge.rs").as_ref(),
            &huge.as_str().into(),
            Default::default(),
        )
        .await
        .expect("the large file is written");
        cx.executor().run_until_parked();
        let (server, _registration) = start_language_server(&project, cx).await;
        server.set_request_handler::<lsp::request::References, _, _>(|_, _| async move {
            Ok(Some(
                [path!("/root/src/other.rs"), path!("/root/src/huge.rs")]
                    .into_iter()
                    .map(|abs_path| {
                        lsp::Location::new(
                            lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
                            lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 9)),
                        )
                    })
                    .collect(),
            ))
        });

        #[allow(clippy::arc_with_non_send_sync)]
        let find_references = Arc::new(FindReferencesTool::new(project.clone()));
        let (event_stream, _events) = ToolCallEventStream::test();
        let references = cx
            .update(|cx| {
                find_references.run(
                    ToolInput::resolved(FindReferencesToolInput {
                        symbol: SymbolLocator {
                            file_path: "root/src/lib.rs".into(),
                            line: 2,
                            symbol_name: "helper".into(),
                        },
                    }),
                    event_stream,
                    cx,
                )
            })
            .await
            .expect("the references are found");
        assert!(references.contains("other.rs"), "{references}");
        assert!(!references.contains("huge.rs"), "{references}");
        assert!(
            references.contains("1 more reference was left out"),
            "{references}"
        );
    }

    /// The model names the file to start from, so it can name one it may not
    /// read. Resolving it would hand out the line: as the snippet of the
    /// answer, or inside the "symbol not found on this line" error.
    #[gpui::test]
    async fn test_a_symbol_is_not_resolved_in_a_file_read_file_refuses(cx: &mut TestAppContext) {
        let project = project_with_withheld_files(cx).await;

        let resolved = resolve(&project, "root/src/other.rs", "helper", cx).await;
        match resolved {
            Ok(resolved) => assert!(resolved.line_text.contains(VISIBLE)),
            Err(error) => panic!("a file of the project resolves: {error}"),
        }
        let error = resolve(&project, "root/src/other.rs", "absent", cx)
            .await
            .err()
            .expect("the symbol is not on the line");
        assert!(
            error.contains(VISIBLE),
            "a file of the project shows the line that was searched: {error}"
        );

        for (file_path, reason) in [
            ("root/secret.rs", "`private_files`"),
            ("root/excluded/generated.rs", "`file_scan_exclusions`"),
            ("root/link/linked.rs", "lies outside the project"),
            // The link sits in an ignored directory, which is not scanned, so
            // nothing says where it leads until the file is opened.
            ("root/vendor/keys/linked.rs", "lies outside the project"),
        ] {
            for symbol_name in ["helper", "absent"] {
                let result = resolve(&project, file_path, symbol_name, cx).await;
                let error = match result {
                    Ok(resolved) => panic!(
                        "{file_path} resolved `{symbol_name}` and returned its line: {}",
                        resolved.line_text
                    ),
                    Err(error) => error,
                };
                assert!(error.contains(reason), "{file_path}: {error}");
                for mark in WITHHELD {
                    assert!(!error.contains(mark), "{mark} reached the model: {error}");
                }
            }
        }

        let secret = locator("root/secret.rs", "helper");
        #[allow(clippy::arc_with_non_send_sync)]
        let find_references = Arc::new(FindReferencesTool::new(project.clone()));
        #[allow(clippy::arc_with_non_send_sync)]
        let go_to_definition = Arc::new(GoToDefinitionTool::new(project.clone()));
        #[allow(clippy::arc_with_non_send_sync)]
        let get_code_actions = Arc::new(GetCodeActionsTool::new(project.clone(), cx.new(|_| None)));
        #[allow(clippy::arc_with_non_send_sync)]
        let rename = Arc::new(RenameTool::new(project.clone()));
        #[allow(clippy::arc_with_non_send_sync)]
        let diagnostics = Arc::new(DiagnosticsTool::new(project.clone()));
        let results = [
            cx.update(|cx| {
                diagnostics.run(
                    ToolInput::resolved(DiagnosticsToolInput {
                        path: Some("root/secret.rs".into()),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await,
            cx.update(|cx| {
                find_references.run(
                    ToolInput::resolved(FindReferencesToolInput {
                        symbol: secret.clone(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await,
            cx.update(|cx| {
                go_to_definition.run(
                    ToolInput::resolved(GoToDefinitionToolInput {
                        symbol: secret.clone(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await,
            cx.update(|cx| {
                get_code_actions.run(
                    ToolInput::resolved(GetCodeActionsToolInput {
                        symbol: secret.clone(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await,
            cx.update(|cx| {
                rename.run(
                    ToolInput::resolved(RenameToolInput {
                        symbol: secret,
                        new_name: "renamed".into(),
                    }),
                    ToolCallEventStream::test().0,
                    cx,
                )
            })
            .await,
        ];
        for result in results {
            let error = result.expect_err("a private file is refused before anything runs");
            assert!(error.contains("`private_files`"), "{error}");
        }
    }
}
