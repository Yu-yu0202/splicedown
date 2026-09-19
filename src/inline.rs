//! Expand Rust's out-of-line module declarations into an in-memory syntax tree.
//!
//! The directory used to resolve a module is deliberately carried separately
//! from the source file being parsed.  For a source file at `foo.rs` (or
//! `foo/mod.rs`), declarations inside that file are resolved below
//! `mod_dir/foo/`, which is the same rule Rust uses for both module file
//! layouts.

use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};
use syn::{File, Item, ItemMod, token};

/// Load `path`, parse it, and recursively inline all out-of-line modules.
pub(crate) fn load_and_inline(path: &Path) -> Result<File> {
    let mut file = parse_file(path)?;
    let mod_dir = path.parent().unwrap_or_else(|| Path::new("."));
    inline_mods(&mut file.items, mod_dir, path)?;
    Ok(file)
}

fn parse_file(path: &Path) -> Result<File> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("failed to read Rust source {}", path.display()))?;
    syn::parse_file(&source)
        .with_context(|| format!("failed to parse Rust source {}", path.display()))
}

fn inline_mods(items: &mut [Item], mod_dir: &Path, source_path: &Path) -> Result<()> {
    for item in items {
        let Item::Mod(item_mod) = item else {
            continue;
        };

        reject_path_attribute(item_mod, source_path)?;

        let child_mod_dir = mod_dir.join(module_name(&item_mod.ident));
        if let Some((_, nested_items)) = &mut item_mod.content {
            // An inline module still gets its own directory for declarations
            // below it: `mod foo { mod bar; }` resolves `foo/bar.rs`.
            inline_mods(nested_items, &child_mod_dir, source_path)?;
            continue;
        }

        let module_name = module_name(&item_mod.ident);
        let child_path = find_module_file(mod_dir, &module_name, source_path)?;
        let mut child_file = parse_file(&child_path).with_context(|| {
            format!(
                "while expanding module `{module_name}` declared in {}",
                source_path.display()
            )
        })?;

        inline_mods(&mut child_file.items, &child_mod_dir, &child_path).with_context(|| {
            format!(
                "while expanding module `{module_name}` declared in {}",
                source_path.display()
            )
        })?;

        // In syn, inner attributes from a file are stored in File::attrs. An
        // ItemMod stores both its outer and inner attributes in one vector;
        // extending the vector preserves the inner style and makes
        // pretty-printers place them inside the generated module body.
        item_mod.attrs.extend(child_file.attrs);
        item_mod.content = Some((token::Brace::default(), child_file.items));
        item_mod.semi = None;
    }

    Ok(())
}

fn reject_path_attribute(item_mod: &ItemMod, source_path: &Path) -> Result<()> {
    if item_mod
        .attrs
        .iter()
        .any(|attribute| attribute.path().is_ident("path"))
    {
        bail!(
            "unsupported #[path] attribute on module `{}` declared in {}",
            module_name(&item_mod.ident),
            source_path.display()
        );
    }

    Ok(())
}

fn find_module_file(mod_dir: &Path, name: &str, source_path: &Path) -> Result<PathBuf> {
    let flat = mod_dir.join(format!("{name}.rs"));
    let nested = mod_dir.join(name).join("mod.rs");

    match (flat.is_file(), nested.is_file()) {
        (true, false) => Ok(flat),
        (false, true) => Ok(nested),
        (true, true) => bail!(
            "module `{name}` declared in {} has multiple source files: {} and {}",
            source_path.display(),
            flat.display(),
            nested.display()
        ),
        (false, false) => bail!(
            "module `{name}` declared in {} has no source file (tried {} and {})",
            source_path.display(),
            flat.display(),
            nested.display()
        ),
    }
}

fn module_name(ident: &syn::Ident) -> String {
    // A raw identifier names the ordinary filesystem item (`mod r#type;`
    // looks for `type.rs`).  proc_macro2 normally prints raw identifiers with
    // the `r#` prefix, so remove it explicitly for path construction.
    let name = ident.to_string();
    name.strip_prefix("r#").unwrap_or(&name).to_owned()
}

#[cfg(test)]
mod tests {
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
        assert!(
            flat.attrs.iter().any(|attr| attr.path().is_ident("allow")
                && matches!(attr.style, syn::AttrStyle::Inner(_)))
        );
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
            .err()
            .expect("ambiguous module should fail")
            .to_string();
        assert!(error.contains("multiple source files"), "{error}");
        assert!(error.contains("foo.rs"), "{error}");
        assert!(error.contains("foo/mod.rs"), "{error}");

        let missing = fixture.write("missing.rs", "mod absent;\n");
        let error = load_and_inline(&missing)
            .err()
            .expect("missing module should fail")
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
            .err()
            .expect("path attribute should fail")
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
            .err()
            .expect("missing source should fail")
            .to_string();
        assert!(error.contains("failed to read Rust source"), "{error}");
        assert!(error.contains("does-not-exist.rs"), "{error}");

        let invalid = fixture.write("invalid.rs", "mod {");
        let error = load_and_inline(&invalid)
            .err()
            .expect("invalid source should fail")
            .to_string();
        assert!(error.contains("failed to parse Rust source"), "{error}");
        assert!(error.contains("invalid.rs"), "{error}");
    }
}
