use anyhow::{Context, Result, anyhow};
use cargo_metadata::camino::Utf8PathBuf;
use cargo_metadata::semver::Version;
use cargo_metadata::{
    DependencyKind, Edition, MetadataCommand, Node, Package, PackageId, PackageName, TargetKind,
};
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;
use syn::Ident;

use crate::preset::ExcludePreset;

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Plan {
    pub entry: Entry,
    pub deps: Vec<Dep>,
    pub skip_pkgs: Vec<Package>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Entry {
    pub pkg: Package,
    pub src: Utf8PathBuf,
    pub extern_map: HashMap<String, Ident>,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Dep {
    pub pkg: Package,
    pub mangled: Ident,
    pub lib_src: Utf8PathBuf,
    pub extern_map: HashMap<String, Ident>,
}

impl Plan {
    pub fn collect(
        manifest: &Path,
        entry: &Path,
        exclude: &[String],
        exclude_presets: &[ExcludePreset],
    ) -> Result<Plan> {
        let metadata = MetadataCommand::new().manifest_path(manifest).exec()?;
        let resolve = metadata
            .resolve
            .as_ref()
            .context("no resolve in metadata")?;
        let root = resolve
            .root
            .as_ref()
            .context("no root in resolve (maybe virtual manifest?)")?;

        let pkg_of: HashMap<&PackageId, &Package> =
            metadata.packages.iter().map(|p| (&p.id, p)).collect();
        let node_of: HashMap<&PackageId, &Node> =
            resolve.nodes.iter().map(|n| (&n.id, n)).collect();

        let (mut bundled, skip_pkgs) = bfs_deps(root, &pkg_of, &node_of, exclude, exclude_presets)?;
        bundled.sort_unstable_by(|a, b| {
            (&pkg_of[a].name, &pkg_of[a].version).cmp(&(&pkg_of[b].name, &pkg_of[b].version))
        });

        // そのうち `cargo fix --edition` 使って Edition 差異対応させられるかも
        assert_all_2024(
            pkg_of[root],
            &bundled.iter().map(|id| pkg_of[id]).collect::<Vec<_>>(),
        )?;

        let mangled_of: HashMap<PackageId, Ident> = bundled
            .iter()
            .map(|id| {
                let m = Ident::new(&mangled_name(pkg_of[id]), proc_macro2::Span::call_site());
                (id.clone(), m)
            })
            .collect();

        let deps: Vec<Dep> = bundled
            .iter()
            .map(|id| {
                let pkg = pkg_of[id];
                let node = node_of[id];
                Ok(Dep {
                    pkg: pkg.clone(),
                    mangled: mangled_of[id].clone(),
                    lib_src: require_lib_src(pkg)?.clone(),
                    extern_map: extern_map_of(node, &mangled_of),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let src = Utf8PathBuf::from_path_buf(entry.to_path_buf())
            .map_err(|_| anyhow!("entry path is not valid UTF-8: {}", entry.display()))?;

        Ok(Plan {
            entry: Entry {
                pkg: pkg_of[root].clone(),
                src,
                extern_map: extern_map_of(node_of[root], &mangled_of),
            },
            deps,
            skip_pkgs,
        })
    }
}

fn bfs_deps(
    root: &PackageId,
    pkg_of: &HashMap<&PackageId, &Package>,
    node_of: &HashMap<&PackageId, &Node>,
    exclude: &[String],
    exclude_presets: &[ExcludePreset],
) -> Result<(Vec<PackageId>, Vec<Package>)> {
    let mut visited: HashSet<PackageId> = HashSet::new();
    let mut queue: VecDeque<PackageId> = VecDeque::new();

    visited.insert(root.clone());
    queue.push_back(root.clone());

    let mut bundled: Vec<PackageId> = Vec::new();
    let mut skip_pkgs: Vec<Package> = Vec::new();

    while let Some(id) = queue.pop_front() {
        let Some(node) = node_of.get(&id) else {
            continue;
        };

        for dep in &node.deps {
            let is_normal = dep
                .dep_kinds
                .iter()
                .any(|k| k.kind == DependencyKind::Normal);
            if !is_normal {
                continue;
            }
            let Some(&dep_pkg) = pkg_of.get(&dep.pkg) else {
                continue;
            };

            // An explicit exclusion intentionally accepts every version and therefore
            // overrides the stricter version check performed by presets.
            let is_explicitly_excluded = exclude.iter().any(|name| name == dep_pkg.name.as_str());
            let is_skip = is_explicitly_excluded
                || preset_excludes(dep_pkg, exclude_presets)?
                || is_proc_macro(dep_pkg);

            if visited.insert(dep_pkg.id.clone()) {
                if is_skip {
                    skip_pkgs.push(dep_pkg.clone());
                } else {
                    bundled.push(dep_pkg.id.clone());
                    queue.push_back(dep_pkg.id.clone());
                }
            }
        }
    }

    Ok((bundled, skip_pkgs))
}

fn preset_excludes(pkg: &Package, presets: &[ExcludePreset]) -> Result<bool> {
    let provided_by: Vec<_> = presets
        .iter()
        .flat_map(|preset| {
            preset
                .packages()
                .iter()
                .filter(|expected| expected.name == pkg.name.as_str())
                .map(|expected| (*preset, expected.version))
        })
        .collect();

    if provided_by.is_empty() {
        return Ok(false);
    }
    if !is_crates_io_package(pkg) {
        return Ok(false);
    }
    if provided_by
        .iter()
        .any(|(_, expected)| *expected == pkg.version.to_string())
    {
        return Ok(true);
    }

    let expectations = provided_by
        .iter()
        .map(|(preset, version)| format!("{} provides v{version}", preset.name()))
        .collect::<Vec<_>>()
        .join(", ");
    let preset_flags = provided_by
        .iter()
        .map(|(preset, _)| format!("--exclude-preset {}", preset.name()))
        .collect::<Vec<_>>()
        .join(" / ");

    anyhow::bail!(
        "dependency {} resolved to v{}, but the selected exclude preset expects a different version ({expectations})\n\
         hint: align the dependency version, remove {preset_flags}, or pass --exclude {} to explicitly keep it external",
        pkg.name,
        pkg.version,
        pkg.name,
    )
}

fn is_crates_io_package(pkg: &Package) -> bool {
    pkg.source.as_ref().is_some_and(|source| {
        matches!(
            source.repr.as_str(),
            "registry+https://github.com/rust-lang/crates.io-index"
                | "registry+https://index.crates.io/"
                | "sparse+https://index.crates.io/"
        )
    })
}

fn assert_all_2024(entry: &Package, deps: &[&Package]) -> Result<()> {
    let mut bad: Vec<(PackageName, Version, Edition)> = Vec::new();

    if entry.edition != Edition::E2024 {
        bad.push((entry.name.clone(), entry.version.clone(), entry.edition));
    }

    for &dep in deps {
        if dep.edition != Edition::E2024 {
            bad.push((dep.name.clone(), dep.version.clone(), dep.edition));
        }
    }

    if !bad.is_empty() {
        let mut msg = String::from("the following packages are not using edition 2024:\n");
        for (name, version, edition) in bad {
            msg.push_str(&format!("  {} {}: {}\n", name, version, edition));
        }
        msg.push_str(
            "hint: use --exclude <crate> for judge-provided crates, or --exclude-preset atcoder-2025-10\n",
        );
        anyhow::bail!(msg)
    } else {
        Ok(())
    }
}

/// Prefix shared by every generated module name of a bundled dependency.
///
/// The minify pass uses it to find the names it may shorten, and nothing else
/// depends on the spelling, so the constant keeps both sides in one place.
pub(crate) const MANGLED_PREFIX: &str = "__splicedown_";

fn mangled_name(pkg: &Package) -> String {
    mangle(&pkg.name, &pkg.version.to_string(), &pkg.id.repr)
}

fn mangle(name: &str, version: &str, id_repr: &str) -> String {
    let hash = crate::util::fnv_1a::fnv1a32(id_repr.as_bytes());

    format!("{MANGLED_PREFIX}{}_{}_{:08x}", name, version, hash)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

pub(crate) fn is_proc_macro(pkg: &Package) -> bool {
    pkg.targets
        .iter()
        .any(|t| t.kind.contains(&TargetKind::ProcMacro))
}

fn require_lib_src(pkg: &Package) -> Result<&Utf8PathBuf> {
    debug_assert!(!is_proc_macro(pkg));

    pkg.targets
        .iter()
        .find(|t| {
            t.kind
                .iter()
                .any(|k| matches!(k, TargetKind::Lib | TargetKind::RLib | TargetKind::DyLib))
        })
        .map(|t| &t.src_path)
        .ok_or_else(|| anyhow::anyhow!("package {} has no lib target", pkg.name))
}

fn extern_map_of(node: &Node, mangled_of: &HashMap<PackageId, Ident>) -> HashMap<String, Ident> {
    node.deps
        .iter()
        .filter(|d| d.dep_kinds.iter().any(|k| k.kind == DependencyKind::Normal))
        .filter(|d| !d.name.is_empty())
        .filter_map(|d| mangled_of.get(&d.pkg).map(|m| (d.name.clone(), m.clone())))
        .collect()
}

#[cfg(test)]
mod tests;
