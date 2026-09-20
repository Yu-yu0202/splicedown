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

fn mangled_name(pkg: &Package) -> String {
    mangle(&pkg.name, &pkg.version.to_string(), &pkg.id.repr)
}

fn mangle(name: &str, version: &str, id_repr: &str) -> String {
    let hash = crate::util::fnv_1a::fnv1a32(id_repr.as_bytes());

    format!("__splicedown_{}_{}_{:08x}", name, version, hash)
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
mod tests {
    use super::*;
    use std::path::PathBuf;

    // =====================================================================
    // 定数・ヘルパ
    // =====================================================================

    /// fixture のパッケージ名
    const PKG_MAIN: &str = "bin-2024";
    const PKG_A: &str = "lib-2024-a";
    const PKG_B: &str = "lib-2024-b";
    const PKG_C: &str = "lib-2024-c";
    const PKG_PM: &str = "lib-pm";

    /// パッケージ名 → extern_map のキー(lib target 名。ハイフンはアンダースコア化される)
    fn key(name: &str) -> String {
        name.replace('-', "_")
    }

    fn fixture(p: &str) -> PathBuf {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures")
            .join(p);
        assert!(path.exists(), "fixture not found: {}", path.display(),);
        path
    }

    fn collect_main(exclude: &[&str]) -> Result<Plan> {
        let exclude: Vec<String> = exclude.iter().map(|s| s.to_string()).collect();
        Plan::collect(
            &fixture("bin-2024/Cargo.toml"),
            &fixture("bin-2024/src/main.rs"),
            &exclude,
            &[],
        )
    }

    fn pkg_names(pkgs: &[Package]) -> Vec<String> {
        pkgs.iter().map(|p| p.name.to_string()).collect()
    }

    fn mark_as_crates_io(pkg: &mut Package) {
        pkg.source = Some(cargo_metadata::Source {
            repr: "registry+https://github.com/rust-lang/crates.io-index".to_owned(),
        });
    }

    // =====================================================================
    // mangled 名
    // =====================================================================

    #[test]
    fn mangle_is_valid_deterministic_and_distinct() {
        // 同一入力には同一出力(決定性)
        let a = mangle("lib-a", "0.1.0", "file:///x/lib-a#0.1.0");
        assert_eq!(a, mangle("lib-a", "0.1.0", "file:///x/lib-a#0.1.0"));

        // プレフィックスとサニタイズ(hyphen / dot → underscore)
        assert!(a.starts_with("__splicedown_lib_a_0_1_0_"), "got: {a}");

        // Rust の識別子として有効
        assert!(syn::parse_str::<Ident>(&a).is_ok(), "got: {a}");

        // バージョン違いは別名
        assert_ne!(a, mangle("lib-a", "0.2.0", "file:///x/lib-a#0.2.0"));

        // 実体(id repr)違いは別名(rename で参照名が同一になった場合の分離)
        assert_ne!(mangle("x", "1.0.0", "r1"), mangle("x", "1.0.0", "r2"));

        // サニタイズだけでは衝突する組も hash で分離される
        assert_ne!(
            mangle("x", "1.0.0-alpha.1", "r1"),
            mangle("x", "1.0.0-alpha-1", "r2"),
        );
    }

    // =====================================================================
    // 通常収集
    // =====================================================================

    #[test]
    fn collect_bundles_two_libs() {
        let plan = collect_main(&[]).unwrap();

        // (1) 依存数。BFS の重複pushバグ(lib-2024-a が2経路から入る)は
        //     ここで捕捉される
        assert_eq!(
            plan.deps.len(),
            3,
            "deps: {:?}",
            plan.deps
                .iter()
                .map(|d| d.pkg.name.to_string())
                .collect::<Vec<_>>(),
        );

        // (2) root パッケージが bin-2024
        assert_eq!(plan.entry.pkg.name.to_string(), PKG_MAIN);

        // (3) entry パスがそのまま伝播している
        assert!(plan.entry.src.ends_with("bin-2024/src/main.rs"));

        // (4) 名前順ソート済み
        let names: Vec<String> = plan.deps.iter().map(|d| d.pkg.name.to_string()).collect();
        assert_eq!(
            names,
            [PKG_A.to_string(), PKG_B.to_string(), PKG_C.to_string()]
        );

        // (5) lib_src が lib ターゲットの src_path(require_lib_src の配線)
        assert!(plan.deps[0].lib_src.ends_with("lib-2024-a/src/lib.rs"));
        assert!(plan.deps[1].lib_src.ends_with("lib-2024-b/src/lib.rs"));
        assert!(plan.deps[2].lib_src.ends_with("lib-2024-c/src/lib.rs"));

        // (6) lib-2024-a は依存を持たないので extern_map は空
        assert!(
            plan.deps[0].extern_map.is_empty(),
            "lib-2024-a の extern_map は空のはず: {:?}",
            plan.deps[0].extern_map.keys().collect::<Vec<_>>(),
        );

        // (7) lib-2024-b から見える lib_2024_a と lib_2024_c は、それぞれの mangled と同一
        assert_eq!(plan.deps[1].extern_map.len(), 2);
        assert_eq!(plan.deps[1].extern_map[&key(PKG_A)], plan.deps[0].mangled);
        assert_eq!(plan.deps[1].extern_map[&key(PKG_C)], plan.deps[2].mangled);

        // (8) entry からはバンドル対象の2つだけが見え、値は各 dep の mangled と一致
        assert_eq!(plan.entry.extern_map.len(), 2);
        assert_eq!(plan.entry.extern_map[&key(PKG_A)], plan.deps[0].mangled);
        assert_eq!(plan.entry.extern_map[&key(PKG_B)], plan.deps[1].mangled);

        // (9) proc-macro(lib-pm)は extern_map に載らない(= 参照は素通しして
        //     judge 側の extern crate 解決に委ねられる)
        assert!(!plan.entry.extern_map.contains_key(&key(PKG_PM)));

        // (10) lib-pm は auto-skip 参照されるが、バンドル対象には入らない
        assert_eq!(plan.skip_pkgs.len(), 1);
        assert_eq!(plan.skip_pkgs[0].name.to_string(), PKG_PM);
        assert!(
            !pkg_names(&plan.deps.iter().map(|d| d.pkg.clone()).collect::<Vec<_>>())
                .contains(&PKG_PM.to_string())
        );

        // (11) lib-2024-b は lib-2024-a と lib-2024-c を参照する
        assert_eq!(plan.deps[1].extern_map.len(), 2);
    }

    // =====================================================================
    // edition ゲート
    // =====================================================================

    #[test]
    fn collect_rejects_edition_2018() {
        let err = Plan::collect(
            &fixture("2018/main/Cargo.toml"),
            &fixture("2018/main/src/main.rs"),
            &[],
            &[],
        )
        .unwrap_err();

        let msg = format!("{err:#}");

        // 我々自身のゲートエラーであること(cargo 実行エラーにすり替わっていないこと)
        assert!(
            msg.contains("not using edition 2024"),
            "edition ゲートのエラーであるべき: {msg}"
        );
        // 違反パッケージが名指しされていること
        assert!(msg.contains("lib_old"), "message: {msg}");
        // edition が表示されていること(E2018 / 2018 のどちらの表記でも通る)
        assert!(msg.contains("2018"), "message: {msg}");
        assert!(msg.contains("hint:"), "message: {msg}");
        assert!(msg.contains("--exclude-preset atcoder-2025-10"));
    }

    // =====================================================================
    // --exclude
    // =====================================================================

    #[test]
    fn collect_excludes_pkg_b() {
        let plan = collect_main(&[PKG_B]).unwrap();

        assert_eq!(plan.deps.len(), 1);
        assert_eq!(plan.deps[0].pkg.name.to_string(), PKG_A);

        // entry の地図から lib_2024_b が消える(= `use lib_2024_b::...` は無変換で残る)
        assert!(!plan.entry.extern_map.contains_key(&key(PKG_B)));

        // lib-2024-a は地図に残る
        assert!(plan.entry.extern_map.contains_key(&key(PKG_A)));

        // skip は lib-2024-b(--exclude)と lib-pm(proc-macro)の2つ
        let skip = pkg_names(&plan.skip_pkgs);
        assert_eq!(skip.len(), 2, "skip_pkgs: {skip:?}");
        assert!(skip.contains(&PKG_B.to_string()));
        assert!(skip.contains(&PKG_PM.to_string()));
    }

    #[test]
    fn collect_exclude_cuts_traversal_at_pkg_a() {
        let plan = collect_main(&[PKG_A]).unwrap();

        // lib-2024-b と lib-2024-c はバンドルされる
        assert_eq!(plan.deps.len(), 2);
        assert_eq!(plan.deps[0].pkg.name.to_string(), PKG_B);
        assert_eq!(plan.deps[1].pkg.name.to_string(), PKG_C);

        // 探索が lib-2024-a で打ち切られるため、lib-2024-b 内の lib_2024_a 参照は
        // 書き換え対象外になる(= extern_map が空。仕様の意味論を固定するテスト)
        assert!(
            plan.deps[0].extern_map.len() == 1 && plan.deps[0].extern_map.contains_key(&key(PKG_C)),
            "lib-2024-a 除外時の lib-2024-b の extern_map は lib-2024-c だけのはず: {:?}",
            plan.deps[0].extern_map.keys().collect::<Vec<_>>(),
        );

        // lib-2024-a は bin-2024 と lib-2024-b の2経路から除外判定に掛かるが、
        // skip_pkgs へは1回だけ記録される(重複登録バグの捕捉)
        let skip = pkg_names(&plan.skip_pkgs);
        assert_eq!(skip.len(), 2, "skip_pkgs が重複している可能性: {skip:?}");
        assert_eq!(
            skip.iter().filter(|n| n.as_str() == PKG_A).count(),
            1,
            "lib-2024-a が skip_pkgs に重複記録されている: {skip:?}"
        );
        assert!(skip.contains(&PKG_PM.to_string()));
    }

    #[test]
    fn matching_preset_package_is_excluded() {
        let mut pkg = collect_main(&[]).unwrap().entry.pkg;
        pkg.name = "proconio".parse().unwrap();
        pkg.version = Version::parse("0.5.0").unwrap();
        mark_as_crates_io(&mut pkg);

        assert!(preset_excludes(&pkg, &[ExcludePreset::Atcoder2025October]).unwrap());
    }

    #[test]
    fn preset_rejects_a_different_package_version_with_hint() {
        let mut pkg = collect_main(&[]).unwrap().entry.pkg;
        pkg.name = "proconio".parse().unwrap();
        pkg.version = Version::parse("0.6.0").unwrap();
        mark_as_crates_io(&mut pkg);

        let error = preset_excludes(&pkg, &[ExcludePreset::Atcoder2025October]).unwrap_err();
        let message = format!("{error:#}");
        assert!(message.contains("proconio"), "message: {message}");
        assert!(message.contains("v0.6.0"), "message: {message}");
        assert!(message.contains("v0.5.0"), "message: {message}");
        assert!(message.contains("atcoder-2025-10"), "message: {message}");
        assert!(message.contains("hint:"), "message: {message}");
        assert!(message.contains("--exclude proconio"), "message: {message}");
    }

    #[test]
    fn preset_ignores_packages_not_in_its_snapshot() {
        let pkg = collect_main(&[]).unwrap().entry.pkg;

        assert!(!preset_excludes(&pkg, &[ExcludePreset::Atcoder2025October]).unwrap());
    }

    #[test]
    fn preset_does_not_replace_a_local_fork_with_the_judge_crate() {
        let mut pkg = collect_main(&[]).unwrap().entry.pkg;
        pkg.name = "proconio".parse().unwrap();
        pkg.version = Version::parse("0.5.0").unwrap();
        pkg.source = None;

        assert!(!preset_excludes(&pkg, &[ExcludePreset::Atcoder2025October]).unwrap());
    }
}
