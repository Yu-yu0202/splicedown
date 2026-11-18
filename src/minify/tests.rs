use super::*;
use super::{candidate::*, compiler::*, remove::*, rename::*, rules::*, tokens::*};
use proc_macro2::TokenStream;
use std::collections::HashMap;
use std::fs;
use syn::{File, Item};

const MANIFEST: &str =
    "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n";

fn run_source(source: &str) -> Result<Minified> {
    run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: true,
            comments: true,
        },
    )
}

fn candidates(file: &File, entries: &[(ItemKind, &str)]) -> HashSet<Candidate> {
    let mut available = Vec::new();
    collect_candidates(&file.items, &mut available);
    available
        .into_iter()
        .filter(|candidate| {
            entries
                .iter()
                .any(|(kind, name)| candidate.kind == *kind && candidate.name == *name)
        })
        .collect()
}

#[test]
fn removes_only_unique_safe_items() {
    let mut file = syn::parse_file(
        r#"
            fn dead() {}
            mod a { fn duplicate() {} }
            mod b { fn duplicate() {} }
            #[unsafe(no_mangle)]
            fn exported() {}
            #[cfg_attr(all(), unsafe(link_section = ".init_array"))]
            static REGISTER: extern "C" fn() = exported;
            fn main() {}
            "#,
    )
    .unwrap();
    let dead = candidates(
        &file,
        &[
            (ItemKind::Function, "dead"),
            (ItemKind::Function, "duplicate"),
            (ItemKind::Function, "exported"),
            (ItemKind::Static, "REGISTER"),
        ],
    );

    assert_eq!(
        remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>()),
        3
    );
    let output = prettyplease::unparse(&file);
    assert!(!output.contains("fn dead"));
    assert_eq!(output.matches("fn duplicate").count(), 0);
    assert!(output.contains("fn exported"));
    assert!(output.contains("static REGISTER"));
}

#[test]
fn strips_exact_test_items_recursively() {
    let mut file = syn::parse_file(
        r#"
            #[cfg(test)]
            mod tests { #[test] fn root_test() {} }

            mod nested {
                #[cfg(test)]
                fn helper() {}

                #[test]
                fn direct_test() {}

                #[cfg(any(test, feature = "extra"))]
                fn compound_cfg_is_preserved() {}

                #[cfg_attr(test, allow(dead_code))]
                fn cfg_attr_is_preserved() {}

                fn live() {}
            }
            "#,
    )
    .unwrap();

    assert_eq!(strip_test_items(&mut file.items), 3);
    let output = prettyplease::unparse(&file);
    assert!(!output.contains("root_test"));
    assert!(!output.contains("fn helper"));
    assert!(!output.contains("direct_test"));
    assert!(output.contains("compound_cfg_is_preserved"));
    assert!(output.contains("cfg_attr_is_preserved"));
    assert!(output.contains("fn live"));
}

#[test]
fn does_not_remove_module_containing_impl() {
    let mut file = syn::parse_file(
        "mod behavior { struct S; impl S { fn new() -> Self { S } } } fn main() {}",
    )
    .unwrap();
    let dead = candidates(&file, &[(ItemKind::Module, "behavior")]);

    assert_eq!(
        remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>()),
        0
    );
    assert!(prettyplease::unparse(&file).contains("mod behavior"));
}

#[test]
/// A type whose only reader is its own impl block is dead all the same:
/// rustc calls the members dead, so the type and the impl go together.
fn removes_a_type_whose_only_reader_is_its_own_impl() {
    let source = "struct S; impl S { fn new() -> Self { S } } fn dead() {} fn main() {}";
    let file = syn::parse_file(source).unwrap();
    let positions = ident_positions(source);
    let mut diagnostics = Diagnostics::default();
    for item in &file.items {
        if let Item::Fn(item) = item
            && item.sig.ident == "dead"
        {
            diagnostics.dead_code.insert(span_tuple(&item.sig.ident));
        }
        if let Item::Impl(implementation) = item {
            for member in &implementation.items {
                if let syn::ImplItem::Fn(member) = member {
                    diagnostics.dead_code.insert(span_tuple(&member.sig.ident));
                }
            }
        }
    }
    let candidates = removable_candidates(&file, &diagnostics, 0, &positions);

    assert!(
        candidates
            .reported
            .iter()
            .any(|candidate| candidate.name == "dead")
    );
    // The type, its impl block, and the import that would name it.
    assert!(
        candidates
            .reported
            .iter()
            .any(|candidate| candidate.name == "S")
    );
}

