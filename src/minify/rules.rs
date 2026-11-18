use super::candidate::{
    Candidate, Candidates, ItemKind, collect_candidates, ident_candidate, item_candidate, span_of,
    span_tuple,
};
use super::compiler::Diagnostics;
use super::remove::{
    attrs_are_safe, item_is_removable, item_is_safe_to_remove, join_path, path_text,
};
use super::support::{collect_imports_of_removed, collect_orphan_types};
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use syn::{File, Item};

pub(crate) fn removable_candidates(
    file: &File,
    diagnostics: &Diagnostics,
    span_offset: usize,
    positions: &HashMap<String, Vec<Range<usize>>>,
) -> Candidates {
    let mut available = Vec::new();
    collect_candidates(&file.items, &mut available);
    let mut impl_types = HashSet::new();
    collect_impl_types(&file.items, &mut impl_types);

    // Which members rustc called dead decides that first: a type whose impl
    // blocks lose every member can go with them in the same batch.
    let mut reported = HashSet::new();
    for candidate in available {
        let span = (
            candidate.byte_range.0 + span_offset,
            candidate.byte_range.1 + span_offset,
        );
        if diagnostics
            .spans_for(candidate.kind)
            .any(|spans| spans.contains(&span))
            && item_is_removable(&file.items, &candidate)
        {
            reported.insert(candidate);
        }
    }

    // A type rustc called dead takes its impl blocks with it: an impl of a type
    // nothing can name is unreachable, whatever trait it implements. Collecting
    // them here also lets the type itself through the guard below, because the
    // impls that would reference a missing type are in the same batch.
    let dead_types = reported
        .iter()
        .filter(|candidate| {
            matches!(
                candidate.kind,
                ItemKind::Struct | ItemKind::Enum | ItemKind::Union | ItemKind::TypeAlias
            )
        })
        .map(|candidate| candidate.name.clone())
        .collect::<HashSet<_>>();
    collect_impl_blocks_for(&file.items, "", &dead_types, &mut reported);

    let mut candidates = Candidates::default();
    for candidate in reported {
        candidates.reported.insert(candidate);
    }

    // A dead trait can only go together with the impls that reference it.
    let dead_traits = dead_trait_paths(file, diagnostics, span_offset);
    if !dead_traits.is_empty() {
        collect_impl_blocks(&file.items, "", &dead_traits, &mut candidates.reported);
    }
    // An impl block with nothing but dead members is dead as well, which is
    // what keeps the type it belongs to from losing its last reason to stay.
    collect_dead_impl_blocks(
        &file.items,
        diagnostics,
        span_offset,
        &mut candidates.reported,
    );
    // A type rustc never called dead because an impl block keeps it alive is
    // dead all the same when the only readers of its name are that impl block
    // and the items rustc already called dead.
    let mut orphan_dead = candidates.reported.clone();
    collect_orphan_types(
        file,
        positions,
        std::mem::take(&mut orphan_dead),
        &diagnostics.dead_code,
        span_offset,
        &mut candidates.reported,
    );
    // An import that only names what the batch takes out has to go with it,
    // and the same goes for an import that a removed name kept alive.
    let imports = candidates.reported.clone();
    collect_imports_of_removed(&file.items, &imports, &mut candidates.reported);

    // rustc treats the public API of a binary crate as externally reachable, so
    // root items need the token level reference count instead.
    for candidate in unreferenced_candidates(file, positions) {
        if item_is_removable(&file.items, &candidate) {
            candidates.speculative.insert(candidate);
        }
    }

    // rustc never reports a module that has nothing left in it.
    for candidate in empty_module_candidates(&file.items) {
        candidates.speculative.insert(candidate);
    }

    candidates
}

