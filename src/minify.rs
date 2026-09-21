use anyhow::{Context, Result, bail};
use cargo_metadata::Message;
use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufReader, Cursor};
use std::path::PathBuf;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};
use syn::{Attribute, File, Item, Meta};

static MINIFY_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Result of conservative, rustc-guided tree shaking.
#[derive(Debug)]
pub(crate) struct Minified {
    pub(crate) source: String,
    pub(crate) removed_items: usize,
    pub(crate) passes: usize,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Options {
    pub(crate) dead_code: bool,
    pub(crate) test_code: bool,
}

/// Remove items which rustc reports as dead from an already assembled bundle.
///
/// `manifest` must describe the same temporary check crate that will be used
/// for the final validation. The working directory and Cargo target directory
/// are reused across passes, so a fixpoint normally requires only incremental
/// checks. A failed speculative pass is rolled back to the last source that
/// successfully compiled.
pub(crate) fn run(source: &str, manifest: &str, options: Options) -> Result<Minified> {
    let mut file = syn::parse_file(source).context("failed to parse bundle before minifying")?;
    let work = WorkDir::create()?;
    work.prepare(manifest)?;

    let mut last_good = prettyplease::unparse(&file);
    let mut removed_items = 0;
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

    let test_items = if options.test_code {
        strip_test_items(&mut file.items)
    } else {
        0
    };
    if test_items > 0 {
        let speculative = prettyplease::unparse(&file);
        work.write_source(&speculative)?;
        let validation = work.check()?;
        passes += 1;
        if !validation.success {
            return work.finish(Minified {
                source: last_good,
                removed_items,
                passes,
            });
        }
        removed_items += test_items;
        last_good = speculative;
        check = validation;
    }

    if !options.dead_code {
        return work.finish(Minified {
            source: last_good,
            removed_items,
            passes,
        });
    }

    loop {
        let mut candidates = removable_candidates(&file, &check.dead_items)
            .into_iter()
            .collect::<Vec<_>>();
        candidates.sort();
        let mut removed_this_pass = 0;
        let mut batches = vec![candidates];
        while let Some(batch) = batches.pop() {
            let mut speculative_file = file.clone();
            let removed =
                remove_unique_candidates(&mut speculative_file, &batch.iter().cloned().collect());
            if removed == 0 {
                continue;
            }

            let speculative = prettyplease::unparse(&speculative_file);
            work.write_source(&speculative)?;
            let validation = work.check()?;
            passes += 1;
            if !validation.success {
                if batch.len() > 1 {
                    let middle = batch.len() / 2;
                    batches.push(batch[middle..].to_vec());
                    batches.push(batch[..middle].to_vec());
                }
                continue;
            }

            file = speculative_file;
            last_good = speculative;
            removed_items += removed;
            removed_this_pass += removed;
            check = validation;
        }

        if removed_this_pass == 0 {
            return work.finish(Minified {
                source: last_good,
                removed_items,
                passes,
            });
        }
    }
}

/// Remove code which is unconditionally absent from a normal binary build.
///
/// rustc cannot report these items as dead because `cfg(test)` removes them
/// before linting. The exact predicate is deliberately narrow: compound cfg
/// expressions and namespaced test attributes are left untouched.
fn strip_test_items(items: &mut Vec<Item>) -> usize {
    let mut removed = 0;
    let mut kept = Vec::with_capacity(items.len());

    for mut item in items.drain(..) {
        if item_attrs_for_test(&item).is_some_and(is_test_only) {
            removed += 1;
            continue;
        }

        if let Item::Mod(module) = &mut item
            && let Some((_, nested)) = &mut module.content
        {
            removed += strip_test_items(nested);
        }
        kept.push(item);
    }

    *items = kept;
    removed
}

fn is_test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().is_ident("test") {
            return true;
        }
        let Meta::List(meta) = &attr.meta else {
            return false;
        };
        if !meta.path.is_ident("cfg") {
            return false;
        }
        syn::parse2::<Meta>(meta.tokens.clone())
            .is_ok_and(|meta| matches!(meta, Meta::Path(path) if path.is_ident("test")))
    })
}

