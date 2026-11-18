//! Macro handling for the bundled source.
//!
//! [`exports`] keeps `#[macro_export]` definitions addressable inside their
//! wrapper module; [`tokens`] rewrites paths in the token streams that syn's
//! normal AST visitors never reach.

mod exports;
mod tokens;

#[cfg(test)]
mod tests;

pub(crate) use exports::fix_macro_exports;
pub(crate) use tokens::rewrite_macro_tokens;
