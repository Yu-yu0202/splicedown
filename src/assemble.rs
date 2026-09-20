use syn::{File, Ident, Item, ItemMod, parse_quote};

pub(crate) fn assemble(mut main: File, deps: Vec<(Ident, File)>) -> File {
    let mut items = Vec::with_capacity(deps.len() + main.items.len());

    items.append(&mut main.items);

    for (name, dep) in deps {
        let mut module: ItemMod = parse_quote!(mod #name {});
        module.attrs = dep.attrs;
        module.content = Some((Default::default(), dep.items));
        items.push(Item::Mod(module));
    }

    main.items = items;
    main.attrs.insert(
        0,
        parse_quote!(#![allow(dead_code, unused_imports, unused_macros, unused_variables)]),
    );
    main
}

#[cfg(test)]
mod tests {
    use super::*;
    use proc_macro2::Span;
    use syn::{AttrStyle, Item};

    #[test]
    fn entry_precedes_dependencies_and_keeps_inner_attributes() {
        let main = syn::parse_file("#![allow(clippy::all)]\nmod utils {}\nfn main() {}").unwrap();
        let dep = syn::parse_file("#![no_implicit_prelude]\npub fn value() -> i32 { 1 }").unwrap();
        let name = Ident::new("__splicedown_dep", Span::call_site());

        let assembled = assemble(main, vec![(name.clone(), dep)]);

        assert_eq!(assembled.attrs.len(), 2);
        assert!(matches!(assembled.attrs[0].style, AttrStyle::Inner(_)));
        assert!(matches!(&assembled.items[0], Item::Mod(m) if m.ident == "utils"));
        assert!(matches!(&assembled.items[1], Item::Fn(f) if f.sig.ident == "main"));
        let Item::Mod(dep_mod) = &assembled.items[2] else {
            panic!("last item was not a dependency module");
        };
        assert_eq!(dep_mod.ident, name);
        assert!(matches!(dep_mod.attrs[0].style, AttrStyle::Inner(_)));
        assert_eq!(dep_mod.content.as_ref().unwrap().1.len(), 1);
    }

    #[test]
    fn output_round_trips_through_syn() {
        let main = syn::parse_file("fn main() {}").unwrap();
        let dep = syn::parse_file("pub const VALUE: i32 = 1;").unwrap();
        let name = Ident::new("__splicedown_dep", Span::call_site());

        let output = prettyplease::unparse(&assemble(main, vec![(name, dep)]));

        syn::parse_file(&output).unwrap();
        assert!(output.contains("mod __splicedown_dep"));
    }
}
