use anyhow::{Context, Result, bail};
use cargo_metadata::Package;
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static CHECK_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Check a generated bundle in an isolated temporary Cargo package.
///
/// The temporary package is removed when this function returns unless
/// keep_dir is set. Cargo's output is copied to stderr so that a caller
/// using stdout for the bundle never receives diagnostic text there.
pub(crate) fn run(source: &str, skip_pkgs: &[Package], keep_dir: bool) -> Result<()> {
    // Generate the manifest before creating the temporary directory. In
    // particular, an unsupported git dependency should fail without leaving
    // an otherwise empty check directory behind.
    let manifest = manifest_toml(skip_pkgs)?;
    let check_dir = CheckDir::create(keep_dir)?;
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
        .arg("--manifest-path")
        .arg(&manifest_path)
        .current_dir(check_dir.path())
        .output()
        .with_context(|| "failed to execute cargo check")?;

    forward_cargo_output(&output);

    if !output.status.success() {
        bail!("cargo check failed ({})", status_description(&output));
    }

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

fn forward_cargo_output(output: &Output) {
    if !output.stdout.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stdout));
    }
    if !output.stderr.is_empty() {
        eprint!("{}", String::from_utf8_lossy(&output.stderr));
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
    use std::path::PathBuf;

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
}
