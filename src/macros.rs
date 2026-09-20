//! Rewrites macro definitions and the token streams hidden from syn's normal
//! AST visitors.

use std::collections::HashMap;

use anyhow::Result;
use proc_macro2::{Delimiter, Group, Ident, Punct, Spacing, Span, TokenStream, TokenTree};
use syn::Meta;
use syn::punctuated::Punctuated;
use syn::visit_mut::VisitMut;
use syn::{Attribute, File, Item, ItemUse, Token, UseName, UsePath, UseTree};

/// Keep exported dependency macros inside their wrapper module.
///
/// `#[macro_export]` would otherwise place the macro at the root of the final
/// crate.  Removing that attribute and re-exporting the macro next to its
/// definition makes it addressable through the dependency's mangled module.
pub(crate) fn fix_macro_exports(file: &mut File) -> Result<()> {
    let mut root_reexports = Vec::new();
    fix_macro_exports_in_items(&mut file.items, &[], &[], &mut root_reexports)?;
    file.items.extend(root_reexports);
    Ok(())
}

fn fix_macro_exports_in_items(
    items: &mut Vec<Item>,
    module_path: &[Ident],
    inherited_conditions: &[Attribute],
    root_reexports: &mut Vec<Item>,
) -> Result<()> {
    let old_items = std::mem::take(items);
    let mut new_items = Vec::with_capacity(old_items.len());

    for mut item in old_items {
        if let Item::Mod(item_mod) = &mut item
            && let Some((_, nested)) = &mut item_mod.content
        {
            let mut nested_path = module_path.to_vec();
            nested_path.push(item_mod.ident.clone());
            let mut nested_conditions = inherited_conditions.to_vec();
            nested_conditions.extend(condition_attrs(&item_mod.attrs));
            fix_macro_exports_in_items(nested, &nested_path, &nested_conditions, root_reexports)?;
        }

        let exported_macro = if let Item::Macro(item_macro) = &mut item {
            if let Some(export_condition) = take_macro_export(&mut item_macro.attrs)? {
                let mut conditions = condition_attrs(&item_macro.attrs);
                if let Some(export_condition) = export_condition {
                    conditions.push(export_condition);
                }
                Some((
                    item_macro.ident.clone().ok_or_else(|| {
                        anyhow::anyhow!("#[macro_export] can only be used on a named macro")
                    })?,
                    conditions,
                ))
            } else {
                None
            }
        } else {
            None
        };

        new_items.push(item);
        if let Some((ident, own_conditions)) = exported_macro {
            new_items.push(Item::Use(make_reexport(
                std::slice::from_ref(&ident),
                own_conditions.clone(),
            )));

            // `#[macro_export]` makes a nested macro available at the original
            // crate root. Preserve that API in addition to the local export
            // used through the containing module.
            if !module_path.is_empty() {
                let mut path = module_path.to_vec();
                path.push(ident);
                let mut conditions = inherited_conditions.to_vec();
                conditions.extend(own_conditions);
                root_reexports.push(Item::Use(make_reexport(&path, conditions)));
            }
        }
    }

    *items = new_items;
    Ok(())
}

