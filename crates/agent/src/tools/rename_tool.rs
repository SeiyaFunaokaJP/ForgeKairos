use std::fmt::Write;
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use collections::HashSet;
use gpui::{App, Entity, SharedString, Task};
use project::Project;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::symbol_locator::{
    SymbolLocator, has_language_server, mark_card_missing_language_server,
    no_language_server_message,
};
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Renames a symbol across the project using the language server.
///
/// This performs a semantic rename, updating all references to the symbol across all files in the project. The language server determines which occurrences to rename based on the symbol's type and scope.
///
/// Before using this tool, use read_file or grep to find the exact symbol name and line number.
///
/// This tool needs a running language server for the file's language, so it cannot work in a language that has none. When it reports that, relay that fact to the user instead of guessing at editor configuration, and use `edit_file` only if the user asks for a plain textual replacement.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct RenameToolInput {
    /// The symbol to rename.
    pub symbol: SymbolLocator,

    /// The new name for the symbol.
    pub new_name: String,
}

pub struct RenameTool {
    project: Entity<Project>,
}

impl RenameTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for RenameTool {
    type Input = RenameToolInput;
    type Output = String;

    const NAME: &'static str = "rename_symbol";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Other
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        if let Ok(input) = input {
            rename_title(&input.symbol.symbol_name, &input.new_name).into()
        } else {
            "シンボルの名前変更".into()
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

            // A semantic rename edits and saves every file that references the
            // symbol without going through the agent's diff review, so the only
            // place the user can stop it is here.
            cx.update(|cx| {
                event_stream.authorize(
                    rename_title(&input.symbol.symbol_name, &input.new_name),
                    crate::ToolPermissionContext::new(
                        Self::NAME,
                        vec![input.symbol.symbol_name.clone()],
                    ),
                    cx,
                )
            })
            .await
            .map_err(|error| {
                format!(
                    "名前の変更は実行されませんでした ({error})。\
                     ユーザーが許可しなかったので、同じ変更を `edit_file` などで\
                     代わりに適用しないでください。"
                )
            })?;

            // ADR-F50: the language server's edit can change files in any
            // worktree, and create, rename or delete them.
            project
                .update(cx, |project, cx| project.note_agent_write_anywhere(cx))
                .await;
            let rename_task = project.update(cx, |project, cx| {
                project.perform_rename(
                    resolved.buffer.clone(),
                    resolved.position,
                    input.new_name.clone(),
                    None,
                    cx,
                )
            });

            let rename_result = rename_task.await;
            // A rename with no language server behind it comes back the same
            // way as one the server declined -- an error, or a transaction
            // that changed nothing -- so check before reporting either.
            let changed_nothing = match &rename_result {
                Ok(transaction) => transaction.0.is_empty(),
                Err(_) => true,
            };
            if changed_nothing && !has_language_server(&project, &resolved.buffer, cx) {
                mark_card_missing_language_server(
                    &event_stream,
                    &rename_title(&input.symbol.symbol_name, &input.new_name),
                );
                return Ok(no_language_server_message(
                    "名前の変更",
                    &input.symbol.file_path,
                ));
            }

            let transaction = rename_result.map_err(|e| format!("Rename failed: {e}"))?;

            if transaction.0.is_empty() {
                return Ok(format!(
                    "No changes were made. The language server could not rename '{}'.",
                    input.symbol.symbol_name
                ));
            }

            let buffers = transaction.0.keys().cloned().collect::<HashSet<_>>();
            project
                .update(cx, |project, cx| project.save_buffers(buffers, cx))
                .await
                .map_err(|e| format!("Rename succeeded, but failed to save renamed files: {e}"))?;

            let mut output = format!(
                "Renamed `{}` to `{}` in {} file(s):\n",
                input.symbol.symbol_name,
                input.new_name,
                transaction.0.len()
            );

            for (buffer, _) in &transaction.0 {
                buffer.read_with(cx, |buffer, cx| {
                    let path = buffer
                        .file()
                        .map(|f| f.full_path(cx).display().to_string())
                        .unwrap_or_else(|| "<untitled>".to_string());
                    writeln!(output, "- {path}").ok();
                });
            }

            Ok(output)
        })
    }
}

fn rename_title(symbol_name: &str, new_name: &str) -> String {
    format!("`{symbol_name}` の名前を `{new_name}` に変更")
}
