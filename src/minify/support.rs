use super::candidate::{
    Candidate, ItemKind, definition_candidates, definition_name, foreign_candidate, impl_candidate,
    span_of, span_tuple, takes_whole_item,
};
use super::remove::attrs_are_safe;
use crate::verify;
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use syn::{File, Item, UseTree};

/// Spans of every definition, marked with whether this batch takes it out.
///
/// A module the batch removes takes everything inside it with it. A macro
/// definition is never a reader: it carries its own copies of whatever it
/// expands to, and the check crate is what decides whether the expansion names
/// something that has to stay.
/// One definition of the bundle, with the reason an occurrence inside it is or
/// is not a reader of a name.
#[derive(Clone)]
struct Span {
    range: Range<usize>,
    /// This batch takes the definition out, so nothing inside it survives.
    taken_out: bool,
    /// An import resolves a name instead of reading it, so what is inside it is
    /// only evidence when something outside the import reads the name too.
    is_import: bool,
}

/// Collect every definition of the bundle with how this batch treats it.
///
/// `dead` holds the spans rustc already called dead: such a definition is going
/// away in this round or a later one, so what is inside it is not a reader of
/// anything that survives. That keeps two dead definitions from naming each
/// other and keeping both alive.
fn partition_spans(
    items: &[Item],
    candidates: &HashSet<&Candidate>,
    dead: &HashSet<(usize, usize)>,
    span_offset: usize,
    out: &mut Vec<Span>,
) {
    for item in items {
        let whole = takes_whole_item(item, candidates);
        let definition = definition_name(item).is_some_and(|candidate| {
            dead.contains(&(
                candidate.byte_range.0 + span_offset,
                candidate.byte_range.1 + span_offset,
            ))
        });
        let macro_definition = matches!(
            item,
            Item::Macro(item) if item.ident.is_some() && item.mac.path.is_ident("macro_rules")
        );
        let is_import = matches!(item, Item::Use(_) | Item::ExternCrate(_));
        out.push(Span {
            range: span_of(item),
            taken_out: whole || definition || macro_definition,
            is_import,
        });
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
            && !whole
        {
            partition_spans(nested, candidates, dead, span_offset, out);
        }
        // Members of a surviving block are taken out one by one, so the
        // definitions sitting inside it still count as their own.
        if let Item::Impl(implementation) = item
            && implementation.trait_.is_none()
            && attrs_are_safe(&implementation.attrs)
        {
            for member in &implementation.items {
                if impl_candidate(member).is_some_and(|candidate| candidates.contains(&candidate)) {
                    out.push(Span {
                        range: span_of(member),
                        taken_out: true,
                        is_import: false,
                    });
                }
            }
        }
        if let Item::ForeignMod(block) = item
            && attrs_are_safe(&block.attrs)
        {
            for member in &block.items {
                if foreign_candidate(member)
                    .is_some_and(|candidate| candidates.contains(&candidate))
                {
                    out.push(Span {
                        range: span_of(member),
                        taken_out: true,
                        is_import: false,
                    });
                }
            }
        }
    }
}

/// Whether an occurrence of a name is inside a definition that survives.
///
/// The smallest definition that holds the occurrence decides, so that the
/// references inside an impl block the batch takes out do not keep a name alive,
/// and a module that merely holds it is not a reader. An occurrence that is
/// itself the name of a definition is not a reader either: a name can be
/// defined more than once in different places of a bundle, as a module and a
/// function it holds.
fn occurrence_is_supported(
    occurrence: &Range<usize>,
    spans: &[Span],
    definitions: &[Range<usize>],
) -> bool {
    if definitions.iter().any(|span| span == occurrence) {
        return false;
    }
    let innermost = spans
        .iter()
        .filter(|span| span.range.start <= occurrence.start && occurrence.end <= span.range.end)
        .min_by_key(|span| span.range.end - span.range.start);
    // An import that is still there resolves the name, but it only has to
    // stay when a definition that survives reads it, so the import itself
    // is never the evidence that the name is in use.
    innermost.is_some_and(|span| !span.taken_out && !span.is_import)
}

/// Drop the candidates a surviving definition still names.
///
/// rustc can call an item dead while a `use` that nothing reported keeps
/// importing it, for example a re-export inside a module the lint never looked
/// into. Removing the item alone cannot compile, so it waits for the round after
/// the reader is gone instead of costing a rejected batch.
pub(crate) fn keep_only_unreferenced(
    file: &File,
    positions: &HashMap<String, Vec<Range<usize>>>,
    candidates: &[Candidate],
    dead: &HashSet<(usize, usize)>,
    span_offset: usize,
) -> Vec<Candidate> {
    let wanted = candidates.iter().collect::<HashSet<&Candidate>>();
    let mut spans = Vec::new();
    partition_spans(&file.items, &wanted, dead, span_offset, &mut spans);
    let mut definitions = Vec::new();
    definition_candidates(&file.items, &mut definitions);
    let definitions = definitions
        .iter()
        .map(|candidate| candidate.byte_range.0..candidate.byte_range.1)
        .collect::<Vec<_>>();
    candidates
        .iter()
        .filter(|candidate| {
            // An import path is a reference, not a definition: whether it is
            // still needed is what the lint that proposed it already decided.
            !occurrences_of(&candidate.name, positions)
                .any(|occurrence| occurrence_is_supported(&occurrence, &spans, &definitions))
                || matches!(candidate.kind, ItemKind::UseItem | ItemKind::UsePath)
        })
        .cloned()
        .collect()
}

