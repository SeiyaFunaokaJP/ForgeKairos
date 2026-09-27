use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Expr, Ident, LitStr, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

/// Translates a message written in the same syntax as `format!`.
///
/// `t!("Open File")` expands to a `&'static str`; with arguments or placeholders it
/// expands to a `String`.
#[proc_macro]
pub fn t(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as SingularInput);
    finish(expand(Message {
        context: None,
        count: None,
        templates: vec![input.template],
        arguments: input.arguments,
        plural_macro_name: None,
    }))
}

/// Translates a message with a context (the PO `msgctxt`).
///
/// `tc!("action", "Open")` expands to a `&'static str`; with arguments or
/// placeholders it expands to a `String`.
#[proc_macro]
pub fn tc(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ContextInput);
    finish(expand(Message {
        context: Some(input.context),
        count: None,
        templates: vec![input.template],
        arguments: input.arguments,
        plural_macro_name: None,
    }))
}

/// Translates a message with plural forms: `tn!(count, "{count} file", "{count} files")`.
///
/// `{count}` is bound to the first argument.
#[proc_macro]
pub fn tn(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as PluralInput);
    finish(expand(Message {
        context: None,
        count: Some(input.count),
        templates: vec![input.singular, input.plural],
        arguments: input.arguments,
        plural_macro_name: Some("tn!"),
    }))
}

/// Translates a message with a context and plural forms:
/// `tcn!("context", count, "{count} file", "{count} files")`.
#[proc_macro]
pub fn tcn(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as ContextPluralInput);
    finish(expand(Message {
        context: Some(input.context),
        count: Some(input.count),
        templates: vec![input.singular, input.plural],
        arguments: input.arguments,
        plural_macro_name: Some("tcn!"),
    }))
}

/// Marks a literal for extraction without translating it here; it expands to the
/// literal itself so it can be used in `const` items. Translate it where it is shown
/// with `i18n::tr`.
#[proc_macro]
pub fn msg(input: TokenStream) -> TokenStream {
    let literal = parse_macro_input!(input as LitStr);
    quote!(#literal).into()
}

fn finish(result: syn::Result<TokenStream2>) -> TokenStream {
    match result {
        Ok(tokens) => tokens.into(),
        Err(error) => error.to_compile_error().into(),
    }
}

struct FormatArgument {
    name: Option<Ident>,
    expression: Expr,
}

fn parse_trailing_arguments(input: ParseStream) -> syn::Result<Vec<FormatArgument>> {
    let mut arguments = Vec::new();
    while !input.is_empty() {
        input.parse::<Token![,]>()?;
        if input.is_empty() {
            break;
        }
        let name = if input.peek(Ident) && input.peek2(Token![=]) && !input.peek2(Token![==]) {
            let name = input.parse::<Ident>()?;
            input.parse::<Token![=]>()?;
            Some(name)
        } else {
            None
        };
        let expression = input.parse::<Expr>()?;
        arguments.push(FormatArgument { name, expression });
    }
    Ok(arguments)
}

struct SingularInput {
    template: LitStr,
    arguments: Vec<FormatArgument>,
}

impl Parse for SingularInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let template = input.parse()?;
        let arguments = parse_trailing_arguments(input)?;
        Ok(Self {
            template,
            arguments,
        })
    }
}

struct ContextInput {
    context: LitStr,
    template: LitStr,
    arguments: Vec<FormatArgument>,
}

impl Parse for ContextInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let context = input.parse()?;
        input.parse::<Token![,]>()?;
        let template = input.parse()?;
        let arguments = parse_trailing_arguments(input)?;
        Ok(Self {
            context,
            template,
            arguments,
        })
    }
}

struct PluralInput {
    count: Expr,
    singular: LitStr,
    plural: LitStr,
    arguments: Vec<FormatArgument>,
}

impl Parse for PluralInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let count = input.parse()?;
        input.parse::<Token![,]>()?;
        let singular = input.parse()?;
        input.parse::<Token![,]>()?;
        let plural = input.parse()?;
        let arguments = parse_trailing_arguments(input)?;
        Ok(Self {
            count,
            singular,
            plural,
            arguments,
        })
    }
}

