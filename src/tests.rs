use super::*;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_PROJECT: AtomicUsize = AtomicUsize::new(0);

struct TestProject {
    root: PathBuf,
}

struct TestWorkspace {
    root: PathBuf,
}

struct TempOutput {
    path: PathBuf,
}

impl TempOutput {
    fn new(label: &str) -> Self {
        let id = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("splicedown-{label}-{}-{id}.rs", std::process::id()));
        let _ = fs::remove_file(&path);
        Self { path }
    }
}

impl Drop for TempOutput {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir_all(self.path.with_extension("run"));
    }
}

impl TempOutput {
    /// A directory for compiling a bundled program, removed with the file.
    fn run_directory(&self) -> PathBuf {
        let directory = self.path.with_extension("run");
        fs::create_dir_all(&directory).unwrap();
        directory
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
            exclude_preset: Vec::new(),
            minify: false,
            no_minify: true,
            minify_test: false,
            no_minify_test: true,
            minify_comment: false,
            no_minify_comment: true,
            no_check: false,
            keep_check_dir: false,
        }
    }
}

impl TestWorkspace {
    fn new(members: &[&str]) -> Self {
        let id = NEXT_PROJECT.fetch_add(1, Ordering::Relaxed);
        let root =
            std::env::temp_dir().join(format!("splicedown-deps-test-{}-{id}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let members = members
            .iter()
            .map(|member| format!("\"{member}\""))
            .collect::<Vec<_>>()
            .join(", ");
        fs::write(
            root.join("Cargo.toml"),
            format!("[workspace]\nmembers = [{members}]\nresolver = \"3\"\n"),
        )
        .unwrap();
        Self { root }
    }

    fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&path, contents).unwrap();
        path
    }

    fn cli(&self, output: PathBuf, minify: bool) -> cli::Cli {
        cli::Cli {
            entry: self.root.join("app/src/main.rs"),
            manifest_path: Some(self.root.join("app/Cargo.toml")),
            output: Some(output),
            exclude: Vec::new(),
            exclude_preset: Vec::new(),
            minify,
            no_minify: !minify,
            minify_test: false,
            no_minify_test: !minify,
            minify_comment: false,
            no_minify_comment: !minify,
            no_check: true,
            keep_check_dir: false,
        }
    }
}

