use proc_macro2::{TokenStream, TokenTree};
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;

/// Occurrences of every identifier, with the byte ranges they were read from.
pub(crate) fn ident_positions(source: &str) -> HashMap<String, Vec<Range<usize>>> {
    let mut out = HashMap::new();
    if let Ok(tokens) = source.parse::<TokenStream>() {
        collect_ident_ranges(tokens, &mut out);
    }
    out
}

fn collect_ident_ranges(tokens: TokenStream, out: &mut HashMap<String, Vec<Range<usize>>>) {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) => {
                out.entry(ident.to_string())
                    .or_default()
                    .push(ident.span().byte_range());
            }
            TokenTree::Group(group) => collect_ident_ranges(group.stream(), out),
            _ => {}
        }
    }
}

/// Macros that render their token arguments as text. Renaming an identifier
/// that reaches one of them would change what the program prints, so the
/// rename pass leaves those identifiers alone.
///
/// The set stays small on purpose: `println!` and friends print the *value* of
/// an expression, so they are safe to rename inside. What must not be renamed
/// is anything that turns its own tokens into a string. A macro definition
/// that renders text by calling one of these is handled the same way in
/// [`collect_text_sensitive`].
const TEXT_SENSITIVE_MACROS: &[&str] = &["stringify", "concat", "stringify_idents"];

/// Identifiers that some macro could render as text.
///
/// Renaming them would change what the program prints, so the rename pass
/// leaves them alone. When no macro in the bundle renders text, no identifier
/// is at risk and the rename pass may shorten all of them.
pub(crate) fn text_sensitive_idents(source: &str) -> HashSet<String> {
    let Ok(tokens) = source.parse::<TokenStream>() else {
        return HashSet::new();
    };
    let rendering = text_rendering_macros(&tokens);
    if rendering.is_empty() {
        return HashSet::new();
    }
    // A macro that renders text may receive any token that lives inside a macro
    // body or a macro call, including tokens that reach it through another
    // macro. Everything in such a position therefore stays untouched.
    let mut out = HashSet::new();
    collect_macro_idents(tokens, &mut out);
    out
}

/// Macro definitions whose expansion turns some of its tokens into text.
pub(crate) fn text_rendering_macros(tokens: &TokenStream) -> HashSet<String> {
    let mut definitions = HashMap::new();
    collect_macro_definitions(tokens, &mut definitions);
    let mut rendering = HashSet::new();
    loop {
        let mut changed = false;
        for (name, body) in &definitions {
            if rendering.contains(name) {
                continue;
            }
            if macro_body_renders(body, &definitions, &rendering) {
                rendering.insert(name.clone());
                changed = true;
            }
        }
        if !changed {
            return rendering;
        }
    }
}

/// Whether the body of a macro definition can render text, directly or through
/// a macro it calls.
fn macro_body_renders(
    body: &TokenStream,
    definitions: &HashMap<String, TokenStream>,
    rendering: &HashSet<String>,
) -> bool {
    let tokens = body.clone().into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < tokens.len() {
        if let Some((_, name, group, next)) = macro_invocation(&tokens, index) {
            if TEXT_SENSITIVE_MACROS.contains(&name.as_str()) || rendering.contains(&name) {
                return true;
            }
            if let Some(nested) = definitions.get(&name)
                && macro_body_renders(nested, definitions, rendering)
            {
                return true;
            }
            if let Some(TokenTree::Group(group)) = tokens.get(group)
                && macro_body_renders(&group.stream(), definitions, rendering)
            {
                return true;
            }
            index = next;
            continue;
        }
        if let TokenTree::Group(group) = &tokens[index]
            && macro_body_renders(&group.stream(), definitions, rendering)
        {
            return true;
        }
        index += 1;
    }
    false
}

/// Parse a macro call, or a `macro_rules!` definition, at `index`.
///
/// Returns the keyword in front of the `!`, the macro name, the index of its
/// group and the index to continue scanning from.
fn macro_invocation(tokens: &[TokenTree], index: usize) -> Option<(String, String, usize, usize)> {
    let head = match tokens.get(index)? {
        TokenTree::Ident(ident) => ident.to_string(),
        _ => return None,
    };
    match tokens.get(index + 1)? {
        TokenTree::Punct(punct) if punct.as_char() == '!' => {}
        _ => return None,
    }
    match tokens.get(index + 2) {
        // `name! { ... }`
        Some(TokenTree::Group(_)) => Some((head.clone(), head, index + 2, index + 3)),
        // `macro_rules! report { ... }`, where the group belongs to `report`
        Some(TokenTree::Ident(ident))
            if tokens
                .get(index + 3)
                .is_some_and(|token| matches!(token, TokenTree::Group(_))) =>
        {
            Some((head, ident.to_string(), index + 3, index + 4))
        }
        _ => None,
    }
}

fn collect_macro_definitions(tokens: &TokenStream, out: &mut HashMap<String, TokenStream>) {
    let tokens = tokens.clone().into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < tokens.len() {
        if let Some((head, name, group, _)) = macro_invocation(&tokens, index)
            && head == "macro_rules"
            && let Some(TokenTree::Group(body)) = tokens.get(group)
        {
            out.insert(name, body.stream());
        }
        if let TokenTree::Group(group) = &tokens[index] {
            // Walking one token at a time also reaches definitions nested in a
            // macro body, which a jump over the body would skip.
            collect_macro_definitions(&group.stream(), out);
        }
        index += 1;
    }
}

/// Collect every identifier inside a macro definition body or macro call.
fn collect_macro_idents(tokens: TokenStream, out: &mut HashSet<String>) {
    let tokens = tokens.into_iter().collect::<Vec<_>>();
    let mut index = 0;
    while index < tokens.len() {
        if let Some((_, _, group, next)) = macro_invocation(&tokens, index) {
            if let Some(TokenTree::Group(group)) = tokens.get(group) {
                collect_idents(group.stream(), out);
            }
            index = next;
            continue;
        }
        if let TokenTree::Group(group) = &tokens[index] {
            collect_macro_idents(group.stream(), out);
        }
        index += 1;
    }
}

fn collect_idents(tokens: TokenStream, out: &mut HashSet<String>) {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) => {
                out.insert(ident.to_string());
            }
            TokenTree::Group(group) => collect_idents(group.stream(), out),
            _ => {}
        }
    }
}