struct ContextPluralInput {
    context: LitStr,
    count: Expr,
    singular: LitStr,
    plural: LitStr,
    arguments: Vec<FormatArgument>,
}

impl Parse for ContextPluralInput {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let context = input.parse()?;
        input.parse::<Token![,]>()?;
        let PluralInput {
            count,
            singular,
            plural,
            arguments,
        } = input.parse()?;
        Ok(Self {
            context,
            count,
            singular,
            plural,
            arguments,
        })
    }
}

#[derive(Debug, PartialEq)]
enum Piece {
    Text(String),
    Placeholder {
        argument: ArgumentReference,
        spec: String,
    },
}

#[derive(Debug, PartialEq)]
enum ArgumentReference {
    Next,
    Index(usize),
    Name(String),
}

fn parse_template(source: &str) -> Result<Vec<Piece>, String> {
    let mut pieces = Vec::new();
    let mut text = String::new();
    let mut characters = source.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            '{' => {
                if characters.peek() == Some(&'{') {
                    characters.next();
                    text.push('{');
                    continue;
                }
                let mut inner = String::new();
                let mut closed = false;
                for next in characters.by_ref() {
                    match next {
                        '}' => {
                            closed = true;
                            break;
                        }
                        '{' => {
                            return Err(
                                "invalid format string: unexpected `{` inside a placeholder; \
                                 if you intended to print `{`, escape it as `{{`"
                                    .to_string(),
                            );
                        }
                        other => inner.push(other),
                    }
                }
                if !closed {
                    return Err("invalid format string: expected `}` but the string ended; \
                         if you intended to print `{`, escape it as `{{`"
                        .to_string());
                }
                if !text.is_empty() {
                    pieces.push(Piece::Text(std::mem::take(&mut text)));
                }
                pieces.push(parse_placeholder(&inner)?);
            }
            '}' => {
                if characters.peek() == Some(&'}') {
                    characters.next();
                    text.push('}');
                } else {
                    return Err("invalid format string: unmatched `}` found; \
                         if you intended to print `}`, escape it as `}}`"
                        .to_string());
                }
            }
            other => text.push(other),
        }
    }
    if !text.is_empty() {
        pieces.push(Piece::Text(text));
    }
    Ok(pieces)
}

fn parse_placeholder(inner: &str) -> Result<Piece, String> {
    let (argument_text, spec) = match inner.split_once(':') {
        Some((argument_text, spec)) => (argument_text, spec),
        None => (inner, ""),
    };
    // `format!` accepts whitespace after the argument (`{name }`, `{ }`) but not before it.
    let argument_text = argument_text.trim_end();
    if argument_text.starts_with(char::is_whitespace) {
        return Err(format!(
            "invalid format string: `{argument_text}` starts with whitespace"
        ));
    }
    let argument = if argument_text.is_empty() {
        ArgumentReference::Next
    } else if argument_text
        .chars()
        .all(|character| character.is_ascii_digit())
    {
        let index = argument_text
            .parse::<usize>()
            .map_err(|error| format!("invalid argument index `{argument_text}`: {error}"))?;
        ArgumentReference::Index(index)
    } else if argument_text == "self" || syn::parse_str::<Ident>(argument_text).is_ok() {
        ArgumentReference::Name(argument_text.to_string())
    } else {
        return Err(format!(
            "invalid format string: `{argument_text}` is not an argument index or an identifier"
        ));
    };
    // The runtime template only substitutes already formatted strings, so a width or
    // precision taken from another argument cannot be carried into a translation.
    if spec.contains('$') || spec.contains('*') {
        return Err(format!(
            "the format spec `{{{inner}}}` takes its width or precision from an argument, \
             which translated messages do not support; format that value into a String first"
        ));
    }
    Ok(Piece::Placeholder {
        argument,
        spec: spec.to_string(),
    })
}

fn normalized_msgid(pieces: &[Piece]) -> String {
    let mut msgid = String::new();
    for piece in pieces {
        match piece {
            Piece::Text(text) => {
                for character in text.chars() {
                    match character {
                        '{' => msgid.push_str("{{"),
                        '}' => msgid.push_str("}}"),
                        other => msgid.push(other),
                    }
                }
            }
            Piece::Placeholder { argument, .. } => match argument {
                ArgumentReference::Next => msgid.push_str("{}"),
                ArgumentReference::Index(index) => {
                    msgid.push('{');
                    msgid.push_str(&index.to_string());
                    msgid.push('}');
                }
                ArgumentReference::Name(name) => {
                    msgid.push('{');
                    msgid.push_str(name);
                    msgid.push('}');
                }
            },
        }
    }
    msgid
}

