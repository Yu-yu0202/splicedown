/// Render one definition the way it would be written, on its own.
use syn::Item;

pub(crate) fn fingerprint(item: &Item) -> String {
    prettyplease::unparse(&syn::File {
        shebang: None,
        attrs: Vec::new(),
        frontmatter: None,
        items: vec![item.clone()],
    })
}

/// Render the parts of a module that its contents do not change.
pub(crate) fn module_fingerprint(module: &syn::ItemMod) -> String {
    let mut prelude = module.clone();
    prelude.content = Some((Default::default(), Vec::new()));
    fingerprint(&Item::Mod(prelude))
}

/// Render the parts of an extern block that its members do not change.
pub(crate) fn block_fingerprint(block: &syn::ItemForeignMod) -> String {
    let mut prelude = block.clone();
    prelude.items = Vec::new();
    fingerprint(&Item::ForeignMod(prelude))
}

/// Render the parts of an impl block that its members do not change.
pub(crate) fn impl_fingerprint(implementation: &syn::ItemImpl) -> String {
    let mut prelude = implementation.clone();
    prelude.items = Vec::new();
    fingerprint(&Item::Impl(prelude))
}

/// Render one impl member inside the impl block it was found in.
///
/// The wrapper is the block being matched, so both sides render the member in
/// the same position and only the member itself can differ.
pub(crate) fn impl_member_fingerprint(
    member: &syn::ImplItem,
    implementation: &syn::ItemImpl,
) -> String {
    let mut wrapper = implementation.clone();
    wrapper.items = vec![member.clone()];
    fingerprint(&Item::Impl(wrapper))
}

/// Render one extern block member inside the block it was found in.
pub(crate) fn foreign_member_fingerprint(
    member: &syn::ForeignItem,
    block: &syn::ItemForeignMod,
) -> String {
    let mut wrapper = block.clone();
    wrapper.items = vec![member.clone()];
    fingerprint(&Item::ForeignMod(wrapper))
}
