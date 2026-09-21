use anyhow::{Context, Result, bail};
use cargo_metadata::Package;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static CHECK_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Conservatively retain every dependency name occurring anywhere in tokens,
/// including macro bodies and attributes. Proc macros may emit hidden paths,
/// so their presence disables this optimization.
pub(crate) fn referenced_packages(source: &str, packages: &[Package]) -> Result<Vec<Package>> {
    fn identifiers(
        tokens: proc_macro2::TokenStream,
        names: &mut std::collections::HashSet<String>,
    ) {
        for token in tokens {
            match token {
                proc_macro2::TokenTree::Ident(name) => {
                    names.insert(name.to_string().trim_start_matches("r#").to_owned());
                }
                proc_macro2::TokenTree::Group(group) => identifiers(group.stream(), names),
                _ => {}
            }
        }
    }
    let file = syn::parse_file(source)?;
    // Parsing the file first removes a possible shebang.
    let tokens = prettyplease::unparse(&syn::File {
        shebang: None,
        ..file
    })
    .parse::<proc_macro2::TokenStream>()
    .map_err(|error| anyhow::anyhow!("failed to scan dependency references: {error}"))?;
    let mut names = std::collections::HashSet::new();
    identifiers(tokens, &mut names);
    let referenced = |package: &&Package| {
        names.contains(&package.name.to_string().replace('-', "_"))
            || package.targets.iter().any(|target| {
                target.kind.iter().any(|kind| {
                    matches!(
                        kind,
                        cargo_metadata::TargetKind::Lib | cargo_metadata::TargetKind::ProcMacro
                    )
                }) && names.contains(&target.name.replace('-', "_"))
            })
    };
    let selected = packages
        .iter()
        .filter(referenced)
        .cloned()
        .collect::<Vec<_>>();
    if selected.iter().any(crate::metadata::is_proc_macro) {
        return Ok(packages.to_vec());
    }
    Ok(selected)
}

/// Check a generated bundle in an isolated temporary Cargo package.
///
/// The temporary package is removed when this function returns unless
/// keep_dir is set. Cargo's output is copied to stderr so that a caller
/// using stdout for the bundle never receives diagnostic text there.
pub(crate) fn run(source: &str, skip_pkgs: &[Package], keep_dir: bool) -> Result<()> {
    // Generate the manifest before creating the temporary directory. In
    // particular, an unsupported git dependency should fail without leaving
    // an otherwise empty check directory behind.
    let manifest =
        manifest_toml(skip_pkgs).context("[cargo-check] failed to generate the check manifest")?;
    let check_dir =
        CheckDir::create(keep_dir).context("[cargo-check] failed to create the check directory")?;
    let src_dir = check_dir.path().join("src");

    fs::create_dir(&src_dir).with_context(|| format!("failed to create {}", src_dir.display()))?;

    let manifest_path = check_dir.path().join("Cargo.toml");
    fs::write(&manifest_path, manifest)
        .with_context(|| format!("failed to write {}", manifest_path.display()))?;

    let source_path = src_dir.join("main.rs");
    fs::write(&source_path, source)
        .with_context(|| format!("failed to write {}", source_path.display()))?;

    let output = Command::new("cargo")
        .arg("check")
        .arg("--quiet")
        .arg("--manifest-path")
        .arg(&manifest_path)
        .current_dir(check_dir.path())
        .output()
        .with_context(|| "[cargo-check] failed to execute cargo check")?;

    if !output.status.success() {
        forward_failure_output(&output);
        bail!(
            "[cargo-check] cargo check failed ({})\n{}",
            status_description(&output),
            failure_hint(check_dir.path(), keep_dir)
        );
    }

    // `--quiet` suppresses Cargo's progress messages while leaving compiler
    // diagnostics such as warnings available on stderr.
    forward_diagnostics(&output);

    Ok(())
}

