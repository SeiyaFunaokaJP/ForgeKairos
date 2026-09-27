//! Parsing of `format!` templates, and rewriting a translated template into the placeholder
//! vocabulary of the English original.

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ArgumentReference {
    /// `{}` — the n-th implicit positional argument.
    Next(usize),
    /// `{0}`
    Index(usize),
    /// `{name}` — a named argument or an implicitly captured variable.
    Name(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placeholder {
    pub raw: String,
    pub argument: ArgumentReference,
    /// Everything after the `:`, empty when there is no format spec.
    pub spec: String,
    /// Byte range of the placeholder inside the template.
    pub start: usize,
    pub end: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    pub placeholders: Vec<Placeholder>,
}

pub fn parse(template: &str) -> Result<Template, String> {
    let mut placeholders = Vec::new();
    let mut next_implicit = 0;
    let bytes = template.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'{' if bytes.get(index + 1) == Some(&b'{') => index += 2,
            b'}' if bytes.get(index + 1) == Some(&b'}') => index += 2,
            b'}' => return Err(format!("unmatched `}}` at byte {index}")),
            b'{' => {
                let close = template[index..]
                    .find('}')
                    .map(|offset| index + offset)
                    .ok_or_else(|| format!("unclosed `{{` at byte {index}"))?;
                let inner = &template[index + 1..close];
                if inner.contains('{') {
                    return Err(format!("nested `{{` at byte {index}"));
                }
                let (argument_text, spec) = match inner.find(':') {
                    Some(colon) => (inner[..colon].trim(), &inner[colon + 1..]),
                    None => (inner.trim(), ""),
                };
                if spec.contains('$') || spec.contains(".*") {
                    return Err(format!(
                        "width or precision taken from an argument in `{{{inner}}}`"
                    ));
                }
                let argument = if argument_text.is_empty() {
                    let argument = ArgumentReference::Next(next_implicit);
                    next_implicit += 1;
                    argument
                } else if let Ok(position) = argument_text.parse::<usize>() {
                    ArgumentReference::Index(position)
                } else if argument_text
                    .chars()
                    .next()
                    .is_some_and(|character| character == '_' || character.is_alphabetic())
                    && argument_text
                        .chars()
                        .all(|character| character == '_' || character.is_alphanumeric())
                {
                    ArgumentReference::Name(argument_text.to_string())
                } else {
                    return Err(format!("unsupported argument `{argument_text}`"));
                };
                placeholders.push(Placeholder {
                    raw: template[index..=close].to_string(),
                    argument,
                    spec: spec.to_string(),
                    start: index,
                    end: close + 1,
                });
                index = close + 1;
            }
            _ => index += 1,
        }
    }
    Ok(Template { placeholders })
}

/// True when the literal contains something that `format!` (and therefore `t!`) would treat as
/// a placeholder or an escape.
pub fn has_format_syntax(value: &str) -> bool {
    value.contains('{') || value.contains('}')
}

/// The arguments that follow a format string, as normalized token text.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FormatArguments {
    pub positional: Vec<String>,
    pub named: Vec<(String, String)>,
}

impl FormatArguments {
    /// The expression a placeholder stands for, or `None` if it cannot be resolved.
    pub fn resolve(&self, argument: &ArgumentReference) -> Option<String> {
        match argument {
            ArgumentReference::Next(position) | ArgumentReference::Index(position) => {
                self.positional.get(*position).cloned()
            }
            ArgumentReference::Name(name) => Some(
                self.named
                    .iter()
                    .find(|(named, _)| named == name)
                    .map(|(_, expression)| expression.clone())
                    .unwrap_or_else(|| name.clone()),
            ),
        }
    }
}

