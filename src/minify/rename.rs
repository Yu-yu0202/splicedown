use super::compiler::WorkDir;
use super::tokens::{ident_positions, text_sensitive_idents};
use anyhow::{Context, Result, bail};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::HashMap;
use std::collections::HashSet;
use std::ops::Range;
use syn::Item;

/// Outcome of giving the generated dependency modules a short name.
pub(crate) struct RenameOutcome {
    pub(crate) source: String,
    pub(crate) map: HashMap<String, String>,
    pub(crate) renamed: usize,
}

/// Rename identifiers by replacing their byte ranges.
///
/// Tokenizer spans keep the edit inside identifier tokens, so string literals,
/// comments and formatting are preserved byte for byte.
pub(crate) fn apply_renames(source: &str, renames: &HashMap<String, String>) -> Result<String> {
    let tokens = source
        .parse::<TokenStream>()
        .map_err(|error| anyhow::anyhow!("failed to tokenize the bundle: {error}"))?;
    let mut edits = Vec::new();
    collect_ident_edits(tokens, renames, &mut edits);
    if edits.is_empty() {
        return Ok(source.to_owned());
    }
    edits.sort_by_key(|(range, _)| range.start);

    let mut out = String::with_capacity(source.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        if range.start < cursor {
            bail!("identifier ranges overlap while renaming to `{replacement}`");
        }
        out.push_str(&source[cursor..range.start]);
        out.push_str(&replacement);
        cursor = range.end;
    }
    out.push_str(&source[cursor..]);
    Ok(out)
}

fn collect_ident_edits(
    tokens: TokenStream,
    renames: &HashMap<String, String>,
    out: &mut Vec<(Range<usize>, String)>,
) {
    for token in tokens {
        match token {
            TokenTree::Ident(ident) => {
                if let Some(replacement) = renames.get(ident.to_string().as_str()) {
                    out.push((ident.span().byte_range(), replacement.clone()));
                }
            }
            TokenTree::Group(group) => collect_ident_edits(group.stream(), renames, out),
            _ => {}
        }
    }
}

/// Give the generated dependency module names a short spelling.
///
/// The mangled names are the longest identifiers in a bundle and appear on
/// every path into a dependency. The rename is validated like any other
/// transformation: if the renamed source does not compile, the renames are
/// retried one by one and the ones that do compile are kept.
pub(crate) fn rename_mangled_modules(
    work: &WorkDir,
    source: &str,
    passes: &mut usize,
) -> Result<RenameOutcome> {
    let file = syn::parse_file(source).context("failed to parse bundle before renaming")?;
    let mut taken = ident_positions(source)
        .keys()
        .cloned()
        .collect::<HashSet<_>>();
    let sensitive = text_sensitive_idents(source);
    let mut map = HashMap::new();
    let mut index = 0;

    for item in &file.items {
        let Item::Mod(module) = item else {
            continue;
        };
        let name = module.ident.to_string();
        if !name.starts_with(crate::metadata::MANGLED_PREFIX) || sensitive.contains(&name) {
            continue;
        }
        let short = loop {
            let candidate = format!("m{index}");
            index += 1;
            if !taken.contains(&candidate) {
                break candidate;
            }
        };
        taken.insert(short.clone());
        map.insert(name, short);
    }

    if map.is_empty() {
        return Ok(RenameOutcome {
            source: source.to_owned(),
            map,
            renamed: 0,
        });
    }

    let renamed = apply_renames(source, &map)?;
    work.write_source(&renamed)?;
    let validation = work.check()?;
    *passes += 1;
    if validation.success {
        return Ok(RenameOutcome {
            source: renamed,
            renamed: map.len(),
            map,
        });
    }

    // A rename only fails when an identifier reaches a place the token rewrite
    // cannot follow. Discover which ones those are by trying them on their own,
    // in a fixed order so that the accepted set does not depend on the hash map.
    let mut retried = map.iter().collect::<Vec<_>>();
    retried.sort_by_key(|(name, _)| *name);
    let mut kept = HashMap::new();
    let mut source_left = source.to_owned();
    for (name, replacement) in retried {
        let mut single = HashMap::new();
        single.insert(name.clone(), replacement.clone());
        let Ok(candidate) = apply_renames(&source_left, &single) else {
            continue;
        };
        work.write_source(&candidate)?;
        let validation = work.check()?;
        *passes += 1;
        if validation.success {
            kept.insert(name.clone(), replacement.clone());
            source_left = candidate;
        }
    }

    Ok(RenameOutcome {
        source: source_left,
        renamed: kept.len(),
        map: kept,
    })
}
