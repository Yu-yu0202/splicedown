use std::collections::HashMap;

use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use syn::File;
use syn::visit_mut::VisitMut;

/// Rewrite paths occurring inside macro token streams.
pub(crate) fn rewrite_macro_tokens(
    file: &mut File,
    extern_map: &HashMap<String, Ident>,
    self_mod: Option<&Ident>,
) {
    MacroTokenRewriter {
        extern_map,
        self_mod,
    }
    .visit_file_mut(file);
}

struct MacroTokenRewriter<'a> {
    extern_map: &'a HashMap<String, Ident>,
    self_mod: Option<&'a Ident>,
}

impl VisitMut for MacroTokenRewriter<'_> {
    fn visit_macro_mut(&mut self, node: &mut syn::Macro) {
        node.tokens = rewrite_tokens(node.tokens.clone(), self.extern_map, self.self_mod);
        // A Macro contains no other AST children that need this visitor. In
        // particular, revisiting the generated tokens would duplicate paths.
    }
}

fn rewrite_tokens(
    tokens: TokenStream,
    extern_map: &HashMap<String, Ident>,
    self_mod: Option<&Ident>,
) -> TokenStream {
    let input: Vec<_> = tokens.into_iter().collect();
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;
    let mut path_continuation = false;
    let mut angle_contexts = Vec::new();

    while index < input.len() {
        // `use ::{dep::item, skipped::item}` cannot retain its leading `::`
        // after a bundled branch becomes `crate::...`. Removing it preserves
        // the ordinary extern-prelude lookup of untouched branches in edition
        // 2024 while allowing the rewritten `crate` branch.
        if matches!(&input[index], TokenTree::Ident(ident) if ident == "use")
            && has_colon2(&input, index + 1)
            && matches!(input.get(index + 3), Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Brace)
        {
            let TokenTree::Group(group) = &input[index + 3] else {
                unreachable!()
            };
            output.push(input[index].clone());
            output.push(TokenTree::Group(rewrite_group(group, extern_map, self_mod)));
            index += 4;
            path_continuation = false;
            continue;
        }

        if let TokenTree::Group(group) = &input[index] {
            output.push(TokenTree::Group(rewrite_group(group, extern_map, self_mod)));
            index += 1;
            path_continuation = false;
            continue;
        }

        // Angle-delimited generic arguments are not proc_macro Groups. Paths
        // inside them start afresh, while the path outside continues after
        // the matching `>`, including a following `::segment`.
        if is_punct(&input[index], '<') {
            angle_contexts.push(path_continuation);
            output.push(input[index].clone());
            index += 1;
            path_continuation = false;
            continue;
        }
        if is_punct(&input[index], '>')
            && let Some(outer_continuation) = angle_contexts.pop()
        {
            output.push(input[index].clone());
            index += 1;
            path_continuation = outer_continuation;
            continue;
        }

        // A macro metavariable followed by `::` is one path root. `$crate`
        // receives the dependency wrapper, while all other metavariables are
        // copied without interpreting their name as an extern crate.
        if is_punct(&input[index], '$')
            && matches!(input.get(index + 1), Some(TokenTree::Ident(_)))
            && has_colon2(&input, index + 2)
        {
            output.extend(input[index..index + 4].iter().cloned());
            if matches!(&input[index + 1], TokenTree::Ident(ident) if ident == "crate")
                && let Some(self_mod) = self_mod
            {
                let span = token_span(&input[index + 1]);
                output.push(with_span(self_mod, span));
                output.extend(colon2(span));
            }
            index += 4;
            path_continuation = true;
            continue;
        }

        // An absolute extern path has no identifier before its initial `::`.
        // Consume the whole root prefix so later segments cannot be mistaken
        // for another extern root.
        if !path_continuation
            && has_colon2(&input, index)
            && let Some(TokenTree::Ident(ident)) = input.get(index + 2)
            && has_colon2(&input, index + 3)
        {
            let span = ident.span();
            if let Some(mangled) = extern_map.get(&ident.to_string()) {
                output.push(TokenTree::Ident(Ident::new("crate", span)));
                output.extend(colon2(span));
                output.push(with_span(mangled, span));
                output.extend(colon2(span));
            } else {
                output.extend(input[index..index + 5].iter().cloned());
            }
            index += 5;
            path_continuation = true;
            continue;
        }

        // This `::` follows a generic argument list belonging to an existing
        // path. It is a separator, not the beginning of an absolute path.
        if path_continuation && has_colon2(&input, index) {
            output.extend(input[index..index + 2].iter().cloned());
            index += 2;
            continue;
        }

        if let TokenTree::Ident(ident) = &input[index]
            && has_colon2(&input, index + 1)
        {
            // Once the first segment has been consumed, every subsequent
            // `ident::` belongs to that same path, even when its spelling is
            // also present in extern_map.
            let replacement = if path_continuation {
                None
            } else if ident == "crate" {
                self_mod.map(|self_mod| (false, self_mod))
            } else {
                extern_map
                    .get(&ident.to_string())
                    .map(|mangled| (true, mangled))
            };

            if let Some((prefix_crate, mangled)) = replacement {
                let span = ident.span();
                if prefix_crate {
                    output.push(TokenTree::Ident(Ident::new("crate", span)));
                    output.extend(colon2(span));
                } else {
                    output.push(input[index].clone());
                    output.extend(input[index + 1..index + 3].iter().cloned());
                }
                output.push(with_span(mangled, span));
                output.extend(colon2(span));
                index += 3;
                path_continuation = true;
                continue;
            }

            output.extend(input[index..index + 3].iter().cloned());
            index += 3;
            path_continuation = true;
            continue;
        }

        output.push(input[index].clone());
        index += 1;
        path_continuation = false;
    }

    output.into_iter().collect()
}

fn rewrite_group(
    group: &Group,
    extern_map: &HashMap<String, Ident>,
    self_mod: Option<&Ident>,
) -> Group {
    let stream = rewrite_tokens(group.stream(), extern_map, self_mod);
    let mut rewritten = Group::new(group.delimiter(), stream);
    rewritten.set_span(group.span());
    rewritten
}

fn has_colon2(tokens: &[TokenTree], index: usize) -> bool {
    tokens.get(index).is_some_and(|token| is_punct(token, ':'))
        && tokens
            .get(index + 1)
            .is_some_and(|token| is_punct(token, ':'))
}

fn is_punct(token: &TokenTree, expected: char) -> bool {
    matches!(token, TokenTree::Punct(punct) if punct.as_char() == expected)
}

fn colon2(span: Span) -> [TokenTree; 2] {
    let mut first = Punct::new(':', Spacing::Joint);
    first.set_span(span);
    let mut second = Punct::new(':', Spacing::Alone);
    second.set_span(span);
    [TokenTree::Punct(first), TokenTree::Punct(second)]
}

fn with_span(ident: &Ident, span: Span) -> TokenTree {
    let mut ident = ident.clone();
    ident.set_span(span);
    TokenTree::Ident(ident)
}

fn token_span(token: &TokenTree) -> Span {
    token.span()
}
