use super::describe::{foreign_item_name, impl_item_name, macro_path_text, use_tree_text};
use super::{Report, Unit};
use syn::Item;

pub(crate) fn record_removed(item: &Item, path: &[String], report: &mut Report) {
    let unit = match item {
        Item::Fn(item) => Unit::new(path, "function", item.sig.ident.to_string()),
        Item::Const(item) => Unit::new(path, "const", item.ident.to_string()),
        Item::Static(item) => Unit::new(path, "static", item.ident.to_string()),
        Item::Struct(item) => Unit::new(path, "struct", item.ident.to_string()),
        Item::Enum(item) => Unit::new(path, "enum", item.ident.to_string()),
        Item::Union(item) => Unit::new(path, "union", item.ident.to_string()),
        Item::Type(item) => Unit::new(path, "type alias", item.ident.to_string()),
        Item::Trait(item) => Unit::new(path, "trait", item.ident.to_string()),
        Item::TraitAlias(item) => Unit::new(path, "trait alias", item.ident.to_string()),
        Item::Mod(item) => Unit::new(path, "module", item.ident.to_string()),
        Item::Use(item) => Unit::new(path, "import", use_tree_text(&item.tree)),
        Item::ForeignMod(_) => Unit::new(path, "extern block", "extern block"),
        Item::Macro(item) => Unit::new(
            path,
            "macro definition",
            item.ident
                .as_ref()
                .map_or_else(|| macro_path_text(&item.mac.path), ToString::to_string),
        ),
        Item::ExternCrate(item) => Unit::new(path, "extern crate", item.ident.to_string()),
        Item::Impl(_) | Item::Verbatim(_) => Unit::new(path, "item", "<unnamed>"),
        _ => Unit::new(path, "item", "<unnamed>"),
    };
    report.removed.push(unit);
    let mut child = path.to_vec();
    match item {
        Item::Mod(module) => {
            if let Some((_, items)) = &module.content {
                child.push(module.ident.to_string());
                for nested in items {
                    record_removed(nested, &child, report);
                }
            }
        }
        Item::Impl(implementation) => {
            for member in &implementation.items {
                record_removed_impl(member, path, report);
            }
        }
        Item::ForeignMod(block) => {
            for member in &block.items {
                record_removed_foreign(member, path, report);
            }
        }
        Item::Use(item) => record_removed_use_tree(&item.tree, path, report),
        _ => {}
    }
}

pub(crate) fn record_removed_impl(member: &syn::ImplItem, path: &[String], report: &mut Report) {
    let name = impl_item_name(member).unwrap_or_default();
    let kind = match member {
        syn::ImplItem::Const(_) => "associated const",
        _ => "method",
    };
    report.removed.push(Unit::new(path, kind, name));
}

pub(crate) fn record_removed_foreign(
    member: &syn::ForeignItem,
    path: &[String],
    report: &mut Report,
) {
    let name = foreign_item_name(member).unwrap_or_default();
    let kind = match member {
        syn::ForeignItem::Type(_) => "extern type",
        syn::ForeignItem::Static(_) => "extern static",
        _ => "extern function",
    };
    report.removed.push(Unit::new(path, kind, name));
}

pub(crate) fn record_removed_use(tree: &syn::UseTree, path: &[String], report: &mut Report) {
    report
        .removed
        .push(Unit::new(path, "import", use_tree_text(tree)));
    record_removed_use_tree(tree, path, report);
}

fn record_removed_use_tree(tree: &syn::UseTree, path: &[String], report: &mut Report) {
    if let syn::UseTree::Group(group) = tree {
        for child in &group.items {
            record_removed_use(child, path, report);
        }
    }
}
