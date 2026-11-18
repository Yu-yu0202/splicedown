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
mod tests;
