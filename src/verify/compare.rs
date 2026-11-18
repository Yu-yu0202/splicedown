use super::Report;
use super::describe::{describe, foreign_item_name, impl_item_name, use_tree_text};
use super::fingerprint::{
    block_fingerprint, fingerprint, foreign_member_fingerprint, impl_fingerprint,
    impl_member_fingerprint, module_fingerprint,
};
use super::record::{
    record_removed, record_removed_foreign, record_removed_impl, record_removed_use,
};
use anyhow::{Context, Result, bail};
use syn::Item;
pub(crate) fn compare_items(
    original: &[Item],
    minified: &[Item],
    path: &[String],
    original_label: &str,
    minified_label: &str,
    report: &mut Report,
) -> Result<()> {
    let mut next = 0;
    for item in minified {
        let mut matched = None;
        for (index, candidate) in original.iter().enumerate().skip(next) {
            if same_item(candidate, item, path, report)? {
                matched = Some(index);
                break;
            }
        }
        let index = matched.with_context(|| {
            format!(
                "{minified_label} contains `{}`, which is not a definition of the {original_label}",
                describe(item)
            )
        })?;
        for skipped in original.iter().take(index).skip(next) {
            record_removed(skipped, path, report);
        }
        report.kept += 1;
        next = index + 1;
    }
    for skipped in original.iter().skip(next) {
        record_removed(skipped, path, report);
    }
    Ok(())
}

/// Decide whether `minified` can only have been derived from `original`, and
/// record the nested definitions that were removed along the way.
///
/// A container whose contents do not line up is *not* a match: the caller keeps
/// looking for the same definition further along the bundle. Only a definition
/// that survived under the same name but with different content is a
/// modification, and those are refused instead of being mistaken for another
/// container. That distinction matters because a bundle can hold several
/// containers with the same prelude, such as two inherent `impl` blocks for the
/// same type, and minify drops the ones that lost their last member.
fn same_item(
    original: &Item,
    minified: &Item,
    path: &[String],
    report: &mut Report,
) -> Result<bool> {
    match (original, minified) {
        (Item::Mod(original), Item::Mod(minified))
            if module_fingerprint(original) == module_fingerprint(minified) =>
        {
            match (&original.content, &minified.content) {
                (Some((_, items)), Some((_, nested))) => {
                    let mut child = path.to_vec();
                    child.push(original.ident.to_string());
                    compare_items(
                        items,
                        nested,
                        &child,
                        "pre-minify bundle",
                        "minified bundle",
                        report,
                    )?;
                }
                (None, None) => {}
                (Some(_), None) => bail!("minified bundle emptied the inline contents of a module"),
                (None, Some(_)) => bail!("minified bundle invented inline contents for a module"),
            }
            Ok(true)
        }
        (Item::Impl(original), Item::Impl(minified))
            if impl_fingerprint(original) == impl_fingerprint(minified) =>
        {
            Ok(compare_impl_items(
                &original.items,
                &minified.items,
                minified,
                path,
                report,
            )?)
        }
        (Item::Use(original), Item::Use(minified))
            if original.leading_colon == minified.leading_colon
                && use_trees_match(&original.tree, &minified.tree) =>
        {
            if !use_trees_match(&original.tree, &minified.tree) {
                return Ok(false);
            }
            record_use_removals(&original.tree, &minified.tree, path, report);
            Ok(true)
        }
        (Item::ForeignMod(original), Item::ForeignMod(minified))
            if block_fingerprint(original) == block_fingerprint(minified) =>
        {
            Ok(compare_foreign_items(
                &original.items,
                &minified.items,
                minified,
                path,
                report,
            )?)
        }
        (original, minified) if fingerprint(original) == fingerprint(minified) => Ok(true),
        // A module whose prelude matches but whose contents do not form a
        // subsequence is handled above; reaching this arm means the item was
        // rewritten or replaced.
        _ => Ok(false),
    }
}

/// Whether the members of the minified impl are members of the original one.
///
/// The members are matched without recording anything first, so a container
/// that is not the one the output came from is refused instead of matching half
/// of it.
fn compare_impl_items(
    original: &[syn::ImplItem],
    minified: &[syn::ImplItem],
    wrapper: &syn::ItemImpl,
    path: &[String],
    report: &mut Report,
) -> Result<bool> {
    let mut matched = Vec::with_capacity(minified.len());
    let mut next = 0;
    for item in minified {
        let mut index = None;
        for (candidate, member) in original.iter().enumerate().skip(next) {
            if impl_items_match(member, item, wrapper)? {
                index = Some(candidate);
                break;
            }
        }
        let Some(index) = index else {
            return Ok(false);
        };
        matched.push(index);
        next = index + 1;
    }

    let mut next = 0;
    for index in &matched {
        for skipped in original.iter().take(*index).skip(next) {
            record_removed_impl(skipped, path, report);
        }
        report.kept += 1;
        next = index + 1;
    }
    for skipped in original.iter().skip(next) {
        record_removed_impl(skipped, path, report);
    }
    Ok(true)
}