/// Render the manifest used by the temporary check package.
pub(crate) fn manifest_toml(skip_pkgs: &[Package]) -> Result<String> {
    // A Cargo manifest cannot express two dependencies with the same key.
    // Keep identical entries only once, and provide a useful error for the
    // multiple-version case (which would require a dependency rename).
    let mut dependencies = BTreeMap::<String, String>::new();
    for package in skip_pkgs {
        let name = package.name.to_string();
        let line = dep_line(package)?;

        if let Some(previous) = dependencies.get(&name) {
            if previous != &line {
                bail!(
                    "cannot represent skipped dependencies with the same name '{}'; \
                     dependency renames are not supported by the check crate",
                    name
                );
            }
            continue;
        }
        dependencies.insert(name, line);
    }

    let mut manifest = String::from(concat!(
        "[package]\n",
        "name = \"splicedown-check\"\n",
        "version = \"0.0.0\"\n",
        "edition = \"2024\"\n",
        "publish = false\n",
        "\n",
        "[workspace]\n",
        "\n",
        "[dependencies]\n",
    ));

    for (name, line) in dependencies {
        manifest.push_str(&toml_key(&name));
        manifest.push_str(" = ");
        manifest.push_str(&line);
        manifest.push('\n');
    }

    Ok(manifest)
}

/// Render the right-hand side of one entry in [dependencies].
pub(crate) fn dep_line(package: &Package) -> Result<String> {
    match package.source.as_ref() {
        None => {
            let package_dir = package
                .manifest_path
                .parent()
                .context("package manifest has no parent directory")?;
            let package_dir = fs::canonicalize(package_dir).with_context(|| {
                format!(
                    "failed to resolve package path for '{}': {}",
                    package.name, package_dir
                )
            })?;

            Ok(format!(
                "{{ path = {} }}",
                toml_string(&package_dir.to_string_lossy())
            ))
        }
        Some(source) if source.repr.starts_with("git+") => bail!(
            "git dependency '{}' is not supported by the check crate; use --no-check",
            package.name
        ),
        Some(_) => Ok(toml_string(&format!("={}", package.version))),
    }
}

fn toml_key(value: &str) -> String {
    if !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        value.to_owned()
    } else {
        toml_string(value)
    }
}

fn toml_string(value: &str) -> String {
    let mut result = String::with_capacity(value.len() + 2);
    result.push('"');

    for ch in value.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '"' => result.push_str("\\\""),
            '\u{08}' => result.push_str("\\b"),
            '\u{09}' => result.push_str("\\t"),
            '\u{0a}' => result.push_str("\\n"),
            '\u{0c}' => result.push_str("\\f"),
            '\u{0d}' => result.push_str("\\r"),
            ch if ch.is_control() => {
                let code = ch as u32;
                if code <= 0xffff {
                    result.push_str(&format!("\\u{code:04x}"));
                } else {
                    result.push_str(&format!("\\U{code:08x}"));
                }
            }
            ch => result.push(ch),
        }
    }

    result.push('"');
    result
}

/// Forward captured cargo output only when the check failed.
///
/// A successful check is intentionally silent at Cargo's level: `--quiet`
/// suppresses progress messages while compiler diagnostics remain visible.
/// On failure, both streams are sent to stderr so rustc and Cargo diagnostics
/// stay visible in one place.
fn forward_failure_output(output: &Output) {
    if !output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
}

fn forward_diagnostics(output: &Output) {
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
    }
}

fn failure_hint(check_dir: &Path, keep_dir: bool) -> String {
    if keep_dir {
        format!(
            "hint: inspect the generated crate at {}",
            check_dir.display()
        )
    } else {
        "hint: rerun with --keep-check-dir to inspect the generated crate".to_owned()
    }
}

fn status_description(output: &Output) -> String {
    output.status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| format!("exit code {code}"),
    )
}

struct CheckDir {
    path: PathBuf,
    keep: bool,
}

