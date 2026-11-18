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
fn manifest_can_select_a_unique_binary_target() {
    let manifest = manifest_for_target(&manifest_toml(&[]).unwrap(), "splicedown-check-42");

    assert!(manifest.contains("[[bin]]\nname = \"splicedown-check-42\""));
    assert!(manifest.contains("path = \"src/main.rs\""));
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