fn item_attrs_for_test(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Const(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::ExternCrate(item) => Some(&item.attrs),
        Item::Fn(item) => Some(&item.attrs),
        Item::ForeignMod(item) => Some(&item.attrs),
        Item::Impl(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        Item::Mod(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Trait(item) => Some(&item.attrs),
        Item::TraitAlias(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Union(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        Item::Verbatim(_) => None,
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
enum ItemKind {
    Function,
    Constant,
    Static,
    Struct,
    Enum,
    TypeAlias,
    Module,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Candidate {
    kind: ItemKind,
    name: String,
}

fn candidate_from_diagnostic(diagnostic: &Diagnostic) -> Option<Candidate> {
    if diagnostic.level != DiagnosticLevel::Warning || diagnostic.code.as_ref()?.code != "dead_code"
    {
        return None;
    }

    let (prefix, kind) = [
        ("function `", ItemKind::Function),
        ("constant `", ItemKind::Constant),
        ("static `", ItemKind::Static),
        ("struct `", ItemKind::Struct),
        ("enum `", ItemKind::Enum),
        ("type alias `", ItemKind::TypeAlias),
        ("module `", ItemKind::Module),
    ]
    .into_iter()
    .find(|(prefix, _)| diagnostic.message.starts_with(prefix))?;

    let rest = diagnostic.message.strip_prefix(prefix)?;
    let name = rest.split('`').next()?;
    if name.is_empty() {
        return None;
    }

    Some(Candidate {
        kind,
        name: name.to_owned(),
    })
}

/// Remove a diagnostic candidate only when its kind/name pair identifies one
/// AST item in the whole bundle. This deliberately gives up some size savings
/// for duplicate names rather than risking removal from the wrong module.
fn remove_unique_candidates(file: &mut File, candidates: &HashSet<Candidate>) -> usize {
    let removable = removable_candidates(file, candidates);
    remove_items(&mut file.items, &removable)
}

fn removable_candidates(file: &File, candidates: &HashSet<Candidate>) -> HashSet<Candidate> {
    let mut counts = HashMap::<Candidate, usize>::new();
    count_items(&file.items, &mut counts);
    let mut impl_types = HashSet::new();
    collect_impl_types(&file.items, &mut impl_types);
    candidates
        .iter()
        .filter(|candidate| counts.get(*candidate) == Some(&1))
        .filter(|candidate| {
            !matches!(
                candidate.kind,
                ItemKind::Struct | ItemKind::Enum | ItemKind::TypeAlias
            ) || !impl_types.contains(&candidate.name)
        })
        .filter(|candidate| item_is_removable(&file.items, candidate))
        .cloned()
        .collect()
}

fn item_is_removable(items: &[Item], candidate: &Candidate) -> bool {
    items.iter().any(|item| {
        item_candidate(item).as_ref() == Some(candidate) && item_is_safe_to_remove(item)
            || matches!(item, Item::Mod(module) if module.content.as_ref().is_some_and(|(_, nested)| item_is_removable(nested, candidate)))
    })
}

fn collect_impl_types(items: &[Item], names: &mut HashSet<String>) {
    for item in items {
        match item {
            Item::Impl(item) => {
                if let syn::Type::Path(path) = item.self_ty.as_ref()
                    && let Some(segment) = path.path.segments.last()
                {
                    names.insert(segment.ident.to_string());
                }
            }
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_impl_types(nested, names);
                }
            }
            _ => {}
        }
    }
}

fn count_items(items: &[Item], counts: &mut HashMap<Candidate, usize>) {
    for item in items {
        if let Some(candidate) = item_candidate(item) {
            *counts.entry(candidate).or_default() += 1;
        }
        if let Item::Mod(module) = item
            && let Some((_, nested)) = &module.content
        {
            count_items(nested, counts);
        }
    }
}

fn remove_items(items: &mut Vec<Item>, candidates: &HashSet<Candidate>) -> usize {
    let mut removed = 0;
    let mut kept = Vec::with_capacity(items.len());

    for mut item in items.drain(..) {
        let should_remove = item_candidate(&item)
            .is_some_and(|candidate| candidates.contains(&candidate))
            && item_is_safe_to_remove(&item);

        if should_remove {
            removed += 1;
            continue;
        }

        if let Item::Mod(module) = &mut item
            && let Some((_, nested)) = &mut module.content
        {
            removed += remove_items(nested, candidates);
        }
        kept.push(item);
    }

    *items = kept;
    removed
}

fn item_candidate(item: &Item) -> Option<Candidate> {
    let (kind, name) = match item {
        Item::Fn(item) => (ItemKind::Function, item.sig.ident.to_string()),
        Item::Const(item) => (ItemKind::Constant, item.ident.to_string()),
        Item::Static(item) => (ItemKind::Static, item.ident.to_string()),
        Item::Struct(item) => (ItemKind::Struct, item.ident.to_string()),
        Item::Enum(item) => (ItemKind::Enum, item.ident.to_string()),
        Item::Type(item) => (ItemKind::TypeAlias, item.ident.to_string()),
        Item::Mod(item) => (ItemKind::Module, item.ident.to_string()),
        _ => return None,
    };
    Some(Candidate { kind, name })
}

fn item_is_safe_to_remove(item: &Item) -> bool {
    let attrs = match item {
        Item::Fn(item) => &item.attrs,
        Item::Const(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Mod(item) => {
            let Some((_, nested)) = &item.content else {
                return false;
            };
            if !module_contents_are_safe(nested) {
                return false;
            }
            &item.attrs
        }
        _ => return false,
    };

    attrs_are_safe(attrs)
}

fn module_contents_are_safe(items: &[Item]) -> bool {
    items.iter().all(|item| match item {
        Item::Fn(_)
        | Item::Const(_)
        | Item::Static(_)
        | Item::Struct(_)
        | Item::Enum(_)
        | Item::Type(_)
        | Item::Use(_)
        | Item::Macro(_) => item_attrs(item).is_none_or(attrs_are_safe),
        Item::Mod(module) => {
            attrs_are_safe(&module.attrs)
                && module
                    .content
                    .as_ref()
                    .is_some_and(|(_, items)| module_contents_are_safe(items))
        }
        _ => false,
    })
}

fn item_attrs(item: &Item) -> Option<&[Attribute]> {
    match item {
        Item::Fn(item) => Some(&item.attrs),
        Item::Const(item) => Some(&item.attrs),
        Item::Static(item) => Some(&item.attrs),
        Item::Struct(item) => Some(&item.attrs),
        Item::Enum(item) => Some(&item.attrs),
        Item::Type(item) => Some(&item.attrs),
        Item::Use(item) => Some(&item.attrs),
        Item::Macro(item) => Some(&item.attrs),
        _ => None,
    }
}

fn attrs_are_safe(attrs: &[Attribute]) -> bool {
    attrs.iter().all(|attr| {
        let Some(ident) = attr.path().get_ident() else {
            return false;
        };
        matches!(
            ident.to_string().as_str(),
            "allow"
                | "warn"
                | "deny"
                | "forbid"
                | "cfg"
                | "doc"
                | "inline"
                | "cold"
                | "must_use"
                | "deprecated"
                | "repr"
                | "non_exhaustive"
        )
    })
}

struct CheckResult {
    success: bool,
    dead_items: HashSet<Candidate>,
    errors: String,
}

struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn finish(&self, mut result: Minified) -> Result<Minified> {
        // Restore last-good source after a rejected batch, and validate with
        // ordinary lint settings: --force-warn would override deny(dead_code).
        self.write_source(&result.source)?;
        let output = Command::new("cargo")
            .args(["check", "--quiet"])
            .arg("--manifest-path")
            .arg(self.path.join("Cargo.toml"))
            .current_dir(&self.path)
            .output()
            .context("failed to execute final minify validation")?;
        result.passes += 1;
        if !output.status.success() {
            bail!(
                "cargo check failed after minification\n{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }
        Ok(result)
    }
    fn create() -> Result<Self> {
        let base = std::env::temp_dir();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let pid = std::process::id();

        for _ in 0..128 {
            let sequence = MINIFY_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("splicedown-minify-{pid}-{timestamp}-{sequence}"));
            match fs::create_dir(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create minify directory {}", path.display())
                    });
                }
            }
        }
        bail!(
            "failed to create a unique minify directory in {}",
            base.display()
        )
    }

    fn prepare(&self, manifest: &str) -> Result<()> {
        fs::create_dir(self.path.join("src"))
            .with_context(|| format!("failed to create {}/src", self.path.display()))?;
        fs::write(self.path.join("Cargo.toml"), manifest)
            .with_context(|| format!("failed to write {}/Cargo.toml", self.path.display()))
    }

    fn write_source(&self, source: &str) -> Result<()> {
        fs::write(self.path.join("src/main.rs"), source)
            .with_context(|| format!("failed to write {}/src/main.rs", self.path.display()))
    }

    fn check(&self) -> Result<CheckResult> {
        let mut command = Command::new("cargo");
        command
            .arg("check")
            .arg("--quiet")
            .arg("--message-format=json")
            .arg("--manifest-path")
            .arg(self.path.join("Cargo.toml"))
            .current_dir(&self.path);
        append_dead_code_rustflag(&mut command);
        let output = command
            .output()
            .context("failed to execute cargo check for --minify")?;
        parse_check_output(&output)
    }
}

fn append_dead_code_rustflag(command: &mut Command) {
    const FORCE_DEAD_CODE: &str = "--force-warn=dead_code";

    if let Some(mut flags) = std::env::var_os("CARGO_ENCODED_RUSTFLAGS") {
        if !flags.is_empty() {
            flags.push("\x1f");
        }
        flags.push(FORCE_DEAD_CODE);
        command.env("CARGO_ENCODED_RUSTFLAGS", flags);
    } else {
        let mut flags = std::env::var_os("RUSTFLAGS").unwrap_or_default();
        if !flags.is_empty() {
            flags.push(" ");
        }
        flags.push(FORCE_DEAD_CODE);
        command.env("RUSTFLAGS", flags);
    }
}

impl Drop for WorkDir {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "splicedown: warning: failed to remove minify directory {}: {}",
                self.path.display(),
                error
            );
        }
    }
}

