use anyhow::{Context, Result, bail};
use cargo_metadata::Message;
use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel};
use std::collections::HashSet;
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
        // Diagnostic locations refer to the emitted source, not the AST that
        // prettyplease was given. Reparse only between rounds so pending
        // batches continue to refer to the same AST locations.
        file = syn::parse_file(&last_good).context("failed to parse checked bundle")?;
        // syn removes the shebang text before assigning spans, while rustc
        // reports byte offsets in the complete emitted file (newline included).
        let span_offset = file.shebang.as_deref().map_or(0, str::len);
        let mut candidates = removable_candidates(&file, &check.dead_items, span_offset)
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
    Method,
    AssociatedConstant,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct Candidate {
    kind: ItemKind,
    name: String,
    byte_range: (usize, usize),
}

// Only primary spans in the checked source can identify removable AST nodes.
// In particular, spans in a macro expansion must not identify its invocation.
fn dead_spans(diagnostic: &Diagnostic, source_path: &std::path::Path) -> HashSet<(usize, usize)> {
    if diagnostic.level != DiagnosticLevel::Warning
        || diagnostic
            .code
            .as_ref()
            .is_none_or(|code| code.code != "dead_code")
    {
        return HashSet::new();
    }
    diagnostic
        .spans
        .iter()
        .filter(|span| span.is_primary && span.expansion.is_none())
        .filter(|span| {
            let path = std::path::Path::new(&span.file_name);
            path == source_path || path == std::path::Path::new("src/main.rs")
        })
        .map(|span| (span.byte_start as usize, span.byte_end as usize))
        .collect()
}

fn remove_unique_candidates(file: &mut File, candidates: &HashSet<Candidate>) -> usize {
    let removable = removable_candidates(
        file,
        &candidates
            .iter()
            .map(|candidate| candidate.byte_range)
            .collect(),
        0,
    );
    remove_items(&mut file.items, &removable)
}

fn removable_candidates(
    file: &File,
    dead_spans: &HashSet<(usize, usize)>,
    span_offset: usize,
) -> HashSet<Candidate> {
    let mut available = Vec::new();
    collect_candidates(&file.items, &mut available);
    let mut impl_types = HashSet::new();
    collect_impl_types(&file.items, &mut impl_types);
    available
        .into_iter()
        .filter(|candidate| {
            dead_spans.contains(&(
                candidate.byte_range.0 + span_offset,
                candidate.byte_range.1 + span_offset,
            )) && item_is_removable(&file.items, candidate)
                && (!matches!(
                    candidate.kind,
                    ItemKind::Struct | ItemKind::Enum | ItemKind::TypeAlias
                ) || !impl_types.contains(&candidate.name))
        })
        .collect()
}

