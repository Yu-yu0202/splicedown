mod assemble;
mod check;
mod cli;
mod inline;
mod metadata;
mod rewrite;
mod util;

use crate::metadata::Plan;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub async fn run() -> Result<()> {
    execute(cli::parse())
}

fn execute(cli: cli::Cli) -> Result<()> {
    let entry = absolute_file(&cli.entry).context("failed to resolve entry file")?;
    let manifest = resolve_manifest(&entry, cli.manifest_path.as_deref())?;
    let plan = Plan::collect(&manifest, &entry, &cli.exclude)
        .context("failed to collect the dependency graph")?;
    let source = bundle(&plan)?;

    if !cli.no_check {
        check::run(&source, &plan.skip_pkgs, cli.keep_check_dir)
            .context("bundled source failed cargo check")?;
    }

    emit_output(&source, cli.output.as_deref())
}

fn bundle(plan: &Plan) -> Result<String> {
    let mut main = inline::load_and_inline(plan.entry.src.as_std_path())
        .with_context(|| format!("failed to load entry {}", plan.entry.src))?;
    rewrite::rewrite(&mut main, &plan.entry.extern_map, None);

    let deps = plan
        .deps
        .iter()
        .map(|dep| {
            let mut file = inline::load_and_inline(dep.lib_src.as_std_path())
                .with_context(|| format!("failed to load dependency {}", dep.pkg.name))?;
            rewrite::rewrite(&mut file, &dep.extern_map, Some(&dep.mangled));
            Ok((dep.mangled.clone(), file))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(prettyplease::unparse(&assemble::assemble(main, deps)))
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
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_PROJECT: AtomicUsize = AtomicUsize::new(0);

    struct TestProject {
        root: PathBuf,
    }

    struct TempOutput {
        path: PathBuf,
    }

    impl TempOutput {
        fn new(label: &str) -> Self {
            let id = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("splicedown-{label}-{}-{id}.rs", std::process::id()));
            let _ = fs::remove_file(&path);
            Self { path }
        }
    }

    impl Drop for TempOutput {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    impl TestProject {
        fn new(main: &str, utils: Option<&str>) -> Self {
            let id = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "splicedown-phase2-test-{}-{id}",
                std::process::id()
            ));
            fs::create_dir_all(root.join("src")).unwrap();
            fs::write(
                root.join("Cargo.toml"),
                "[package]\nname = \"phase2-test\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[workspace]\n",
            )
            .unwrap();
            fs::write(root.join("src/main.rs"), main).unwrap();
            if let Some(utils) = utils {
                fs::write(root.join("src/utils.rs"), utils).unwrap();
            }
            Self { root }
        }

        fn cli(&self, output: PathBuf) -> cli::Cli {
            cli::Cli {
                entry: self.root.join("src/main.rs"),
                manifest_path: Some(self.root.join("Cargo.toml")),
                output: Some(output),
                exclude: Vec::new(),
                no_check: false,
                keep_check_dir: false,
            }
        }
    }

    impl Drop for TestProject {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(path)
    }

    #[test]
    fn manifest_is_found_from_entry_directory() {
        let entry = fixture("bin-2024/src/main.rs").canonicalize().unwrap();
        assert_eq!(
            resolve_manifest(&entry, None).unwrap(),
            fixture("bin-2024/Cargo.toml").canonicalize().unwrap()
        );
    }

    #[test]
    fn local_module_bundle_passes_check_and_is_written() {
        let project = TestProject::new(
            "mod utils;\nuse utils::mul;\nfn main() { let _ = utils::add(1, 2); let _ = crate::utils::add(3, 4); let _ = mul(5, 6); }\n",
            Some(
                "pub(crate) fn add(a: i32, b: i32) -> i32 { a + b }\npub(crate) fn mul(a: i32, b: i32) -> i32 { a * b }\n",
            ),
        );
        let output = project.root.join("out.rs");

        execute(project.cli(output.clone())).unwrap();

        let bundled = fs::read_to_string(output).unwrap();
        assert!(bundled.contains("mod utils {"));
        assert!(bundled.contains("utils::add(1, 2)"));
        assert!(bundled.contains("crate::utils::add(3, 4)"));
        assert!(bundled.contains("use utils::mul;"));
    }

    #[test]
    fn failed_check_does_not_create_output() {
        let project = TestProject::new("fn main() { missing(); }\n", None);
        let output = project.root.join("out.rs");

        let error = execute(project.cli(output.clone())).unwrap_err();

        assert!(error.to_string().contains("cargo check"));
        assert!(!output.exists());
    }

    #[test]
    fn phase3_fixture_rewrites_paths_and_passes_check() {
        let output = TempOutput::new("phase3");
        let root = fixture("phase3");
        let cli = cli::Cli {
            entry: root.join("bin/src/main.rs"),
            manifest_path: Some(root.join("bin/Cargo.toml")),
            output: Some(output.path.clone()),
            exclude: Vec::new(),
            no_check: false,
            keep_check_dir: false,
        };

        execute(cli).unwrap();

        let bundled = fs::read_to_string(&output.path).unwrap();
        assert!(bundled.contains("self as phase3_a"));
        assert!(bundled.contains("crate::__splicedown_phase3_a"));
        assert!(bundled.contains("crate::__splicedown_phase3_b"));
        assert!(bundled.contains("crate::__splicedown_phase3_c"));
        assert!(bundled.contains("super::nested_value()"));
        assert!(bundled.contains("crate::utils::add(3, 4)"));
    }
}
