use std::fmt::Write as _;
use std::sync::Arc;

use agent_client_protocol::schema::v2 as acp;
use gpui::{App, Entity, SharedString, Task};
use ide_query::{
    LinePosition, MAX_OTHER_TYPE_CANDIDATES, TypeIndexUnavailable, TypeIndexUnavailableReason,
    TypeLayoutKind, TypeLayoutLocation, TypeLayoutReport, TypeLayoutRequest, TypeLayoutSubject,
    TypeLayoutTimedOut, WITHHELD_MARKER,
};
use project::{Project, ProjectPath, symbol_index_store::Confidence};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use util::markdown::MarkdownInlineCode;

use super::ide_tools::resolve_shown_paths;
use crate::{AgentTool, ToolCallEventStream, ToolInput};

/// Shows what a C or C++ struct, union, enum, typedef or macro holds: its members with their types, bit-fields, `#if` conditions and comments, the named types inside it expanded a few levels deep, and where each one is defined.
///
/// Use it whenever you need to know what a type or macro contains, before looking for its header with `grep` or `find_path` and reading it with `read_file`.
///
/// Ask by name (`name`, e.g. "struct file_operations", "foo_t" or "MAX_LEN"), or by position (`path`, `line` and `symbol`) to get the type of a variable, a parameter or a member such as `ops` in `dev->ops`.
///
/// The answer comes from Zed's symbol index, which matches names instead of compiling the code: a different type with the same name may be the one actually used. It only covers C and C++.
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema)]
pub struct TypeLayoutToolInput {
    /// The type, typedef or macro to describe, e.g. "struct file_operations", "foo_t" or "MAX_LEN". Give either this, or `path` and `line` (with `symbol`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Only with `name`: when several definitions share the name, prefer the one closest to this file (a path starting with a project root folder name, e.g. "my-app/src/driver.c").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub near_path: Option<String>,
    /// Instead of `name`: the file where the variable, member or type name appears, starting with a project root folder name (e.g. "my-app/src/driver.c").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// With `path`: the 1-based line where `symbol` appears.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    /// With `path`: the variable, parameter, member (e.g. `ops` in `dev->ops`), type or macro name as written on that line; its first occurrence is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    /// How many levels of named types inside the type to expand, from 1 to 4. Defaults to 2: the type's members, and the members of the types they use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    /// The most characters of layout to return. Defaults to 8000; at most 16000.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_chars: Option<u32>,
}

pub struct TypeLayoutTool {
    project: Entity<Project>,
}

impl TypeLayoutTool {
    pub fn new(project: Entity<Project>) -> Self {
        Self { project }
    }
}

impl AgentTool for TypeLayoutTool {
    type Input = TypeLayoutToolInput;
    type Output = String;

    const NAME: &'static str = "type_layout";

    fn kind() -> acp::ToolKind {
        acp::ToolKind::Search
    }

    fn initial_title(
        &self,
        input: Result<Self::Input, serde_json::Value>,
        _cx: &mut App,
    ) -> SharedString {
        match input {
            Ok(input) => type_layout_title(&input).into(),
            Err(_) => i18n::t!("Type layout").into(),
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
                .map_err(|error| format!("Failed to receive tool input: {error}"))?;
            let subject = match (non_empty(&input.name), non_empty(&input.path), input.line) {
                (Some(name), _, _) => {
                    let near = match non_empty(&input.near_path) {
                        Some(near_path) => resolve_shown_paths(&project, [near_path], cx)
                            .await?
                            .pop(),
                        None => None,
                    };
                    TypeLayoutSubject::Name {
                        name: name.to_string(),
                        near,
                    }
                }
                (None, Some(path), Some(line)) => {
                    let path = resolve_shown_paths(&project, [path], cx)
                        .await?
                        .pop()
                        .ok_or_else(|| format!("Couldn't resolve `{path}`"))?;
                    TypeLayoutSubject::Position {
                        path,
                        position: LinePosition {
                            line,
                            symbol: input.symbol.clone(),
                            column: None,
                        },
                    }
                }
                _ => {
                    return Err(
                        "Give `name` (e.g. \"struct foo\"), or `path` and `line` with the `symbol` on that line."
                            .to_string(),
                    );
                }
            };
            let request = TypeLayoutRequest {
                subject,
                depth: input.depth.map_or(0, |depth| depth.max(1) as usize),
                max_chars: input.max_chars.map_or(0, |max_chars| max_chars.max(1) as usize),
            };
            let report = cx
                .update(|cx| ide_query::type_layout(&project, request, cx))
                .await;
            match report {
                Ok(report) => {
                    let locations = project.read_with(cx, |project, cx| {
                        report
                            .definitions
                            .iter()
                            .filter_map(|location| {
                                let abs_path = project.absolute_path(&location.path, cx)?;
                                Some(
                                    acp::ToolCallLocation::new(abs_path)
                                        .line(Some(location.line.saturating_sub(1))),
                                )
                            })
                            .collect::<Vec<_>>()
                    });
                    if !locations.is_empty() {
                        event_stream
                            .update_fields(|update| update.locations(locations));
                    }
                    Ok(project.read_with(cx, |project, cx| {
                        format_type_layout(project, &report, cx)
                    }))
                }
                Err(error) => {
                    if let Some(unavailable) = error.downcast_ref::<TypeIndexUnavailable>() {
                        event_stream.update_fields(|update| update.title(
                            i18n::t!("{} (no name index)", type_layout_title(&input)),
                        ));
                        return Ok(index_unavailable_message(unavailable, &input));
                    }
                    if error.downcast_ref::<TypeLayoutTimedOut>().is_some() {
                        return Err(format!(
                            "{error}. Ask again with a smaller `depth`, or read the definition with `read_file`."
                        ));
                    }
                    Err(error.to_string())
                }
            }
        })
    }
}