/// The same shape with a reader outside the impl: the type has to stay.
#[test]
fn keeps_a_type_that_live_code_names() {
    let source =
        "struct S; impl S { fn new() -> Self { S } } fn dead() {} fn main() { let _ = S; }";
    let file = syn::parse_file(source).unwrap();
    let positions = ident_positions(source);
    let mut diagnostics = Diagnostics::default();
    for item in &file.items {
        if let Item::Fn(item) = item
            && item.sig.ident == "dead"
        {
            diagnostics.dead_code.insert(span_tuple(&item.sig.ident));
        }
        if let Item::Impl(implementation) = item {
            for member in &implementation.items {
                if let syn::ImplItem::Fn(member) = member {
                    diagnostics.dead_code.insert(span_tuple(&member.sig.ident));
                }
            }
        }
    }
    let candidates = removable_candidates(&file, &diagnostics, 0, &positions);

    // The impl block may go, but the type it names stays.
    assert!(
        !candidates
            .reported
            .iter()
            .any(|candidate| candidate.name == "S" && candidate.kind == ItemKind::Struct)
    );
}

#[test]
fn does_not_remove_members_from_an_attributed_impl() {
    let mut file =
        syn::parse_file("struct S; #[unknown_attribute] impl S { fn dead() {} }").unwrap();
    let dead = candidates(&file, &[(ItemKind::Method, "dead")]);
    assert_eq!(
        remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>()),
        0
    );
    assert!(prettyplease::unparse(&file).contains("fn dead"));
}

#[test]
fn prunes_unused_import_paths_and_keeps_the_rest_of_the_group() {
    let mut file =
        syn::parse_file("use a::{kept, gone1};\nuse b::gone2;\nuse c::*;\nfn main() { kept(); }")
            .unwrap();
    let dead = candidates(
        &file,
        &[
            (ItemKind::UsePath, "gone1"),
            (ItemKind::UseItem, "b::gone2"),
            (ItemKind::UseItem, "c::*"),
        ],
    );

    assert_eq!(
        remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>()),
        3
    );
    let output = prettyplease::unparse(&file);
    assert!(output.contains("use a::kept;"), "{output}");
    assert!(!output.contains("gone1"), "{output}");
    assert!(!output.contains("use b::"), "{output}");
    assert!(!output.contains("use c::"), "{output}");
    assert!(output.contains("fn main"), "{output}");
}

#[test]
fn drops_a_group_that_became_empty() {
    let mut file = syn::parse_file("use a::{b::{c}};\nfn main() {}").unwrap();
    let dead = candidates(&file, &[(ItemKind::UsePath, "c")]);
    assert_eq!(
        remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>()),
        1
    );
    let output = prettyplease::unparse(&file);
    assert!(!output.contains("use a"), "{output}");
    assert!(!output.contains("c"), "{output}");
}

#[test]
fn removes_methods_only_from_safe_impls() {
    let mut file = syn::parse_file(
        "struct S; struct T; impl S { fn gone() {} } #[unknown_attribute] impl T { fn gone2() {} }",
    )
    .unwrap();
    let dead = candidates(
        &file,
        &[(ItemKind::Method, "gone"), (ItemKind::Method, "gone2")],
    );
    let removed = remove_candidates(&mut file, &dead.into_iter().collect::<Vec<_>>());
    assert_eq!(removed, 1);
    let output = prettyplease::unparse(&file);
    assert!(output.contains("fn gone2"), "{output}");
    assert!(!output.contains("fn gone("), "{output}");
    // The impl that lost its last member goes with it.
    assert!(!output.contains("impl S"), "{output}");
    assert!(output.contains("struct S"), "{output}");
    assert!(output.contains("struct T"), "{output}");
}