/// Whether an impl member of the output is an impl member of the bundle.
fn impl_items_match(
    original: &syn::ImplItem,
    minified: &syn::ImplItem,
    wrapper: &syn::ItemImpl,
) -> Result<bool> {
    if impl_member_fingerprint(original, wrapper) == impl_member_fingerprint(minified, wrapper) {
        return Ok(true);
    }
    if let (Some(original), Some(minified)) = (impl_item_name(original), impl_item_name(minified))
        && original == minified
    {
        bail!("minified bundle modified the impl member `{original}`");
    }
    Ok(false)
}

/// Whether the members of the minified extern block are members of the original.
fn compare_foreign_items(
    original: &[syn::ForeignItem],
    minified: &[syn::ForeignItem],
    wrapper: &syn::ItemForeignMod,
    path: &[String],
    report: &mut Report,
) -> Result<bool> {
    let mut matched = Vec::with_capacity(minified.len());
    let mut next = 0;
    for item in minified {
        let mut index = None;
        for (candidate, member) in original.iter().enumerate().skip(next) {
            if foreign_items_match(member, item, wrapper)? {
                index = Some(candidate);
                break;
            }
        }
        let Some(index) = index else {
            return Ok(false);
        };
        matched.push(index);
        next = index + 1;
    }

    let mut next = 0;
    for index in &matched {
        for skipped in original.iter().take(*index).skip(next) {
            record_removed_foreign(skipped, path, report);
        }
        report.kept += 1;
        next = index + 1;
    }
    for skipped in original.iter().skip(next) {
        record_removed_foreign(skipped, path, report);
    }
    Ok(true)
}

/// Whether an extern block member of the output is one of the bundle.
fn foreign_items_match(
    original: &syn::ForeignItem,
    minified: &syn::ForeignItem,
    wrapper: &syn::ItemForeignMod,
) -> Result<bool> {
    if foreign_member_fingerprint(original, wrapper)
        == foreign_member_fingerprint(minified, wrapper)
    {
        return Ok(true);
    }
    if let (Some(original), Some(minified)) =
        (foreign_item_name(original), foreign_item_name(minified))
        && original == minified
    {
        bail!("minified bundle modified the extern block member `{original}`");
    }
    Ok(false)
}

/// Whether an import tree of the output is an import tree of the bundle.
///
/// A group that lost every path but one prints without its braces, and a child
/// that is itself a group may lose paths the same way, so both shapes are
/// accepted. A tree that is not in the bundle is simply not a match, and the
/// caller reports the import that contains it.
fn use_trees_match(original: &syn::UseTree, minified: &syn::UseTree) -> bool {
    use syn::UseTree;
    match (original, minified) {
        (UseTree::Path(original), UseTree::Path(minified)) if original.ident == minified.ident => {
            use_trees_match(&original.tree, &minified.tree)
        }
        (UseTree::Group(original), UseTree::Group(minified)) => {
            use_children_match(original, &minified.items.iter().collect::<Vec<_>>())
        }
        (UseTree::Group(original), minified) => {
            use_children_match(original, std::slice::from_ref(&minified))
        }
        (original, minified) => use_tree_text(original) == use_tree_text(minified),
    }
}

fn use_children_match(original: &syn::UseGroup, minified: &[&syn::UseTree]) -> bool {
    let mut next = 0;
    for item in minified {
        let index = (next..original.items.len())
            .find(|&index| use_trees_match(&original.items[index], item));
        let Some(index) = index else {
            return false;
        };
        next = index + 1;
    }
    true
}

/// Record the import paths the minified tree no longer contains.
fn record_use_removals(
    original: &syn::UseTree,
    minified: &syn::UseTree,
    path: &[String],
    report: &mut Report,
) {
    use syn::UseTree;
    match (original, minified) {
        (UseTree::Path(original), UseTree::Path(minified)) if original.ident == minified.ident => {
            record_use_removals(&original.tree, &minified.tree, path, report);
        }
        (UseTree::Group(original), _) => {
            let kept = minified_children(minified);
            let mut next = 0;
            for item in &kept {
                let item: &syn::UseTree = item;
                let index = original
                    .items
                    .iter()
                    .enumerate()
                    .skip(next)
                    .find(|(_, candidate)| *candidate == item)
                    .map(|(index, _)| index);
                if let Some(index) = index {
                    for skipped in original.items.iter().take(index).skip(next) {
                        record_removed_use(skipped, path, report);
                    }
                    next = index + 1;
                }
            }
            for skipped in original.items.iter().skip(next) {
                record_removed_use(skipped, path, report);
            }
        }
        _ => {}
    }
}

/// The import paths of a minified tree that survived, one per child.
fn minified_children(tree: &syn::UseTree) -> Vec<&syn::UseTree> {
    match tree {
        syn::UseTree::Group(group) => group.items.iter().collect(),
        tree => vec![tree],
    }
}
