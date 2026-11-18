use super::Minified;
use super::candidate::ItemKind;
use anyhow::{Context, Result, bail};
use cargo_metadata::Message;
use cargo_metadata::diagnostic::{Diagnostic, DiagnosticLevel};
use std::collections::HashSet;
use std::fs;
use std::io::{BufReader, Cursor};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static MINIFY_DIR_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Names the compiler reports as missing after a rejected batch.
///
/// `cannot find function |name| in this scope` and its siblings name the
/// definitions that still have readers, so the retry can keep exactly those
/// instead of splitting the batch one check at a time.
pub(crate) fn missing_definitions(errors: &str) -> HashSet<String> {
    let mut names = HashSet::new();
    for line in errors.lines() {
        if !line.contains("cannot find") {
            continue;
        }
        // The name is the first quoted token of the line.
        if let Some(name) = line.split(char::from(96)).nth(1) {
            names.insert(name.to_owned());
        }
    }
    names
}

/// Byte spans of the diagnostics the minify pass forces to warn.
///
/// Only primary spans in the checked source can identify removable AST nodes.
/// In particular, spans in a macro expansion must not identify its invocation.
#[derive(Debug, Default)]
pub(crate) struct Diagnostics {
    pub(crate) dead_code: HashSet<(usize, usize)>,
    pub(crate) unused_imports: HashSet<(usize, usize)>,
    pub(crate) unused_macros: HashSet<(usize, usize)>,
}

impl Diagnostics {
    /// Lints that are allowed to pronounce a verdict about a kind of node.
    pub(crate) fn spans_for(
        &self,
        kind: ItemKind,
    ) -> impl Iterator<Item = &HashSet<(usize, usize)>> {
        match kind {
            ItemKind::UseItem | ItemKind::UsePath => Vec::from([&self.unused_imports]).into_iter(),
            ItemKind::MacroDefinition => {
                Vec::from([&self.dead_code, &self.unused_macros]).into_iter()
            }
            _ => Vec::from([&self.dead_code]).into_iter(),
        }
    }
}

/// Collect the primary spans of one forced lint for the bundle itself.
fn lint_spans(diagnostic: &Diagnostic, source_path: &Path, out: &mut Diagnostics) {
    if diagnostic.level != DiagnosticLevel::Warning {
        return;
    }
    let spans = match diagnostic.code.as_ref().map(|code| code.code.as_str()) {
        Some("dead_code") => &mut out.dead_code,
        Some("unused_imports") => &mut out.unused_imports,
        Some("unused_macros") => &mut out.unused_macros,
        _ => return,
    };
    spans.extend(
        diagnostic
            .spans
            .iter()
            .filter(|span| span.is_primary && span.expansion.is_none())
            .filter(|span| {
                let path = Path::new(&span.file_name);
                path == source_path || path == Path::new("src/main.rs")
            })
            .map(|span| (span.byte_start as usize, span.byte_end as usize)),
    );
}

pub(crate) struct CheckResult {
    pub(crate) success: bool,
    pub(crate) diagnostics: Diagnostics,
    pub(crate) errors: String,
}

pub(crate) struct WorkDir {
    pub(crate) path: PathBuf,
}

impl WorkDir {
    pub(crate) fn target_name(&self) -> &str {
        self.path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("minify directory has a UTF-8 name")
    }

    pub(crate) fn finish(&self, result: &mut Minified) -> Result<()> {
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
        Ok(())
    }

    pub(crate) fn create() -> Result<Self> {
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

    pub(crate) fn prepare(&self, manifest: &str) -> Result<()> {
        fs::create_dir(self.path.join("src"))
            .with_context(|| format!("failed to create {}/src", self.path.display()))?;
        fs::write(
            self.path.join("Cargo.toml"),
            crate::check::manifest_for_target(manifest, self.target_name()),
        )
        .with_context(|| format!("failed to write {}/Cargo.toml", self.path.display()))
    }

    pub(crate) fn write_source(&self, source: &str) -> Result<()> {
        fs::write(self.path.join("src/main.rs"), source)
            .with_context(|| format!("failed to write {}/src/main.rs", self.path.display()))
    }

    pub(crate) fn check(&self) -> Result<CheckResult> {
        let mut command = Command::new("cargo");
        command
            .arg("rustc")
            .arg("--quiet")
            .args(["--profile", "check"])
            .arg("--message-format=json")
            .arg("--manifest-path")
            .arg(self.path.join("Cargo.toml"))
            .args(["--bin", self.target_name(), "--"])
            .args([
                "--force-warn=dead_code",
                "--force-warn=unused_imports",
                "--force-warn=unused_macros",
            ])
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
    source_path: &Path,
) -> Result<CheckResult> {
    let mut diagnostics = Diagnostics::default();
    let mut errors = String::new();
    let reader = BufReader::new(Cursor::new(&output.stdout));

    for message in Message::parse_stream(reader) {
        let message = message.context("failed to parse cargo diagnostics for --minify")?;
        let Message::CompilerMessage(message) = message else {
            continue;
        };
        if message.target.name == target_name {
            lint_spans(&message.message, source_path, &mut diagnostics);
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
        diagnostics,
        errors,
    })
}
