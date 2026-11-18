use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let id = NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("splicedown-inline-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn module<'a>(file: &'a File, name: &str) -> &'a ItemMod {
    file.items
        .iter()
        .find_map(|item| match item {
            Item::Mod(item_mod) if item_mod.ident == name => Some(item_mod),
            _ => None,
        })
        .unwrap_or_else(|| panic!("module `{name}` not found"))
}

fn nested_module<'a>(item_mod: &'a ItemMod, name: &str) -> &'a ItemMod {
    let (_, items) = item_mod
        .content
        .as_ref()
        .unwrap_or_else(|| panic!("module `{}` has no content", item_mod.ident));
    items
        .iter()
        .find_map(|item| match item {
            Item::Mod(item_mod) if item_mod.ident == name => Some(item_mod),
            _ => None,
        })
        .unwrap_or_else(|| panic!("nested module `{name}` not found"))
}

#[test]
fn expands_flat_nested_and_inline_modules_with_their_own_directories() {
    let fixture = Fixture::new();
    let root = fixture.write(
        "src/main.rs",
        r#"#![allow(dead_code)]
#[cfg(any())]
mod flat;

mod inline {
    #![allow(unused)]
    mod nested;
}
"#,
    );
    fixture.write(
        "src/flat.rs",
        r#"#![allow(unused_imports)]
pub mod deep;
"#,
    );
    fixture.write("src/flat/deep.rs", "pub const DEEP: i32 = 1;\n");
    fixture.write("src/inline/nested.rs", "pub const NESTED: i32 = 2;\n");

    let file = load_and_inline(&root).unwrap();
    assert_eq!(file.attrs.len(), 1);

    let flat = module(&file, "flat");
    assert!(flat.semi.is_none());
    assert!(flat.content.is_some());
    assert!(flat.attrs.iter().any(|attr| attr.path().is_ident("cfg")));
    assert!(flat.attrs.iter().any(
        |attr| attr.path().is_ident("allow") && matches!(attr.style, syn::AttrStyle::Inner(_))
    ));
    assert!(nested_module(flat, "deep").content.is_some());

    let inline = module(&file, "inline");
    assert!(nested_module(inline, "nested").content.is_some());
}

#[test]
fn expands_directory_module_layout() {
    let fixture = Fixture::new();
    let root = fixture.write("src/lib.rs", "mod foo;\n");
    fixture.write("src/foo/mod.rs", "mod bar;\n");
    fixture.write("src/foo/bar.rs", "pub const VALUE: i32 = 3;\n");

    let file = load_and_inline(&root).unwrap();
    let foo = module(&file, "foo");
    assert!(nested_module(foo, "bar").content.is_some());
}

#[test]
fn rejects_ambiguous_and_missing_module_files() {
    let fixture = Fixture::new();
    let ambiguous = fixture.write("ambiguous.rs", "mod foo;\n");
    fixture.write("foo.rs", "");
    fixture.write("foo/mod.rs", "");
    let error = load_and_inline(&ambiguous)
        .expect_err("ambiguous module should fail")
        .to_string();
    assert!(error.contains("multiple source files"), "{error}");
    assert!(error.contains("foo.rs"), "{error}");
    assert!(error.contains("foo/mod.rs"), "{error}");

    let missing = fixture.write("missing.rs", "mod absent;\n");
    let error = load_and_inline(&missing)
        .expect_err("missing module should fail")
        .to_string();
    assert!(error.contains("no source file"), "{error}");
    assert!(error.contains("absent.rs"), "{error}");
    assert!(error.contains("absent/mod.rs"), "{error}");
}

#[test]
fn rejects_path_attribute_with_context() {
    let fixture = Fixture::new();
    let root = fixture.write("src/lib.rs", "#[path = \"elsewhere.rs\"] mod foo;\n");
    let error = load_and_inline(&root)
        .expect_err("path attribute should fail")
        .to_string();
    assert!(error.contains("unsupported #[path]"), "{error}");
    assert!(error.contains("foo"), "{error}");
    assert!(error.contains("lib.rs"), "{error}");
}

#[test]
fn reports_read_and_parse_context() {
    let fixture = Fixture::new();
    let missing = fixture.root.join("does-not-exist.rs");
    let error = load_and_inline(&missing)
        .expect_err("missing source should fail")
        .to_string();
    assert!(error.contains("failed to read Rust source"), "{error}");
    assert!(error.contains("does-not-exist.rs"), "{error}");

    let invalid = fixture.write("invalid.rs", "mod {");
    let error = load_and_inline(&invalid)
        .expect_err("invalid source should fail")
        .to_string();
    assert!(error.contains("failed to parse Rust source"), "{error}");
    assert!(error.contains("invalid.rs"), "{error}");
}
