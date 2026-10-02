//! `getTypeLayout`: C / C++ の型 (struct・union・enum・typedef) とマクロの中身を、自前の索引
//! (`ide_query::type_layout_with`) から返す。結果は名前の一致で、言語サーバーの解決ではない。
//!
//! 中身を見せてよいかは、ほかの読み取り結果と同じ [`AssistContext::allows_contents`] で、
//! 結果の**各定義**について決める (サンドボックスではセッションのルートの外・リンクの先・
//! 接続先が断るファイルも伏せる)。伏せる定義は名前も場所も出さず、件数だけを返す。木の途中に
//! 伏せる定義があれば、`ide_query` がその枝だけを `(withheld)` にして組み直す。

use anyhow::Result;
use futures::{FutureExt as _, future::LocalBoxFuture};
use gpui::AsyncApp;
use ide_query::{
    MAX_TYPE_LAYOUT_CHARS, MAX_TYPE_LAYOUT_DEPTH, TypeIndexUnavailable, TypeLayoutKind,
    TypeLayoutLocation, TypeLayoutPaths, TypeLayoutReport, TypeLayoutRequest, TypeLayoutSubject,
};
use project::{ProjectPath, symbol_index_store::Confidence};
use serde_json::{Value, json};

use super::{
    AssistContext, MAX_QUERY_TEXT_CHARS, PATH_DESCRIPTION, line_field, line_schema,
    optional_string, optional_text, query_failure, text_schema,
};
use crate::tools::{client_error, object_schema};

pub(super) const DESCRIPTION: &str = branding::concat_const!(
    "Get what a C or C++ struct, union, enum, typedef or macro \
     contains: its members with their types, bit-fields, #if conditions and comments, the named \
     types inside it expanded a few levels, and where each is defined. Ask by name, or by \
     position (path + line + symbol) for the type of a variable or a member such as `ops` in \
     `dev->ops`. Use it before searching for a header and reading it. It is a name match from ",
    crate::PRODUCT_NAME,
    "'s symbol index (a same-name type elsewhere may be the one used), not a compiler's view."
);

pub(super) fn type_layout_schema() -> Value {
    object_schema(
        json!({
            "name": text_schema(
                MAX_QUERY_TEXT_CHARS,
                "The type, typedef or macro, e.g. \"struct file_operations\", \"foo_t\" or \"MAX_LEN\". Give this, or path and line.",
            ),
            "nearPath": {
                "type": "string",
                "description": "With name: prefer the same-name definition closest to this file.",
            },
            "path": { "type": "string", "description": PATH_DESCRIPTION },
            "line": line_schema("Instead of name: the 1-based line where symbol appears."),
            "symbol": text_schema(
                MAX_QUERY_TEXT_CHARS,
                "The variable, parameter, member, type or macro name as written on that line.",
            ),
            "column": line_schema("1-based column in characters; wins over symbol."),
            "depth": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TYPE_LAYOUT_DEPTH,
                "description": "Levels of named types inside the type to expand (default 2).",
            },
            "maxChars": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_TYPE_LAYOUT_CHARS,
                "description": "Most characters of layout text (default 8000).",
            },
        }),
        &[],
    )
}

/// `ide_query` に渡す、定義ごとの「見せてよいか」。
struct AssistTypeLayoutPaths<'a, 'b> {
    context: &'a mut AssistContext<'b>,
}

impl TypeLayoutPaths for AssistTypeLayoutPaths<'_, '_> {
    fn allows<'c>(
        &'c mut self,
        path: &'c ProjectPath,
        cx: &'c mut AsyncApp,
    ) -> LocalBoxFuture<'c, bool> {
        async move {
            let in_project = self.context.project.read_with(cx, |project, cx| {
                ide_query::agent_withheld_reason(project, path, cx).is_none()
            });
            in_project && self.context.allows_contents(path, cx).await
        }
        .boxed_local()
    }
}

