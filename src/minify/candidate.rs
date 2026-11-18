use super::remove::{attrs_are_safe, impl_block_is_wanted};
use crate::verify;
use std::collections::HashSet;
use std::ops::Range;
use syn::spanned::Spanned;
use syn::{Item, UseTree};

/// Spans of the definitions that are still there once a batch is applied, and
/// of the subtrees it takes out.
///
/// A module the batch removes takes everything inside it with it, so its whole
/// span belongs to the removed side. A `use` group that only loses some of its
/// paths keeps its span, because what survives inside it still names things.
/// Whether this batch takes the whole item out, impl blocks included.
///
/// Impl blocks are matched by their span, because two different rules propose
/// them: a dead trait names the trait, a dead type names the type.
pub(crate) fn takes_whole_item(item: &Item, candidates: &HashSet<&Candidate>) -> bool {
    if let Item::Impl(implementation) = item
        && attrs_are_safe(&implementation.attrs)
        && impl_block_is_wanted(candidates, span_tuple(implementation))
    {
        return true;
    }
    item_candidate(item).is_some_and(|candidate| {
        candidates.contains(&candidate) && candidate.kind != ItemKind::UsePath
    })
}

/// The identifiers that name a definition.
///
/// A name can be defined more than once in different places of a bundle, so an
/// occurrence that is itself a definition is not a reader of the other one.
pub(crate) fn definition_candidates(items: &[Item], out: &mut Vec<Candidate>) {
    for item in items {
        if let Some(candidate) = definition_name(item) {
            out.push(candidate);
        }
        match item {
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    definition_candidates(nested, out);
                }
            }
            Item::Impl(implementation) => {
                for member in &implementation.items {
                    if let Some((start, end)) = definition_member_name(member) {
                        out.push(Candidate {
                            kind: ItemKind::Method,
                            name: String::new(),
                            byte_range: (start, end),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

/// The identifier that names an item, when it has one.
pub(crate) fn definition_name(item: &Item) -> Option<Candidate> {
    Some(match item {
        Item::Fn(item) => ident_candidate(ItemKind::Function, &item.sig.ident),
        Item::Const(item) => ident_candidate(ItemKind::Constant, &item.ident),
        Item::Static(item) => ident_candidate(ItemKind::Static, &item.ident),
        Item::Struct(item) => ident_candidate(ItemKind::Struct, &item.ident),
        Item::Enum(item) => ident_candidate(ItemKind::Enum, &item.ident),
        Item::Union(item) => ident_candidate(ItemKind::Union, &item.ident),
        Item::Type(item) => ident_candidate(ItemKind::TypeAlias, &item.ident),
        Item::Mod(item) => ident_candidate(ItemKind::Module, &item.ident),
        Item::Trait(item) => ident_candidate(ItemKind::Trait, &item.ident),
        Item::TraitAlias(item) => ident_candidate(ItemKind::TypeAlias, &item.ident),
        _ => return None,
    })
}

/// The identifier that names a block member, when it has one.
fn definition_member_name(member: &syn::ImplItem) -> Option<(usize, usize)> {
    Some(match member {
        syn::ImplItem::Fn(item) => span_tuple(&item.sig.ident),
        syn::ImplItem::Const(item) => span_tuple(&item.ident),
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) enum ItemKind {
    Function,
    Constant,
    Static,
    Struct,
    Enum,
    Union,
    TypeAlias,
    Module,
    Method,
    AssociatedConstant,
    Trait,
    MacroDefinition,
    ForeignFunction,
    ForeignStatic,
    ForeignType,
    ImplBlock,
    UseItem,
    UsePath,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Candidate {
    pub(crate) kind: ItemKind,
    pub(crate) name: String,
    pub(crate) byte_range: (usize, usize),
}

/// Candidates for one pass, split by how much the compiler already knows.
///
/// `reported` are the definitions a forced lint called out, so removing all of
/// them at once is what the compiler already proved safe. `speculative` are
/// the ones only the check crate can judge, so they are tried on their own
/// and split apart when they fail instead of taking the reported ones with
/// them.
#[derive(Debug, Default)]
pub(crate) struct Candidates {
    pub(crate) reported: HashSet<Candidate>,
    pub(crate) speculative: HashSet<Candidate>,
}

pub(crate) fn collect_candidates(items: &[Item], out: &mut Vec<Candidate>) {
    for item in items {
        if let Some(candidate) = item_candidate(item) {
            out.push(candidate);
        }
        match item {
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_candidates(nested, out);
                }
            }
            Item::Use(item) if attrs_are_safe(&item.attrs) => collect_use_candidates(item, out),
            Item::Impl(implementation)
                if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) =>
            {
                out.extend(implementation.items.iter().filter_map(impl_candidate));
            }
            Item::ForeignMod(block) if attrs_are_safe(&block.attrs) => {
                out.extend(block.items.iter().filter_map(foreign_candidate));
            }
            _ => {}
        }
    }
}

/// Every node of a `use` tree that rustc may call dead.
fn collect_use_candidates(item: &syn::ItemUse, out: &mut Vec<Candidate>) {
    out.push(Candidate {
        kind: ItemKind::UseItem,
        name: verify::use_tree_text(&item.tree),
        byte_range: span_tuple(&item.tree),
    });
    collect_use_paths(&item.tree, out);
}

fn collect_use_paths(tree: &UseTree, out: &mut Vec<Candidate>) {
    match tree {
        UseTree::Path(path) => collect_use_paths(&path.tree, out),
        UseTree::Group(group) => {
            for child in &group.items {
                out.push(Candidate {
                    kind: ItemKind::UsePath,
                    name: verify::use_tree_text(child),
                    byte_range: span_tuple(child),
                });
                collect_use_paths(child, out);
            }
        }
        UseTree::Name(_) | UseTree::Rename(_) | UseTree::Glob(_) => {}
    }
}

pub(crate) fn span_of<Node: Spanned>(node: &Node) -> Range<usize> {
    node.span().byte_range()
}

pub(crate) fn span_tuple<Node: Spanned>(node: &Node) -> (usize, usize) {
    let range = span_of(node);
    (range.start, range.end)
}

pub(crate) fn ident_candidate(kind: ItemKind, ident: &syn::Ident) -> Candidate {
    let range = ident.span().byte_range();
    Candidate {
        kind,
        name: ident.to_string(),
        byte_range: (range.start, range.end),
    }
}

pub(crate) fn item_candidate(item: &Item) -> Option<Candidate> {
    Some(match item {
        Item::Fn(item) => ident_candidate(ItemKind::Function, &item.sig.ident),
        Item::Const(item) => ident_candidate(ItemKind::Constant, &item.ident),
        Item::Static(item) => ident_candidate(ItemKind::Static, &item.ident),
        Item::Struct(item) => ident_candidate(ItemKind::Struct, &item.ident),
        Item::Enum(item) => ident_candidate(ItemKind::Enum, &item.ident),
        Item::Union(item) => ident_candidate(ItemKind::Union, &item.ident),
        Item::Type(item) => ident_candidate(ItemKind::TypeAlias, &item.ident),
        Item::Mod(item) => ident_candidate(ItemKind::Module, &item.ident),
        Item::Trait(item) => ident_candidate(ItemKind::Trait, &item.ident),
        // Only `macro_rules!` definitions can be removed; an item position
        // macro invocation may run arbitrary code.
        Item::Macro(item) if item.ident.is_some() && item.mac.path.is_ident("macro_rules") => {
            ident_candidate(ItemKind::MacroDefinition, item.ident.as_ref().unwrap())
        }
        _ => return None,
    })
}

pub(crate) fn impl_candidate(item: &syn::ImplItem) -> Option<Candidate> {
    Some(match item {
        syn::ImplItem::Fn(item) => ident_candidate(ItemKind::Method, &item.sig.ident),
        syn::ImplItem::Const(item) => ident_candidate(ItemKind::AssociatedConstant, &item.ident),
        _ => return None,
    })
}

pub(crate) fn foreign_candidate(item: &syn::ForeignItem) -> Option<Candidate> {
    Some(match item {
        syn::ForeignItem::Fn(item) => ident_candidate(ItemKind::ForeignFunction, &item.sig.ident),
        syn::ForeignItem::Static(item) => ident_candidate(ItemKind::ForeignStatic, &item.ident),
        syn::ForeignItem::Type(item) => ident_candidate(ItemKind::ForeignType, &item.ident),
        _ => return None,
    })
}
