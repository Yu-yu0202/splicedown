use proc_macro::TokenStream;

#[proc_macro_derive(PmDummy)]
pub fn derive_pm_dummy(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}