struct Message {
    context: Option<LitStr>,
    count: Option<Expr>,
    templates: Vec<LitStr>,
    arguments: Vec<FormatArgument>,
    plural_macro_name: Option<&'static str>,
}

#[derive(Clone, Copy, PartialEq)]
enum Slot {
    Count,
    Positional(usize),
    Named(usize),
    Capture(usize),
}

struct Use {
    key: String,
    slot: Slot,
    spec: String,
    span: Span,
}

fn plural_suffix(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

fn expand(message: Message) -> syn::Result<TokenStream2> {
    let mut parsed_templates = Vec::new();
    for template in &message.templates {
        let pieces = parse_template(&template.value())
            .map_err(|error| syn::Error::new(template.span(), error))?;
        parsed_templates.push((template, pieces));
    }

    let mut positional: Vec<&Expr> = Vec::new();
    let mut named: Vec<(&Ident, &Expr)> = Vec::new();
    for argument in &message.arguments {
        match &argument.name {
            Some(name) => {
                if named.iter().any(|(existing, _)| *existing == name) {
                    return Err(syn::Error::new(
                        name.span(),
                        format!("duplicate argument named `{name}`"),
                    ));
                }
                if let Some(macro_name) = message.plural_macro_name
                    && name == "count"
                {
                    return Err(syn::Error::new(
                        name.span(),
                        format!(
                            "`count` is bound to the count argument of `{macro_name}` automatically; \
                             remove `count = ...`"
                        ),
                    ));
                }
                named.push((name, &argument.expression));
            }
            None => {
                if !named.is_empty() {
                    return Err(syn::Error::new_spanned(
                        &argument.expression,
                        "positional arguments cannot follow named arguments",
                    ));
                }
                positional.push(&argument.expression);
            }
        }
    }

    let context_tokens = match &message.context {
        Some(context) => quote!(::core::option::Option::Some(#context)),
        None => quote!(::core::option::Option::None),
    };

    if message.count.is_none()
        && message.arguments.is_empty()
        && let [(template, pieces)] = parsed_templates.as_slice()
        && pieces.iter().all(|piece| matches!(piece, Piece::Text(_)))
        && !template.value().contains(['{', '}'])
    {
        return Ok(quote!(::i18n::__private::lookup_literal(#context_tokens, #template)));
    }

    let mut uses: Vec<Use> = Vec::new();
    let mut captures: Vec<Ident> = Vec::new();
    let mut used_positional = vec![false; positional.len()];
    let mut used_named = vec![false; named.len()];

    for (template, pieces) in &parsed_templates {
        let mut next_index = 0;
        let mut implicit_count = 0;
        let mut max_requested = None;
        for piece in pieces {
            let Piece::Placeholder { argument, spec } = piece else {
                continue;
            };
            let (key, slot) = match argument {
                ArgumentReference::Next => {
                    let index = next_index;
                    next_index += 1;
                    implicit_count += 1;
                    if index >= positional.len() {
                        max_requested = Some(index);
                        continue;
                    }
                    (index.to_string(), Slot::Positional(index))
                }
                ArgumentReference::Index(index) => {
                    if *index >= positional.len() {
                        return Err(syn::Error::new(
                            template.span(),
                            format!(
                                "invalid reference to positional argument {index} ({} positional argument{} given)",
                                positional.len(),
                                plural_suffix(positional.len()),
                            ),
                        ));
                    }
                    (index.to_string(), Slot::Positional(*index))
                }
                ArgumentReference::Name(name) => {
                    let slot = if message.count.is_some() && name == "count" {
                        Slot::Count
                    } else if let Some(index) = named
                        .iter()
                        .position(|(argument_name, _)| *argument_name == name)
                    {
                        Slot::Named(index)
                    } else {
                        let index = match captures.iter().position(|capture| capture == name) {
                            Some(index) => index,
                            None => {
                                // `syn` refuses keywords as identifiers, but `format!`
                                // captures `self`.
                                let mut capture = if name == "self" {
                                    Ident::new("self", template.span())
                                } else {
                                    syn::parse_str::<Ident>(name).map_err(|error| {
                                        syn::Error::new(template.span(), error.to_string())
                                    })?
                                };
                                // The literal's span resolves at the call site, so the
                                // captured name finds the caller's local variable.
                                capture.set_span(template.span());
                                captures.push(capture);
                                captures.len() - 1
                            }
                        };
                        Slot::Capture(index)
                    };
                    (name.clone(), slot)
                }
            };
            match slot {
                Slot::Positional(index) => used_positional[index] = true,
                Slot::Named(index) => used_named[index] = true,
                Slot::Count | Slot::Capture(_) => {}
            }
            match uses.iter().find(|existing| existing.key == key) {
                Some(existing) if existing.spec != *spec => {
                    return Err(syn::Error::new(
                        template.span(),
                        format!(
                            "argument `{key}` is used with different format specs (`{}` and `{spec}`); \
                             translated messages format each argument once",
                            existing.spec,
                        ),
                    ));
                }
                Some(_) => {}
                None => uses.push(Use {
                    key,
                    slot,
                    spec: spec.clone(),
                    span: template.span(),
                }),
            }
        }
        if max_requested.is_some() {
            return Err(syn::Error::new(
                template.span(),
                format!(
                    "{implicit_count} positional argument{} in format string, but there {} {} argument{}",
                    plural_suffix(implicit_count),
                    if positional.len() == 1 { "is" } else { "are" },
                    positional.len(),
                    plural_suffix(positional.len()),
                ),
            ));
        }
    }

    let mut unused_error: Option<syn::Error> = None;
    let mut report_unused = |error: syn::Error| match &mut unused_error {
        Some(existing) => existing.combine(error),
        None => unused_error = Some(error),
    };
    for (index, used) in used_positional.iter().enumerate() {
        if !used {
            report_unused(syn::Error::new_spanned(
                positional[index],
                "argument never used in the format string",
            ));
        }
    }
    for (index, used) in used_named.iter().enumerate() {
        if !used {
            report_unused(syn::Error::new(
                named[index].0.span(),
                format!(
                    "named argument `{}` never used in the format string",
                    named[index].0
                ),
            ));
        }
    }
    if let Some(error) = unused_error {
        return Err(error);
    }

    let count_binding = format_ident!("__i18n_count", span = Span::mixed_site());
    let positional_bindings: Vec<Ident> = (0..positional.len())
        .map(|index| format_ident!("__i18n_argument_{}", index, span = Span::mixed_site()))
        .collect();
    let named_bindings: Vec<Ident> = (0..named.len())
        .map(|index| format_ident!("__i18n_named_{}", index, span = Span::mixed_site()))
        .collect();
    let capture_bindings: Vec<Ident> = (0..captures.len())
        .map(|index| format_ident!("__i18n_capture_{}", index, span = Span::mixed_site()))
        .collect();

    let mut bound_expressions: Vec<TokenStream2> = Vec::new();
    let mut bound_patterns: Vec<&Ident> = Vec::new();
    if let Some(count) = &message.count {
        bound_expressions.push(quote!(#count));
        bound_patterns.push(&count_binding);
    }
    for (expression, binding) in positional.iter().zip(&positional_bindings) {
        bound_expressions.push(quote!(#expression));
        bound_patterns.push(binding);
    }
    for ((_, expression), binding) in named.iter().zip(&named_bindings) {
        bound_expressions.push(quote!(#expression));
        bound_patterns.push(binding);
    }
    for (capture, binding) in captures.iter().zip(&capture_bindings) {
        bound_expressions.push(quote!(#capture));
        bound_patterns.push(binding);
    }

    let mut formatting = Vec::new();
    let mut argument_entries = Vec::new();
    for (index, argument_use) in uses.iter().enumerate() {
        let binding = match argument_use.slot {
            Slot::Count => &count_binding,
            Slot::Positional(slot_index) => &positional_bindings[slot_index],
            Slot::Named(slot_index) => &named_bindings[slot_index],
            Slot::Capture(slot_index) => &capture_bindings[slot_index],
        };
        let formatted = format_ident!("__i18n_formatted_{}", index, span = Span::mixed_site());
        let format_literal = LitStr::new(&format!("{{:{}}}", argument_use.spec), argument_use.span);
        let key = &argument_use.key;
        // Formatting the dereferenced binding gives the trait the same `&T` that
        // `format!` would, which matters for `{:p}`.
        formatting.push(quote!(let #formatted = ::std::format!(#format_literal, *#binding);));
        argument_entries.push(quote!((#key, #formatted.as_str())));
    }

    let (lookup, fallback, count_value) = match parsed_templates.as_slice() {
        [(_, pieces)] => {
            let msgid = normalized_msgid(pieces);
            (
                quote!(::i18n::__private::lookup_literal(#context_tokens, #msgid)),
                quote!(#msgid),
                quote!(),
            )
        }
        [(_, singular_pieces), (_, plural_pieces)] => {
            let singular = normalized_msgid(singular_pieces);
            let plural = normalized_msgid(plural_pieces);
            let count_value = format_ident!("__i18n_count_value", span = Span::mixed_site());
            (
                quote!(::i18n::__private::lookup_plural_literal(
                    #context_tokens, #singular, #plural, #count_value
                )),
                quote!(::i18n::__private::english_plural(#singular, #plural, #count_value)),
                quote!(let #count_value = ::i18n::__private::PluralCount::plural_count(#count_binding);),
            )
        }
        _ => {
            return Err(syn::Error::new(
                Span::call_site(),
                "expected one or two message templates",
            ));
        }
    };

    let message_binding = format_ident!("__i18n_message", span = Span::mixed_site());
    // Temporaries in a `match` scrutinee live until the end of the enclosing statement;
    // the `let` ends them here, as `format!` does, so guards created by the arguments
    // are not held across a later `.await` in the same statement.
    Ok(quote! {
        {
            let #message_binding = match ( #( &(#bound_expressions), )* ) {
                ( #( #bound_patterns, )* ) => {
                    #count_value
                    #( #formatting )*
                    ::i18n::__private::format_translated(
                        #lookup,
                        #fallback,
                        &[ #( #argument_entries ),* ],
                    )
                }
            };
            #message_binding
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholder(argument: ArgumentReference, spec: &str) -> Piece {
        Piece::Placeholder {
            argument,
            spec: spec.to_string(),
        }
    }

    #[test]
    fn parses_every_placeholder_form() {
        let pieces = parse_template("a {} {0} {name} {x:.1} {:?} {{b}}").unwrap_or_default();
        assert_eq!(
            pieces,
            vec![
                Piece::Text("a ".to_string()),
                placeholder(ArgumentReference::Next, ""),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Index(0), ""),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Name("name".to_string()), ""),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Name("x".to_string()), ".1"),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Next, "?"),
                Piece::Text(" {b}".to_string()),
            ]
        );
    }

    #[test]
    fn normalization_drops_specs_and_keeps_escapes() {
        let pieces = parse_template("{x:>5} {:?} {1:.2} {{literal}} {}").unwrap_or_default();
        assert_eq!(normalized_msgid(&pieces), "{x} {} {1} {{literal}} {}");
    }

    #[test]
    fn trailing_whitespace_in_a_placeholder_is_ignored_like_format() {
        let pieces = parse_template("{name } {0 :?} { } {self}").unwrap_or_default();
        assert_eq!(
            pieces,
            vec![
                placeholder(ArgumentReference::Name("name".to_string()), ""),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Index(0), "?"),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Next, ""),
                Piece::Text(" ".to_string()),
                placeholder(ArgumentReference::Name("self".to_string()), ""),
            ]
        );
        assert_eq!(normalized_msgid(&pieces), "{name} {0} {} {self}");
    }

    #[test]
    fn rejects_malformed_templates() {
        for template in [
            "{ name}",
            "{ 0}",
            "{type}",
            "{",
            "}",
            "a { b",
            "{a{b}",
            "{not an ident}",
            "{:width$}",
            "{:.*}",
            "{0:1$}",
        ] {
            assert!(
                parse_template(template).is_err(),
                "{template:?} should be rejected"
            );
        }
    }
}
