/// Simple description used in error messages for an item without a name.
use syn::Item;

pub(crate) fn describe(item: &Item) -> String {
    match item {
        Item::Impl(_) => "impl block".to_owned(),
        Item::Verbatim(_) => "verbatim item".to_owned(),
        Item::ForeignMod(_) => "extern block".to_owned(),
        _ => {
            let kind = kind_of(item);
            let name = name_of(item);
            format!("{kind} `{name}`")
        }
    }
}

pub(crate) fn kind_of(item: &Item) -> &'static str {
    match item {
        Item::Fn(_) => "function",
        Item::Const(_) => "const",
        Item::Static(_) => "static",
        Item::Struct(_) => "struct",
        Item::Enum(_) => "enum",
        Item::Union(_) => "union",
        Item::Type(_) => "type alias",
        Item::Trait(_) => "trait",
        Item::TraitAlias(_) => "trait alias",
        Item::Mod(_) => "module",
        Item::Use(_) => "import",
        Item::ForeignMod(_) => "extern block",
        Item::Macro(_) => "macro definition",
        Item::ExternCrate(_) => "extern crate",
        Item::Impl(_) | Item::Verbatim(_) | _ => "item",
    }
}

pub(crate) fn name_of(item: &Item) -> String {
    match item {
        Item::Fn(item) => item.sig.ident.to_string(),
        Item::Const(item) => item.ident.to_string(),
        Item::Static(item) => item.ident.to_string(),
        Item::Struct(item) => item.ident.to_string(),
        Item::Enum(item) => item.ident.to_string(),
        Item::Union(item) => item.ident.to_string(),
        Item::Type(item) => item.ident.to_string(),
        Item::Trait(item) => item.ident.to_string(),
        Item::TraitAlias(item) => item.ident.to_string(),
        Item::Mod(item) => item.ident.to_string(),
        Item::ExternCrate(item) => item.ident.to_string(),
        Item::Use(item) => use_tree_text(&item.tree),
        _ => String::new(),
    }
}

pub(crate) fn impl_item_name(member: &syn::ImplItem) -> Option<String> {
    Some(match member {
        syn::ImplItem::Const(item) => item.ident.to_string(),
        syn::ImplItem::Fn(item) => item.sig.ident.to_string(),
        syn::ImplItem::Type(item) => item.ident.to_string(),
        _ => return None,
    })
}

pub(crate) fn foreign_item_name(member: &syn::ForeignItem) -> Option<String> {
    Some(match member {
        syn::ForeignItem::Fn(item) => item.sig.ident.to_string(),
        syn::ForeignItem::Static(item) => item.ident.to_string(),
        syn::ForeignItem::Type(item) => item.ident.to_string(),
        _ => return None,
    })
}

pub(crate) fn macro_path_text(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|segment| segment.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

/// Render a use tree the way it would appear in source.
pub(crate) fn use_tree_text(tree: &syn::UseTree) -> String {
    use syn::UseTree;
    match tree {
        UseTree::Path(path) => format!("{}::{}", path.ident, use_tree_text(&path.tree)),
        UseTree::Name(name) => name.ident.to_string(),
        UseTree::Rename(rename) => format!("{} as {}", rename.ident, rename.rename),
        UseTree::Glob(_) => "*".to_owned(),
        UseTree::Group(_) => "{ ... }".to_owned(),
    }
}