fn item_is_removable(items: &[Item], candidate: &Candidate) -> bool {
    items.iter().any(|item| {
        item_candidate(item).as_ref() == Some(candidate) && item_is_safe_to_remove(item)
            || matches!(item, Item::Mod(module) if module.content.as_ref().is_some_and(|(_, nested)| item_is_removable(nested, candidate)))
            || matches!(item, Item::Impl(implementation) if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) && implementation.items.iter().any(|member| impl_candidate(member).as_ref() == Some(candidate) && impl_item_is_safe(member)))
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

fn collect_candidates(items: &[Item], out: &mut Vec<Candidate>) {
    for item in items {
        if let Some(candidate) = item_candidate(item) {
            out.push(candidate);
        }
        match item {
            Item::Mod(module) => {
                if let Some((_, nested)) = &module.content {
                    collect_candidates(nested, out);
                }
            }
            Item::Impl(implementation)
                if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) =>
            {
                out.extend(implementation.items.iter().filter_map(impl_candidate));
            }
            _ => {}
        }
    }
}

fn remove_items(items: &mut Vec<Item>, candidates: &HashSet<Candidate>) -> usize {
    let mut removed = 0;
    let mut kept = Vec::with_capacity(items.len());
    for mut item in items.drain(..) {
        if item_candidate(&item).is_some_and(|candidate| candidates.contains(&candidate))
            && item_is_safe_to_remove(&item)
        {
            removed += 1;
            continue;
        }
        match &mut item {
            Item::Mod(module) => {
                if let Some((_, nested)) = &mut module.content {
                    removed += remove_items(nested, candidates);
                }
            }
            Item::Impl(implementation)
                if implementation.trait_.is_none() && attrs_are_safe(&implementation.attrs) =>
            {
                let before = implementation.items.len();
                implementation.items.retain(|member| {
                    !impl_candidate(member).is_some_and(|candidate| candidates.contains(&candidate))
                        || !impl_item_is_safe(member)
                });
                removed += before - implementation.items.len();
                if implementation.items.is_empty() && attrs_are_safe(&implementation.attrs) {
                    continue;
                }
            }
            _ => {}
        }
        kept.push(item);
    }
    *items = kept;
    removed
}

fn ident_candidate(kind: ItemKind, ident: &syn::Ident) -> Candidate {
    let range = ident.span().byte_range();
    Candidate {
        kind,
        name: ident.to_string(),
        byte_range: (range.start, range.end),
    }
}

fn item_candidate(item: &Item) -> Option<Candidate> {
    Some(match item {
        Item::Fn(item) => ident_candidate(ItemKind::Function, &item.sig.ident),
        Item::Const(item) => ident_candidate(ItemKind::Constant, &item.ident),
        Item::Static(item) => ident_candidate(ItemKind::Static, &item.ident),
        Item::Struct(item) => ident_candidate(ItemKind::Struct, &item.ident),
        Item::Enum(item) => ident_candidate(ItemKind::Enum, &item.ident),
        Item::Type(item) => ident_candidate(ItemKind::TypeAlias, &item.ident),
        Item::Mod(item) => ident_candidate(ItemKind::Module, &item.ident),
        _ => return None,
    })
}

fn impl_candidate(item: &syn::ImplItem) -> Option<Candidate> {
    Some(match item {
        syn::ImplItem::Fn(item) => ident_candidate(ItemKind::Method, &item.sig.ident),
        syn::ImplItem::Const(item) => ident_candidate(ItemKind::AssociatedConstant, &item.ident),
        _ => return None,
    })
}

fn impl_item_is_safe(item: &syn::ImplItem) -> bool {
    match item {
        syn::ImplItem::Fn(item) => attrs_are_safe(&item.attrs),
        syn::ImplItem::Const(item) => attrs_are_safe(&item.attrs),
        _ => false,
    }
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
    dead_items: HashSet<(usize, usize)>,
    errors: String,
}

struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    fn target_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("minify directory has a UTF-8 name")
    }

    fn finish(&self, mut result: Minified) -> Result<Minified> {
        // Restore last-good source after a rejected batch, and validate with
        // ordinary lint settings: --force-warn would override deny(dead_code).
        self.write_source(&result.source)?;
        let output = Command::new("cargo")
            .args(["check", "--quiet"])
            .arg("--manifest-path")
            .arg(self.path.join("Cargo.toml"))
            .args(["--bin", self.target_name()])
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
        fs::write(
            self.path.join("Cargo.toml"),
            crate::check::manifest_for_target(manifest, self.target_name()),
        )
        .with_context(|| format!("failed to write {}/Cargo.toml", self.path.display()))
    }

    fn write_source(&self, source: &str) -> Result<()> {
        fs::write(self.path.join("src/main.rs"), source)
            .with_context(|| format!("failed to write {}/src/main.rs", self.path.display()))
    }

    fn check(&self) -> Result<CheckResult> {
        let mut command = Command::new("cargo");
        command
            .arg("rustc")
            .arg("--quiet")
            .args(["--profile", "check"])
            .arg("--message-format=json")
            .arg("--manifest-path")
            .arg(self.path.join("Cargo.toml"))
            .args(["--bin", self.target_name(), "--", "--force-warn=dead_code"])
            .current_dir(&self.path);
        let output = command
            .output()
            .context("failed to execute cargo check for --minify")?;
        parse_check_output(&output, self.target_name(), &self.path.join("src/main.rs"))
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

fn parse_check_output(
    output: &Output,
    target_name: &str,
    source_path: &std::path::Path,
) -> Result<CheckResult> {
    let mut dead_items = HashSet::new();
    let mut errors = String::new();
    let reader = BufReader::new(Cursor::new(&output.stdout));

    for message in Message::parse_stream(reader) {
        let message = message.context("failed to parse cargo diagnostics for --minify")?;
        let Message::CompilerMessage(message) = message else {
            continue;
        };
        if message.target.name == target_name {
            dead_items.extend(dead_spans(&message.message, source_path));
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

    fn candidates(file: &File, entries: &[(ItemKind, &str)]) -> HashSet<Candidate> {
        let mut available = Vec::new();
        collect_candidates(&file.items, &mut available);
        available
            .into_iter()
            .filter(|candidate| {
                entries
                    .iter()
                    .any(|(kind, name)| candidate.kind == *kind && candidate.name == *name)
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
        let dead = candidates(
            &file,
            &[
                (ItemKind::Function, "dead"),
                (ItemKind::Function, "duplicate"),
                (ItemKind::Function, "exported"),
                (ItemKind::Static, "REGISTER"),
            ],
        );

        assert_eq!(remove_unique_candidates(&mut file, &dead), 3);
        let output = prettyplease::unparse(&file);
        assert!(!output.contains("fn dead"));
        assert_eq!(output.matches("fn duplicate").count(), 0);
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
        let dead = candidates(&file, &[(ItemKind::Module, "behavior")]);

        assert_eq!(remove_unique_candidates(&mut file, &dead), 0);
        assert!(prettyplease::unparse(&file).contains("mod behavior"));
    }

    #[test]
    fn does_not_remove_a_type_with_an_impl() {
        let mut file = syn::parse_file(
            "struct S; impl S { fn new() -> Self { S } } fn dead() {} fn main() {}",
        )
        .unwrap();
        let dead = candidates(
            &file,
            &[(ItemKind::Struct, "S"), (ItemKind::Function, "dead")],
        );

        assert_eq!(remove_unique_candidates(&mut file, &dead), 1);
        let output = prettyplease::unparse(&file);
        assert!(output.contains("struct S"));
        assert!(!output.contains("fn dead"));
    }

    #[test]
    fn does_not_remove_members_from_an_attributed_impl() {
        let mut file =
            syn::parse_file("struct S; #[unknown_attribute] impl S { fn dead() {} }").unwrap();
        let dead = candidates(&file, &[(ItemKind::Method, "dead")]);
        assert_eq!(remove_unique_candidates(&mut file, &dead), 0);
        assert!(prettyplease::unparse(&file).contains("fn dead"));
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
    fn minifies_source_with_a_shebang() {
        for shebang in [
            "#!/usr/bin/env rust-script",
            "#!/usr/bin/env rust-script 日本語",
        ] {
            let source =
                format!("{shebang}\n#![allow(dead_code)]\nfn dead() {{}}\nfn main() {{}}\n");
            let result = run(
                &source,
                "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
                Options { dead_code: true, test_code: false },
            ).unwrap();
            assert!(result.source.starts_with(&format!("{shebang}\n")));
            assert!(!result.source.contains("fn dead"), "{}", result.source);
            assert!(result.source.contains("fn main"));
            assert_eq!(result.removed_items, 1);
            assert_eq!(result.passes, 3);
        }
    }

    #[test]
    fn shared_target_directory_checks_each_generated_source() {
        let shared = WorkDir::create().unwrap();
        let valid = WorkDir::create().unwrap();
        let invalid = WorkDir::create().unwrap();
        let manifest = "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n";
        for work in [&valid, &invalid] {
            work.prepare(manifest).unwrap();
            fs::create_dir(work.path.join(".cargo")).unwrap();
            // Both source files exist before the first compilation. Without
            // distinct bin names Cargo can consider the second crate fresh.
            fs::write(
                work.path.join(".cargo/config.toml"),
                format!(
                    "[build]\ntarget-dir = \"../{}/target\"\n",
                    shared.target_name()
                ),
            )
            .unwrap();
        }
        valid.write_source("fn main() {}\n").unwrap();
        invalid
            .write_source("fn main() { missing_symbol(); }\n")
            .unwrap();
        assert!(valid.check().unwrap().success);
        let result = invalid.check().unwrap();
        assert!(!result.success);
        assert!(result.errors.contains("missing_symbol"));
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
        assert_eq!(result.removed_items, 21);
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
            trait T { fn value() -> usize; }
            struct Holder;
            impl T for Holder { fn value() -> usize { TRANSITIVE } }
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
        assert!(result.source.contains("impl T for Holder"));
        assert!(!result.source.contains("fn removable"));
    }

    #[test]
    fn preserves_referenced_empty_module_namespaces() {
        let manifest = "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n";
        for module in ["mod empty {}", "mod empty { fn dead_member() {} }"] {
            let source = format!(
                "#![allow(dead_code, unused_imports)]\n{module}\nmod child {{ pub fn live() {{ use crate::empty; }} }}\nfn dead() {{}}\nfn main() {{ child::live(); }}"
            );
            let result = run(
                &source,
                manifest,
                Options {
                    dead_code: true,
                    test_code: false,
                },
            )
            .unwrap();
            assert!(result.source.contains("mod empty"), "{}", result.source);
            assert!(
                result.source.contains("use crate::empty"),
                "{}",
                result.source
            );
            assert!(!result.source.contains("fn dead_member"));
            assert!(!result.source.contains("fn dead()"));
        }
    }

    #[test]
    fn removes_dead_names_and_inherent_members_individually() {
        let source = r#"
            #![allow(dead_code)]
            mod left { pub fn same() {} }
            mod right { fn same() {} }
            struct Used;
            impl Used {
                fn live() -> usize { Self::VALUE }
                const VALUE: usize = 7;
                fn dead() -> usize { Self::DEAD_VALUE }
                const DEAD_VALUE: usize = 8;
            }
            fn main() { left::same(); assert_eq!(Used::live(), 7); }
        "#;
        let result = run(source,
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: true, test_code: false }).unwrap();
        assert!(result.source.contains("mod right"));
        assert!(result.source.contains("mod right {}"), "{}", result.source);
        assert!(result.source.contains("mod left"));
        assert!(result.source.contains("fn live"));
        assert!(result.source.contains("const VALUE"));
        assert!(!result.source.contains("fn dead"));
        assert!(!result.source.contains("const DEAD_VALUE"));
    }

    #[test]
    fn diagnostic_locations_handle_unicode_raw_identifiers_and_macro_expansions() {
        let source = r#"
            #![allow(dead_code)]
            macro_rules! generated { () => { fn r#unused() {} }; }
            generated!();
            mod 名前 { pub fn r#type() {} fn r#unused() {} }
            fn main() { 名前::r#type(); }
        "#;
        let result = run(source,
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: true, test_code: false }).unwrap();
        assert!(result.source.contains("generated!"));
        assert!(result.source.contains("r#type"));
        assert!(!result.source.contains("fn r#unused() {}\n}"));
    }

    #[test]
    fn removes_every_method_in_a_combined_dead_code_diagnostic() {
        let source = r#"
            #![allow(dead_code)]
            struct S;
            impl S {
                fn first() {}
                fn second() {}
                fn third() {}
                const VALUE: usize = 3;
            }
            fn main() {}
        "#;
        let result = run(source,
            "[package]\nname=\"splicedown-check\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
            Options { dead_code: true, test_code: false }).unwrap();
        assert_eq!(result.removed_items, 5);
        assert!(!result.source.contains("impl S"));
        assert!(!result.source.contains("struct S"));
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
