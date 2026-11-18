use super::*;
use proc_macro2::Span;
use syn::parse_quote;

fn ident(name: &str) -> Ident {
    Ident::new(name, Span::call_site())
}

fn map(entries: &[(&str, &str)]) -> HashMap<String, Ident> {
    entries
        .iter()
        .map(|(name, mangled)| ((*name).to_owned(), ident(mangled)))
        .collect()
}

fn rewrite_text(source: &str, entries: &[(&str, &str)], self_mod: Option<&str>) -> String {
    let mut file = syn::parse_file(source).unwrap();
    let self_mod = self_mod.map(ident);
    rewrite(&mut file, &map(entries), self_mod.as_ref());
    prettyplease::unparse(&file)
}

#[test]
fn rewrites_external_and_dependency_crate_paths() {
    let output = rewrite_text(
        "fn f() { let _ = dep::value(); let _ = crate::local(); }",
        &[("dep", "__splicedown_dep")],
        Some("__splicedown_self"),
    );

    assert!(output.contains("crate::__splicedown_dep::value()"));
    assert!(output.contains("crate::__splicedown_self::local()"));
}

#[test]
fn rewrites_type_paths_and_keeps_self_type_paths() {
    let output = rewrite_text(
        "trait Trait { type Item; fn get() -> Self::Item; } type External = ::dep::Value;",
        &[("dep", "__splicedown_dep")],
        None,
    );

    assert!(output.contains("Self::Item"));
    assert!(output.contains("type External = crate::__splicedown_dep::Value;"));
}

#[test]
fn leaves_entry_crate_and_local_paths_untouched() {
    let output = rewrite_text(
        "fn f() { let _ = crate::local(); let _ = self::local(); let _ = super::local(); let _ = local; }",
        &[("dep", "__splicedown_dep")],
        None,
    );

    assert!(output.contains("crate::local()"));
    assert!(output.contains("self::local()"));
    assert!(output.contains("super::local()"));
    assert!(output.contains("let _ = local;"));
}

#[test]
fn rewrites_macro_invocation_path_but_not_macro_tokens() {
    let output = rewrite_text(
        "dep::call!(dep::inside(crate::value()));",
        &[("dep", "__splicedown_dep")],
        Some("__splicedown_self"),
    );

    assert!(output.contains("crate::__splicedown_dep::call!"));
    assert!(output.contains("dep::inside(crate ::value())"));
}

#[test]
fn rewrites_use_roots_and_preserves_nested_local_names() {
    let output = rewrite_text(
        "use dep::item; use dep::{self, value}; use {dep::other, local::{dep::nested}, other::value};",
        &[("dep", "__splicedown_dep"), ("other", "__splicedown_other")],
        None,
    );

    assert!(output.contains("use crate::__splicedown_dep::item;"));
    assert!(output.contains("use crate::__splicedown_dep::{self as dep, value};"));
    assert!(output.contains("crate::__splicedown_dep::other"));
    assert!(output.contains("local::dep::nested"));
    assert!(output.contains("crate::__splicedown_other::value"));
}

#[test]
fn rewrites_bare_use_roots_and_explicit_aliases() {
    let output = rewrite_text(
        "use dep; use dep as alias; use {dep, dep as another, skipped};",
        &[("dep", "__splicedown_dep")],
        None,
    );

    assert!(output.contains("use crate::__splicedown_dep as dep;"));
    assert!(output.contains("use crate::__splicedown_dep as alias;"));
    assert!(output.contains(
        "use {crate::__splicedown_dep as dep, crate::__splicedown_dep as another, skipped};"
    ));
}

#[test]
fn rewrites_dependency_self_alias() {
    let output = rewrite_text(
        "use crate as this_crate; fn value() { this_crate::local(); }",
        &[],
        Some("__splicedown_self"),
    );

    assert!(output.contains("use crate::__splicedown_self as this_crate;"));
    assert!(output.contains("this_crate::local()"));
}

#[test]
fn removes_leading_colon_only_when_a_use_root_is_rewritten() {
    let output = rewrite_text(
        "use ::dep::item; use ::unknown::item;",
        &[("dep", "__splicedown_dep")],
        None,
    );

    assert!(output.contains("use crate::__splicedown_dep::item;"));
    assert!(output.contains("use ::unknown::item;"));
}

#[test]
fn splits_absolute_root_groups_in_files_modules_and_blocks() {
    let output = rewrite_text(
        r#"
#[cfg(feature = "imports")]
pub use ::{{dep::item, skipped::item}};

mod nested {
    #[allow(unused_imports)]
    pub use ::{dep::nested, skipped::nested};

    fn f() {
        use ::{dep::block, skipped::block};
    }
}
"#,
        &[("dep", "__splicedown_dep")],
        None,
    );

    assert!(output.contains("pub use crate::__splicedown_dep::item;"));
    assert!(output.contains("pub use ::skipped::item;"));
    assert!(output.contains("pub use crate::__splicedown_dep::nested;"));
    assert!(output.contains("pub use ::skipped::nested;"));
    assert!(output.contains("use crate::__splicedown_dep::block;"));
    assert!(output.contains("use ::skipped::block;"));
}

#[test]
fn use_tree_construction_round_trips() {
    let mut file: File = parse_quote! {
        use dep::{self, item};
    };
    rewrite(&mut file, &map(&[("dep", "__splicedown_dep")]), None);
    let output = prettyplease::unparse(&file);
    syn::parse_file(&output).unwrap();
}
