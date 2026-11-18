mod assemble;
mod check;
mod cli;
mod header;
mod inline;
mod macros;
mod metadata;
mod minify;
mod preset;
mod rewrite;
mod util;
mod verify;

use crate::metadata::Plan;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use tracing::{info, warn};
pub async fn run() -> Result<()> {
    execute(cli::parse())
}

fn execute(cli: cli::Cli) -> Result<()> {
    let entry = absolute_file(&cli.entry).context("[input] failed to resolve entry file")?;
    let manifest = resolve_manifest(&entry, cli.manifest_path.as_deref())
        .context("[input] failed to resolve Cargo.toml")?;
    let plan = Plan::collect(&manifest, &entry, &cli.exclude, &cli.exclude_preset)
        .context("[metadata] failed to collect the dependency graph")?;
    warn_about_proc_macros(&plan);

    let (mut source, omitted_deps) = bundle(&plan, cli.minify_enabled())
        .context("[bundle] failed to build the bundled source")?;
    // Read the bundled modules before minifying: the minify pass shortens the
    // generated module names, so the header is rendered from the unminified
    // bundle and stays attached to the right packages.
    let mut bundled_deps = bundled_deps_in_source(&plan, &source)?;

    let minify = minify::Options {
        dead_code: cli.minify_enabled(),
        test_code: cli.minify_test_enabled(),
        comments: cli.minify_comment_enabled(),
    };
    let mut check_packages = check::referenced_packages(&source, &plan.skip_pkgs)?;
    let mut validated = false;
    if minify.dead_code || minify.test_code {
        let manifest = check::manifest_toml(&check_packages)
            .context("[minify] failed to prepare the validation manifest")?;
        let result = minify::run(&source, &manifest, minify)
            .or_else(|error| {
                if omitted_deps {
                    // Proc macros can synthesize references that are absent from
                    // the source tokens used to select bundled dependencies.
                    let (full_source, _) = bundle(&plan, false)?;
                    source = full_source;
                    bundled_deps = bundled_deps_in_source(&plan, &source)?;
                    check_packages = plan.skip_pkgs.clone();
                    return minify::run(&source, &check::manifest_toml(&check_packages)?, minify);
                }
                if check_packages.len() == plan.skip_pkgs.len() {
                    return Err(error);
                }
                // Macro expansion can synthesize references to external crates
                // that are absent from source tokens.
                check_packages = plan.skip_pkgs.clone();
                minify::run(&source, &check::manifest_toml(&check_packages)?, minify)
            })
            .context("[minify] failed to remove unused bundled items")?;
        info!(
            "minified bundle: removed {} definitions and {} doc comments in {} compiler passes, \
             shortened {} dependency modules, verified as a subset of the bundle",
            result.removed_items, result.removed_comments, result.passes, result.renames
        );
        source = result.source;
        validated = true;
    }

    source = prepend_header(&source, &header::render(&bundled_deps));

    if !cli.no_check && (!validated || cli.keep_check_dir) {
        check::run(&source, &check_packages, cli.keep_check_dir)
            .or_else(|error| {
                if check_packages.len() == plan.skip_pkgs.len() {
                    return Err(error);
                }
                check::run(&source, &plan.skip_pkgs, cli.keep_check_dir)
            })
            .context("[check] generated source validation failed")?;
    }

    emit_output(&source, cli.output.as_deref()).context("[output] failed to emit bundled source")
}

fn warn_about_proc_macros(plan: &Plan) {
    for package in &plan.skip_pkgs {
        if metadata::is_proc_macro(package) {
            warn!(
                "proc-macro dependency {} v{} remains external; it must be available in the judge environment",
                package.name, package.version
            );
        }
    }
}

fn bundled_deps_in_source(plan: &Plan, source: &str) -> Result<Vec<metadata::Dep>> {
    let file = syn::parse_file(source).context("failed to inspect the assembled bundle")?;
    let module_names = file
        .items
        .iter()
        .filter_map(|item| match item {
            syn::Item::Mod(module) => Some(module.ident.to_string()),
            _ => None,
        })
        .collect::<std::collections::HashSet<_>>();

    Ok(plan
        .deps
        .iter()
        .filter(|dep| module_names.contains(&dep.mangled.to_string()))
        .cloned()
        .collect())
}

