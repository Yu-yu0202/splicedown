use super::*;

fn removed(source: &str, minified: &str) -> Result<Report> {
    subset(source, minified)
}

#[test]
fn reports_every_vanished_definition_and_nothing_else() {
    let original = r#"
            fn dead() {}
            mod m { pub fn kept() {} pub fn gone() {} }
            struct S;
            impl S { fn used() {} fn unused() {} }
            fn main() {}
        "#;
    let minified = r#"
            mod m { pub fn kept() {} }
            struct S;
            impl S { fn used() {} }
            fn main() {}
        "#;

    let report = removed(original, minified).unwrap();
    // The module, its child, the struct, the impl, the impl member and main.
    assert_eq!(report.kept, 6);
    let mut names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    names.sort();
    assert_eq!(
        names,
        ["function `dead`", "function `m::gone`", "method `unused`"],
        "{names:?}"
    );
}

#[test]
fn reports_modules_with_their_removed_contents() {
    let report = removed(
        "mod inner { fn a() {} mod deeper { fn b() {} } } fn main() {}",
        "fn main() {}",
    )
    .unwrap();
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "module `inner`",
            "function `inner::a`",
            "module `inner::deeper`",
            "function `inner::deeper::b`",
        ],
        "{names:?}"
    );
    assert_eq!(report.kept, 1);
}

#[test]
fn accepts_removed_import_paths_inside_a_group() {
    let original = "use a::{kept, gone1};\nuse b::gone2;\nuse c::*;\nfn main() { kept(); }";
    let minified = "use a::{\n    kept,\n};\nfn main() {\n    kept();\n}";
    let report = removed(original, minified).unwrap();
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["import `gone1`", "import `b::gone2`", "import `c::*`"],
        "{names:?}"
    );
}

#[test]
fn rejects_a_modified_surviving_definition() {
    let error = removed("fn main() { 1 }", "fn main() { 2 }").unwrap_err();
    assert!(error.to_string().contains("main"), "{error}");
    assert!(removed("fn main() {}", "use nope::gone;\nfn main() {}").is_err());
}

#[test]
fn rejects_an_import_that_never_existed() {
    let error = removed("fn main() {}", "use nope::a;\nfn main() {}").unwrap_err();
    assert!(error.to_string().contains("not a definition"), "{error}");
}

#[test]
fn rejects_reordered_definitions() {
    let error = removed("fn a() {} fn b() {}", "fn b() {} fn a() {}").unwrap_err();
    assert!(error.to_string().contains("not a definition"), "{error}");
}

#[test]
fn reports_removed_traits_impls_and_unions() {
    let original = "trait T { fn m(&self); }\nimpl T for u32 { fn m(&self) {} }\nunion U { a: u32 }\nfn main() {}";
    let minified = "fn main() {}";
    let report = removed(original, minified).unwrap();
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["trait `T`", "item `<unnamed>`", "method `m`", "union `U`"],
        "{names:?}"
    );
}

#[test]
fn reports_removed_macro_definitions_and_extern_block_members() {
    let original =
        "macro_rules! gone { () => { 1 }; }\nunsafe extern \"C\" { fn ffi(); }\nfn main() {}";
    let minified = "fn main() {}";
    let report = removed(original, minified).unwrap();
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "macro definition `gone`",
            "extern block `extern block`",
            "extern function `ffi`",
        ],
        "{names:?}"
    );
}

#[test]
fn reports_removed_use_items_as_single_units() {
    let report = removed("use a::b;\nfn main() {}", "fn main() {}").unwrap();
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["import `a::b`"], "{names:?}");
}

#[test]
fn compares_attributes_and_visibility_of_surviving_items() {
    let original = "#[inline]\npub fn kept() {} fn main() {}";
    let changed = "#[cold]\npub fn kept() {} fn main() {}";
    let unexported = "fn kept() {} fn main() {}";
    assert_eq!(removed(original, original).unwrap().kept, 2);
    assert!(removed(original, changed).is_err());
    assert!(removed(original, unexported).is_err());
}

#[test]
fn ignores_documentation_comments_on_both_sides() {
    let original = r#"
            //! Module docs.
            /// Kept for the judges.
            #[doc = "explicit"]
            pub fn kept() {}
            #[doc(hidden)]
            pub fn gone() {}
            fn main() { kept(); }
        "#;
    let minified = r#"
            //! Module docs.
            /// Kept for the judges.
            #[doc = "explicit"]
            pub fn kept() {}
            fn main() { kept(); }
        "#;

    // Removing a doc comment is not a modification, but every other
    // attribute still has to survive.
    let report = removed(original, minified).unwrap();
    assert_eq!(report.kept, 2);
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["function `gone`"], "{names:?}");
}

#[test]
fn reports_a_removed_attribute_that_is_not_a_documentation_comment() {
    // `#[inline]` is what the compiler reads, so losing it is a change.
    let original = "#[inline] fn kept() {}\nfn main() {}";
    let changed = "fn kept() {}\nfn main() {}";
    assert!(removed(original, changed).is_err());
}

#[test]
fn matches_the_second_of_two_containers_with_the_same_prelude() {
    // Minify drops the impl block that lost its last member, so the
    // surviving block has the same prelude as the removed one and has to be
    // matched against the later of the two.
    let original = "struct S;\nimpl S { fn gone() {} }\nimpl S { fn kept() {} }\nfn main() {}";
    let minified = "struct S;\nimpl S { fn kept() {} }\nfn main() {}";
    let report = removed(original, minified).unwrap();
    // The struct, the impl, its member and main.
    assert_eq!(report.kept, 4);
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(names, ["item `<unnamed>`", "method `gone`"], "{names:?}");
}

#[test]
fn matches_the_second_of_two_extern_blocks_with_the_same_abi() {
    let original =
        "unsafe extern \"C\" { fn gone(); }\nunsafe extern \"C\" { fn kept(); }\nfn main() {}";
    let minified = "unsafe extern \"C\" {\n    fn kept();\n}\nfn main() {}";
    let report = removed(original, minified).unwrap();
    // The extern block, its member and main.
    assert_eq!(report.kept, 3);
    let names = report
        .removed
        .iter()
        .map(|unit| unit.to_string())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["extern block `extern block`", "extern function `gone`"],
        "{names:?}"
    );
}

#[test]
fn refuses_a_container_member_that_was_modified_not_replaced() {
    // `b` is not in the first impl block, so the block is not the one the
    // output came from. `kept` is in both, but only unchanged.
    let original = "struct S;\nimpl S { fn kept() {} }\nimpl S { fn kept() {} }\nfn main() {}";
    let changed = "struct S;\nimpl S { fn kept() -> u32 { 1 } }\nfn main() {}";
    let report = removed(original, changed);
    assert!(report.is_err(), "{report:?}");
    assert!(
        report
            .unwrap_err()
            .to_string()
            .contains("modified the impl member `kept`")
    );
}

#[test]
fn reports_an_import_that_never_existed_even_behind_a_matching_prefix() {
    let error = removed(
        "use a::b;\nuse a::c;\nfn main() {}",
        "use a::c;\nfn main() {}",
    );
    assert!(error.is_ok(), "{:?}", error.err());
}
