use super::candidate::{
    Candidate, ItemKind, foreign_candidate, impl_candidate, item_candidate, span_tuple,
};
use crate::verify;
use std::collections::HashSet;
use syn::{Attribute, File, Item, Meta, UseTree};

/// Remove code which is unconditionally absent from a normal binary build.
///
/// rustc cannot report these items as dead because `cfg(test)` removes them
/// before linting. The exact predicate is deliberately narrow: compound cfg
/// expressions and namespaced test attributes are left untouched.
pub(crate) fn strip_test_items(items: &mut Vec<Item>) -> usize {
    let mut removed = 0;
    let mut kept = Vec::with_capacity(items.len());

    for mut item in items.drain(..) {
        if item_attrs_for_test(&item).is_some_and(is_test_only) {
            removed += 1;
            continue;
        }

        if let Item::Mod(module) = &mut item
            && let Some((_, nested)) = &mut module.content
        {
            removed += strip_test_items(nested);
        }
        kept.push(item);
    }

    *items = kept;
    removed
}

fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        let Meta::List(meta) = &attr.meta else {
            return false;
        };
        if !meta.path.is_ident("cfg") {
            return false;
        }
        syn::parse2::<Meta>(meta.tokens.clone())
            .is_ok_and(|meta| matches!(meta, Meta::Path(path) if path.is_ident("test")))
    })
}

fn item_attrs_for_test(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::ExternCrate(item) => Some(&item.attrs),
        Item::Fn(item) => Some(&item.attrs),
        Item::ForeignMod(item) => Some(&item.attrs),
        Item::Impl(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        Item::Mod(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Trait(item) => Some(&item.attrs),
        Item::TraitAlias(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Union(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        Item::Verbatim(_) => None,
        _ => None,
    }
}

pub(crate) fn item_is_removable(items: &[Item], candidate: &Candidate) -> bool {
    items.iter().any(|item| {
        item_candidate(item).as_ref() == Some(candidate) && item_is_safe_to_remove(item)
            || matches!(item, Item::Mod(module) if module.content.as_ref().is_some_and(|(_, nested)| item_is_removable(nested, candidate)))
            || matches!(item, Item::Impl(implementation) if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) && implementation.items.iter().any(|member| impl_candidate(member).as_ref() == Some(candidate) && impl_item_is_safe(member)))
            || matches!(item, Item::ForeignMod(block) if attrs_are_safe(&block.attrs) && block.items.iter().any(|member| foreign_candidate(member).as_ref() == Some(candidate) && foreign_item_is_safe(member)))
            || matches!(item, Item::Use(item) if attrs_are_safe(&item.attrs) && use_contains(&item.tree, candidate))
    })
}

fn use_contains(tree: &UseTree, candidate: &Candidate) -> bool {
    let span = span_tuple(tree);
    match candidate.kind {
        ItemKind::UseItem => span == candidate.byte_range,
        ItemKind::UsePath => {
            span == candidate.byte_range
                || match tree {
                    UseTree::Path(path) => use_contains(&path.tree, candidate),
                    UseTree::Group(group) => group
                        .items
                        .iter()
                        .any(|child| use_contains(child, candidate)),
                    _ => false,
                }
        }
        _ => false,
    }
}

pub(crate) fn remove_candidates(file: &mut File, candidates: &[Candidate]) -> usize {
    let wanted = candidates.iter().collect::<HashSet<&Candidate>>();
    remove_items(&mut file.items, &wanted)
}

fn remove_items(items: &mut Vec<Item>, wanted: &HashSet<&Candidate>) -> usize {
    let mut removed = 0;
    let mut kept = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        if is_wanted(&item, wanted) {
            removed += 1;
            continue;
        }
        match &mut item {
            Item::Mod(module) => {
                if let Some((_, nested)) = &mut module.content {
                    removed += remove_items(nested, wanted);
                }
            }
            Item::Impl(implementation)
                if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) =>
            {
                let before = implementation.items.len();
                implementation.items.retain(|member| {
                    !impl_candidate(member).is_some_and(|candidate| wanted.contains(&candidate))
                        || !impl_item_is_safe(member)
                });
                removed += before - implementation.items.len();
                if implementation.items.is_empty() {
                    continue;
                }
            }
            Item::Use(item) if attrs_are_safe(&item.attrs) => {
                let candidate = Candidate {
                    kind: ItemKind::UseItem,
                    name: verify::use_tree_text(&item.tree),
                    byte_range: span_tuple(&item.tree),
                };
                if wanted.contains(&candidate) {
                    removed += 1;
                    continue;
                }
                if prune_use_tree(&mut item.tree, wanted) {
                    removed += 1;
                }
                if use_tree_is_exhausted(&item.tree) {
                    continue;
                }
            }
            Item::ForeignMod(block) if attrs_are_safe(&block.attrs) => {
                let before = block.items.len();
                block.items.retain(|member| {
                    !foreign_candidate(member).is_some_and(|candidate| wanted.contains(&candidate))
                        || !foreign_item_is_safe(member)
                });
                removed += before - block.items.len();
                if block.items.is_empty() {
                    continue;
                }
            }
            _ => {}
        }
        kept.push(item);
    }
    *items = kept;
    removed
}

