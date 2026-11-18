//! Conservative, rustc-guided tree shaking of an assembled bundle.
//!
//! The pass is driven by `run`, which delegates to the submodules:
//!
//! - `candidate` collects the definitions a batch could take out,
//! - `rules` decides which candidates the compiler already proved dead,
//! - `support` drops the candidates a surviving definition still names,
//! - `remove` applies an accepted batch to the syntax tree,
//! - `compiler` runs the check crate and reads its diagnostics,
//! - `tokens` walks token streams for identifiers and text-rendering macros,
//! - `rename` shortens the generated dependency module names.
//!
//! The proof that a minified bundle is not missing anything lives in
//! `crate::verify`.

mod candidate;
mod compiler;
mod remove;
mod rename;
mod rules;
mod support;
mod tokens;

#[cfg(test)]
mod tests;

use crate::minify::candidate::Candidate;
use crate::minify::compiler::{WorkDir, missing_definitions};
use crate::minify::remove::{remove_candidates, strip_test_items};
use crate::minify::rename::{apply_renames, rename_mangled_modules};
use crate::minify::rules::removable_candidates;
use crate::minify::support::keep_only_unreferenced;
use crate::minify::tokens::ident_positions;
use crate::verify;
use anyhow::{Context, Result, bail};
use std::collections::HashSet;

/// Result of conservative, rustc-guided tree shaking.
#[derive(Debug)]
pub(crate) struct Minified {
    /// Source of the minified bundle, verified to compile on its own.
    pub(crate) source: String,
    /// Definitions that disappeared, counted by the structural verification.
    pub(crate) removed_items: usize,
    /// Documentation comments that were taken out.
    pub(crate) removed_comments: usize,
    /// Number of `cargo rustc` invocations used for diagnostics and validation.
    pub(crate) passes: usize,
    /// Bundler-generated dependency modules that received a short name.
    pub(crate) renames: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub(crate) dead_code: bool,
    pub(crate) test_code: bool,
    pub(crate) comments: bool,
}