/// Remove unconditional and directly conditional `macro_export` attributes.
///
/// The outer option indicates whether the macro is exported at all. The inner
/// option is a `cfg` attribute that gates the generated imports when export is
/// conditional. An unconditional export wins over all conditional branches.
fn take_macro_export(attrs: &mut Vec<Attribute>) -> Result<Option<Option<Attribute>>> {
    let mut unconditional = false;
    let mut predicates = Vec::new();
    let mut retained = Vec::with_capacity(attrs.len());

    for attr in std::mem::take(attrs) {
        if attr.path().is_ident("macro_export") {
            unconditional = true;
            continue;
        }

        if !attr.path().is_ident("cfg_attr") {
            retained.push(attr);
            continue;
        }

        let args = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        let mut args = args.into_iter();
        let Some(predicate) = args.next() else {
            retained.push(attr);
            continue;
        };
        let mut remaining = Vec::new();
        let mut conditional_export = false;
        for meta in args {
            if matches!(&meta, Meta::Path(path) if path.is_ident("macro_export")) {
                conditional_export = true;
            } else {
                if nested_cfg_attr_exports(&meta)? {
                    anyhow::bail!(
                        "nested cfg_attr(..., macro_export) is not supported while bundling macros"
                    );
                }
                remaining.push(meta);
            }
        }

        if conditional_export {
            predicates.push(predicate.clone());
            if !remaining.is_empty() {
                retained.push(syn::parse_quote!(#[cfg_attr(#predicate, #(#remaining),*)]));
            }
        } else {
            retained.push(attr);
        }
    }

    *attrs = retained;
    if unconditional {
        Ok(Some(None))
    } else if predicates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Some(syn::parse_quote!(#[cfg(any(#(#predicates),*))]))))
    }
}

fn nested_cfg_attr_exports(meta: &Meta) -> Result<bool> {
    let Meta::List(list) = meta else {
        return Ok(false);
    };
    if !list.path.is_ident("cfg_attr") {
        return Ok(false);
    }

    let args = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
    for nested in args.iter().skip(1) {
        if matches!(nested, Meta::Path(path) if path.is_ident("macro_export"))
            || nested_cfg_attr_exports(nested)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn condition_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
        .cloned()
        .collect()
}

fn make_reexport(path: &[Ident], attrs: Vec<Attribute>) -> ItemUse {
    let mut tree = UseTree::Name(UseName {
        ident: path.last().expect("re-export path is non-empty").clone(),
    });
    for ident in path[..path.len() - 1].iter().rev() {
        tree = UseTree::Path(UsePath {
            ident: ident.clone(),
            colon2_token: Default::default(),
            tree: Box::new(tree),
        });
    }

    let mut item: ItemUse = syn::parse_quote!(
        pub(crate) use placeholder;
    );
    item.attrs = attrs;
    item.tree = tree;
    item
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn compact(file: &File) -> String {
        prettyplease::unparse(file)
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect()
    }

    fn ident(name: &str) -> Ident {
        Ident::new(name, Span::call_site())
    }

    #[test]
    fn fixes_exported_macros_recursively_and_preserves_other_attributes() {
        let mut file: File = syn::parse_quote! {
            mod nested {
                #[doc = "kept"]
                #[macro_export]
                macro_rules! exported { () => {} }
                macro_rules! private { () => {} }
            }
        };

        fix_macro_exports(&mut file).unwrap();
        let rendered = compact(&file);
        assert!(!rendered.contains("macro_export"));
        assert!(rendered.contains("pub(crate)useexported;"));
        assert!(rendered.contains("pub(crate)usenested::exported;"));
        assert!(!rendered.contains("useprivate"));

        let Item::Mod(module) = &file.items[0] else {
            panic!("expected module")
        };
        let (_, items) = module.content.as_ref().unwrap();
        let Item::Macro(exported) = &items[0] else {
            panic!("expected exported macro")
        };
        assert!(
            exported
                .attrs
                .iter()
                .any(|attr| attr.path().is_ident("doc"))
        );
    }

    #[test]
    fn nested_reexport_preserves_module_and_macro_conditions() {
        let mut file: File = syn::parse_quote! {
            #[cfg(feature = "parent")]
            mod nested {
                #[cfg_attr(feature = "switch", cfg(feature = "child"))]
                mod deep {
                    #[cfg(feature = "macro")]
                    #[macro_export]
                    macro_rules! conditional { () => {} }
                }
            }
        };

        fix_macro_exports(&mut file).unwrap();

        let Item::Use(root_use) = file.items.last().unwrap() else {
            panic!("expected root re-export")
        };
        assert_eq!(root_use.attrs.len(), 3);
        assert!(root_use.attrs[0].path().is_ident("cfg"));
        assert!(root_use.attrs[1].path().is_ident("cfg_attr"));
        assert!(root_use.attrs[2].path().is_ident("cfg"));
        assert!(compact(&file).contains("usenested::deep::conditional;"));

        let Item::Mod(nested) = &file.items[0] else {
            panic!("expected nested module")
        };
        let Item::Mod(deep) = &nested.content.as_ref().unwrap().1[0] else {
            panic!("expected deep module")
        };
        let Item::Use(local_use) = &deep.content.as_ref().unwrap().1[1] else {
            panic!("expected local re-export")
        };
        assert_eq!(local_use.attrs.len(), 1);
        assert!(local_use.attrs[0].path().is_ident("cfg"));
    }

    #[test]
    fn root_export_only_gets_its_own_conditions() {
        let mut file: File = syn::parse_quote! {
            #[cfg(feature = "macro")]
            #[macro_export]
            macro_rules! conditional { () => {} }
        };

        fix_macro_exports(&mut file).unwrap();
        assert_eq!(file.items.len(), 2);
        let Item::Use(local_use) = &file.items[1] else {
            panic!("expected local re-export")
        };
        assert_eq!(local_use.attrs.len(), 1);
        assert!(local_use.attrs[0].path().is_ident("cfg"));
    }

    #[test]
    fn conditional_macro_export_becomes_a_conditional_reexport() {
        let mut file: File = syn::parse_quote! {
            #[cfg_attr(all(), macro_export, allow(unused_macros))]
            macro_rules! conditional { () => {} }
        };

        fix_macro_exports(&mut file).unwrap();
        let rendered = compact(&file);
        assert!(!rendered.contains("macro_export"));
        assert!(rendered.contains("cfg_attr(all(),allow(unused_macros))"));
        assert!(rendered.contains("cfg(any(all()))"));
        assert!(rendered.contains("pub(crate)useconditional;"));
    }

    #[test]
    fn rejects_nested_conditional_macro_export() {
        let mut file: File = syn::parse_quote! {
            #[cfg_attr(all(), cfg_attr(all(), macro_export))]
            macro_rules! conditional { () => {} }
        };

        let error = fix_macro_exports(&mut file).unwrap_err();
        assert!(error.to_string().contains("nested cfg_attr"));
    }

    #[test]
    fn rejects_unnamed_exported_macro() {
        let mut file: File = syn::parse_quote! {
            #[macro_export]
            some_macro!();
        };
        let Item::Macro(item) = &mut file.items[0] else {
            panic!("expected macro item")
        };
        item.ident = None;

        assert!(fix_macro_exports(&mut file).is_err());
    }

    #[test]
    fn rewrites_definition_tokens_and_nested_groups() {
        let mut file: File = syn::parse_quote! {
            macro_rules! example {
                ($value:expr) => {{
                    [$crate::own($value), crate::local($value), dep::call($value)]
                }};
            }
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);
        let self_mod = ident("__self");

        rewrite_macro_tokens(&mut file, &extern_map, Some(&self_mod));
        let rendered = compact(&file);
        assert!(rendered.contains("$crate::__self::own"));
        assert!(rendered.contains("crate::__self::local"));
        assert!(rendered.contains("crate::__dep::call"));
    }

    #[test]
    fn rewrites_invocation_arguments_but_preserves_metavariables() {
        let mut file: File = syn::parse_quote! {
            invoke!(crate::local(), dep::call(), $dep::associated());
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);
        let self_mod = ident("__self");

        rewrite_macro_tokens(&mut file, &extern_map, Some(&self_mod));
        let rendered = compact(&file);
        assert!(rendered.contains("crate::__self::local()"));
        assert!(rendered.contains("crate::__dep::call()"));
        assert!(rendered.contains("$dep::associated()"));
    }

    #[test]
    fn entry_crate_paths_remain_unchanged() {
        let mut file: File = syn::parse_quote! {
            invoke!($crate::own(), crate::local(), dep::call());
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);

        rewrite_macro_tokens(&mut file, &extern_map, None);
        let rendered = compact(&file);
        assert!(rendered.contains("$crate::own()"));
        assert!(rendered.contains("crate::local()"));
        assert!(rendered.contains("crate::__dep::call()"));
    }

    #[test]
    fn leaves_non_path_uses_of_crate_and_unknown_dependencies_unchanged() {
        let mut file: File = syn::parse_quote! {
            invoke!(pub(crate), extern crate dep, unknown::call(), "dep::call");
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);
        let self_mod = ident("__self");

        rewrite_macro_tokens(&mut file, &extern_map, Some(&self_mod));
        let rendered = compact(&file);
        assert!(rendered.contains("pub(crate)"));
        assert!(rendered.contains("externcratedep"));
        assert!(rendered.contains("unknown::call()"));
        assert!(rendered.contains("\"dep::call\""));
    }

    #[test]
    fn only_rewrites_dependency_names_at_a_path_root() {
        let mut file: File = syn::parse_quote! {
            invoke!(
                crate::dep::one(),
                $crate::dep::two(),
                self::dep::three(),
                super::dep::four(),
                foo::dep::five(),
                ::dep::six(),
            );
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);
        let self_mod = ident("__self");

        rewrite_macro_tokens(&mut file, &extern_map, Some(&self_mod));
        let rendered = compact(&file);
        assert!(rendered.contains("crate::__self::dep::one()"));
        assert!(rendered.contains("$crate::__self::dep::two()"));
        assert!(rendered.contains("self::dep::three()"));
        assert!(rendered.contains("super::dep::four()"));
        assert!(rendered.contains("foo::dep::five()"));
        assert!(rendered.contains("crate::__dep::six()"));
        assert!(!rendered.contains("::__dep::__dep"));
    }

    #[test]
    fn generic_arguments_have_independent_roots_without_breaking_the_outer_path() {
        let mut file: File = syn::parse_quote! {
            invoke!(
                crate::foo::<dep::Type>::dep::one(),
                $crate::foo::<dep::Type>::dep::two(),
                self::foo::<dep::Type>::dep::three(),
                super::foo::<dep::Type>::dep::four(),
            );
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);
        let self_mod = ident("__self");

        rewrite_macro_tokens(&mut file, &extern_map, Some(&self_mod));
        let rendered = compact(&file);
        assert!(rendered.contains("crate::__self::foo::<crate::__dep::Type>::dep::one()"));
        assert!(rendered.contains("$crate::__self::foo::<crate::__dep::Type>::dep::two()"));
        assert!(rendered.contains("self::foo::<crate::__dep::Type>::dep::three()"));
        assert!(rendered.contains("super::foo::<crate::__dep::Type>::dep::four()"));
    }

    #[test]
    fn absolute_use_group_drops_its_incompatible_leading_colons() {
        let mut file: File = syn::parse_quote! {
            quote_like!(use ::{dep::item, skipped::item};);
        };
        let extern_map = HashMap::from([("dep".to_owned(), ident("__dep"))]);

        rewrite_macro_tokens(&mut file, &extern_map, None);
        let rendered = compact(&file);
        assert!(rendered.contains("use{crate::__dep::item,skipped::item}"));
        assert!(!rendered.contains("use::{"));
    }
}