pub(super) async fn get_type_layout(
    context: &mut AssistContext<'_>,
    arguments: &Value,
    cx: &mut AsyncApp,
) -> Result<Value> {
    let name = optional_text(arguments, "name", MAX_QUERY_TEXT_CHARS)?
        .filter(|name| !name.trim().is_empty());
    let has_position = !matches!(arguments.get("path"), None | Some(Value::Null))
        || !matches!(arguments.get("line"), None | Some(Value::Null));
    let subject = match name {
        Some(name) => {
            let near = match optional_string(arguments, "nearPath")? {
                Some(near_path) if !near_path.trim().is_empty() => {
                    Some(context.resolve(near_path, cx).await?)
                }
                _ => None,
            };
            TypeLayoutSubject::Name { name, near }
        }
        None if has_position => {
            let (path, position) = context.position(arguments, cx).await?;
            TypeLayoutSubject::Position { path, position }
        }
        None => {
            return Err(client_error(
                "Give name (e.g. \"struct foo\"), or path and line with the symbol on that line.",
            ));
        }
    };
    let depth = match arguments.get("depth") {
        None | Some(Value::Null) => 0,
        Some(_) => (line_field(arguments, "depth")? as usize).min(MAX_TYPE_LAYOUT_DEPTH),
    };
    let max_chars = match arguments.get("maxChars") {
        None | Some(Value::Null) => 0,
        Some(_) => (line_field(arguments, "maxChars")? as usize).min(MAX_TYPE_LAYOUT_CHARS),
    };
    let request = TypeLayoutRequest {
        subject,
        depth,
        max_chars,
    };
    let project = context.project.clone();
    let sandboxed = context.is_sandboxed();
    let report = {
        let mut paths = AssistTypeLayoutPaths {
            context: &mut *context,
        };
        ide_query::type_layout_with(&project, request, &mut paths, cx).await
    };
    let report = report.map_err(|error| type_layout_failure(sandboxed, error))?;
    Ok(cx.update(|cx| report_json(context, &report, cx)))
}

/// 索引が使えないときは、モデルが一般論に逃げないよう次の一手を添える。
pub(super) fn type_layout_failure(sandboxed: bool, error: anyhow::Error) -> anyhow::Error {
    if let Some(unavailable) = error.downcast_ref::<TypeIndexUnavailable>() {
        return client_error(format!(
            "{unavailable}. Instead, find the definition with searchSymbols, or grep for \
             `struct NAME {{`, `typedef` lines or `#define NAME`, and read it."
        ));
    }
    query_failure(sandboxed, "getTypeLayout", error)
}

fn location_json(
    context: &AssistContext<'_>,
    location: &TypeLayoutLocation,
    cx: &gpui::App,
) -> Value {
    let mut object = serde_json::Map::new();
    context.path_fields(&location.path, &mut object, cx);
    object.insert("line".to_string(), json!(location.line));
    Value::Object(object)
}

fn confidence_name(confidence: Confidence) -> &'static str {
    match confidence {
        Confidence::Declaration => "declaration",
        Confidence::Local => "local",
        Confidence::Parameter => "parameter",
        Confidence::FileScope => "fileScope",
        Confidence::NameOnly => "nameOnly",
    }
}

fn report_json(context: &AssistContext<'_>, report: &TypeLayoutReport, cx: &gpui::App) -> Value {
    let kind = match report.kind {
        TypeLayoutKind::Type => "type",
        TypeLayoutKind::Macro => "macro",
        TypeLayoutKind::NotFound => "notFound",
    };
    let inferred = report.inferred.as_ref().map(|inferred| {
        json!({
            "name": inferred.name,
            "typeText": inferred.type_text,
            "confidence": inferred.confidence.map(confidence_name),
            "memberOf": inferred.member_of,
        })
    });
    json!({
        "kind": kind,
        "name": report.requested,
        "title": if report.title.is_empty() { Value::Null } else { json!(report.title) },
        "definition": report
            .definition
            .as_ref()
            .map(|definition| location_json(context, definition, cx)),
        "text": report.text,
        "definitions": report
            .definitions
            .iter()
            .map(|location| location_json(context, location, cx))
            .collect::<Vec<_>>(),
        "candidates": report.candidates,
        "otherCandidates": report
            .other_candidates
            .iter()
            .map(|location| location_json(context, location, cx))
            .collect::<Vec<_>>(),
        "moreCandidates": report.candidates_truncated
            || report.candidates > report.other_candidates.len() + 1,
        "withheldDefinitions": report.withheld,
        "withheldNote": report.withheld_note,
        "inferred": inferred,
        "pruned": report.pruned,
        "truncated": report.truncated,
        "indexStatus": if report.index_note.is_some() { "indexing" } else { "ready" },
        "indexNote": report.index_note,
        "source": "nameIndex",
        "note": format!("Found by name in {}'s symbol index, not resolved by a language server. Paths in text are relative to the definition's workspace folder; members under every #if branch are listed; macros are not expanded; (withheld) marks a type in a file you can't read.", crate::PRODUCT_NAME),
    })
}