impl Drop for TestProject {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

impl Drop for TestWorkspace {
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
fn provenance_header_follows_a_shebang() {
    let source = "#!/usr/bin/env rust-script\nfn main() {}\n";
    let output = prepend_header(source, "// generated\n\n");

    assert!(output.starts_with("#!/usr/bin/env rust-script\n// generated\n\n"));
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

fn minified_cli(project: &TestProject, output: PathBuf) -> cli::Cli {
    cli::Cli {
        entry: project.root.join("src/main.rs"),
        manifest_path: Some(project.root.join("Cargo.toml")),
        output: Some(output),
        exclude: Vec::new(),
        exclude_preset: Vec::new(),
        minify: true,
        no_minify: false,
        minify_test: true,
        no_minify_test: false,
        minify_comment: true,
        no_minify_comment: false,
        no_check: false,
        keep_check_dir: false,
    }
}

/// Compile a bundle and run it, returning its standard output.
fn compile_and_run(source: &str, directory: &Path, name: &str) -> Result<Output> {
    let source_path = directory.join(format!("{name}.rs"));
    fs::write(&source_path, source)?;
    let binary = directory.join(name);
    let compiled = Command::new("rustc")
        .arg("--edition")
        .arg("2024")
        .arg("-o")
        .arg(&binary)
        .arg(&source_path)
        .output()?;
    assert!(
        compiled.status.success(),
        "bundled source did not compile:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    Command::new(&binary).output().map_err(Into::into)
}

#[test]
fn minified_bundle_is_a_verified_subset_and_behaves_identically() {
    let project = TestProject::new(
        r#"
use std::collections::HashMap;

mod helper {
    pub fn used() -> u32 { 7 }
    pub fn unused_inside() -> u32 { 8 }
    pub mod dead_inner { pub fn never() -> u32 { 9 } }
}

pub fn used_root() -> u32 { helper::used() + 1 }
pub fn unused_root() -> u32 { 2 }
pub struct UnusedRoot;

trait DeadTrait { fn value(&self) -> u32; }
impl DeadTrait for u32 { fn value(&self) -> u32 { 3 } }
pub union DeadUnion { a: u32 }
macro_rules! unused_def { () => { 4 }; }
mod empty_dead {}

fn main() {
    let map: HashMap<u32, u32> = HashMap::new();
    println!("{} {} {}", used_root(), helper::used(), map.len());
}
"#,
        None,
    );
    let unminified = project.root.join("unminified.rs");
    let minified = project.root.join("minified.rs");
    execute(project.cli(unminified.clone())).unwrap();
    execute(minified_cli(&project, minified.clone())).unwrap();

    let unminified = fs::read_to_string(&unminified).unwrap();
    let minified = fs::read_to_string(&minified).unwrap();
    assert!(
        minified.len() < unminified.len(),
        "minify did not shrink the bundle: {} bytes",
        unminified.len()
    );

    // Nothing is missing: every surviving definition is a definition of the
    // bundle, and the diff accounts for each one that disappeared.
    let report = verify::subset(&unminified, &minified).unwrap();
    assert!(report.removed.len() >= 6, "{:?}", report.removed);
    for unit in &report.removed {
        assert!(
            !minified.contains(&unit.name),
            "{} was reported removed but is still in the output:\n{minified}",
            unit
        );
    }

    // The bundle also still does exactly the same thing.
    let unminified_output = compile_and_run(&unminified, &project.root, "unminified_bin").unwrap();
    let minified_output = compile_and_run(&minified, &project.root, "minified_bin").unwrap();
    assert_eq!(
        unminified_output.status.code(),
        minified_output.status.code()
    );
    assert_eq!(unminified_output.stdout, minified_output.stdout);
    assert_eq!(unminified_output.stdout, b"8 7 0\n");
    assert!(minified_output.stderr.is_empty());
}

#[test]
fn fixture_bundle_survives_minification_with_the_same_behavior() {
    // The phase3 fixture bundles three dependency crates, including the
    // macros that qualify a path where it is defined.
    let root = fixture("phase3");
    let unminified = fixture_directory("phase3-unminified", &root, false).unwrap();
    let minified = fixture_directory("phase3", &root, true).unwrap();

    assert!(
        minified.len() < unminified.len(),
        "minify did not shrink the fixture bundle: {} bytes",
        unminified.len()
    );
    // Every generated dependency module received its short name.
    assert!(!minified.contains("__splicedown_"), "{minified}");

    // Minify also has to keep what the bundle does, not only what it says:
    // compiling both bundles and running them must print the same thing.
    let run = TempOutput::new("phase3-run");
    let run_directory = run.run_directory();
    let unminified_output =
        compile_and_run(&unminified, &run_directory, "phase3-unminified").unwrap();
    let minified_output = compile_and_run(&minified, &run_directory, "phase3-minified").unwrap();
    assert_eq!(unminified_output.status.code(), Some(0));
    assert_eq!(unminified_output.status, minified_output.status);
    assert_eq!(unminified_output.stdout, minified_output.stdout);
    assert_eq!(unminified_output.stderr, minified_output.stderr);
    assert!(!unminified_output.stdout.is_empty());
}

fn fixture_directory(kind: &str, root: &Path, minify: bool) -> Result<String> {
    let output = TempOutput::new(kind);
    let entry = root.join("bin/src/main.rs");
    let cli = cli::Cli {
        entry,
        manifest_path: Some(root.join("bin/Cargo.toml")),
        output: Some(output.path.clone()),
        exclude: Vec::new(),
        exclude_preset: Vec::new(),
        minify,
        no_minify: !minify,
        minify_test: false,
        no_minify_test: true,
        minify_comment: false,
        no_minify_comment: true,
        no_check: false,
        keep_check_dir: false,
    };
    execute(cli)?;
    fs::read_to_string(&output.path).map_err(Into::into)
}

#[test]
fn failed_check_does_not_create_output() {
    let project = TestProject::new("fn main() { missing(); }\n", None);
    let output = project.root.join("out.rs");

    let error = execute(project.cli(output.clone())).unwrap_err();

    assert!(format!("{error:#}").contains("cargo check"));
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
        exclude_preset: Vec::new(),
        minify: false,
        no_minify: true,
        minify_test: false,
        no_minify_test: true,
        minify_comment: false,
        no_minify_comment: true,
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

#[test]
fn macro_fixture_rewrites_tokens_and_passes_check() {
    let output = TempOutput::new("macros");
    let root = fixture("bin-2024");
    let cli = cli::Cli {
        entry: root.join("src/main.rs"),
        manifest_path: Some(root.join("Cargo.toml")),
        output: Some(output.path.clone()),
        exclude: Vec::new(),
        exclude_preset: Vec::new(),
        minify: false,
        no_minify: true,
        minify_test: false,
        no_minify_test: true,
        minify_comment: false,
        no_minify_comment: true,
        no_check: false,
        keep_check_dir: false,
    };

    execute(cli).unwrap();

    let bundled = fs::read_to_string(&output.path).unwrap();
    assert!(bundled.starts_with("// bundled by splicedown v"));
    assert!(bundled.contains("// - lib-2024-a (v0.0.0) (MIT) (https://example.com/lib-2024-a)"));
    assert!(!bundled.contains("#[macro_export]"));
    assert!(bundled.contains("pub(crate) use a_report;"));
    assert!(bundled.contains("__splicedown_lib_2024_a"));
    assert!(bundled.contains("__splicedown_lib_2024_b"));
    assert!(bundled.contains("use lib_pm::PmDummy;"));
}

#[test]
fn minify_removes_compiler_confirmed_dead_items() {
    let output = TempOutput::new("minify");
    let root = fixture("bin-2024");
    let cli = cli::Cli {
        entry: root.join("src/main.rs"),
        manifest_path: Some(root.join("Cargo.toml")),
        output: Some(output.path.clone()),
        exclude: Vec::new(),
        exclude_preset: Vec::new(),
        minify: true,
        no_minify: false,
        minify_test: true,
        no_minify_test: false,
        minify_comment: true,
        no_minify_comment: false,
        no_check: false,
        keep_check_dir: false,
    };

    execute(cli).unwrap();

    let bundled = fs::read_to_string(&output.path).unwrap();
    assert!(!bundled.contains("unused_phase5"));
    assert!(bundled.contains("pub const C_CONST"));
    assert!(!bundled.contains("struct Dummy"));
    assert!(bundled.starts_with("// bundled by splicedown v"));
}

#[test]
fn minification_skips_unused_dependency_source_before_loading_it() {
    let workspace = TestWorkspace::new(&["app", "used", "unused-heavy"]);
    workspace.write(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nused = { path = \"../used\" }\nunused-heavy = { path = \"../unused-heavy\" }\n",
        );
    workspace.write("app/src/main.rs", "fn main() { let _ = used::value(); }\n");
    workspace.write(
        "used/Cargo.toml",
        "[package]\nname = \"used\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    workspace.write("used/src/lib.rs", "pub fn value() -> usize { 7 }\n");
    workspace.write(
        "unused-heavy/Cargo.toml",
        "[package]\nname = \"unused-heavy\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    let mut heavy_source = String::from("#[path = \"unsupported.rs\"] mod unsupported;\n");
    for index in 0..1_000 {
        heavy_source.push_str(&format!("pub fn heavy_{index}() -> usize {{ {index} }}\n"));
    }
    workspace.write("unused-heavy/src/lib.rs", &heavy_source);

    let output = workspace.root.join("out.rs");
    execute(workspace.cli(output.clone(), true)).unwrap();

    let bundled = fs::read_to_string(output).unwrap();
    assert!(!bundled.contains("unused-heavy"));
    assert!(!bundled.contains("unsupported.rs"));
    assert!(!bundled.contains("heavy_999"));

    let error = execute(workspace.cli(workspace.root.join("unminified.rs"), false)).unwrap_err();
    assert!(format!("{error:#}").contains("unsupported #[path] attribute"));
}

#[test]
fn dependency_reachability_scans_renamed_paths_in_local_modules_and_macros() {
    let workspace = TestWorkspace::new(&["app", "root-dep", "transitive", "unused-heavy"]);
    workspace.write(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ntop_alias = { package = \"root-dep\", path = \"../root-dep\" }\nunused-heavy = { path = \"../unused-heavy\" }\n",
        );
    workspace.write(
        "app/src/main.rs",
        "mod local;\nfn main() { let _ = local::call(); }\n",
    );
    workspace.write(
        "app/src/local.rs",
        "pub fn call() -> usize { top_alias::use_transitive!() }\n",
    );
    workspace.write(
            "root-dep/Cargo.toml",
            "[package]\nname = \"root-dep\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\ntransitive = { path = \"../transitive\" }\n",
        );
    workspace.write(
        "root-dep/src/lib.rs",
        "#[macro_export]\nmacro_rules! use_transitive { () => { transitive::value() }; }\n",
    );
    workspace.write(
        "transitive/Cargo.toml",
        "[package]\nname = \"transitive\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    workspace.write("transitive/src/lib.rs", "pub fn value() -> usize { 42 }\n");
    workspace.write(
        "unused-heavy/Cargo.toml",
        "[package]\nname = \"unused-heavy\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    workspace.write(
        "unused-heavy/src/lib.rs",
        "pub fn unused() -> usize { 999 }\n",
    );

    let output = workspace.root.join("out.rs");
    execute(workspace.cli(output.clone(), true)).unwrap();

    let bundled = fs::read_to_string(output).unwrap();
    assert!(bundled.contains("root-dep"));
    assert!(bundled.contains("transitive"));
    assert!(!bundled.contains("unused-heavy"));
    // Minify shortens the generated dependency module names, but every
    // path into the dependency is shortened with them.
    assert!(!bundled.contains("__splicedown_"), "{bundled}");
    assert!(bundled.contains("mod m0 {"), "{bundled}");
    assert!(bundled.contains("mod m1 {"), "{bundled}");
    assert!(bundled.contains("crate::m0::use_transitive"), "{bundled}");
}

#[test]
fn minification_retries_with_full_bundle_for_proc_macro_generated_dependency_paths() {
    let workspace = TestWorkspace::new(&["app", "hidden-dep", "source-macro"]);
    workspace.write(
            "app/Cargo.toml",
            "[package]\nname = \"app\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[dependencies]\nhidden-dep = { path = \"../hidden-dep\" }\nsource-macro = { path = \"../source-macro\" }\n",
        );
    let entry = workspace.write(
        "app/src/main.rs",
        "#[source_macro::emit_hidden]\nstruct Marker;\nfn main() {}\n",
    );
    workspace.write(
        "hidden-dep/Cargo.toml",
        "[package]\nname = \"hidden-dep\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
    );
    workspace.write("hidden-dep/src/lib.rs", "pub fn hidden_value() {}\n");
    workspace.write(
            "source-macro/Cargo.toml",
            "[package]\nname = \"source-macro\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\n[lib]\nproc-macro = true\n",
        );
    workspace.write(
            "source-macro/src/lib.rs",
            "extern crate proc_macro;\nuse proc_macro::TokenStream;\n#[proc_macro_attribute]\npub fn emit_hidden(_: TokenStream, item: TokenStream) -> TokenStream { item }\n",
        );

    let plan = Plan::collect(&workspace.root.join("app/Cargo.toml"), &entry, &[], &[]).unwrap();
    let hidden_module = plan
        .deps
        .iter()
        .find(|dep| dep.pkg.name == "hidden-dep")
        .unwrap()
        .mangled
        .to_string();
    let macro_source = format!(
        "extern crate proc_macro;\nuse proc_macro::TokenStream;\n#[proc_macro_attribute]\npub fn emit_hidden(_: TokenStream, item: TokenStream) -> TokenStream {{\n    let mut output = item;\n    output.extend({:?}.parse::<TokenStream>().unwrap());\n    output\n}}\n",
        format!("const _: fn() = crate::{hidden_module}::hidden_value;")
    );
    workspace.write("source-macro/src/lib.rs", &macro_source);

    let output = workspace.root.join("out.rs");
    execute(workspace.cli(output.clone(), true)).unwrap();

    let bundled = fs::read_to_string(output).unwrap();
    assert!(bundled.contains("hidden-dep"));
    assert!(bundled.contains("source_macro::emit_hidden"));
}