/// Whether the whole item is one of the candidates to take out.
fn is_wanted(item: &Item, wanted: &HashSet<&Candidate>) -> bool {
    if let Item::Impl(implementation) = item
        && attrs_are_safe(&implementation.attrs)
        && impl_block_is_wanted(wanted, span_tuple(implementation))
    {
        return true;
    }
    item_candidate(item)
        .is_some_and(|candidate| wanted.contains(&candidate) && item_is_safe_to_remove(item))
}

/// Whether a candidate in this batch is the impl block that sits at `range`.
pub(crate) fn impl_block_is_wanted(
    candidates: &HashSet<&Candidate>,
    range: (usize, usize),
) -> bool {
    candidates
        .iter()
        .any(|candidate| candidate.kind == ItemKind::ImplBlock && candidate.byte_range == range)
}

pub(crate) fn path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

pub(crate) fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}::{name}")
    }
}

/// Drop the import paths rustc called dead from a use tree.
fn prune_use_tree(tree: &mut UseTree, wanted: &HashSet<&Candidate>) -> bool {
    let mut removed = false;
    match tree {
        UseTree::Path(path) => removed |= prune_use_tree(&mut path.tree, wanted),
        UseTree::Group(group) => {
            let before = group.items.len();
            let mut kept = Vec::with_capacity(before);
            for mut child in std::mem::take(&mut group.items) {
                let candidate = Candidate {
                    kind: ItemKind::UsePath,
                    name: verify::use_tree_text(&child),
                    byte_range: span_tuple(&child),
                };
                if wanted.contains(&candidate) {
                    continue;
                }
                prune_use_tree(&mut child, wanted);
                if use_tree_is_exhausted(&child) {
                    continue;
                }
                kept.push(child);
            }
            for child in kept {
                group.items.push(child);
            }
            removed |= group.items.len() != before;
        }
        UseTree::Name(_) | UseTree::Rename(_) | UseTree::Glob(_) => {}
    }
    removed
}

/// Whether a use tree has no import left, so its item has to go as well.
fn use_tree_is_exhausted(tree: &UseTree) -> bool {
    match tree {
        UseTree::Path(path) => use_tree_is_exhausted(&path.tree),
        UseTree::Group(group) => group.items.is_empty(),
        UseTree::Name(_) | UseTree::Rename(_) | UseTree::Glob(_) => false,
    }
}

fn impl_item_is_safe(item: &syn::ImplItem) -> bool {
    match item {
        syn::ImplItem::Fn(item) => attrs_are_safe(&item.attrs),
        syn::ImplItem::Const(item) => attrs_are_safe(&item.attrs),
        _ => false,
    }
}

fn foreign_item_is_safe(item: &syn::ForeignItem) -> bool {
    match item {
        syn::ForeignItem::Fn(item) => attrs_are_safe(&item.attrs),
        syn::ForeignItem::Static(item) => attrs_are_safe(&item.attrs),
        syn::ForeignItem::Type(item) => attrs_are_safe(&item.attrs),
        _ => false,
    }
}

pub(crate) fn item_is_safe_to_remove(item: &Item) -> bool {
    let attrs = match item {
        Item::Fn(item) => &item.attrs,
        Item::Const(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::Mod(item) => {
            let Some((_, nested)) = &item.content else {
                return false;
            };
            if !module_contents_are_safe(nested) {
                return false;
            }
            &item.attrs
        }
        Item::Macro(item) => &item.attrs,
        _ => return false,
    };

    attrs_are_safe(attrs)
}

fn module_contents_are_safe(items: &[Item]) -> bool {
    items.iter().all(|item| match item {
        Item::Fn(_)
        | Item::Const(_)
        | Item::Static(_)
        | Item::Struct(_)
        | Item::Enum(_)
        | Item::Type(_)
        | Item::Use(_)
        | Item::Macro(_) => item_attrs(item).is_none_or(attrs_are_safe),
        Item::Mod(module) => {
            attrs_are_safe(&module.attrs)
                && module
                    .content
                    .as_ref()
                    .is_some_and(|(_, items)| module_contents_are_safe(items))
        }
        _ => false,
    })
}

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Fn(item) => Some(&item.attrs),
        Item::Const(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        _ => None,
    }
}

/// Whether every attribute of an item is inert once the item itself is gone.
///
/// Lint levels, documentation, optimisation hints, debugging attributes and
/// derives do not change what the program does: a derive expands to code for
/// the item it is written on, and that code goes with the item. Attributes that
/// reach the ABI or the linker are deliberately absent (`#[no_mangle]`,
/// `#[export_name]`, `#[link_section]`, `#[used]`): an item carrying one of
/// those can be needed by something that never names it.
pub(crate) fn attrs_are_safe(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        let Some(ident) = attr.path().get_ident() else {
            return false;
        };
        matches!(
            ident.to_string().as_str(),
            "allow"
                | "warn"
                | "deny"
                | "forbid"
                | "cfg"
                | "doc"
                | "inline"
                | "cold"
                | "must_use"
                | "deprecated"
                | "repr"
                | "non_exhaustive"
                | "track_caller"
                | "automatically_derived"
                | "derive"
        )
    })
}
