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
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Jumps to the definition of a symbol using the language server.
///
/// Returns the file path and line number of the symbol's definition, along with a snippet of the source code at that location.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number of a usage you want to navigate from.
///
/// This tool needs a running language server for the file's language, so it cannot work in a language that has none. When it reports that, relay that fact to the user instead of guessing at editor configuration, and use `grep` for the search.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct GoToDefinitionToolInput {
    /// The symbol to find the definition of.
    pub symbol: SymbolLocator,
}

pub struct GoToDefinitionTool {
    project: Entity<Project>,
}

impl GoToDefinitionTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for GoToDefinitionTool {
    type Input = GoToDefinitionToolInput;
    type Output = String;

    const NAME: &'static str = "go_to_definition";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            go_to_definition_title(&input.symbol.symbol_name).into()
        } else {
            i18n::t!("Go to definition").into()
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

            let definitions_task = project.update(cx, |project, cx| {
                project.definitions(&resolved.buffer, resolved.position, cx)
            });

            let definitions = definitions_task
                .await
                .map_err(|e| format!("Go to definition failed: {e}"))?
                .unwrap_or_default();

            if definitions.is_empty() {
                if !has_language_server(&project, &resolved.buffer, cx) {
                    mark_card_missing_language_server(
                        &event_stream,
                        &go_to_definition_title(&input.symbol.symbol_name),
                    );
                    return Ok(no_language_server_message(
                        "find the definition",
                        &input.symbol.file_path,
                    ));
                }
                return Ok(format!(
                    "No definition found for '{}'.",
                    input.symbol.symbol_name
                ));
            }

            let mut output = String::new();

            if definitions.len() == 1 {
                write!(output, "Definition of `{}`:\n", input.symbol.symbol_name).ok();
            } else {
                write!(
                    output,
                    "Found {} definitions of `{}`:\n",
                    definitions.len(),
                    input.symbol.symbol_name
                )
                .ok();
            }

            for link in &definitions {
                let display = link.target.buffer.read_with(cx, |_, cx| {
                    LocationDisplay::from_location(&link.target, &project, cx)
                });
                write!(output, "\n## {display}\n").ok();
            }

            Ok(output)
        })
    }
}

fn go_to_definition_title(symbol_name: &str) -> String {
    i18n::t!("Go to definition of `{symbol_name}`")
}
