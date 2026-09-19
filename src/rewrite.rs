//! Rewrite paths after dependency crates have been placed in private modules.
//!
//! A bundled dependency is no longer available through the extern prelude.  A
//! reference such as `some_dep::item` must therefore become
//! `crate::__splicedown_some_dep::item`, while a `crate::item` reference in a
//! dependency must be redirected through that dependency's wrapper module.
//!
//! `syn::VisitMut` deliberately does not descend into macro token streams.  A
//! later phase handles those tokens; this phase only rewrites paths that syn
//! parsed as AST nodes.

use std::collections::HashMap;

use proc_macro2::Span;
use syn::visit_mut::{self, VisitMut};
use syn::{
    Block, File, Ident, Item, ItemMod, ItemUse, Path, PathArguments, PathSegment, Stmt, UseGroup,
    UsePath, UseRename, UseTree,
};

/// Rewrite parsed paths in `file`.
///
/// `extern_map` contains the direct dependencies visible from this source
/// file.  `self_mod` is the wrapper module used for a bundled dependency and
/// is `None` for the entry crate.
pub(crate) fn rewrite(
    file: &mut File,
    extern_map: &HashMap<String, Ident>,
    self_mod: Option<&Ident>,
) {
    let mut rewriter = PathRewriter {
        extern_map,
        self_mod,
    };
    rewriter.visit_file_mut(file);
}

struct PathRewriter<'a> {
    extern_map: &'a HashMap<String, Ident>,
    self_mod: Option<&'a Ident>,
}

impl PathRewriter<'_> {
    /// Rewrite one `use` tree.  `root` identifies a path prefix that starts a
    /// branch of the use declaration.  Once a prefix such as `local::` has
    /// been consumed, names below it are local to that prefix and must not be
    /// mistaken for another external crate.
    fn rewrite_use_tree(&mut self, tree: &mut UseTree, root: bool) -> bool {
        if !root {
            return false;
        }

        match tree {
            UseTree::Group(group) => {
                let mut changed = false;
                for item in &mut group.items {
                    changed |= self.rewrite_use_tree(item, true);
                }
                changed
            }
            UseTree::Path(path) => self.rewrite_use_path(path),
            UseTree::Name(name) => {
                let source = name.ident.clone();
                self.rewrite_use_leaf(tree, &source, None)
            }
            UseTree::Rename(rename) => {
                let source = rename.ident.clone();
                let alias = rename.rename.clone();
                self.rewrite_use_leaf(tree, &source, Some(alias))
            }
            UseTree::Glob(_) => false,
        }
    }

    fn rewrite_use_leaf(&self, tree: &mut UseTree, source: &Ident, alias: Option<Ident>) -> bool {
        let mangled = if is_ident(source, "crate") {
            // `use crate as alias` is a valid way for a crate to name itself.
            // A bare `use crate` has no usable binding name, so leave that
            // invalid form to rustc rather than manufacturing `as crate`.
            if alias.is_none() {
                return false;
            }
            let Some(self_mod) = self.self_mod else {
                return false;
            };
            self_mod
        } else {
            let Some(mangled) = self.extern_map.get(&source.to_string()) else {
                return false;
            };
            mangled
        };

        // A bare `use dep;` binds the dependency under `dep`; an explicit
        // `use dep as alias;` keeps the explicit alias. Once the crate has
        // moved under its wrapper module, both forms need an explicit rename
        // so the original binding remains available to the rest of the file.
        let rename = UseTree::Rename(UseRename {
            ident: mangled.clone(),
            as_token: Default::default(),
            rename: alias.unwrap_or_else(|| source.clone()),
        });
        *tree = prepend_use_path(rename, crate_ident(source.span()));
        true
    }

    fn rewrite_use_path(&mut self, path: &mut UsePath) -> bool {
        let root_ident = path.ident.clone();

        // `use crate::...` inside a dependency refers to that dependency's
        // original crate root.  The wrapper module is the corresponding root
        // after assembly.  The entry crate has no wrapper, so its `crate::`
        // uses stay untouched.
        if is_ident(&root_ident, "crate") {
            let Some(self_mod) = self.self_mod else {
                return false;
            };

            let child = take_use_tree(&mut path.tree);
            *path.tree = prepend_use_path(child, self_mod.clone());
            return true;
        }

        let Some(mangled) = self.extern_map.get(&root_ident.to_string()) else {
            return false;
        };

        // A use tree has no leading-colon field of its own.  Replacing the
        // root with `crate` is done by adding an explicit crate prefix and is
        // paired with clearing ItemUse::leading_colon by the caller.
        let child = take_use_tree(&mut path.tree);
        let child = alias_direct_self(child, &root_ident);
        let child = prepend_use_path(child, mangled.clone());
        path.ident = crate_ident(root_ident.span());
        *path.tree = child;
        true
    }

    fn rewrite_path(&mut self, path: &mut Path) {
        // Rewrite paths inside generic arguments before changing the outer
        // path.  The inserted prefix is not visited again in this traversal.
        visit_mut::visit_path_mut(self, path);

        if path.segments.len() < 2 {
            return;
        }

        let first = &path.segments[0];
        if !first.arguments.is_none() {
            return;
        }

        if is_ident(&first.ident, "crate") {
            let Some(self_mod) = self.self_mod else {
                return;
            };
            insert_after_first(path, self_mod.clone());
            path.leading_colon = None;
            return;
        }

        let Some(mangled) = self.extern_map.get(&first.ident.to_string()) else {
            return;
        };

        let span = first.ident.span();
        path.leading_colon = None;
        path.segments[0].ident = crate_ident(span);
        insert_after_first(path, mangled.clone());
    }
}