fn prepend_header(source: &str, header: &str) -> String {
    if source.starts_with("#!")
        && !source.starts_with("#![")
        && let Some(newline) = source.find('\n')
    {
        let (shebang, rest) = source.split_at(newline + 1);
        return format!("{shebang}{header}{rest}");
    }
    format!("{header}{source}")
}

fn bundle(plan: &Plan, prune_deps: bool) -> Result<(String, bool)> {
    let mut main = inline::load_and_inline(plan.entry.src.as_std_path())
        .with_context(|| format!("[inline] failed to load entry {}", plan.entry.src))?;
    rewrite::rewrite(&mut main, &plan.entry.extern_map, None);
    macros::rewrite_macro_tokens(&mut main, &plan.entry.extern_map, None);

    let mut loaded = std::collections::HashMap::<String, syn::File>::new();
    if prune_deps {
        let mut reachable = check::referenced_identifiers(&main)
            .context("failed to scan bundled dependency references in the entry")?;
        while let Some(dep) = plan.deps.iter().find(|dep| {
            let name = dep.mangled.to_string();
            !loaded.contains_key(&name) && reachable.contains(&name)
        }) {
            let file = load_dependency(dep)?;
            reachable.extend(
                check::referenced_identifiers(&file)
                    .with_context(|| format!("failed to scan dependency {}", dep.pkg.name))?,
            );
            loaded.insert(dep.mangled.to_string(), file);
        }
    } else {
        for dep in &plan.deps {
            loaded.insert(dep.mangled.to_string(), load_dependency(dep)?);
        }
    }

    let omitted_deps = loaded.len() != plan.deps.len();
    let deps = plan
        .deps
        .iter()
        .filter_map(|dep| {
            loaded
                .remove(&dep.mangled.to_string())
                .map(|file| (dep.mangled.clone(), file))
        })
        .collect::<Vec<_>>();

    Ok((
        prettyplease::unparse(&assemble::assemble(main, deps)),
        omitted_deps,
    ))
}

fn load_dependency(dep: &metadata::Dep) -> Result<syn::File> {
    let mut file = inline::load_and_inline(dep.lib_src.as_std_path()).with_context(|| {
        format!(
            "[inline] failed to load dependency {} {} from {}",
            dep.pkg.name, dep.pkg.version, dep.lib_src
        )
    })?;
    rewrite::rewrite(&mut file, &dep.extern_map, Some(&dep.mangled));
    macros::fix_macro_exports(&mut file).with_context(|| {
        format!(
            "[macro] failed to localize macros in {} {}",
            dep.pkg.name, dep.pkg.version
        )
    })?;
    macros::rewrite_macro_tokens(&mut file, &dep.extern_map, Some(&dep.mangled));
    Ok(file)
}

fn resolve_manifest(entry: &Path, explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return absolute_file(path).context("failed to resolve --manifest-path");
    }

    let mut dir = entry
        .parent()
        .context("entry file has no parent directory")?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.is_file() {
            return candidate
                .canonicalize()
                .with_context(|| format!("failed to resolve {}", candidate.display()));
        }
        let Some(parent) = dir.parent() else {
            break;
        };
        dir = parent;
    }

    bail!(
        "could not find Cargo.toml in {} or any parent directory",
        entry.display()
    )
}

fn absolute_file(path: &Path) -> Result<PathBuf> {
    path.canonicalize()
        .with_context(|| format!("failed to resolve {}", path.display()))
        .and_then(|path| {
            if path.is_file() {
                Ok(path)
            } else {
                bail!("{} is not a file", path.display())
            }
        })
}

fn emit_output(source: &str, output: Option<&Path>) -> Result<()> {
    if let Some(path) = output {
        fs::write(path, source).with_context(|| format!("failed to write {}", path.display()))
    } else {
        let stdout = io::stdout();
        let mut stdout = stdout.lock();
        stdout
            .write_all(source.as_bytes())
            .context("failed to write bundled source to stdout")?;
        stdout.flush().context("failed to flush stdout")
    }
}

#[cfg(test)]
mod tests;
