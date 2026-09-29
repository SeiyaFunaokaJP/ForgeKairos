use std::str::FromStr;

use lsp::{DiagnosticSeverity, DiagnosticTag};
use project::lsp_command::*;
use rpc::proto::{self};
use serde_json::json;

#[test]
fn test_serialize_lsp_diagnostic() {
    let lsp_diagnostic = lsp::Diagnostic {
        range: lsp::Range {
            start: lsp::Position::new(0, 1),
            end: lsp::Position::new(2, 3),
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: Some(lsp::NumberOrString::String("E001".to_string())),
        source: Some("test-source".to_string()),
        message: lsp::DiagnosticMessage::from("Test error message"),
        related_information: None,
        tags: Some(vec![DiagnosticTag::DEPRECATED]),
        code_description: None,
        data: Some(json!({"detail": "test detail"})),
    };

    let proto_diagnostic = GetDocumentDiagnostics::serialize_lsp_diagnostic(lsp_diagnostic)
        .expect("Failed to serialize diagnostic");

    let start = proto_diagnostic.start.unwrap();
    let end = proto_diagnostic.end.unwrap();
    assert_eq!(start.row, 0);
    assert_eq!(start.column, 1);
    assert_eq!(end.row, 2);
    assert_eq!(end.column, 3);
    assert_eq!(
        proto_diagnostic.severity,
        proto::lsp_diagnostic::Severity::Error as i32
    );
    assert_eq!(proto_diagnostic.code, Some("E001".to_string()));
    assert_eq!(proto_diagnostic.source, Some("test-source".to_string()));
    assert_eq!(proto_diagnostic.message, "Test error message");
}

#[test]
fn test_deserialize_lsp_diagnostic() {
    let proto_diagnostic = proto::LspDiagnostic {
        start: Some(proto::PointUtf16 { row: 0, column: 1 }),
        end: Some(proto::PointUtf16 { row: 2, column: 3 }),
        severity: proto::lsp_diagnostic::Severity::Warning as i32,
        code: Some("ERR".to_string()),
        source: Some("Prism".to_string()),
        message: "assigned but unused variable - a".to_string(),
        related_information: vec![],
        tags: vec![],
        code_description: None,
        data: None,
        markup_message_kind: None,
    };

    let lsp_diagnostic = GetDocumentDiagnostics::deserialize_lsp_diagnostic(proto_diagnostic)
        .expect("Failed to deserialize diagnostic");

    assert_eq!(lsp_diagnostic.range.start.line, 0);
    assert_eq!(lsp_diagnostic.range.start.character, 1);
    assert_eq!(lsp_diagnostic.range.end.line, 2);
    assert_eq!(lsp_diagnostic.range.end.character, 3);
    assert_eq!(lsp_diagnostic.severity, Some(DiagnosticSeverity::WARNING));
    assert_eq!(
        lsp_diagnostic.code,
        Some(lsp::NumberOrString::String("ERR".to_string()))
    );
    assert_eq!(lsp_diagnostic.source, Some("Prism".to_string()));
    assert_eq!(
        lsp_diagnostic.message,
        lsp::DiagnosticMessage::from("assigned but unused variable - a")
    );
}

#[test]
fn test_related_information() {
    let related_info = lsp::DiagnosticRelatedInformation {
        location: lsp::Location {
            uri: lsp::Uri::from_str("file:///test.rs").unwrap(),
            range: lsp::Range {
                start: lsp::Position::new(1, 1),
                end: lsp::Position::new(1, 5),
            },
        },
        message: "Related info message".to_string(),
    };

    let lsp_diagnostic = lsp::Diagnostic {
        range: lsp::Range {
            start: lsp::Position::new(0, 0),
            end: lsp::Position::new(0, 1),
        },
        severity: Some(DiagnosticSeverity::INFORMATION),
        code: None,
        source: Some("Prism".to_string()),
        message: lsp::DiagnosticMessage::from("assigned but unused variable - a"),
        related_information: Some(vec![related_info]),
        tags: None,
        code_description: None,
        data: None,
    };

    let proto_diagnostic = GetDocumentDiagnostics::serialize_lsp_diagnostic(lsp_diagnostic)
        .expect("Failed to serialize diagnostic");

    assert_eq!(proto_diagnostic.related_information.len(), 1);
    let related = &proto_diagnostic.related_information[0];
    assert_eq!(related.location_url, Some("file:///test.rs".to_string()));
    assert_eq!(related.message, "Related info message");
}

#[test]
fn test_serialize_lsp_diagnostic_markup_message() {
    let message = lsp::DiagnosticMessage::MarkupContent(lsp::MarkupContent {
        kind: lsp::MarkupKind::Markdown,
        value: "unused variable `a`\n\nsee [docs](https://example.com)".to_string(),
    });
    let lsp_diagnostic = lsp::Diagnostic {
        range: lsp::Range {
            start: lsp::Position::new(0, 1),
            end: lsp::Position::new(2, 3),
        },
        severity: Some(DiagnosticSeverity::WARNING),
        message: message.clone(),
        ..lsp::Diagnostic::default()
    };

    let proto_diagnostic = GetDocumentDiagnostics::serialize_lsp_diagnostic(lsp_diagnostic)
        .expect("Failed to serialize diagnostic");
    assert_eq!(
        proto_diagnostic.message,
        "unused variable `a`\n\nsee [docs](https://example.com)"
    );
    assert_eq!(
        proto_diagnostic.markup_message_kind,
        Some(proto::MarkupKind::Markdown as i32)
    );

    let lsp_diagnostic = GetDocumentDiagnostics::deserialize_lsp_diagnostic(proto_diagnostic)
        .expect("Failed to deserialize diagnostic");
    assert_eq!(lsp_diagnostic.message, message);
}

#[test]
fn test_invalid_ranges() {
    let proto_diagnostic = proto::LspDiagnostic {
        start: None,
        end: Some(proto::PointUtf16 { row: 2, column: 3 }),
        severity: proto::lsp_diagnostic::Severity::Error as i32,
        code: None,
        source: None,
        message: "Test message".to_string(),
        related_information: vec![],
        tags: vec![],
        code_description: None,
        data: None,
        markup_message_kind: None,
    };

    let result = GetDocumentDiagnostics::deserialize_lsp_diagnostic(proto_diagnostic);
    assert!(result.is_err());
}

mod result_buffers {
    use std::sync::Arc;

    use fs::FakeFs;
    use futures::StreamExt as _;
    use gpui::{Entity, TestAppContext};
    use language::{Buffer, FakeLspAdapter, rust_lang};
    use lsp::FakeLanguageServer;
    use project::{
        Project,
        lsp_store::{
            OpenLspBufferHandle,
            result_buffers::{MAX_BYTES_OPENED_PER_RESPONSE, MAX_FILES_OPENED_PER_RESPONSE},
        },
    };
    use serde_json::json;
    use util::path;

    use crate::init_test;

    struct Opened {
        project: Entity<Project>,
        buffer: Entity<Buffer>,
        server: FakeLanguageServer,
        _registration: OpenLspBufferHandle,
    }

    async fn open_main(fs: Arc<FakeFs>, cx: &mut TestAppContext) -> Opened {
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let language_registry = project.read_with(cx, |project, _| project.languages().clone());
        language_registry.add(rust_lang());
        let mut fake_servers = language_registry.register_fake_lsp(
            "Rust",
            FakeLspAdapter {
                capabilities: lsp::ServerCapabilities {
                    definition_provider: Some(lsp::OneOf::Left(true)),
                    references_provider: Some(lsp::OneOf::Left(true)),
                    call_hierarchy_provider: Some(lsp::CallHierarchyServerCapability::Simple(true)),
                    ..lsp::ServerCapabilities::default()
                },
                ..FakeLspAdapter::default()
            },
        );
        let (buffer, registration) = project
            .update(cx, |project, cx| {
                project.open_local_buffer_with_lsp(path!("/dir/main.rs"), cx)
            })
            .await
            .expect("the file opens");
        let server = fake_servers.next().await.expect("the server starts");
        cx.executor().run_until_parked();
        Opened {
            project,
            buffer,
            server,
            _registration: registration,
        }
    }

    fn location(abs_path: &str) -> lsp::Location {
        lsp::Location::new(
            lsp::Uri::from_file_path(abs_path).expect("an absolute path"),
            lsp::Range::new(lsp::Position::new(0, 3), lsp::Position::new(0, 4)),
        )
    }

    fn call_hierarchy_item(abs_path: &str) -> lsp::CallHierarchyItem {
        let location = location(abs_path);
        lsp::CallHierarchyItem {
            name: "f".to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            tags: None,
            detail: None,
            uri: location.uri,
            range: location.range,
            selection_range: location.range,
            data: None,
        }
    }

    fn file_names(
        buffers: impl IntoIterator<Item = Entity<Buffer>>,
        cx: &TestAppContext,
    ) -> Vec<String> {
        buffers
            .into_iter()
            .map(|buffer| {
                buffer.read_with(cx, |buffer, _| {
                    buffer
                        .file()
                        .and_then(|file| file.path().file_name().map(str::to_string))
                        .expect("a buffer of a file")
                })
            })
            .collect()
    }

    /// The language server names the files, so it can name one that is too large to read for
    /// the sake of one location. Such a location is dropped and the rest of the response is
    /// kept; a file the user already has open costs nothing and is kept whatever its size.
    #[gpui::test]
    async fn test_a_response_does_not_open_more_bytes_than_its_bound(cx: &mut TestAppContext) {
        init_test(cx);
        let line = "fn f() {}\n";
        let huge = line.repeat(MAX_BYTES_OPENED_PER_RESPONSE as usize / line.len() + 1);
        assert!(huge.len() as u64 > MAX_BYTES_OPENED_PER_RESPONSE);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "main.rs": "fn main() { f(); }\n",
                "small.rs": line,
                "huge.txt": huge,
            }),
        )
        .await;
        let Opened {
            project,
            buffer,
            server,
            _registration,
        } = open_main(fs, cx).await;

        server.set_request_handler::<lsp::request::References, _, _>(|_, _| async move {
            Ok(Some(vec![
                location(path!("/dir/huge.txt")),
                location(path!("/dir/small.rs")),
            ]))
        });
        server.set_request_handler::<lsp::request::GotoDefinition, _, _>(|_, _| async move {
            Ok(Some(lsp::GotoDefinitionResponse::Array(vec![
                location(path!("/dir/huge.txt")),
                location(path!("/dir/small.rs")),
            ])))
        });
        server.set_request_handler::<lsp::request::CallHierarchyPrepare, _, _>(|_, _| async move {
            Ok(Some(vec![
                call_hierarchy_item(path!("/dir/huge.txt")),
                call_hierarchy_item(path!("/dir/small.rs")),
            ]))
        });
        server.set_request_handler::<lsp::request::CallHierarchyIncomingCalls, _, _>(
            |_, _| async move {
                Ok(Some(
                    [path!("/dir/huge.txt"), path!("/dir/small.rs")]
                        .into_iter()
                        .map(|abs_path| lsp::CallHierarchyIncomingCall {
                            from: call_hierarchy_item(abs_path),
                            from_ranges: vec![location(abs_path).range],
                        })
                        .collect(),
                ))
            },
        );
        server.set_request_handler::<lsp::request::CallHierarchyOutgoingCalls, _, _>(
            |_, _| async move {
                Ok(Some(
                    [path!("/dir/huge.txt"), path!("/dir/small.rs")]
                        .into_iter()
                        .map(|abs_path| lsp::CallHierarchyOutgoingCall {
                            to: call_hierarchy_item(abs_path),
                            from_ranges: Vec::new(),
                        })
                        .collect(),
                ))
            },
        );

        for expected in [vec!["small.rs"], vec!["huge.txt", "small.rs"]] {
            // The second round holds the large file open the way an editor tab does.
            let _held_open = if expected.len() == 2 {
                Some(
                    project
                        .update(cx, |project, cx| {
                            project.open_local_buffer(path!("/dir/huge.txt"), cx)
                        })
                        .await
                        .expect("the user can open a file of any size"),
                )
            } else {
                None
            };

            let references = project
                .update(cx, |project, cx| project.references(&buffer, 15, cx))
                .await
                .expect("the request succeeds")
                .expect("the server answered");
            assert_eq!(
                file_names(references.into_iter().map(|location| location.buffer), cx),
                expected,
                "references"
            );

            let definitions = project
                .update(cx, |project, cx| project.definitions(&buffer, 15, cx))
                .await
                .expect("the request succeeds")
                .expect("the server answered");
            assert_eq!(
                file_names(definitions.into_iter().map(|link| link.target.buffer), cx),
                expected,
                "definitions"
            );

            let items = project
                .update(cx, |project, cx| {
                    project.prepare_call_hierarchy(&buffer, 15, cx)
                })
                .await
                .expect("the request succeeds")
                .expect("the server answered");
            assert_eq!(
                file_names(items.iter().map(|item| item.buffer.clone()), cx),
                expected,
                "prepare call hierarchy"
            );
            let item = items.last().expect("the small file's item").clone();
            drop(items);

            let incoming = project
                .update(cx, |project, cx| project.incoming_calls(item.clone(), cx))
                .await
                .expect("the request succeeds")
                .expect("the server answered");
            assert_eq!(
                file_names(incoming.iter().map(|call| call.from.buffer.clone()), cx),
                expected,
                "incoming calls"
            );
            drop(incoming);

            let outgoing = project
                .update(cx, |project, cx| project.outgoing_calls(item, cx))
                .await
                .expect("the request succeeds")
                .expect("the server answered");
            assert_eq!(
                file_names(outgoing.iter().map(|call| call.to.buffer.clone()), cx),
                expected,
                "outgoing calls"
            );
            drop(outgoing);
        }
    }

    /// The bound is on what the response opens in all, so that one large file still opens:
    /// going to a definition inside a generated source of many megabytes has to work.
    #[gpui::test]
    async fn test_the_bound_is_on_the_sum_of_a_response(cx: &mut TestAppContext) {
        init_test(cx);
        let line = "0123456789abcdef\n";
        let large = line.repeat(MAX_BYTES_OPENED_PER_RESPONSE as usize * 3 / 5 / line.len());
        assert!(large.len() as u64 > 4 * 1024 * 1024);
        assert!(large.len() as u64 <= MAX_BYTES_OPENED_PER_RESPONSE);
        assert!(2 * large.len() as u64 > MAX_BYTES_OPENED_PER_RESPONSE);
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            path!("/dir"),
            json!({
                "main.rs": "fn main() { f(); }\n",
                "first.txt": large.clone(),
                "second.txt": large,
                "small.rs": "fn f() {}\n",
            }),
        )
        .await;
        let Opened {
            project,
            buffer,
            server,
            _registration,
        } = open_main(fs, cx).await;
        server.set_request_handler::<lsp::request::References, _, _>(|_, _| async move {
            Ok(Some(vec![
                location(path!("/dir/first.txt")),
                location(path!("/dir/second.txt")),
                location(path!("/dir/small.rs")),
            ]))
        });

        let references = project
            .update(cx, |project, cx| project.references(&buffer, 15, cx))
            .await
            .expect("the request succeeds")
            .expect("the server answered");
        assert_eq!(
            file_names(references.into_iter().map(|location| location.buffer), cx),
            ["first.txt", "small.rs"]
        );
    }

    #[gpui::test]
    async fn test_a_response_does_not_open_more_files_than_its_bound(cx: &mut TestAppContext) {
        init_test(cx);
        let file_count = MAX_FILES_OPENED_PER_RESPONSE + 2;
        let mut files = serde_json::Map::new();
        files.insert("main.rs".to_string(), json!("fn main() { f(); }\n"));
        for index in 0..file_count {
            files.insert(format!("f{index:04}.rs"), json!("fn f() {}\n"));
        }
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), serde_json::Value::Object(files))
            .await;
        let Opened {
            project,
            buffer,
            server,
            _registration,
        } = open_main(fs, cx).await;

        server.set_request_handler::<lsp::request::References, _, _>(move |_, _| async move {
            Ok(Some(
                (0..file_count)
                    // Twice each: the bound counts files, not locations.
                    .flat_map(|index| [(index, 0), (index, 3)])
                    .map(|(index, column)| {
                        let mut location = location(&format!("{}/f{index:04}.rs", path!("/dir")));
                        location.range.start.character = column;
                        location
                    })
                    .collect(),
            ))
        });

        // Already open, so it is not one of the files the response opens.
        let last = format!("f{:04}.rs", file_count - 1);
        let _held_open = project
            .update(cx, |project, cx| {
                project.open_local_buffer(format!("{}/{last}", path!("/dir")), cx)
            })
            .await
            .expect("the file opens");

        let references = project
            .update(cx, |project, cx| project.references(&buffer, 15, cx))
            .await
            .expect("the request succeeds")
            .expect("the server answered");
        let names = file_names(references.into_iter().map(|location| location.buffer), cx);

        let mut expected = (0..MAX_FILES_OPENED_PER_RESPONSE)
            .flat_map(|index| [index, index])
            .map(|index| format!("f{index:04}.rs"))
            .collect::<Vec<_>>();
        expected.extend([last.clone(), last]);
        assert_eq!(names.len(), expected.len());
        assert_eq!(names, expected);
    }
}