fn parse_check_output(output: &Output) -> Result<CheckResult> {
    let mut dead_items = HashSet::new();
    let mut errors = String::new();
    let reader = BufReader::new(Cursor::new(&output.stdout));

    for message in Message::parse_stream(reader) {
        let message = message.context("failed to parse cargo diagnostics for --minify")?;
        let Message::CompilerMessage(message) = message else {
            continue;
        };
        if message.target.name == "splicedown-check"
            && let Some(candidate) = candidate_from_diagnostic(&message.message)
        {
            dead_items.insert(candidate);
        }
        if matches!(
            message.message.level,
            DiagnosticLevel::Error | DiagnosticLevel::Ice
        ) && let Some(rendered) = &message.message.rendered
        {
            errors.push_str(rendered);
        }
    }

    if !output.stderr.is_empty() {
        errors.push_str(&String::from_utf8_lossy(&output.stderr));
    }

    Ok(CheckResult {
        success: output.status.success(),
        dead_items,
        errors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidates(entries: &[(ItemKind, &str)]) -> HashSet<Candidate> {
        entries
            .iter()
            .map(|(kind, name)| Candidate {
                kind: *kind,
                name: (*name).to_owned(),
            })
            .collect()
    }

    #[test]
    fn removes_only_unique_safe_items() {
        let mut file = syn::parse_file(
            r#"
            fn dead() {}
            mod a { fn duplicate() {} }
            mod b { fn duplicate() {} }
            #[unsafe(no_mangle)]
            fn exported() {}
            #[cfg_attr(all(), unsafe(link_section = ".init_array"))]
            static REGISTER: extern "C" fn() = exported;
            fn main() {}
            "#,
        )
        .unwrap();
        let dead = candidates(&[
            (ItemKind::Function, "dead"),
            (ItemKind::Function, "duplicate"),
            (ItemKind::Function, "exported"),
            (ItemKind::Static, "REGISTER"),
        ]);

        assert_eq!(remove_unique_candidates(&mut file, &dead), 1);
        let output = prettyplease::unparse(&file);
        assert!(!output.contains("fn dead"));
        assert_eq!(output.matches("fn duplicate").count(), 2);
        assert!(output.contains("fn exported"));
        assert!(output.contains("static REGISTER"));
    }

    #[test]
    fn strips_exact_test_items_recursively() {
        let mut file = syn::parse_file(
            r#"
            #[cfg(test)]
            mod tests { #[test] fn root_test() {} }

            mod nested {
                #[cfg(test)]
                fn helper() {}

                #[test]
                fn direct_test() {}

                #[cfg(any(test, feature = "extra"))]
                fn compound_cfg_is_preserved() {}

                #[cfg_attr(test, allow(dead_code))]
                fn cfg_attr_is_preserved() {}

                fn live() {}
            }
            "#,
        )
        .unwrap();

        assert_eq!(strip_test_items(&mut file.items), 3);
        let output = prettyplease::unparse(&file);
        assert!(!output.contains("root_test"));
        assert!(!output.contains("fn helper"));
        assert!(!output.contains("direct_test"));
        assert!(output.contains("compound_cfg_is_preserved"));
        assert!(output.contains("cfg_attr_is_preserved"));
        assert!(output.contains("fn live"));
    }

    #[test]
    fn does_not_remove_module_containing_impl() {
        let mut file = syn::parse_file(
            "mod behavior { struct S; impl S { fn new() -> Self { S } } } fn main() {}",
        )
        .unwrap();
        let dead = candidates(&[(ItemKind::Module, "behavior")]);

        assert_eq!(remove_unique_candidates(&mut file, &dead), 0);
        assert!(prettyplease::unparse(&file).contains("mod behavior"));
    }

    #[test]
    fn does_not_remove_a_type_with_an_impl() {
        let mut file = syn::parse_file(
            "struct S; impl S { fn new() -> Self { S } } fn dead() {} fn main() {}",
        )
        .unwrap();
        let dead = candidates(&[(ItemKind::Struct, "S"), (ItemKind::Function, "dead")]);

        assert_eq!(remove_unique_candidates(&mut file, &dead), 1);
        let output = prettyplease::unparse(&file);
        assert!(output.contains("struct S"));
        assert!(!output.contains("fn dead"));
    }

    #[test]
    fn minifies_to_a_checked_fixpoint() {
        let source = r#"
            #![allow(dead_code)]
            fn transitively_dead() { leaf(); }
            fn leaf() {}
            fn live() {}
            #[cfg(test)]
            mod tests { #[test] fn smoke() { super::live(); } }
            fn main() { live(); }
        "#;
        let manifest = concat!(
            "[package]\n",
            "name = \"splicedown-check\"\n",
            "version = \"0.0.0\"\n",
            "edition = \"2024\"\n\n",
            "[workspace]\n",
        );

        let result = run(
            source,
            manifest,
            Options {
                dead_code: true,
                test_code: true,
            },
        )
        .unwrap();

        assert_eq!(result.removed_items, 3);
        assert!(!result.source.contains("transitively_dead"));
        assert!(!result.source.contains("fn leaf"));
        assert!(!result.source.contains("mod tests"));
        assert!(result.source.contains("fn live"));
        assert!(result.source.contains("fn main"));
    }

    #[test]
    fn batches_independent_candidates_without_rechecking_the_same_source() {
        let mut source = String::from("#![allow(dead_code)]\nfn main() {}\n");
        for n in 0..20 {
            source.push_str(&format!("fn unused_{n}() {{}}\n"));
        }
        let result = run(&source,
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: true, test_code: false }).unwrap();
        assert_eq!(result.removed_items, 20);
        // Initial diagnostics, one batch, final ordinary-lint validation.
        assert_eq!(result.passes, 3);
    }

    #[test]
    fn does_not_check_candidates_that_cannot_be_removed_from_the_ast() {
        let mut source = String::from("#![allow(dead_code)]\n");
        for n in 0..20 {
            source.push_str(&format!(
                "mod m{n} {{ pub fn live() {{}} fn duplicate() {{}} }}\n"
            ));
        }
        source.push_str("fn main() {");
        for n in 0..20 {
            source.push_str(&format!("m{n}::live();"));
        }
        source.push_str("}\n");
        source.push_str("fn removable() {}\n");
        let result = run(&source,
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: true, test_code: false }).unwrap();
        assert_eq!(result.removed_items, 1);
        assert_eq!(result.passes, 3);
    }

    #[test]
    fn final_validation_respects_denied_dead_code() {
        let error = run("#![deny(dead_code)]\nfn unused() {}\nfn main() {}",
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: false, test_code: true }).unwrap_err();
        assert!(error.to_string().contains("cargo check failed"));
    }

    #[test]
    fn keeps_a_rejected_candidate_but_continues_removing_others() {
        let source = r#"
            #![allow(dead_code)]
            const TRANSITIVE: usize = 1;
            struct Holder;
            impl Holder { fn dead() -> usize { TRANSITIVE } }
            fn removable() {}
            fn main() {}
        "#;
        let manifest = concat!(
            "[package]\n",
            "name = \"splicedown-check\"\n",
            "version = \"0.0.0\"\n",
            "edition = \"2024\"\n\n",
            "[workspace]\n",
        );

        let result = run(
            source,
            manifest,
            Options {
                dead_code: true,
                test_code: true,
            },
        )
        .unwrap();

        assert_eq!(result.removed_items, 1);
        assert!(result.source.contains("const TRANSITIVE"));
        assert!(result.source.contains("impl Holder"));
        assert!(!result.source.contains("fn removable"));
    }

    #[test]
    fn dead_code_and_test_minification_can_be_disabled_independently() {
        let source = r#"
            #![allow(dead_code)]
            fn dead() {}
            #[cfg(test)]
            mod tests { #[test] fn smoke() {} }
            fn main() {}
        "#;
        let manifest = concat!(
            "[package]\n",
            "name = \"splicedown-check\"\n",
            "version = \"0.0.0\"\n",
            "edition = \"2024\"\n\n",
            "[workspace]\n",
        );

        let tests_only = run(
            source,
            manifest,
            Options {
                dead_code: false,
                test_code: true,
            },
        )
        .unwrap();
        assert!(tests_only.source.contains("fn dead"));
        assert!(!tests_only.source.contains("mod tests"));

        let dead_only = run(
            source,
            manifest,
            Options {
                dead_code: true,
                test_code: false,
            },
        )
        .unwrap();
        assert!(!dead_only.source.contains("fn dead"));
        assert!(dead_only.source.contains("mod tests"));
    }
}