/// Impl blocks rustc reported as having nothing but dead members.
///
/// An impl block itself is never called dead, so a type it belongs to can stay
/// alive for the block alone. When every member is dead the block can go, and
/// with it the last reason to keep the type.
fn collect_dead_impl_blocks(
    items: &[Item],
    diagnostics: &Diagnostics,
    span_offset: usize,
    out: &mut HashSet<Candidate>,
) {
    for item in items {
        if let Item::Impl(implementation) = item
            && attrs_are_safe(&implementation.attrs)
            && implementation.items.iter().all(|member| {
                impl_ident_span(member).is_some_and(|(start, end)| {
                    diagnostics
                        .dead_code
                        .contains(&(start + span_offset, end + span_offset))
                })
            })
        {
            let range = span_of(implementation);
            out.insert(Candidate {
                kind: ItemKind::ImplBlock,
                name: impl_block_name(implementation),
                byte_range: (range.start, range.end),
            });
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            collect_dead_impl_blocks(nested, diagnostics, span_offset, out);
        }
    }
}

/// The identifier span of an impl member, when it has one.
fn impl_ident_span(member: &syn::ImplItem) -> Option<(usize, usize)> {
    Some(match member {
        syn::ImplItem::Fn(item) => span_tuple(&item.sig.ident),
        syn::ImplItem::Const(item) => span_tuple(&item.ident),
        _ => return None,
    })
}

/// A name for an impl block that covers both the trait and the self type.
fn impl_block_name(implementation: &syn::ItemImpl) -> String {
    let self_ty = match implementation.self_ty.as_ref() {
        syn::Type::Path(path) => path_text(&path.path),
        _ => "…".to_owned(),
    };
    implementation.trait_.as_ref().map_or_else(
        || self_ty.clone(),
        |(path, _)| format!("{} for {}", path_text(path), self_ty),
    )
}

/// Impl blocks whose self type is a type rustc called dead.
fn collect_impl_blocks_for(
    items: &[Item],
    prefix: &str,
    dead_types: &HashSet<String>,
    out: &mut HashSet<Candidate>,
) {
    for item in items {
        if let Item::Impl(implementation) = item
            && attrs_are_safe(&implementation.attrs)
            && let syn::Type::Path(path) = implementation.self_ty.as_ref()
        {
            let self_paths = trait_path_candidates(prefix, &path.path);
            for path in self_paths {
                if !dead_types.contains(&path) {
                    continue;
                }
                let range = span_of(implementation);
                out.insert(Candidate {
                    kind: ItemKind::ImplBlock,
                    name: path,
                    byte_range: (range.start, range.end),
                });
            }
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            let prefix = join_path(prefix, &module.ident.to_string());
            collect_impl_blocks_for(nested, &prefix, dead_types, out);
        }
    }
}

fn dead_trait_paths(file: &File, diagnostics: &Diagnostics, span_offset: usize) -> HashSet<String> {
    let mut dead = HashSet::new();
    collect_dead_traits(&file.items, "", diagnostics, span_offset, &mut dead);
    dead
}

fn collect_dead_traits(
    items: &[Item],
    prefix: &str,
    diagnostics: &Diagnostics,
    span_offset: usize,
    dead: &mut HashSet<String>,
) {
    for item in items {
        if let Item::Trait(item) = item {
            let candidate = ident_candidate(ItemKind::Trait, &item.ident);
            let span = (
                candidate.byte_range.0 + span_offset,
                candidate.byte_range.1 + span_offset,
            );
            if diagnostics.dead_code.contains(&span) && attrs_are_safe(&item.attrs) {
                dead.insert(join_path(prefix, &item.ident.to_string()));
            }
        }
    }
    for item in items {
        if let Item::Mod(module) = item
            && attrs_are_safe(&module.attrs)
            && let Some((_, nested)) = &module.content
        {
            let prefix = join_path(prefix, &module.ident.to_string());
            collect_dead_traits(nested, &prefix, diagnostics, span_offset, dead);
        }
    }
}