impl CheckDir {
    fn create(keep: bool) -> Result<Self> {
        let base = std::env::temp_dir();
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or_default();
        let pid = std::process::id();

        for _ in 0..128 {
            let sequence = CHECK_DIR_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let path = base.join(format!("splicedown-check-{pid}-{timestamp}-{sequence}"));

            match fs::create_dir(&path) {
                Ok(()) => {
                    if keep {
                        eprintln!(
                            "splicedown: keeping cargo check directory at {}",
                            path.display()
                        );
                    }
                    return Ok(Self { path, keep });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!("failed to create check directory {}", path.display())
                    });
                }
            }
        }

        bail!(
            "failed to create a unique cargo check directory in {}",
            base.display()
        )
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CheckDir {
    fn drop(&mut self) {
        if self.keep {
            return;
        }

        if let Err(error) = fs::remove_dir_all(&self.path) {
            eprintln!(
                "splicedown: warning: failed to remove cargo check directory {}: {}",
                self.path.display(),
                error
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::MetadataCommand;
    use std::path::{Path, PathBuf};

    #[test]
    fn dependency_scan_includes_nested_tokens_but_not_comments() {
        let a = fixture_package("lib-2024-a");
        let mut b = fixture_package("lib-2024-b");
        let mut test_target = b.targets[0].clone();
        test_target.name = "test".to_owned();
        test_target.kind = vec![cargo_metadata::TargetKind::Test];
        b.targets.push(test_target);
        let packages = vec![a.clone(), b];
        let selected = referenced_packages(
            "#!/usr/bin/env rust-script\n// lib_2024_b\nmacro_rules! m { () => { lib_2024_a::f() }; } #[test] fn smoke() {} fn main() {}",
            &packages,
        ).unwrap();
        assert_eq!(selected, vec![a]);
    }

    #[test]
    fn referenced_proc_macro_keeps_hidden_dependencies_available() {
        let packages = vec![fixture_package("lib-pm"), fixture_package("lib-2024-a")];
        assert_eq!(
            referenced_packages("use lib_pm::PmDummy; fn main() {}", &packages).unwrap(),
            packages
        );
    }

    fn fixture_manifest() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/bin-2024/Cargo.toml")
    }

    fn fixture_package(name: &str) -> Package {
        MetadataCommand::new()
            .manifest_path(fixture_manifest())
            .exec()
            .unwrap()
            .packages
            .into_iter()
            .find(|package| package.name == name)
            .unwrap_or_else(|| panic!("fixture package not found: {name}"))
    }

    #[test]
    fn manifest_contains_check_package_workspace_and_path_dependency() {
        let package = fixture_package("lib-pm");
        let manifest = manifest_toml(&[package]).unwrap();

        assert!(manifest.contains("name = \"splicedown-check\""));
        assert!(manifest.contains("edition = \"2024\""));
        assert!(manifest.contains("publish = false"));
        assert!(manifest.contains("[workspace]"));
        assert!(manifest.contains("[dependencies]"));
        assert!(manifest.contains("lib-pm = { path = \""));
        assert!(manifest.contains("/tests/fixtures/lib-pm\" }"));
    }

    #[test]
    fn manifest_pins_registry_dependency_to_exact_version() {
        let mut package = fixture_package("lib-pm");
        package.source = Some(cargo_metadata::Source {
            repr: "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
        });

        let manifest = manifest_toml(&[package.clone()]).unwrap();
        assert!(manifest.contains(&format!("lib-pm = \"={}\"", package.version)));
    }

    #[test]
    fn git_dependency_is_rejected() {
        let mut package = fixture_package("lib-pm");
        package.source = Some(cargo_metadata::Source {
            repr: "git+https://example.invalid/repo".to_owned(),
        });

        let error = dep_line(&package).unwrap_err().to_string();
        assert!(error.contains("git dependency"));
        assert!(error.contains("--no-check"));
    }

    #[test]
    fn toml_strings_escape_special_characters() {
        assert_eq!(toml_string("a\\b\"c\nd"), r#""a\\b\"c\nd""#);
    }

    #[test]
    fn check_dir_is_removed_when_dropped() {
        let path = {
            let directory = CheckDir::create(false).unwrap();
            let path = directory.path().to_owned();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists());
    }

    #[test]
    fn cargo_check_failure_hint_explains_how_to_keep_generated_source() {
        let path = Path::new("splicedown-check-example");

        assert_eq!(
            failure_hint(path, false),
            "hint: rerun with --keep-check-dir to inspect the generated crate"
        );
        assert_eq!(
            failure_hint(path, true),
            "hint: inspect the generated crate at splicedown-check-example"
        );
    }
}
