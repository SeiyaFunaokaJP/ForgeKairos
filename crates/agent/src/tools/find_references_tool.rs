use std::fmt::Write;
use std::sync::Arc;

use super::symbol_locator::{
    LocationDisplay, SymbolLocator, has_language_server, mark_card_missing_language_server,
    no_language_server_message,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};
use agent_client_protocol::schema::v2 as acp;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use project::lsp_store::result_buffers::{
    MAX_BYTES_OPENED_PER_RESPONSE, MAX_FILES_OPENED_PER_RESPONSE,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Finds all references to a symbol across the project using the language server.
///
/// Returns a list of locations where the symbol is referenced, including file paths, line numbers, and code snippets for each reference.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
///
/// This tool needs a running language server for the file's language, so it cannot work in a language that has none. When it reports that, relay that fact to the user instead of guessing at editor configuration, and use `grep` for the search.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct FindReferencesToolInput {
    /// The symbol to find references of.
    pub symbol: SymbolLocator,
}

pub struct FindReferencesTool {
    project: Entity<Project>,
}

impl FindReferencesTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for FindReferencesTool {
    type Input = FindReferencesToolInput;
    type Output = String;

    const NAME: &'static str = "find_references";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            find_references_title(&input.symbol.symbol_name).into()
        } else {
            i18n::t!("Find references").into()
        }
    }

    fn run(
        self: Arc<Self>,
        input: ToolInput<Self::Input>,
        event_stream: ToolCallEventStream,
        cx: &mut App,
    ) -> Task<Result<String, String>> {
        let project = self.project.clone();
        cx.spawn(async move |cx| {
            let input = input
                .recv()
                .await
                .map_err(|e| format!("Failed to receive tool input: {e}"))?;

            let resolved = input.symbol.resolve(&project, cx).await?;

            let references_task = project.update(cx, |project, cx| {
                project.references_reporting_omissions(&resolved.buffer, resolved.position, cx)
            });

            let (references, omitted) = references_task
                .await
                .map_err(|e| format!("Find references failed: {e}"))?
                .map(|answer| (answer.locations, answer.omitted))
                .unwrap_or_default();

            if references.is_empty() && omitted == 0 {
                if !has_language_server(&project, &resolved.buffer, cx) {
                    mark_card_missing_language_server(
                        &event_stream,
                        &find_references_title(&input.symbol.symbol_name),
                    );
                    return Ok(no_language_server_message(
                        "find references",
                        &input.symbol.file_path,
                    ));
                }
                return Ok(format!(
                    "No references found for '{}'.",
                    input.symbol.symbol_name
                ));
            }

            let mut output = format!(
                "Found {} references to `{}`:\n",
                references.len(),
                input.symbol.symbol_name
            );

            for location in &references {
                let display = location.buffer.read_with(cx, |_, cx| {
                    LocationDisplay::from_location(location, &project, cx)
                });
                write!(output, "\n## {display}\n").ok();
            }
            if omitted > 0 {
                write!(output, "\n{}\n", omitted_references_note(omitted)).ok();
            }

            Ok(output)
        })
    }
}

fn omitted_references_note(omitted: usize) -> String {
    format!(
        "{omitted} more reference{} left out: one language server response opens at most {MAX_FILES_OPENED_PER_RESPONSE} files and {} MiB.",
        if omitted == 1 { " was" } else { "s were" },
        MAX_BYTES_OPENED_PER_RESPONSE / (1024 * 1024)
    )
}

fn find_references_title(symbol_name: &str) -> String {
    i18n::t!("Find references to `{symbol_name}`")
}