fn non_empty(text: &Option<String>) -> Option<&str> {
    text.as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
}

fn type_layout_title(input: &TypeLayoutToolInput) -> String {
    let subject = match (non_empty(&input.name), non_empty(&input.symbol)) {
        (Some(name), _) | (None, Some(name)) => MarkdownInlineCode(name).to_string(),
        (None, None) => match (non_empty(&input.path), input.line) {
            (Some(path), Some(line)) => {
                i18n::t!("line {1} of {0}", MarkdownInlineCode(path), line)
            }
            _ => return i18n::t!("Type layout").to_string(),
        },
    };
    i18n::t!("Type layout of {}", subject)
}

/// Without a next step the model tends to describe the type from memory.
fn index_unavailable_message(
    unavailable: &TypeIndexUnavailable,
    input: &TypeLayoutToolInput,
) -> String {
    if unavailable.reason == TypeIndexUnavailableReason::NotCOrCpp {
        return format!(
            "{unavailable}. For other languages use `hover` or `go_to_definition` on the name, \
             or `search_symbols` to find the definition, then read it with `read_file`."
        );
    }
    let name = non_empty(&input.name)
        .or(non_empty(&input.symbol))
        .unwrap_or("NAME");
    let bare_name = name.rsplit(' ').next().unwrap_or(name);
    format!(
        "{unavailable}. Instead, find the definition with `search_symbols` for `{bare_name}`, or \
         with `grep` for `struct {bare_name} {{`, `typedef` lines ending in `{bare_name};` or \
         `#define {bare_name}`, then read it with `read_file`."
    )
}

fn display_path(project: &Project, path: &ProjectPath, cx: &App) -> String {
    let path_style = project.path_style(cx);
    match project.worktree_for_id(path.worktree_id, cx) {
        Some(worktree) => worktree
            .read(cx)
            .root_name()
            .join(&path.path)
            .display(path_style)
            .into_owned(),
        None => path.path.display(path_style).into_owned(),
    }
}

fn format_location(project: &Project, location: &TypeLayoutLocation, cx: &App) -> String {
    format!(
        "{}:{}",
        display_path(project, &location.path, cx),
        location.line
    )
}

fn root_folder(project: &Project, location: &TypeLayoutLocation, cx: &App) -> Option<String> {
    project
        .worktree_for_id(location.path.worktree_id, cx)
        .map(|worktree| worktree.read(cx).root_name_str().to_string())
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Declaration => "its declaration",
        Confidence::Local => "a local variable",
        Confidence::Parameter => "a parameter",
        Confidence::FileScope => "declared at file scope",
        Confidence::NameOnly => "a same-name declaration elsewhere, so it may be a different one",
    }
}

fn withheld_reason(report: &TypeLayoutReport) -> String {
    match report.withheld_note {
        Some(note) => format!("(it could not be checked): {note}"),
        None => "(in files with restricted access).".to_string(),
    }
}