impl VisitMut for PathRewriter<'_> {
    fn visit_file_mut(&mut self, file: &mut File) {
        split_absolute_use_items(&mut file.items);
        visit_mut::visit_file_mut(self, file);
    }

    fn visit_item_mod_mut(&mut self, item: &mut ItemMod) {
        if let Some((_, items)) = &mut item.content {
            split_absolute_use_items(items);
        }
        visit_mut::visit_item_mod_mut(self, item);
    }

    fn visit_block_mut(&mut self, block: &mut Block) {
        split_absolute_use_stmts(&mut block.stmts);
        visit_mut::visit_block_mut(self, block);
    }

    fn visit_item_use_mut(&mut self, item: &mut syn::ItemUse) {
        // This is the same traversal as syn's default implementation, except
        // that UseTree needs root-aware handling of its prefixes.
        for attr in &mut item.attrs {
            self.visit_attribute_mut(attr);
        }
        self.visit_visibility_mut(&mut item.vis);

        let changed = self.rewrite_use_tree(&mut item.tree, true);
        if changed {
            item.leading_colon = None;
        }
    }

    fn visit_path_mut(&mut self, path: &mut Path) {
        self.rewrite_path(path);
    }
}

fn is_ident(ident: &Ident, expected: &str) -> bool {
    ident == expected
}

fn crate_ident(span: Span) -> Ident {
    Ident::new("crate", span)
}

fn take_use_tree(tree: &mut Box<UseTree>) -> UseTree {
    *std::mem::replace(
        tree,
        Box::new(UseTree::Glob(syn::UseGlob {
            star_token: Default::default(),
        })),
    )
}

fn prepend_use_path(tree: UseTree, ident: Ident) -> UseTree {
    UseTree::Path(UsePath {
        ident,
        colon2_token: Default::default(),
        tree: Box::new(tree),
    })
}

fn insert_after_first(path: &mut Path, ident: Ident) {
    let segment = PathSegment {
        ident,
        arguments: PathArguments::None,
    };
    path.segments.insert(1, segment);
}

/// Preserve the binding produced by `use foo::{self, item}` after `foo` has
/// become a wrapper module path.  The direct `self` branch is the only one
/// whose meaning is tied to the original root; nested groups refer to their
/// own prefixes and must remain unchanged.
fn alias_direct_self(tree: UseTree, root_ident: &Ident) -> UseTree {
    let UseTree::Group(mut group) = tree else {
        return tree;
    };

    for item in &mut group.items {
        let UseTree::Name(name) = item else {
            continue;
        };
        if !is_ident(&name.ident, "self") {
            continue;
        }

        let self_ident = name.ident.clone();
        *item = UseTree::Rename(UseRename {
            ident: self_ident,
            as_token: Default::default(),
            rename: root_ident.clone(),
        });
    }

    UseTree::Group(UseGroup {
        brace_token: group.brace_token,
        items: group.items,
    })
}

/// An `ItemUse` has one leading-colon token for the whole tree. If a root
/// group mixes bundled and skipped dependencies, rewriting only the bundled
/// branches would otherwise turn `::skipped::item` into the relative path
/// `skipped::item`. Split such a declaration into one item per root branch so
/// each branch can retain its own absolute marker.
fn split_absolute_use_items(items: &mut Vec<Item>) {
    let mut split = Vec::with_capacity(items.len());

    for item in std::mem::take(items) {
        let Some(use_item) = absolute_root_group(&item) else {
            split.push(item);
            continue;
        };

        for branch in use_group_items(use_item) {
            let mut branch_item = use_item.clone();
            branch_item.tree = branch;
            split.push(Item::Use(branch_item));
        }
    }

    *items = split;
}

fn split_absolute_use_stmts(stmts: &mut Vec<Stmt>) {
    let mut split = Vec::with_capacity(stmts.len());

    for stmt in std::mem::take(stmts) {
        let Stmt::Item(Item::Use(use_item)) = &stmt else {
            split.push(stmt);
            continue;
        };

        let Some(use_item) = absolute_root_group_from_use(use_item) else {
            split.push(stmt);
            continue;
        };

        for branch in use_group_items(use_item) {
            let mut branch_item = use_item.clone();
            branch_item.tree = branch;
            split.push(Stmt::Item(Item::Use(branch_item)));
        }
    }

    *stmts = split;
}

fn absolute_root_group(item: &Item) -> Option<&ItemUse> {
    let Item::Use(use_item) = item else {
        return None;
    };
    absolute_root_group_from_use(use_item)
}

fn absolute_root_group_from_use(item: &ItemUse) -> Option<&ItemUse> {
    if item.leading_colon.is_some() && matches!(item.tree, UseTree::Group(_)) {
        Some(item)
    } else {
        None
    }
}

fn use_group_items(item: &ItemUse) -> Vec<UseTree> {
    fn collect_branches(tree: &UseTree, branches: &mut Vec<UseTree>) {
        if let UseTree::Group(group) = tree {
            for item in &group.items {
                collect_branches(item, branches);
            }
        } else {
            branches.push(tree.clone());
        }
    }

    let mut branches = Vec::new();
    collect_branches(&item.tree, &mut branches);
    branches
}

#[cfg(test)]
mod tests {
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
}