/// Rewrites the Japanese template so that each placeholder refers to the English template's
/// argument, using the English template's vocabulary: `{name}` for named / captured arguments
/// and `{N}` (or `{}` when the order is unchanged) for positional ones. Format specs are dropped
/// because the macro applies them at the call site.
pub fn rewrite_in_english_vocabulary(
    english: &str,
    english_arguments: &FormatArguments,
    japanese: &str,
    japanese_arguments: &FormatArguments,
) -> Result<String, String> {
    let english_template = parse(english).map_err(|error| format!("en: {error}"))?;
    let japanese_template = parse(japanese).map_err(|error| format!("ja: {error}"))?;

    let english_expressions: Vec<String> = english_template
        .placeholders
        .iter()
        .map(|placeholder| {
            english_arguments
                .resolve(&placeholder.argument)
                .ok_or_else(|| format!("en placeholder {} has no argument", placeholder.raw))
        })
        .collect::<Result<_, _>>()?;

    let mut mapped = Vec::new();
    for placeholder in &japanese_template.placeholders {
        let expression = japanese_arguments
            .resolve(&placeholder.argument)
            .ok_or_else(|| format!("ja placeholder {} has no argument", placeholder.raw))?;
        let english_index = english_expressions
            .iter()
            .position(|candidate| *candidate == expression)
            .ok_or_else(|| {
                format!(
                    "ja placeholder {} (`{expression}`) matches no en placeholder",
                    placeholder.raw
                )
            })?;
        let english_placeholder = &english_template.placeholders[english_index];
        if normalized_spec(&english_placeholder.spec) != normalized_spec(&placeholder.spec) {
            return Err(format!(
                "format spec differs: en {} / ja {}",
                english_placeholder.raw, placeholder.raw
            ));
        }
        mapped.push(english_index);
    }

    for (english_index, expression) in english_expressions.iter().enumerate() {
        let used = mapped.iter().any(|index| {
            english_expressions
                .get(*index)
                .is_some_and(|candidate| candidate == expression)
        });
        if !used {
            return Err(format!(
                "ja drops en placeholder {}",
                english_template.placeholders[english_index].raw
            ));
        }
    }

    let all_implicit = english_template
        .placeholders
        .iter()
        .all(|placeholder| matches!(placeholder.argument, ArgumentReference::Next(_)));
    let keeps_implicit_order = all_implicit
        && mapped.len() == english_template.placeholders.len()
        && mapped
            .iter()
            .enumerate()
            .all(|(order, index)| order == *index);

    let mut result = String::new();
    let mut cursor = 0;
    for (placeholder, english_index) in japanese_template.placeholders.iter().zip(&mapped) {
        result.push_str(&japanese[cursor..placeholder.start]);
        let english_placeholder = &english_template.placeholders[*english_index];
        match &english_placeholder.argument {
            ArgumentReference::Name(name) => {
                result.push('{');
                result.push_str(name);
                result.push('}');
            }
            ArgumentReference::Next(position) | ArgumentReference::Index(position) => {
                if keeps_implicit_order {
                    result.push_str("{}");
                } else {
                    result.push_str(&format!("{{{position}}}"));
                }
            }
        }
        cursor = placeholder.end;
    }
    result.push_str(&japanese[cursor..]);
    Ok(result)
}

fn normalized_spec(spec: &str) -> &str {
    spec.trim()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn arguments(positional: &[&str], named: &[(&str, &str)]) -> FormatArguments {
        FormatArguments {
            positional: positional.iter().map(|text| text.to_string()).collect(),
            named: named
                .iter()
                .map(|(name, expression)| (name.to_string(), expression.to_string()))
                .collect(),
        }
    }

    #[test]
    fn parses_placeholders_and_escapes() {
        let template = parse("{{literal}} {} {0} {name:?} {:>5} {x:.1}").unwrap_or_else(|error| {
            panic!("{error}");
        });
        let references: Vec<ArgumentReference> = template
            .placeholders
            .iter()
            .map(|placeholder| placeholder.argument.clone())
            .collect();
        assert_eq!(
            references,
            vec![
                ArgumentReference::Next(0),
                ArgumentReference::Index(0),
                ArgumentReference::Name("name".into()),
                ArgumentReference::Next(1),
                ArgumentReference::Name("x".into()),
            ]
        );
        assert!(parse("{:.*}").is_err());
        assert!(parse("oops }").is_err());
    }

    #[test]
    fn reordered_positional_placeholders_become_indices() {
        let rewritten = rewrite_in_english_vocabulary(
            "Select a {} for {}",
            &arguments(&["term", "path"], &[]),
            "{path} の {term} を選択",
            &arguments(&[], &[]),
        );
        assert_eq!(rewritten, Ok("{1} の {0} を選択".to_string()));
    }

    #[test]
    fn unchanged_order_keeps_implicit_placeholders() {
        let rewritten = rewrite_in_english_vocabulary(
            "Open {} in {}",
            &arguments(&["file", "pane"], &[]),
            "{} を {} で開く",
            &arguments(&["file", "pane"], &[]),
        );
        assert_eq!(rewritten, Ok("{} を {} で開く".to_string()));
    }

    #[test]
    fn named_placeholders_keep_their_names_and_drop_specs() {
        let rewritten = rewrite_in_english_vocabulary(
            "Saved {count} files in {elapsed:.1}s",
            &arguments(&[], &[]),
            "{elapsed:.1} 秒で {} 件保存",
            &arguments(&["count"], &[]),
        );
        assert_eq!(rewritten, Ok("{elapsed} 秒で {count} 件保存".to_string()));
    }

    #[test]
    fn mismatches_are_reported() {
        assert!(
            rewrite_in_english_vocabulary(
                "{} of {}",
                &arguments(&["a", "b"], &[]),
                "{} / {}",
                &arguments(&["a", "c"], &[]),
            )
            .is_err()
        );
        assert!(
            rewrite_in_english_vocabulary(
                "{} of {}",
                &arguments(&["a", "b"], &[]),
                "{} のみ",
                &arguments(&["a"], &[]),
            )
            .is_err()
        );
        assert!(
            rewrite_in_english_vocabulary(
                "{}",
                &arguments(&["a"], &[]),
                "{:?}",
                &arguments(&["a"], &[]),
            )
            .is_err()
        );
    }
}