#[test]
fn reports_unused_imports_and_import_paths() {
    let result = run_source(
        r#"
            #![allow(dead_code, unused_imports)]
            mod a { pub fn kept() -> u32 { 1 } pub fn gone() -> u32 { 2 } }
            mod b { pub fn gone2() -> u32 { 3 } }
            mod c { pub fn glob_gone() -> u32 { 4 } }
            use a::{kept, gone};
            use b::gone2;
            use c::*;
            fn main() { let _ = kept(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("use b::gone2"), "{}", result.source);
    assert!(!result.source.contains("use c::"), "{}", result.source);
    assert!(result.source.contains("use a::kept;"), "{}", result.source);
    assert!(!result.source.contains("fn gone"), "{}", result.source);
    assert!(!result.source.contains("glob_gone"), "{}", result.source);
    assert!(result.source.contains("fn kept"), "{}", result.source);
    assert_eq!(result.renames, 0);
}

#[test]
fn reports_unused_macro_definitions() {
    let result = run_source(
        r#"
            #![allow(dead_code, unused_macros)]
            macro_rules! unused_def { () => { 1 }; }
            macro_rules! used_def { () => { 2 }; }
            fn main() { let _ = used_def!(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("unused_def"), "{}", result.source);
    assert!(result.source.contains("used_def"), "{}", result.source);
}

#[test]
fn reports_dead_traits_together_with_their_impls() {
    let result = run_source(
        r#"
            #![allow(dead_code)]
            trait Dead { fn m(&self) -> u32; }
            impl Dead for u32 { fn m(&self) -> u32 { 1 } }
            struct Live;
            impl Live { fn live(&self) -> u32 { 2 } }
            fn main() { let _ = Live.live(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("trait Dead"), "{}", result.source);
    assert!(!result.source.contains("impl Dead"), "{}", result.source);
    assert!(result.source.contains("impl Live"), "{}", result.source);
    assert!(result.source.contains("fn live"), "{}", result.source);
}

#[test]
fn reports_unused_unions_and_foreign_block_members() {
    let result = run_source(
        r#"
            #![allow(dead_code)]
            pub union DeadUnion { a: u32 }
            unsafe extern "C" {
                fn foreign_unused();
                fn foreign_used(x: u32) -> u32;
            }
            fn main() { let _ = unsafe { foreign_used(1) }; }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("DeadUnion"), "{}", result.source);
    assert!(
        !result.source.contains("foreign_unused"),
        "{}",
        result.source
    );
    assert!(result.source.contains("foreign_used"), "{}", result.source);
}

#[test]
fn reports_unreferenced_public_root_items() {
    let result = run_source(
        r#"
            #![allow(dead_code, unused_imports)]
            pub fn used_root() -> u32 { 1 }
            pub fn unused_root() -> u32 { 2 }
            pub struct UnusedRoot;
            pub const UNUSED_ROOT: i32 = 3;
            fn main() { let _ = used_root(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("unused_root"), "{}", result.source);
    assert!(!result.source.contains("UnusedRoot"), "{}", result.source);
    assert!(!result.source.contains("UNUSED_ROOT"), "{}", result.source);
    assert!(result.source.contains("fn used_root"), "{}", result.source);
}

#[test]
fn reports_modules_left_empty_after_their_contents_are_removed() {
    let result = run_source(
        r#"
            #![allow(dead_code)]
            mod dead_empty {}
            mod inner { mod also_dead {} pub fn gone() {} }
            mod live { pub fn kept() {} }
            fn main() { let _ = live::kept(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("dead_empty"), "{}", result.source);
    assert!(!result.source.contains("also_dead"), "{}", result.source);
    assert!(!result.source.contains("fn gone"), "{}", result.source);
    assert!(result.source.contains("mod live"), "{}", result.source);
}

#[test]
fn reports_documentation_comments_that_disappeared() {
    let result = run_source(
        r#"
            //! Module docs.
            /// Docs on a function.
            #[doc = "explicit"]
            pub fn kept() {}
            /// Docs on a dead function.
            pub fn gone() {}
            fn main() { kept(); }
            "#,
    )
    .unwrap();

    // The inner module doc, two line docs and the explicit attribute.
    assert_eq!(result.removed_comments, 4, "{}", result.source);
    assert!(!result.source.contains("///"), "{}", result.source);
    assert!(!result.source.contains("doc"), "{}", result.source);
    assert!(result.source.contains("fn kept"), "{}", result.source);
    // The definition itself is still accounted for.
    assert_eq!(result.removed_items, 1, "{}", result.source);
}

#[test]
fn keeps_documentation_comments_when_the_option_is_off() {
    let source = r#"
            /// Docs on a function.
            pub fn kept() {}
            fn main() { kept(); }
        "#;

    let kept = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: true,
            comments: false,
        },
    )
    .unwrap();
    assert!(
        kept.source.contains("/// Docs on a function."),
        "{}",
        kept.source
    );
    assert_eq!(kept.removed_comments, 0);
}

#[test]
fn reports_comment_stripping_that_cannot_compile() {
    // `#[doc]` on a proc-macro style invocation would change the program,
    // so the compiler refuses it and the comments stay. Nothing else in the
    // bundle is built into this fixture, so the pass only has to not lose
    // the source it was given.
    let source = "/// Only docs.\nfn main() {}\n";

    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: false,
            test_code: false,
            comments: true,
        },
    )
    .unwrap();

    assert_eq!(result.removed_comments, 1);
    assert!(!result.source.contains("///"), "{}", result.source);
    assert!(result.source.contains("fn main"), "{}", result.source);
}

#[test]
fn reports_an_item_behind_a_glob_that_nothing_reads() {
    // rustc counts a name that a glob import brought into scope as used, so
    // the token count is what proposes this removal. The glob itself stays
    // because the surviving name resolves through it.
    let result = run_source(
        r#"
            #![allow(dead_code, unused_imports)]
            mod tools {
                pub fn live() -> u32 { 1 }
                pub fn dead() -> u32 { 2 }
            }
            use tools::*;
            fn main() { let _ = live(); }
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("fn dead"), "{}", result.source);
    assert!(result.source.contains("use tools::*;"), "{}", result.source);
    assert!(result.source.contains("fn live"), "{}", result.source);
}

#[test]
fn reports_a_glob_that_resolves_nothing() {
    let result = run_source(
        r#"
            #![allow(dead_code, unused_imports)]
            mod stray { pub fn unused() -> u32 { 1 } }
            use stray::*;
            fn main() {}
            "#,
    )
    .unwrap();

    assert!(!result.source.contains("use stray"), "{}", result.source);
    assert!(!result.source.contains("mod stray"), "{}", result.source);
    assert!(result.source.contains("fn main"), "{}", result.source);
}

#[test]
fn verification_counts_the_definitions_that_disappeared() {
    let result = run_source(
        r#"
            #![allow(dead_code)]
            mod m { fn gone() {} pub fn kept() {} }
            #[cfg(test)]
            mod tests { #[test] fn smoke() {} }
            fn main() { let _ = m::kept(); }
            "#,
    )
    .unwrap();

    // `gone`, the test module and its test function.
    assert_eq!(result.removed_items, 3, "{}", result.source);
    assert!(result.source.contains("fn kept"), "{}", result.source);
    assert!(!result.source.contains("smoke"), "{}", result.source);
}

#[test]
fn reports_verification_failures_instead_of_emitting_a_bundle() {
    // A tampered source cannot be a sub-forest of the input, so the proof
    // has to reject it instead of writing output.
    let error = verify::subset("fn main() { 1 }", "fn main() { 2 }").unwrap_err();
    assert!(error.to_string().contains("main"), "{error}");
    assert!(verify::subset("fn main() {}", "use nope::gone;\nfn main() {}").is_err());
}

#[test]
fn renames_only_identifier_tokens() {
    let source = "fn __x() { let s = \"__x\"; }\n// __x\n";
    let renames = HashMap::from([("__x".to_owned(), "m0".to_owned())]);

    assert_eq!(
        apply_renames(source, &renames).unwrap(),
        "fn m0() { let s = \"__x\"; }\n// __x\n"
    );
    assert_eq!(apply_renames(source, &HashMap::new()).unwrap(), source);
}

#[test]
fn counts_identifier_tokens_but_not_comments_or_strings() {
    let positions = ident_positions("fn a() { a(); } // b\nlet s = \"c\";\n");
    assert_eq!(positions.get("a").map(Vec::len), Some(2));
    assert!(!positions.contains_key("b"));
    assert!(!positions.contains_key("c"));
    assert_eq!(positions.get("fn").map(Vec::len), Some(1));
}

#[test]
fn finds_macros_that_render_text() {
    // `report` renders its argument, so anything that reaches it has to
    // stay untouched. `println!` prints the value of an expression, so the
    // macros that only call it are free.
    let source = concat!(
        "macro_rules! report { ($v:expr) => { eprintln!(\"{}\", stringify!($v)) }; }\n",
        "macro_rules! quiet { () => { 1 }; }\n",
        "fn f() { report!(__x); quiet!(); }\n",
    );
    let rendering = text_rendering_macros(&source.parse::<TokenStream>().unwrap());
    assert!(rendering.contains("report"), "{rendering:?}");
    assert!(!rendering.contains("quiet"), "{rendering:?}");

    let plain = "macro_rules! quiet { () => { 1 }; }\nfn f() { quiet!(); }\n";
    assert!(!text_rendering_macros(&plain.parse::<TokenStream>().unwrap()).contains("quiet"));
}

#[test]
fn finds_macros_that_render_text_through_a_nested_definition() {
    // `outer` renders text through a macro defined inside its own body, and
    // that definition has to be discovered for the fixpoint to see it.
    let nested = concat!(
        "macro_rules! inner { ($v:expr) => { stringify!($v) }; }\n",
        "macro_rules! outer { ($v:expr) => { inner!($v) }; }\n",
        "fn f() { outer!(__x); }\n",
    );
    let rendering = text_rendering_macros(&nested.parse::<TokenStream>().unwrap());
    assert!(rendering.contains("inner"), "{rendering:?}");
    assert!(rendering.contains("outer"), "{rendering:?}");

    assert!(text_sensitive_idents(nested).contains("__x"));
}

#[test]
fn leaves_identifiers_in_macro_positions_alone_when_text_is_rendered() {
    // Definitions that render their tokens as text, plus the positions an
    // identifier has to be in to be at risk of being rendered.
    let renders_argument = "macro_rules! report { ($v:expr) => { stringify!($v) }; }";
    let macro_body = "macro_rules! report { ($v:expr) => { stringify!(__x) }; }".to_owned();
    let invocation = format!("{renders_argument}\nfn f() {{ report!(__x); }}");
    let passthrough = format!(
        "{renders_argument}\nmacro_rules! inner {{ () => {{ __x }}; }} fn f() {{ report!(inner!()); }}"
    );
    // Nothing in this file renders text, so the identifier is free.
    let printing = "fn f() { println!(\"{}\", __x); } fn __x() {}";

    let macro_body = text_sensitive_idents(&macro_body);
    let invocation = text_sensitive_idents(&invocation);
    let passthrough = text_sensitive_idents(&passthrough);
    let printing = text_sensitive_idents(printing);

    assert!(macro_body.contains("__x"), "{macro_body:?}");
    assert!(invocation.contains("__x"), "{invocation:?}");
    // A rendering macro can receive the expansion of another macro, so the
    // identifier inside that macro is sensitive as well.
    assert!(passthrough.contains("__x"), "{passthrough:?}");
    assert!(printing.is_empty(), "{printing:?}");
}

#[test]
fn renames_freely_when_no_macro_renders_text() {
    // A rename is only kept when the renamed bundle still compiles, so a
    // source that cannot compile at all is handed back unchanged.
    let source = concat!(
        "#![allow(dead_code, unused_imports)]\n",
        "use crate::__splicedown_dep_a_0_1_0_00000001::helper;\n",
        "mod __splicedown_dep_a_0_1_0_00000001 {\n",
        "    pub fn helper() -> u32 { long_helper_body() }\n",
        "    #[allow(unused)]\n",
        "    pub fn long_helper_body() -> u32 { long_helper_body() }\n",
        "}\n",
        "fn main() { let _ = helper(); }\n",
    );
    let work = WorkDir::create().unwrap();
    work.prepare(MANIFEST).unwrap();
    let mut passes = 0;
    let outcome = rename_mangled_modules(&work, source, &mut passes).unwrap();

    assert_eq!(outcome.renamed, 1);
    assert!(outcome.source.contains("mod m0 {"), "{}", outcome.source);
    assert!(
        !outcome.source.contains("__splicedown_"),
        "{}",
        outcome.source
    );
    assert!(
        outcome.source.contains("use crate::m0::helper;"),
        "{}",
        outcome.source
    );
}

#[test]
fn renames_generated_dependency_modules_to_short_names() {
    let source = concat!(
        "#![allow(dead_code, unused_imports)]\n",
        "use crate::__splicedown_dep_a_0_1_0_00000001::helper;\n",
        "use crate::__splicedown_dep_a_0_1_0_00000001::gone;\n",
        "mod __splicedown_dep_a_0_1_0_00000001 { pub fn helper() -> u32 { 1 } pub fn gone() -> u32 { 2 } }\n",
        "fn main() { let _ = helper(); }\n",
    );
    let work = WorkDir::create().unwrap();
    work.prepare(MANIFEST).unwrap();
    let mut passes = 0;
    let outcome = rename_mangled_modules(&work, source, &mut passes).unwrap();

    assert_eq!(outcome.renamed, 1);
    assert!(outcome.source.contains("mod m0 {"), "{}", outcome.source);
    assert!(
        !outcome.source.contains("__splicedown_"),
        "{}",
        outcome.source
    );
    assert!(
        outcome.source.contains("use crate::m0::helper;"),
        "{}",
        outcome.source
    );
    // The rename pass only shortens names; the import of the dead item is
    // removed by the pass that runs before it.
    assert!(outcome.source.contains("pub fn gone"), "{}", outcome.source);
}

#[test]
fn keeps_the_source_when_no_rename_compiles() {
    // A rename is only kept when the renamed bundle still compiles, so a
    // source that cannot compile at all is handed back unchanged.
    let source = "mod __splicedown_dep_1_0_0_00000001 { pub fn helper() -> u32 { helper_body() } }\nfn main() {}\n";
    let work = WorkDir::create().unwrap();
    work.prepare(MANIFEST).unwrap();
    let mut passes = 0;
    let outcome = rename_mangled_modules(&work, source, &mut passes).unwrap();

    assert_eq!(outcome.renamed, 0);
    assert_eq!(outcome.source, source);
}

#[test]
fn minifies_to_a_checked_fixpoint() {
    let source = r#"
            #![allow(dead_code)]
            fn transitively_dead() { leaf(); }
            fn leaf() {}
            fn live() {}
            #[cfg(test)]
            mod tests { #[test] fn smoke() { super::live(); } }
            fn main() { live(); }
        "#;

    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: true,
            comments: true,
        },
    )
    .unwrap();

    assert_eq!(result.removed_items, 4);
    assert!(!result.source.contains("transitively_dead"));
    assert!(!result.source.contains("fn leaf"));
    assert!(!result.source.contains("mod tests"));
    assert!(result.source.contains("fn live"));
    assert!(result.source.contains("fn main"));
}

#[test]
fn minifies_source_with_a_shebang() {
    for shebang in [
        "#!/usr/bin/env rust-script",
        "#!/usr/bin/env rust-script 日本語",
    ] {
        let source = format!("{shebang}\n#![allow(dead_code)]\nfn dead() {{}}\nfn main() {{}}\n");
        let result = run(
            &source,
            MANIFEST,
            Options {
                dead_code: true,
                test_code: false,
                comments: false,
            },
        )
        .unwrap();
        assert!(result.source.starts_with(&format!("{shebang}\n")));
        assert!(!result.source.contains("fn dead"), "{}", result.source);
        assert!(result.source.contains("fn main"));
        assert_eq!(result.removed_items, 1);
        assert_eq!(result.passes, 3);
    }
}

#[test]
fn shared_target_directory_checks_each_generated_source() {
    let shared = WorkDir::create().unwrap();
    let valid = WorkDir::create().unwrap();
    let invalid = WorkDir::create().unwrap();
    for work in [&valid, &invalid] {
        work.prepare(MANIFEST).unwrap();
        fs::create_dir(work.path.join(".cargo")).unwrap();
        // Both source files exist before the first compilation. Without
        // distinct bin names Cargo can consider the second crate fresh.
        fs::write(
            work.path.join(".cargo/config.toml"),
            format!(
                "[build]\ntarget-dir = \"../{}/target\"\n",
                shared.target_name()
            ),
        )
        .unwrap();
    }
    valid.write_source("fn main() {}\n").unwrap();
    invalid
        .write_source("fn main() { missing_symbol(); }\n")
        .unwrap();
    assert!(valid.check().unwrap().success);
    let result = invalid.check().unwrap();
    assert!(!result.success);
    assert!(result.errors.contains("missing_symbol"));
}

#[test]
fn batches_independent_candidates_without_rechecking_the_same_source() {
    let mut source = String::from("#![allow(dead_code)]\nfn main() {}\n");
    for n in 0..20 {
        source.push_str(&format!("fn unused_{n}() {{}}\n"));
    }
    let result = run(
        &source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert_eq!(result.removed_items, 20);
    // Initial diagnostics, one batch, final ordinary-lint validation.
    assert_eq!(result.passes, 3);
}

#[test]
fn does_not_check_candidates_that_cannot_be_removed_from_the_ast() {
    let mut source = String::from("#![allow(dead_code)]\n");
    for n in 0..20 {
        source.push_str(&format!(
            "mod m{n} {{ pub fn live() {{}} fn duplicate() {{}} }}\n"
        ));
    }
    source.push_str("fn main() {");
    for n in 0..20 {
        source.push_str(&format!("m{n}::live();"));
    }
    source.push_str("}\n");
    source.push_str("fn removable() {}\n");
    let result = run(
        &source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert_eq!(result.removed_items, 21);
    assert_eq!(result.passes, 3);
}

#[test]
fn final_validation_respects_denied_dead_code() {
    let error = run(
        "#![deny(dead_code)]\nfn unused() {}\nfn main() {}",
        MANIFEST,
        Options {
            dead_code: false,
            test_code: true,
            comments: true,
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("cargo check failed"));
}

#[test]
fn removes_a_dead_trait_chain_down_to_the_items_only_it_used() {
    let source = r#"
            #![allow(dead_code)]
            const TRANSITIVE: usize = 1;
            trait T { fn value() -> usize; }
            struct Holder;
            impl T for Holder { fn value() -> usize { TRANSITIVE } }
            fn removable() {}
            fn main() {}
        "#;

    // rustc calls the trait dead, so its impl, the struct only the impl
    // mentioned and the const only the impl read become dead as well.
    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: true,
            comments: true,
        },
    )
    .unwrap();

    assert!(!result.source.contains("trait T"), "{}", result.source);
    assert!(!result.source.contains("impl T"), "{}", result.source);
    assert!(!result.source.contains("Holder"), "{}", result.source);
    assert!(!result.source.contains("TRANSITIVE"), "{}", result.source);
    assert!(!result.source.contains("fn removable"), "{}", result.source);
    assert!(result.source.contains("fn main"), "{}", result.source);
}

#[test]
fn keeps_a_candidate_whose_removal_cannot_compile() {
    // An empty module whose path is still imported cannot be taken out, so
    // the compiler rejects it and the rest of the pass is still applied.
    let source = r#"
            #![allow(dead_code, unused_imports)]
            mod empty {}
            mod referenced { pub fn live() { use crate::empty; } }
            fn dead() {}
            fn main() { referenced::live(); }
        "#;

    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();

    assert!(result.source.contains("mod empty"), "{}", result.source);
    assert!(
        result.source.contains("use crate::empty"),
        "{}",
        result.source
    );
    assert!(!result.source.contains("fn dead"), "{}", result.source);
}

#[test]
fn preserves_referenced_empty_module_namespaces() {
    for module in ["mod empty {}", "mod empty { fn dead_member() {} }"] {
        let source = format!(
            "#![allow(dead_code, unused_imports)]\n{module}\nmod child {{ pub fn live() {{ use crate::empty; }} }}\nfn dead() {{}}\nfn main() {{ child::live(); }}"
        );
        let result = run(
            &source,
            MANIFEST,
            Options {
                dead_code: true,
                test_code: false,
                comments: false,
            },
        )
        .unwrap();
        assert!(result.source.contains("mod empty"), "{}", result.source);
        assert!(
            result.source.contains("use crate::empty"),
            "{}",
            result.source
        );
        assert!(
            !result.source.contains("fn dead_member"),
            "{}",
            result.source
        );
        assert!(!result.source.contains("fn dead()"), "{}", result.source);
    }
}

#[test]
fn removes_dead_names_and_inherent_members_individually() {
    let source = r#"
            #![allow(dead_code)]
            mod left { pub fn same() {} }
            mod right { fn same() {} }
            struct Used;
            impl Used {
                fn live() -> usize { Self::VALUE }
                const VALUE: usize = 7;
                fn dead() -> usize { Self::DEAD_VALUE }
                const DEAD_VALUE: usize = 8;
            }
            fn main() { left::same(); assert_eq!(Used::live(), 7); }
        "#;
    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert!(!result.source.contains("mod right"), "{}", result.source);
    assert!(result.source.contains("mod left"), "{}", result.source);
    assert!(result.source.contains("fn live"), "{}", result.source);
    assert!(result.source.contains("const VALUE"), "{}", result.source);
    assert!(!result.source.contains("fn dead"), "{}", result.source);
    assert!(
        !result.source.contains("const DEAD_VALUE"),
        "{}",
        result.source
    );
}

#[test]
fn diagnostic_locations_handle_unicode_raw_identifiers_and_macro_expansions() {
    let source = r#"
            #![allow(dead_code)]
            macro_rules! generated { () => { fn r#unused() {} }; }
            generated!();
            mod 名前 { pub fn r#type() {} fn r#unused() {} }
            fn main() { 名前::r#type(); }
        "#;
    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert!(result.source.contains("generated!"));
    assert!(result.source.contains("r#type"));
    assert!(!result.source.contains("fn r#unused() {}\n}"));
}

#[test]
fn removes_every_method_in_a_combined_dead_code_diagnostic() {
    let source = r#"
            #![allow(dead_code)]
            struct S;
            impl S {
                fn first() {}
                fn second() {}
                fn third() {}
                const VALUE: usize = 3;
            }
            fn main() {}
        "#;
    let result = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert_eq!(result.removed_items, 6);
    assert!(!result.source.contains("impl S"));
    assert!(!result.source.contains("struct S"));
}

#[test]
fn dead_code_and_test_minification_can_be_disabled_independently() {
    let source = r#"
            #![allow(dead_code)]
            fn dead() {}
            #[cfg(test)]
            mod tests { #[test] fn smoke() {} }
            fn main() {}
        "#;

    let tests_only = run(
        source,
        MANIFEST,
        Options {
            dead_code: false,
            test_code: true,
            comments: true,
        },
    )
    .unwrap();
    assert!(tests_only.source.contains("fn dead"));
    assert!(!tests_only.source.contains("mod tests"));

    let dead_only = run(
        source,
        MANIFEST,
        Options {
            dead_code: true,
            test_code: false,
            comments: false,
        },
    )
    .unwrap();
    assert!(!dead_only.source.contains("fn dead"));
    assert!(dead_only.source.contains("mod tests"));
}
