use crate::metadata::Dep;

/// Render the provenance header prepended to a bundled source file.
///
/// The function only reads package metadata and does not perform any I/O. The
/// input is sorted here instead of relying on `Plan::deps` being sorted so the
/// rendered header remains deterministic for every caller.
pub(crate) fn render(deps: &[Dep]) -> String {
    let mut deps: Vec<&Dep> = deps.iter().collect();
    deps.sort_unstable_by(|left, right| {
        (&left.pkg.name, &left.pkg.version, &left.pkg.id).cmp(&(
            &right.pkg.name,
            &right.pkg.version,
            &right.pkg.id,
        ))
    });

    let mut header = format!(
        "// bundled by splicedown v{}\n//\n// packages:\n",
        env!("CARGO_PKG_VERSION")
    );

    if deps.is_empty() {
        header.push_str("// - (none)\n");
    } else {
        for dep in deps {
            let license = metadata_value(dep.pkg.license.as_deref());
            let repository = metadata_value(dep.pkg.repository.as_deref());
            header.push_str(&format!(
                "// - {} (v{}) ({license}) ({repository})\n",
                dep.pkg.name, dep.pkg.version
            ));
        }
    }

    header.push('\n');
    header
}

fn metadata_value(value: Option<&str>) -> String {
    let Some(value) = value else {
        return "UNKNOWN".to_owned();
    };

    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.is_empty() {
        "UNKNOWN".to_owned()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metadata::Plan;
    use std::path::PathBuf;

    fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(path)
    }

    #[test]
    fn renders_version_and_empty_package_list() {
        let header = render(&[]);

        assert!(header.starts_with("// bundled by splicedown v"));
        assert!(header.contains("// packages:\n// - (none)\n"));
        assert!(header.ends_with("\n\n"));
    }

    #[test]
    fn renders_bundled_packages_in_deterministic_order() {
        let plan = Plan::collect(
            &fixture("bin-2024/Cargo.toml"),
            &fixture("bin-2024/src/main.rs"),
            &[],
            &[],
        )
        .unwrap();

        let mut reversed = plan.deps.clone();
        reversed.reverse();
        let first = render(&plan.deps);
        let second = render(&reversed);

        assert_eq!(first, second);
        let a = first.find("// - lib-2024-a ").unwrap();
        let b = first.find("// - lib-2024-b ").unwrap();
        let c = first.find("// - lib-2024-c ").unwrap();
        assert!(a < b && b < c);
        assert!(first.contains("// - lib-2024-a (v0.0.0) (MIT) (https://example.com/lib-2024-a)"));
    }

    #[test]
    fn normalizes_missing_and_multiline_metadata() {
        assert_eq!(metadata_value(None), "UNKNOWN");
        assert_eq!(
            metadata_value(Some("  MIT\nOR\tApache-2.0  ")),
            "MIT OR Apache-2.0"
        );
        assert_eq!(metadata_value(Some("\n\r\t")), "UNKNOWN");
    }
}
