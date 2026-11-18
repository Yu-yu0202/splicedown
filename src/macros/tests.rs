use super::{exports::fix_macro_exports, tokens::rewrite_macro_tokens};
use proc_macro2::{Ident, Span};
use std::collections::HashMap;
use syn::{File, Item};

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