/// Types whose only readers are their own impl blocks and dead items.
///
/// rustc keeps a type alive as long as it has an impl block, so the type and
/// those blocks have to go together. With the impl blocks and the dead items
/// taken out, the question is the one the filter above asks: is the type still
/// named anywhere? If not, both go into the same batch.
pub(crate) fn collect_orphan_types(
    file: &File,
    positions: &HashMap<String, Vec<Range<usize>>>,
    dead: HashSet<Candidate>,
    dead_spans: &HashSet<(usize, usize)>,
    span_offset: usize,
    out: &mut HashSet<Candidate>,
) {
    let mut impls = Vec::new();
    collect_impl_owners(&file.items, &mut impls);
    if impls.is_empty() {
        return;
    }
    let mut definitions = Vec::new();
    definition_candidates(&file.items, &mut definitions);
    let definitions = definitions
        .into_iter()
        .map(|candidate| candidate.byte_range.0..candidate.byte_range.1)
        .collect::<Vec<_>>();

    // The spans of the dead definitions are the same for every type, so they
    // are collected once and each type adds its own impl blocks to a copy.
    let dead_wanted = dead.iter().collect::<HashSet<&Candidate>>();
    let mut spans_of_dead = Vec::new();
    partition_spans(
        &file.items,
        &dead_wanted,
        dead_spans,
        span_offset,
        &mut spans_of_dead,
    );

    let names = impls
        .iter()
        .map(|(name, _)| name.clone())
        .collect::<HashSet<_>>();
    for name in names {
        // The batch that would take the type away: its own impl blocks and
        // everything rustc already called dead.
        let mut spans = spans_of_dead.clone();
        for (owner, range) in &impls {
            if *owner == name {
                spans.push(Span {
                    range: range.0..range.1,
                    taken_out: true,
                    is_import: false,
                });
            }
        }
        if occurrences_of(&name, positions)
            .any(|occurrence| occurrence_is_supported(&occurrence, &spans, &definitions))
        {
            continue;
        }
        // The type itself, and the impl blocks that kept it alive.
        if let Some(candidate) = definition_candidate_named(&file.items, &name) {
            out.insert(candidate);
        }
        for (owner, range) in &impls {
            if *owner == name {
                out.insert(Candidate {
                    kind: ItemKind::ImplBlock,
                    name: owner.clone(),
                    byte_range: *range,
                });
            }
        }
    }
}

/// The definition of a name, when the bundle has one.
fn definition_candidate_named(items: &[Item], name: &str) -> Option<Candidate> {
    let mut definitions = Vec::new();
    definition_candidates(items, &mut definitions);
    definitions
        .into_iter()
        .find(|candidate| candidate.name == name)
}

/// Imports that only bring in names this batch takes out.
///
/// Such an import cannot survive the batch it belongs to: something it names is
/// about to disappear, so it has to go with it or the bundle stops resolving.
pub(crate) fn collect_imports_of_removed(
    items: &[Item],
    candidates: &HashSet<Candidate>,
    out: &mut HashSet<Candidate>,
) {
    for item in items {
        if let Item::Use(item) = item
            && attrs_are_safe(&item.attrs)
        {
            let names = imported_names(&item.tree);
            if !names.is_empty()
                && names
                    .iter()
                    .all(|name| candidates.iter().any(|candidate| &candidate.name == name))
            {
                out.insert(Candidate {
                    kind: ItemKind::UseItem,
                    name: verify::use_tree_text(&item.tree),
                    byte_range: span_tuple(&item.tree),
                });
            }
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            collect_imports_of_removed(nested, candidates, out);
        }
    }
}

/// The names an import brings into scope.
fn imported_names(tree: &UseTree) -> Vec<String> {
    match tree {
        UseTree::Path(path) => imported_names(&path.tree),
        UseTree::Name(name) => vec![name.ident.to_string()],
        UseTree::Rename(rename) => vec![rename.ident.to_string()],
        UseTree::Glob(_) => vec![String::new()],
        UseTree::Group(group) => group
            .items
            .iter()
            .flat_map(imported_names)
            .collect::<Vec<_>>(),
    }
}

/// The name and span of the self type of every impl block.
fn collect_impl_owners(items: &[Item], out: &mut Vec<(String, (usize, usize))>) {
    for item in items {
        if let Item::Impl(implementation) = item
            && let syn::Type::Path(path) = implementation.self_ty.as_ref()
            && let Some(segment) = path.path.segments.last()
        {
            out.push((segment.ident.to_string(), span_tuple(item)));
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            collect_impl_owners(nested, out);
        }
    }
}

/// The places a name was read from, for deciding what still names it.
fn occurrences_of<'a>(
    name: &str,
    positions: &'a HashMap<String, Vec<Range<usize>>>,
) -> impl Iterator<Item = Range<usize>> + 'a {
    positions
        .get(name)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .cloned()
}