/// Impl blocks that can only be reached through a trait rustc called dead.
fn collect_impl_blocks(
    items: &[Item],
    prefix: &str,
    dead_traits: &HashSet<String>,
    out: &mut HashSet<Candidate>,
) {
    for item in items {
        if let Item::Impl(implementation) = item
            && let Some((trait_path, _)) = &implementation.trait_
            && attrs_are_safe(&implementation.attrs)
        {
            let paths = trait_path_candidates(prefix, trait_path);
            let range = span_of(implementation);
            for path in paths {
                if !dead_traits.contains(&path) {
                    continue;
                }
                out.insert(Candidate {
                    kind: ItemKind::ImplBlock,
                    name: path_text(trait_path),
                    byte_range: (range.start, range.end),
                });
            }
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            let prefix = join_path(prefix, &module.ident.to_string());
            collect_impl_blocks(nested, &prefix, dead_traits, out);
        }
    }
}

/// Absolute paths an impl block's trait path can refer to.
fn trait_path_candidates(prefix: &str, path: &syn::Path) -> Vec<String> {
    let mut segments = path
        .segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>();
    let Some(head) = segments.first().cloned() else {
        return Vec::new();
    };
    let rest = segments.split_off(1);

    if head == "crate" {
        return Vec::from([rest.join("::")]);
    }
    if head == "self" {
        return Vec::from([join_path(prefix, &rest.join("::"))]);
    }
    if head == "super" {
        let mut supers = 1;
        while rest
            .get(supers - 1)
            .is_some_and(|segment| segment == "super")
        {
            supers += 1;
        }
        let mut current = prefix;
        for _ in 0..supers {
            current = current.rsplit_once("::").map_or("", |(parent, _)| parent);
        }
        let remaining = &rest[supers - 1..];
        return Vec::from([join_path(current, &remaining.join("::"))]);
    }

    let mut full = Vec::with_capacity(rest.len() + 1);
    full.push(head);
    full.extend(rest);
    let full = full.join("::");
    let mut candidates = vec![full.clone(), join_path(prefix, &full)];
    candidates.dedup();
    candidates
}

/// Definitions whose identifier appears once in the whole bundle, which is the
/// definition itself.
///
/// rustc cannot call every such definition dead: the public API of a binary
/// crate is externally reachable, and a name that a glob import brought into
/// scope counts as used even when nothing reads it. Both shapes are safe to
/// try, because the compiler confirms every batch before it is kept.
fn unreferenced_candidates(
    file: &File,
    positions: &HashMap<String, Vec<Range<usize>>>,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    collect_unreferenced(&file.items, true, positions, &mut out);
    out
}

fn collect_unreferenced(
    items: &[Item],
    at_root: bool,
    positions: &HashMap<String, Vec<Range<usize>>>,
    out: &mut Vec<Candidate>,
) {
    for item in items {
        if item_is_safe_to_remove(item)
            && let Some(candidate) = item_candidate(item)
            && !matches!(candidate.kind, ItemKind::UseItem | ItemKind::UsePath)
            && !declares_foreign_abi(item)
            // The entry point is the one definition nothing may reference.
            && !(at_root && candidate.kind == ItemKind::Function && candidate.name == "main")
            && positions.get(&candidate.name).is_none_or(|found| found.len() == 1)
        {
            out.push(candidate);
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            collect_unreferenced(nested, false, positions, out);
        }
    }
}

fn declares_foreign_abi(item: &Item) -> bool {
    match item {
        Item::Fn(item) => item.sig.abi.is_some(),
        Item::ForeignMod(_) => true,
        _ => false,
    }
}

fn empty_module_candidates(items: &[Item]) -> Vec<Candidate> {
    let mut out = Vec::new();
    for item in items {
        if let Item::Mod(module) = item
            && module
                .content
                .as_ref()
                .is_some_and(|(_, items)| items.is_empty())
            && attrs_are_safe(&module.attrs)
        {
            out.push(ident_candidate(ItemKind::Module, &module.ident));
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            out.extend(empty_module_candidates(nested));
        }
    }
    out
}

fn collect_impl_types(items: &[Item], names: &mut HashSet<String>) {
    for item in items {
        match item {
            Item::Impl(item) => {
                if let syn::Type::Path(path) = item.self_ty.as_ref()
                    && let Some(segment) = path.path.segments.last()
                {
                    names.insert(segment.ident.to_string());
                }
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_impl_types(nested, names);
                }
            }
            _ => {}
        }
    }
}