/// The model-facing text. It says what the answer is (a name match), what the markers in the
/// tree mean (only those that occur), and what was left out.
fn format_type_layout(project: &Project, report: &TypeLayoutReport, cx: &App) -> String {
    let mut output = String::new();
    if let Some(inferred) = &report.inferred {
        match (&inferred.member_of, inferred.confidence) {
            (Some(owner), _) => writeln!(
                output,
                "`{}` is a member of `{owner}` declared as `{}`.",
                inferred.name, inferred.type_text
            ),
            (None, Some(confidence)) => writeln!(
                output,
                "`{}` is declared as `{}` ({}).",
                inferred.name,
                inferred.type_text,
                confidence_name(confidence)
            ),
            (None, None) => writeln!(
                output,
                "`{}` is declared as `{}`.",
                inferred.name, inferred.type_text
            ),
        }
        .ok();
    }
    match report.kind {
        TypeLayoutKind::NotFound => {
            if report.withheld > 0 {
                writeln!(
                    output,
                    "{} definition{} of `{}` can't be shown {}",
                    report.withheld,
                    if report.withheld == 1 { "" } else { "s" },
                    report.requested,
                    withheld_reason(report)
                )
                .ok();
            } else if report.inferred.is_some() {
                output.push_str(
                    "That type is not a struct, union, enum or typedef defined in the project's C or C++ files (a builtin type, or one the index doesn't have).\n",
                );
            } else {
                writeln!(
                    output,
                    "No struct, union, enum, typedef or macro named `{0}` was found in {1}'s symbol index. It may be declared through a macro, in a file the index skips, or under another name: try `search_symbols` for `{0}`, or `grep` for `{0}`.",
                    report.requested,
                    branding::CURRENT.product_name
                )
                .ok();
            }
            if let Some(note) = &report.index_note {
                writeln!(output, "{note}").ok();
            }
            return output;
        }
        TypeLayoutKind::Macro => {
            write!(output, "Macro `{}`", report.title).ok();
            if let Some(definition) = &report.definition {
                write!(
                    output,
                    " — defined at {}",
                    format_location(project, definition, cx)
                )
                .ok();
            }
            output.push_str(" (name match; macros are not expanded)");
            if report.candidates > 1 {
                write!(output, "; {} definitions", report.candidates).ok();
                if report.candidates_truncated {
                    write!(output, ", the first {} shown", report.definitions.len()).ok();
                }
            }
            output.push_str(".\n");
        }
        TypeLayoutKind::Type => {
            output.push_str(&report.title);
            if let Some(definition) = &report.definition {
                write!(
                    output,
                    " — defined at {}",
                    format_location(project, definition, cx)
                )
                .ok();
            }
            if report.candidates > 1 {
                write!(
                    output,
                    " (name match; {} definitions with this name, showing the nearest)",
                    report.candidates
                )
                .ok();
            } else {
                output.push_str(" (name match)");
            }
            output.push('\n');
        }
    }
    output.push('\n');
    output.push_str(&report.text);
    if !report.text.ends_with('\n') {
        output.push('\n');
    }
    output.push('\n');

    if let Some(folder) = report
        .definition
        .as_ref()
        .and_then(|definition| root_folder(project, definition, cx))
    {
        writeln!(
            output,
            "Paths above are relative to the `{folder}` project folder."
        )
        .ok();
    }
    if report.kind == TypeLayoutKind::Type {
        let markers = [
            ("(recursive)", "the type contains itself"),
            ("(see above)", "expanded earlier in the tree"),
            ("(not found)", "no definition was indexed"),
            (
                "(unparsed)",
                "a declaration the index couldn't read, usually a macro",
            ),
            (
                WITHHELD_MARKER,
                "defined in a file you can't read, so not expanded",
            ),
            (
                "[#if",
                "the member exists only under that preprocessor condition",
            ),
        ];
        let used = markers
            .iter()
            .filter(|(marker, _)| report.text.contains(marker))
            .map(|(marker, meaning)| format!("{marker}: {meaning}"))
            .collect::<Vec<_>>();
        if !used.is_empty() {
            writeln!(output, "Markers: {}.", used.join("; ")).ok();
        }
        output.push_str(
            "This is a name match, not a compiler's view: members under every `#if` branch are listed and macros are not expanded.\n",
        );
    }
    if !report.other_candidates.is_empty() {
        let listed = report
            .other_candidates
            .iter()
            .map(|location| format_location(project, location, cx))
            .collect::<Vec<_>>()
            .join(", ");
        write!(output, "Other definitions with this name: {listed}").ok();
        let unlisted = report
            .candidates
            .saturating_sub(report.other_candidates.len() + 1);
        if unlisted > 0 || report.candidates_truncated {
            write!(output, " and more").ok();
        }
        output.push_str(
            ". Ask with `near_path` set to the file that uses the type to prefer another one.\n",
        );
    } else if report.candidates_truncated && report.kind == TypeLayoutKind::Type {
        writeln!(
            output,
            "More definitions share this name than listed (at most {MAX_OTHER_TYPE_CANDIDATES})."
        )
        .ok();
    }
    if report.withheld > 0 {
        writeln!(
            output,
            "{} definition{} can't be shown {}",
            report.withheld,
            if report.withheld == 1 { "" } else { "s" },
            withheld_reason(report)
        )
        .ok();
    }
    if report.truncated {
        output.push_str(
            "The layout was cut short. Ask with a smaller `depth`, or ask for a nested type by its own name.\n",
        );
    }
    if let Some(note) = &report.index_note {
        writeln!(output, "{note}").ok();
    }
    output
}

#[cfg(test)]
#[path = "type_layout_tool_tests.rs"]
mod tests;