/// Remove items which rustc reports as dead from an already assembled bundle.
///
/// `manifest` must describe the same temporary check crate that will be used
/// for the final validation. The working directory and Cargo target directory
/// are reused across passes, so a fixpoint normally requires only incremental
/// checks. A failed speculative pass is rolled back to the last source that
/// successfully compiled.
///
/// Before returning, the minified source is compared against the source this
/// function was given: the comparison proves that every definition of the
/// output is a definition of the input with the same text, and that every
/// disappearance is reported. A failure aborts instead of emitting a bundle
/// that cannot be accounted for.
pub(crate) fn run(source: &str, manifest: &str, options: Options) -> Result<Minified> {
    let mut file = syn::parse_file(source).context("failed to parse bundle before minifying")?;
    let work = WorkDir::create()?;
    work.prepare(manifest)?;

    let mut last_good = prettyplease::unparse(&file);
    let mut passes = 0;

    work.write_source(&last_good)?;
    let mut check = work.check()?;
    passes += 1;
    if !check.success {
        bail!(
            "cannot minify a bundle that does not pass cargo check\n{}",
            check.errors
        );
    }

    if options.test_code {
        let removed = strip_test_items(&mut file.items);
        if removed > 0 {
            let speculative = prettyplease::unparse(&file);
            work.write_source(&speculative)?;
            let validation = work.check()?;
            passes += 1;
            if validation.success {
                last_good = speculative;
                check = validation;
            } else {
                return verified_result(&work, source, source, passes);
            }
        }
    }

    let mut comments = 0;
    if options.comments {
        comments = verify::strip_doc_comments(&mut file);
        if comments > 0 {
            let speculative = prettyplease::unparse(&file);
            work.write_source(&speculative)?;
            let validation = work.check()?;
            passes += 1;
            if validation.success {
                last_good = speculative;
                check = validation;
            } else {
                // Documentation comments are the only thing this pass removes,
                // so a failure means the bundle needs one of them. The next
                // round starts from the last validated source, which still has
                // them.
                comments = 0;
            }
        }
    }

    if !options.dead_code {
        return verified_result(&work, source, &last_good, passes).map(|mut result| {
            result.removed_comments = comments;
            result
        });
    }

    loop {
        // Diagnostic locations refer to the emitted source, not the AST that
        // prettyplease was given. Reparse only between rounds so pending
        // batches continue to refer to the same AST locations.
        file = syn::parse_file(&last_good).context("failed to parse checked bundle")?;
        // syn removes the shebang text before assigning spans, while rustc
        // reports byte offsets in the complete emitted file (newline included).
        let span_offset = file.shebang.as_deref().map_or(0, str::len);
        let mut positions = ident_positions(&last_good);
        let candidates = removable_candidates(&file, &check.diagnostics, span_offset, &positions);
        // The reported batch is popped first: keeping it free of speculative
        // candidates means a rejected guess never breaks it apart.
        let mut batches = vec![
            sorted_candidates(candidates.speculative),
            sorted_candidates(candidates.reported),
        ];
        let mut removed_this_pass = 0;
        while let Some(batch) = batches.pop() {
            let batch = keep_only_unreferenced(
                &file,
                &positions,
                &batch,
                &check.diagnostics.dead_code,
                span_offset,
            );
            if batch.is_empty() {
                continue;
            }
            let mut speculative = file.clone();
            let removed = remove_candidates(&mut speculative, &batch);
            if removed == 0 {
                continue;
            }

            let text = prettyplease::unparse(&speculative);
            work.write_source(&text)?;
            let validation = work.check()?;
            passes += 1;
            if !validation.success {
                // The rejection names the definitions that have to stay, so the
                // retry leaves exactly those out instead of splitting the batch,
                // which costs one more check per candidate.
                let missing = missing_definitions(&validation.errors);
                let blocked = batch
                    .iter()
                    .filter(|candidate| missing.contains(&candidate.name))
                    .count();
                if 0 < blocked && blocked < batch.len() {
                    batches.push(
                        batch
                            .into_iter()
                            .filter(|candidate| !missing.contains(&candidate.name))
                            .collect(),
                    );
                    continue;
                }
                if batch.len() > 1 {
                    let middle = batch.len() / 2;
                    batches.push(batch[middle..].to_vec());
                    batches.push(batch[..middle].to_vec());
                }
                continue;
            }

            file = speculative;
            last_good = text;
            check = validation;
            removed_this_pass += removed;
            // What just went away may have been the only thing naming a pending
            // candidate, so the rest of this round is filtered again.
            positions = ident_positions(&last_good);
            batches.retain_mut(|batch| {
                let supported = keep_only_unreferenced(
                    &file,
                    &positions,
                    batch,
                    &check.diagnostics.dead_code,
                    span_offset,
                );
                *batch = supported;
                !batch.is_empty()
            });
        }

        if removed_this_pass == 0 {
            break;
        }
    }

    let renamed = rename_mangled_modules(&work, &last_good, &mut passes)?;
    // The rename pass changes the spelling of the generated module names, so
    // the structural proof compares against the same spelling.
    let reference = apply_renames(source, &renamed.map)?;
    verified_result(&work, &reference, &renamed.source, passes).map(|mut result| {
        result.renames = renamed.renamed;
        result.removed_comments = comments;
        result
    })
}

/// Build the report from the structural proof and let the check crate confirm
/// that the accepted source still compiles with ordinary lint settings.
fn verified_result(
    work: &WorkDir,
    original: &str,
    minified: &str,
    passes: usize,
) -> Result<Minified> {
    let report = verify::subset(original, minified)?;
    let mut result = Minified {
        source: minified.to_owned(),
        removed_items: report.removed.len(),
        removed_comments: 0,
        passes,
        renames: 0,
    };
    work.finish(&mut result)?;
    Ok(result)
}

fn sorted_candidates(candidates: HashSet<Candidate>) -> Vec<Candidate> {
    let mut candidates = candidates.into_iter().collect::<Vec<_>>();
    candidates.sort();
    candidates
}
