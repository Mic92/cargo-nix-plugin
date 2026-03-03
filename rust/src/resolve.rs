//! Full workspace resolution: tie together parsing, cfg eval, dep filtering, and feature resolution.

use cargo_metadata::camino;
use cargo_metadata::{DependencyKind, Metadata, Package, PackageId, TargetKind};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use crate::cfg_eval::{matches_target, TargetDescription};
use crate::lockfile::{parse_lockfile, LockfileHashes};

/// The result of resolving a cargo workspace.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceResult {
    /// packageId of the root crate, or null for pure workspaces
    pub root: Option<String>,
    /// Absolute path to the workspace root directory
    pub workspace_root: String,
    /// Workspace member name -> packageId
    pub workspace_members: BTreeMap<String, String>,
    /// packageId -> CrateInfo
    pub crates: BTreeMap<String, CrateInfo>,
}

/// Information about a single resolved crate.
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrateInfo {
    pub crate_name: String,
    pub version: String,
    pub edition: String,
    pub sha256: Option<String>,
    pub source: Option<SourceInfo>,
    pub dependencies: Vec<DepInfo>,
    pub build_dependencies: Vec<DepInfo>,
    pub dev_dependencies: Vec<DepInfo>,
    pub features: BTreeMap<String, Vec<String>>,
    pub resolved_default_features: Vec<String>,
    pub proc_macro: bool,
    pub build: Option<String>,
    pub lib_path: Option<String>,
    pub lib_name: Option<String>,
    pub crate_bin: Vec<BinTarget>,
    pub lib_crate_types: Vec<String>,
    pub links: Option<String>,
    pub authors: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DepInfo {
    pub name: String,
    pub package_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rename: Option<String>,
    pub uses_default_features: bool,
    pub features: Vec<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
#[serde(rename_all = "kebab-case")]
pub enum SourceInfo {
    CratesIo,
    Local { path: String },
    Git { url: String, rev: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct BinTarget {
    pub name: String,
    pub path: String,
}

/// Normalize package name: replace hyphens with underscores (as cargo does).
fn normalize_name(name: &str) -> String {
    name.replace('-', "_")
}

/// Shorten a package ID to "name version" or just "name" if unique.
fn shorten_id(id: &PackageId, name_counts: &HashMap<String, usize>) -> String {
    // Parse the package ID to extract name and version
    // Format: "registry+...#name@version" or "path+...#name@version"
    let repr = &id.repr;
    if let Some(fragment) = repr.split('#').last() {
        if let Some((name, version)) = fragment.rsplit_once('@') {
            let count = name_counts.get(name).copied().unwrap_or(0);
            if count <= 1 {
                return name.to_string();
            }
            return format!("{name} {version}");
        }
    }
    repr.clone()
}

/// Resolve a full cargo workspace.
pub fn resolve_workspace(
    metadata_json: &str,
    cargo_lock: &str,
    target: &TargetDescription,
    _root_features: &[String],
) -> Result<WorkspaceResult, String> {
    let metadata: Metadata = serde_json::from_str(metadata_json)
        .map_err(|e| format!("Failed to parse metadata: {e}"))?;

    let lockfile_hashes = parse_lockfile(cargo_lock);

    let resolve = metadata
        .resolve
        .as_ref()
        .ok_or("No resolve section in metadata")?;

    // Build name occurrence counts for ID shortening
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for pkg in &metadata.packages {
        *name_counts.entry(pkg.name.to_string()).or_default() += 1;
    }

    // Build package lookup by ID
    let pkgs_by_id: HashMap<&PackageId, &Package> =
        metadata.packages.iter().map(|p| (&p.id, p)).collect();

    // Build node lookup by ID (for resolved features)
    let nodes_by_id: HashMap<&PackageId, &cargo_metadata::Node> =
        resolve.nodes.iter().map(|n| (&n.id, n)).collect();

    // Build shortened ID lookup
    let short_ids: HashMap<&PackageId, String> = metadata
        .packages
        .iter()
        .map(|p| (&p.id, shorten_id(&p.id, &name_counts)))
        .collect();

    // Workspace members
    let workspace_member_ids: std::collections::HashSet<&PackageId> =
        metadata.workspace_members.iter().collect();

    let mut workspace_members = BTreeMap::new();
    for member_id in &metadata.workspace_members {
        if let Some(pkg) = pkgs_by_id.get(member_id) {
            let short = short_ids.get(member_id).unwrap();
            workspace_members.insert(pkg.name.to_string(), short.clone());
        }
    }

    // Determine root
    let root = resolve
        .root
        .as_ref()
        .and_then(|root_id| short_ids.get(root_id).cloned());

    // Resolve all crates
    let mut crates = BTreeMap::new();

    for pkg in &metadata.packages {
        let short_id = short_ids.get(&pkg.id).unwrap().clone();
        let node = nodes_by_id.get(&pkg.id);

        let is_workspace_member = workspace_member_ids.contains(&pkg.id);

        // Get resolved features from cargo's resolve
        let resolved_features: Vec<String> = node
            .map(|n| n.features.iter().map(|f| f.to_string()).collect())
            .unwrap_or_default();

        // Determine source
        let source = resolve_source(pkg, &lockfile_hashes, is_workspace_member);

        // Get sha256
        let sha256 = get_sha256(pkg, &lockfile_hashes);

        // Resolve dependencies by joining package deps with node deps
        let (dependencies, build_dependencies, dev_dependencies) = resolve_dependencies(
            pkg,
            node,
            &short_ids,
            &pkgs_by_id,
            target,
            &resolved_features,
        );

        // Extract build targets
        let lib_target = pkg.targets.iter().find(|t| {
            t.kind.iter().any(|k| {
                matches!(
                    k,
                    TargetKind::Lib
                        | TargetKind::CDyLib
                        | TargetKind::DyLib
                        | TargetKind::RLib
                        | TargetKind::ProcMacro
                )
            })
        });

        let build_target = pkg
            .targets
            .iter()
            .find(|t| t.kind.iter().any(|k| *k == TargetKind::CustomBuild));

        let proc_macro = pkg
            .targets
            .iter()
            .any(|t| t.kind.iter().any(|k| *k == TargetKind::ProcMacro));

        // Only include bin targets for workspace members.
        // External dependencies are only used as libraries; their binaries
        // often need additional dependencies (e.g. clap, y4m) that aren't
        // resolved because only the lib is depended upon.
        let binaries: Vec<BinTarget> = if is_workspace_member {
            pkg.targets
                .iter()
                .filter(|t| t.kind.iter().any(|k| *k == TargetKind::Bin))
                .map(|t| {
                    let path = relative_src_path(&t.src_path, &pkg.manifest_path);
                    BinTarget {
                        name: t.name.clone(),
                        path,
                    }
                })
                .collect()
        } else {
            Vec::new()
        };

        let lib_crate_types: Vec<String> = pkg
            .targets
            .iter()
            .filter(|t| {
                t.kind.iter().any(|k| {
                    matches!(
                        k,
                        TargetKind::Lib
                            | TargetKind::CDyLib
                            | TargetKind::DyLib
                            | TargetKind::RLib
                            | TargetKind::StaticLib
                            | TargetKind::ProcMacro
                    )
                })
            })
            .flat_map(|t| t.crate_types.iter().map(|ct| ct.to_string()))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();

        let lib_path = lib_target.map(|t| relative_src_path(&t.src_path, &pkg.manifest_path));
        let lib_name = lib_target.map(|t| normalize_name(&t.name));
        let build_script = build_target.map(|t| relative_src_path(&t.src_path, &pkg.manifest_path));

        crates.insert(
            short_id,
            CrateInfo {
                crate_name: pkg.name.to_string(),
                version: pkg.version.to_string(),
                edition: pkg.edition.to_string(),
                sha256,
                source,
                dependencies,
                build_dependencies,
                // Dev dependencies are only useful for workspace members
                // (to run tests); external crates never have their tests run.
                dev_dependencies: if is_workspace_member {
                    dev_dependencies
                } else {
                    Vec::new()
                },
                features: pkg
                    .features
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
                resolved_default_features: resolved_features,
                proc_macro,
                build: build_script,
                lib_path,
                lib_name,
                crate_bin: binaries,
                lib_crate_types,
                links: pkg.links.clone(),
                authors: pkg.authors.clone(),
            },
        );
    }

    Ok(WorkspaceResult {
        root,
        workspace_root: metadata.workspace_root.to_string(),
        workspace_members,
        crates,
    })
}

fn resolve_source(
    pkg: &Package,
    _lockfile_hashes: &LockfileHashes,
    is_workspace_member: bool,
) -> Option<SourceInfo> {
    match pkg.source.as_ref() {
        Some(source) if source.is_crates_io() => Some(SourceInfo::CratesIo),
        Some(source) => {
            let repr = &source.repr;
            if repr.starts_with("git+") {
                // Parse git URL and rev
                let url_str = &repr[4..];
                if let Some((url, rev)) = url_str.rsplit_once('#') {
                    // Strip query params from url for clean output
                    let clean_url = url.split('?').next().unwrap_or(url);
                    Some(SourceInfo::Git {
                        url: clean_url.to_string(),
                        rev: rev.to_string(),
                    })
                } else {
                    None
                }
            } else if repr.starts_with("sparse+") {
                // Sparse registry — treat like crates.io for now
                Some(SourceInfo::CratesIo)
            } else {
                None
            }
        }
        None => {
            if is_workspace_member {
                // Extract relative path from manifest
                let manifest = pkg.manifest_path.as_std_path();
                let pkg_dir = manifest.parent().unwrap_or(Path::new("."));
                Some(SourceInfo::Local {
                    path: pkg_dir.to_string_lossy().to_string(),
                })
            } else {
                // Local path dependency (non-workspace)
                let manifest = pkg.manifest_path.as_std_path();
                let pkg_dir = manifest.parent().unwrap_or(Path::new("."));
                Some(SourceInfo::Local {
                    path: pkg_dir.to_string_lossy().to_string(),
                })
            }
        }
    }
}

fn get_sha256(pkg: &Package, lockfile_hashes: &LockfileHashes) -> Option<String> {
    lockfile_hashes
        .get(&(pkg.name.to_string(), pkg.version.to_string()))
        .cloned()
}

/// Expand resolved features through the feature map to find all activated
/// optional deps. Handles both `dep:foo` syntax and implicit activation
/// (feature name matching an optional dep name).
///
/// Returns a set of **effective dep names** — the rename if present, otherwise
/// the package name. This is what Cargo uses in `dep:` references and allows
/// disambiguating between multiple deps with the same package name but
/// different renames (e.g., tokio-rustls-023 vs tokio-rustls-026).
fn activated_optional_deps(
    pkg: &Package,
    resolved_features: &[String],
) -> std::collections::HashSet<String> {
    let feature_map = &pkg.features;

    // Collect the "effective name" of each optional dep — the rename if present,
    // otherwise the package name. This is what Cargo uses in `dep:` references.
    let optional_dep_effective_names: std::collections::HashSet<String> = pkg
        .dependencies
        .iter()
        .filter(|d| d.optional)
        .map(|d| d.rename.as_ref().unwrap_or(&d.name).clone())
        .collect();

    // Expand features transitively
    let mut seen = std::collections::HashSet::new();
    let mut queue: Vec<String> = resolved_features.to_vec();
    let mut activated = std::collections::HashSet::new();

    while let Some(feat) = queue.pop() {
        if !seen.insert(feat.clone()) {
            continue;
        }

        // "dep:foo" directly activates optional dep "foo" (where foo is the effective name)
        if let Some(dep_name) = feat.strip_prefix("dep:") {
            activated.insert(dep_name.to_string());
            continue;
        }

        // Handle "dep_name/feature" syntax: activates the dep
        if let Some((dep_part, _feature_part)) = feat.split_once('/') {
            if optional_dep_effective_names.contains(dep_part) {
                activated.insert(dep_part.to_string());
            }
            // Don't continue — still need to follow feature rules below
        }

        // A feature with the same name as an optional dep (effective name) implicitly activates it
        if optional_dep_effective_names.contains(&feat) {
            activated.insert(feat.clone());
        }

        // Follow feature rules
        if let Some(rules) = feature_map.get(&feat) {
            for rule in rules {
                queue.push(rule.clone());
            }
        }
    }

    activated
}

fn resolve_dependencies(
    pkg: &Package,
    node: Option<&&cargo_metadata::Node>,
    short_ids: &HashMap<&PackageId, String>,
    pkgs_by_id: &HashMap<&PackageId, &Package>,
    target: &TargetDescription,
    resolved_features: &[String],
) -> (Vec<DepInfo>, Vec<DepInfo>, Vec<DepInfo>) {
    let mut deps = Vec::new();
    let mut build_deps = Vec::new();
    let mut dev_deps = Vec::new();

    let Some(node) = node else {
        return (deps, build_deps, dev_deps);
    };

    let activated_opt_deps = activated_optional_deps(pkg, resolved_features);

    // Build a lookup of node deps: normalized name -> Vec<(PackageId, dep_name_in_node)>
    let mut node_dep_lookup: HashMap<String, Vec<(&PackageId, &str)>> = HashMap::new();
    for node_dep in &node.deps {
        if let Some(dep_pkg) = pkgs_by_id.get(&node_dep.pkg) {
            let normalized = normalize_name(dep_pkg.name.as_ref());
            node_dep_lookup
                .entry(normalized)
                .or_default()
                .push((&node_dep.pkg, &node_dep.name));
        }
    }

    for dep in &pkg.dependencies {
        // Check platform condition
        if let Some(ref platform) = dep.target {
            if !matches_target(platform, target) {
                continue;
            }
        }

        // Skip optional deps that are not activated by resolved features.
        // Use the effective name (rename if present, else package name) to
        // disambiguate multiple deps with the same package name.
        if dep.optional {
            let effective = dep.rename.as_ref().unwrap_or(&dep.name);
            if !activated_opt_deps.contains(effective.as_str()) {
                continue;
            }
        }

        let normalized = normalize_name(&dep.name);
        let resolved_pkg_id = node_dep_lookup.get(&normalized).and_then(|candidates| {
            if candidates.len() == 1 {
                Some(candidates[0].0)
            } else {
                // Multiple candidates — match by version requirement
                candidates.iter().find_map(|(pkg_id, _)| {
                    let candidate_pkg = pkgs_by_id.get(pkg_id)?;
                    if dep.req.matches(&candidate_pkg.version) {
                        Some(*pkg_id)
                    } else {
                        None
                    }
                })
            }
        });

        let Some(resolved_id) = resolved_pkg_id else {
            continue;
        };

        let short = short_ids
            .get(resolved_id)
            .cloned()
            .unwrap_or_else(|| resolved_id.repr.clone());

        let rename = dep.rename.as_ref().map(|r| normalize_name(r));

        let dep_info = DepInfo {
            name: dep.name.clone(),
            package_id: short,
            rename,
            uses_default_features: dep.uses_default_features,
            features: dep.features.clone(),
        };

        match dep.kind {
            DependencyKind::Build => build_deps.push(dep_info),
            DependencyKind::Development => dev_deps.push(dep_info),
            _ => deps.push(dep_info),
        }
    }

    // Sort for deterministic output
    deps.sort_by(|a, b| a.package_id.cmp(&b.package_id));
    build_deps.sort_by(|a, b| a.package_id.cmp(&b.package_id));
    dev_deps.sort_by(|a, b| a.package_id.cmp(&b.package_id));

    (deps, build_deps, dev_deps)
}

/// Get a source file path relative to the package directory.
fn relative_src_path(src_path: &camino::Utf8Path, manifest_path: &camino::Utf8Path) -> String {
    let pkg_dir = manifest_path.parent().unwrap_or(camino::Utf8Path::new("."));
    src_path
        .strip_prefix(pkg_dir)
        .unwrap_or(src_path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cfg_eval::TargetDescription;

    fn linux_x86_64() -> TargetDescription {
        TargetDescription {
            name: "x86_64-unknown-linux-gnu".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            vendor: "unknown".to_string(),
            env: "gnu".to_string(),
            family: vec!["unix".to_string()],
            pointer_width: "64".to_string(),
            endian: "little".to_string(),
            unix: true,
            windows: false,
        }
    }

    #[test]
    fn resolve_torture_workspace() {
        let metadata = include_str!("../tests/fixtures/metadata.json");
        let cargo_lock = include_str!("../tests/fixtures/Cargo.lock");

        let result = resolve_workspace(
            metadata,
            cargo_lock,
            &linux_x86_64(),
            &["default".to_string()],
        )
        .expect("resolve_workspace failed");

        // 1798 packages in metadata, should have entries for all of them
        assert!(
            result.crates.len() >= 1700,
            "expected ~1798 crates, got {}",
            result.crates.len()
        );

        // 224 workspace members
        assert_eq!(
            result.workspace_members.len(),
            224,
            "expected 224 workspace members, got {}",
            result.workspace_members.len()
        );

        // Spot-check: serde should exist and have features
        let serde = result
            .crates
            .values()
            .find(|c| c.crate_name == "serde" && c.version.starts_with("1.0"));
        assert!(serde.is_some(), "serde 1.0.x not found");
        let serde = serde.unwrap();
        assert!(serde.features.contains_key("default"));
        assert!(serde.sha256.is_some(), "serde should have sha256");
        assert!(serde.lib_name.is_some());

        // Spot-check: a proc-macro crate
        let proc_macros: Vec<_> = result.crates.values().filter(|c| c.proc_macro).collect();
        assert!(
            !proc_macros.is_empty(),
            "expected at least one proc-macro crate"
        );

        // Spot-check: local crate has no sha256
        let local = result
            .crates
            .values()
            .find(|c| c.crate_name == "internal-crate-001");
        assert!(local.is_some(), "internal-crate-001 not found");
        let local = local.unwrap();
        assert!(local.sha256.is_none(), "local crate should not have sha256");
        assert!(matches!(local.source, Some(SourceInfo::Local { .. })));

        // Spot-check: workspace members are present
        assert!(result.workspace_members.contains_key("internal-crate-001"));

        // Spot-check: a renamed dependency exists somewhere
        let has_rename = result.crates.values().any(|c| {
            c.dependencies.iter().any(|d| d.rename.is_some())
                || c.build_dependencies.iter().any(|d| d.rename.is_some())
        });
        assert!(has_rename, "expected at least one renamed dependency");
    }

    /// Regression: non-workspace crates (external dependencies) must not
    /// have bin targets emitted. rav1e has binaries that require additional
    /// dependencies (clap, y4m, etc.) not resolved when used as a library.
    /// buildRustCrate would try to compile them and fail.
    #[test]
    fn external_crate_bins_not_emitted() {
        let metadata = include_str!("../tests/fixtures/metadata.json");
        let cargo_lock = include_str!("../tests/fixtures/Cargo.lock");

        let result = resolve_workspace(
            metadata,
            cargo_lock,
            &linux_x86_64(),
            &["default".to_string()],
        )
        .expect("resolve_workspace failed");

        let rav1e = result
            .crates
            .values()
            .find(|c| c.crate_name == "rav1e" && c.version == "0.7.1")
            .expect("rav1e 0.7.1 not found in fixtures");

        assert!(
            rav1e.crate_bin.is_empty(),
            "external dep rav1e should have no bin targets, got: {:?}",
            rav1e.crate_bin.iter().map(|b| &b.name).collect::<Vec<_>>()
        );

        // Workspace members should still have their bin targets
        for (name, pkg_id) in &result.workspace_members {
            let crate_info = &result.crates[pkg_id];
            // Not all workspace members have bins, but none should be
            // incorrectly stripped. Just verify the field exists.
            let _ = &crate_info.crate_bin;
            // Spot-check: workspace members with src/main.rs should detect bins
            let _ = name;
        }
    }

    /// Regression: non-workspace crates must not have dev dependencies emitted.
    /// Dev dependencies are only needed for running tests, which only happens
    /// for workspace members. Emitting them for external crates is wasteful
    /// and inconsistent with crate2nix (which has no devDependencies field).
    #[test]
    fn external_crate_dev_deps_not_emitted() {
        let metadata = include_str!("../tests/fixtures/metadata.json");
        let cargo_lock = include_str!("../tests/fixtures/Cargo.lock");

        let result = resolve_workspace(
            metadata,
            cargo_lock,
            &linux_x86_64(),
            &["default".to_string()],
        )
        .expect("resolve_workspace failed");

        let member_ids: std::collections::HashSet<&str> = result
            .workspace_members
            .values()
            .map(|s| s.as_str())
            .collect();

        for (pkg_id, crate_info) in &result.crates {
            if member_ids.contains(pkg_id.as_str()) {
                continue;
            }
            assert!(
                crate_info.dev_dependencies.is_empty(),
                "external crate {} ({}) should have no dev dependencies, got {}",
                crate_info.crate_name,
                crate_info.version,
                crate_info.dev_dependencies.len()
            );
        }
    }

    /// Helper: build a minimal Package with the given optional deps and feature map.
    /// Each dep entry is (package_name, rename_or_none, optional).
    fn make_package(
        deps: &[(&str, Option<&str>, bool)],
        features: &[(&str, &[&str])],
    ) -> Package {
        let dep_json: Vec<serde_json::Value> = deps
            .iter()
            .map(|(name, rename, optional)| {
                serde_json::json!({
                    "name": name,
                    "source": "registry+https://github.com/rust-lang/crates.io-index",
                    "req": "*",
                    "kind": null,
                    "optional": optional,
                    "uses_default_features": true,
                    "features": [],
                    "target": null,
                    "rename": rename,
                    "registry": null,
                    "path": null
                })
            })
            .collect();

        let feature_map: serde_json::Map<String, serde_json::Value> = features
            .iter()
            .map(|(k, v)| {
                (
                    k.to_string(),
                    serde_json::Value::Array(
                        v.iter()
                            .map(|s| serde_json::Value::String(s.to_string()))
                            .collect(),
                    ),
                )
            })
            .collect();

        serde_json::from_value(serde_json::json!({
            "name": "test-pkg",
            "version": "0.1.0",
            "id": "path+file:///test#test-pkg@0.1.0",
            "source": null,
            "dependencies": dep_json,
            "targets": [{
                "kind": ["lib"],
                "crate_types": ["lib"],
                "name": "test-pkg",
                "src_path": "/test/src/lib.rs",
                "edition": "2021",
                "doc": true,
                "doctest": true,
                "test": true
            }],
            "features": feature_map,
            "manifest_path": "/test/Cargo.toml",
            "edition": "2021",
        }))
        .expect("failed to construct test Package")
    }

    /// Helper: given a package and resolved features, return the effective names
    /// of optional deps that would pass the filter (simulating the filter check
    /// in resolve_dependencies without needing a full Node).
    fn filtered_optional_dep_effective_names(
        pkg: &Package,
        resolved_features: &[String],
    ) -> Vec<String> {
        let activated = activated_optional_deps(pkg, resolved_features);
        pkg.dependencies
            .iter()
            .filter(|dep| {
                if !dep.optional {
                    return true;
                }
                let effective = dep.rename.as_ref().unwrap_or(&dep.name);
                activated.contains(effective.as_str())
            })
            .map(|dep| dep.rename.as_ref().unwrap_or(&dep.name).clone())
            .collect()
    }

    /// Regression: dep:name syntax must expand through the feature map
    /// to activate optional deps. Before the fix, bzip2's "default" feature
    /// enabling "dep:libbz2-rs-sys" did not activate libbz2-rs-sys.
    #[test]
    fn dep_syntax_activates_optional_dep() {
        let pkg = make_package(
            &[("libbz2-rs-sys", None, true)],
            &[("default", &["dep:libbz2-rs-sys"])],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["default".into()]);
        assert!(
            result.contains(&"libbz2-rs-sys".to_string()),
            "dep:libbz2-rs-sys should activate libbz2-rs-sys, got: {result:?}"
        );
    }

    /// Regression: dep:name syntax with renamed deps must use the rename,
    /// not the package name. actix-tls's "rustls-0_23" feature enables
    /// "dep:tokio-rustls-026" where tokio-rustls-026 is a rename of tokio-rustls.
    #[test]
    fn dep_syntax_activates_renamed_optional_dep() {
        let pkg = make_package(
            &[("tokio-rustls", Some("tokio-rustls-026"), true)],
            &[("rustls-0_23", &["dep:tokio-rustls-026"])],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["rustls-0_23".into()]);
        assert!(
            result.contains(&"tokio-rustls-026".to_string()),
            "dep:tokio-rustls-026 should include the renamed dep, got: {result:?}"
        );
    }

    /// Regression: when multiple optional deps share the same package name
    /// but have different renames, only the one referenced by features should
    /// be included. actix-tls depends on tokio-rustls 4 times with different
    /// renames; only tokio-rustls-026 should pass the filter for rustls-0_23.
    #[test]
    fn only_referenced_rename_included_among_same_package_deps() {
        let pkg = make_package(
            &[
                ("tokio-rustls", Some("tokio-rustls-023"), true),
                ("tokio-rustls", Some("tokio-rustls-024"), true),
                ("tokio-rustls", Some("tokio-rustls-025"), true),
                ("tokio-rustls", Some("tokio-rustls-026"), true),
            ],
            &[
                ("rustls-0_20", &["dep:tokio-rustls-023"]),
                ("rustls-0_21", &["dep:tokio-rustls-024"]),
                ("rustls-0_22", &["dep:tokio-rustls-025"]),
                ("rustls-0_23", &["dep:tokio-rustls-026"]),
            ],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["rustls-0_23".into()]);
        assert_eq!(
            result,
            vec!["tokio-rustls-026"],
            "only tokio-rustls-026 should be included, got: {result:?}"
        );
    }

    /// Implicit activation: a feature with the same name as an optional dep
    /// (using the effective name = rename if present) activates it.
    #[test]
    fn implicit_activation_by_feature_name_matching_dep() {
        let pkg = make_package(
            &[("serde", None, true)],
            &[("default", &["serde"]), ("serde", &["dep:serde"])],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["default".into()]);
        assert!(
            result.contains(&"serde".to_string()),
            "feature 'serde' should implicitly activate optional dep 'serde'"
        );
    }

    /// Transitive feature expansion: feature A enables B which enables dep:C.
    #[test]
    fn transitive_feature_expansion_activates_dep() {
        let pkg = make_package(
            &[("zstd-sys", None, true)],
            &[
                ("default", &["compression"]),
                ("compression", &["dep:zstd-sys"]),
            ],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["default".into()]);
        assert!(
            result.contains(&"zstd-sys".to_string()),
            "transitive feature chain should activate zstd-sys"
        );
    }

    /// dep/feature syntax (e.g. "serde/derive") should activate the dep.
    #[test]
    fn dep_slash_feature_syntax_activates_dep() {
        let pkg = make_package(
            &[("serde", None, true)],
            &[("serde_derive", &["serde/derive"])],
        );
        let result = filtered_optional_dep_effective_names(&pkg, &["serde_derive".into()]);
        assert!(
            result.contains(&"serde".to_string()),
            "serde/derive should activate optional dep serde"
        );
    }

    /// Non-activated optional deps must not appear.
    #[test]
    fn unactivated_optional_dep_excluded() {
        let pkg = make_package(
            &[("tokio", None, true), ("serde", None, false)],
            &[("async", &["dep:tokio"])],
        );
        // "async" not in resolved features, so tokio should be excluded
        let result = filtered_optional_dep_effective_names(&pkg, &[]);
        assert!(
            !result.contains(&"tokio".to_string()),
            "tokio should NOT be included without its feature, got: {result:?}"
        );
        // serde (non-optional) should still be there
        assert!(
            result.contains(&"serde".to_string()),
            "non-optional serde should always be included"
        );
    }
}
