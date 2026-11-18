//! Structural proof that a minified bundle is a sub-forest of its input.
//!
//! The verification parses both sources and walks them in lockstep. Every
//! definition that survives in the output must appear in the input at the same
//! relative position, must keep its defining text, and may only differ by the
//! removal of whole nested definitions. Every definition that is absent from
//! the output is reported as a removal, so a successful run accounts for each
//! disappearance instead of only proving that the output compiles.
//!
//! The comparison uses [`syn`]'s span-insensitive `PartialEq` implementation
//! (the `extra-traits` feature) so that two parses of the same definitions at
//! different byte offsets compare equal.

mod compare;
mod describe;
mod fingerprint;
mod record;

/// Rendering one use tree the way it would appear in source.
pub(crate) use describe::use_tree_text;

use crate::verify::compare::compare_items;
use anyhow::{Context, Result};
use syn::visit_mut::VisitMut;

/// One definition that disappeared from the bundle.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Unit {
    /// Module path the definition was reachable through, `crate` for the root.
    pub(crate) path: Vec<String>,
    /// Human readable item kind, e.g. `"function"`.
    pub(crate) kind: &'static str,
    /// The definition name.
    pub(crate) name: String,
}

impl std::fmt::Display for Unit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.path.is_empty() {
            write!(formatter, "{} `{}`", self.kind, self.name)
        } else {
            write!(
                formatter,
                "{} `{}::{}`",
                self.kind,
                self.path.join("::"),
                self.name
            )
        }
    }
}

impl Unit {
    fn new(path: &[String], kind: &'static str, name: impl Into<String>) -> Self {
        Self {
            path: path.to_vec(),
            kind,
            name: name.into(),
        }
    }
}

/// Outcome of comparing a minified bundle against the source it came from.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// Definitions that are absent from the minified bundle.
    pub(crate) removed: Vec<Unit>,
    /// Definitions that survived unchanged.
    pub(crate) kept: usize,
}

/// Compare `minified` against `original` and return every vanished definition.
///
/// Documentation comments are stripped from both sides first, so the comparison
/// is about definitions and their text, not about what rustdoc would show. A
/// bundle that lost an attribute that was not a doc comment still fails.
///
/// Definitions are compared by their rendered text instead of `syn`'s equality:
/// `syn` compares the spans of punctuation, so the same definition parsed at two
/// different offsets would compare unequal. Rendering is the only comparison
/// that stays independent of where the text sat in the file.
pub(crate) fn subset(original: &str, minified: &str) -> Result<Report> {
    let mut original =
        syn::parse_file(original).context("failed to parse the pre-minify bundle")?;
    let mut minified = syn::parse_file(minified).context("failed to parse the minified bundle")?;
    strip_doc_comments(&mut original);
    strip_doc_comments(&mut minified);
    let mut report = Report::default();
    compare_items(
        &original.items,
        &minified.items,
        &[],
        &format!("pre-minify bundle ({})", original.items.len()),
        &format!("minified bundle ({})", minified.items.len()),
        &mut report,
    )?;
    Ok(report)
}

/// Whether an attribute is a documentation comment or a doc attribute.
///
/// `///` and `//!` comments are parsed into `#[doc]` attributes, and every form
/// of a doc attribute is read by rustdoc alone.
fn is_doc_comment(attribute: &syn::Attribute) -> bool {
    attribute.path().is_ident("doc")
}

/// Remove every documentation comment from a parsed bundle.
///
/// Returns how many were taken out. The walk uses `syn`'s visitor so that items,
/// statements, fields, variants and impl members are all covered by one rule.
pub(crate) fn strip_doc_comments(file: &mut syn::File) -> usize {
    let mut stripper = DocCommentStripper::default();
    stripper.visit_file_mut(file);
    stripper.removed
}

#[derive(Default)]
struct DocCommentStripper {
    removed: usize,
}

impl VisitMut for DocCommentStripper {
    fn visit_attributes_mut(&mut self, attributes: &mut Vec<syn::Attribute>) {
        let before = attributes.len();
        attributes.retain(|attribute| !is_doc_comment(attribute));
        self.removed += before - attributes.len();
        for attribute in attributes.iter_mut() {
            self.visit_attribute_mut(attribute);
        }
    }
}

#[cfg(test)]
mod tests;
