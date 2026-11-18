use anyhow::Result;
use syn::Meta;
use syn::punctuated::Punctuated;
use syn::{Attribute, File, Ident, Item, ItemUse, Token, UseName, UsePath, UseTree};

/// Keep exported dependency macros inside their wrapper module.
///
/// `#[macro_export]` would otherwise place the macro at the root of the final
/// crate.  Removing that attribute and re-exporting the macro next to its
/// definition makes it addressable through the dependency's mangled module.
pub(crate) fn fix_macro_exports(file: &mut File) -> Result<()> {
    let mut root_reexports = Vec::new();
    fix_macro_exports_in_items(&mut file.items, &[], &[], &mut root_reexports)?;
    file.items.extend(root_reexports);
    Ok(())
}

fn fix_macro_exports_in_items(
    items: &mut Vec<Item>,
    module_path: &[Ident],
    inherited_conditions: &[Attribute],
    root_reexports: &mut Vec<Item>,
) -> Result<()> {
    let old_items = std::mem::take(items);
    let mut new_items = Vec::with_capacity(old_items.len());

    for mut item in old_items {
        if let Item::Mod(item_mod) = &mut item
            && let Some((_, nested)) = &mut item_mod.content
        {
            let mut nested_path = module_path.to_vec();
            nested_path.push(item_mod.ident.clone());
            let mut nested_conditions = inherited_conditions.to_vec();
            nested_conditions.extend(condition_attrs(&item_mod.attrs));
            fix_macro_exports_in_items(nested, &nested_path, &nested_conditions, root_reexports)?;
        }

        let exported_macro = if let Item::Macro(item_macro) = &mut item {
            if let Some(export_condition) = take_macro_export(&mut item_macro.attrs)? {
                let mut conditions = condition_attrs(&item_macro.attrs);
                if let Some(export_condition) = export_condition {
                    conditions.push(export_condition);
                }
                Some((
                    item_macro.ident.clone().ok_or_else(|| {
                        anyhow::anyhow!("#[macro_export] can only be used on a named macro")
                    })?,
                    conditions,
                ))
            } else {
                None
            }
        } else {
            None
        };

        new_items.push(item);
        if let Some((ident, own_conditions)) = exported_macro {
            new_items.push(Item::Use(make_reexport(
                std::slice::from_ref(&ident),
                own_conditions.clone(),
            )));

            // `#[macro_export]` makes a nested macro available at the original
            // crate root. Preserve that API in addition to the local export
            // used through the containing module.
            if !module_path.is_empty() {
                let mut path = module_path.to_vec();
                path.push(ident);
                let mut conditions = inherited_conditions.to_vec();
                conditions.extend(own_conditions);
                root_reexports.push(Item::Use(make_reexport(&path, conditions)));
            }
        }
    }

    *items = new_items;
    Ok(())
}

/// Remove unconditional and directly conditional `macro_export` attributes.
///
/// The outer option indicates whether the macro is exported at all. The inner
/// option is a `cfg` attribute that gates the generated imports when export is
/// conditional. An unconditional export wins over all conditional branches.
fn take_macro_export(attrs: &mut Vec<Attribute>) -> Result<Option<Option<Attribute>>> {
    let mut unconditional = false;
    let mut predicates = Vec::new();
    let mut retained = Vec::with_capacity(attrs.len());

    for attr in std::mem::take(attrs) {
        if attr.path().is_ident("macro_export") {
            unconditional = true;
            continue;
        }

        if !attr.path().is_ident("cfg_attr") {
            retained.push(attr);
            continue;
        }

        let args = attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
        let mut args = args.into_iter();
        let Some(predicate) = args.next() else {
            retained.push(attr);
            continue;
        };
        let mut remaining = Vec::new();
        let mut conditional_export = false;
        for meta in args {
            if matches!(&meta, Meta::Path(path) if path.is_ident("macro_export")) {
                conditional_export = true;
            } else {
                if nested_cfg_attr_exports(&meta)? {
                    anyhow::bail!(
                        "nested cfg_attr(..., macro_export) is not supported while bundling macros"
                    );
                }
                remaining.push(meta);
            }
        }

        if conditional_export {
            predicates.push(predicate.clone());
            if !remaining.is_empty() {
                retained.push(syn::parse_quote!(#[cfg_attr(#predicate, #(#remaining),*)]));
            }
        } else {
            retained.push(attr);
        }
    }

    *attrs = retained;
    if unconditional {
        Ok(Some(None))
    } else if predicates.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Some(syn::parse_quote!(#[cfg(any(#(#predicates),*))]))))
    }
}

fn nested_cfg_attr_exports(meta: &Meta) -> Result<bool> {
    let Meta::List(list) = meta else {
        return Ok(false);
    };
    if !list.path.is_ident("cfg_attr") {
        return Ok(false);
    }

    let args = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)?;
    for nested in args.iter().skip(1) {
        if matches!(nested, Meta::Path(path) if path.is_ident("macro_export"))
            || nested_cfg_attr_exports(nested)?
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn condition_attrs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs
        .iter()
        .filter(|attr| attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr"))
        .cloned()
        .collect()
}

fn make_reexport(path: &[Ident], attrs: Vec<Attribute>) -> ItemUse {
    let mut tree = UseTree::Name(UseName {
        ident: path.last().expect("re-export path is non-empty").clone(),
    });
    for ident in path[..path.len() - 1].iter().rev() {
        tree = UseTree::Path(UsePath {
            ident: ident.clone(),
            colon2_token: Default::default(),
            tree: Box::new(tree),
        });
    }

    let mut item: ItemUse = syn::parse_quote!(
        pub(crate) use placeholder;
    );
    item.attrs = attrs;
    item.tree = tree;
    item
}
