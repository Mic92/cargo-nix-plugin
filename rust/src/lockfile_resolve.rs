//! Resolve a cargo workspace from Cargo.lock + registry index, without cargo metadata.
//!
//! This avoids downloading crate sources at eval time. Fields that require
//! reading the crate's `Cargo.toml` (edition, procMacro, libPath, etc.) are
//! left as `None`/default so that `buildRustCrate` can auto-detect them at
//! build time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

use crate::cfg_eval::{matches_target, TargetDescription};
use crate::feature_resolve::{self, DepFeatureInfo, PackageFeatureInfo};
use crate::lockfile::parse_lockfile;
use crate::registry;
use crate::resolve::{BinTarget, CrateInfo, DepInfo, SourceInfo, WorkspaceResult};

/// A parsed Cargo.lock package entry.
#[derive(Debug, Clone)]
struct LockPackage {
    name: String,
    version: String,
    source: Option<String>,
    #[allow(dead_code)]
    checksum: Option<String>,
    /// Dependency references as they appear in Cargo.lock: "name" or "name version".
    dependencies: Vec<String>,
}

/// Parsed workspace Cargo.toml — just what we need for workspace member info.
#[derive(Debug)]
struct WorkspaceManifest {
    /// Workspace members' package names and their manifest directories.
    members: Vec<WorkspaceMember>,
    /// The root package, if this is also a package (not a virtual workspace).
    root_package: Option<WorkspaceMember>,
}

#[derive(Debug, Clone)]
struct WorkspaceMember {
    name: String,
    version: String,
    manifest_dir: String,
    /// Dependencies declared in the member's Cargo.toml (for dep kind/features).
    dependencies: Vec<ManifestDep>,
    build_dependencies: Vec<ManifestDep>,
    dev_dependencies: Vec<ManifestDep>,
    features: BTreeMap<String, Vec<String>>,
    edition: String,
    links: Option<String>,
    proc_macro: bool,
    build_script: Option<String>,
    lib_path: Option<String>,
    lib_name: Option<String>,
    lib_crate_types: Vec<String>,
    bin_targets: Vec<BinTarget>,
    authors: Vec<String>,
}

#[derive(Debug, Clone)]
struct ManifestDep {
    name: String,
    package: Option<String>,
    #[allow(dead_code)]
    version_req: Option<String>,
    #[allow(dead_code)]
    optional: bool,
    default_features: bool,
    features: Vec<String>,
    target: Option<String>,
}

/// Resolve a workspace using Cargo.lock + registry index (no cargo metadata).
///
/// `workspace_root` is the directory containing the workspace Cargo.toml.
/// `cargo_lock` is the contents of Cargo.lock.
/// `cargo_home` is the path to the cargo home directory (for registry index).
pub fn resolve_from_lockfile(
    workspace_root: &Path,
    cargo_lock: &str,
    cargo_home: &Path,
    target: &TargetDescription,
    root_features: &[String],
    no_default_features: bool,
) -> Result<WorkspaceResult, String> {
    // 1. Parse Cargo.lock
    let lock_packages = parse_lock_packages(cargo_lock)?;
    let lockfile_hashes = parse_lockfile(cargo_lock);



    // 2. Parse workspace manifests
    let workspace = parse_workspace(workspace_root)?;

    // 3. Build package ID map: "name" or "name version" -> (name, version)
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for pkg in &lock_packages {
        *name_counts.entry(pkg.name.clone()).or_default() += 1;
    }

    // Build short ID map
    let short_id = |name: &str, version: &str| -> String {
        if name_counts.get(name).copied().unwrap_or(0) <= 1 {
            name.to_string()
        } else {
            format!("{name} {version}")
        }
    };

    // 4. Build the resolved crates
    let workspace_member_names: HashSet<String> =
        workspace.members.iter().map(|m| m.name.clone()).collect();

    // Sparse index URL for crates.io — used for both the old registry+
    // source string and the new sparse+ format.
    let crates_io_sparse_url = "sparse+https://index.crates.io/";
    let crates_io_source = "registry+https://github.com/rust-lang/crates.io-index";

    let mut crates = BTreeMap::new();
    let mut workspace_members = BTreeMap::new();

    for pkg in &lock_packages {
        let sid = short_id(&pkg.name, &pkg.version);
        let is_workspace_member = workspace_member_names.contains(&pkg.name);

        if is_workspace_member {
            // Use the workspace member info from parsed Cargo.toml
            let member = workspace
                .members
                .iter()
                .find(|m| m.name == pkg.name)
                .ok_or_else(|| format!("Workspace member {} not found in manifests", pkg.name))?;

            workspace_members.insert(member.name.clone(), sid.clone());

            let (dependencies, build_dependencies, dev_dependencies) =
                resolve_member_deps(member, &lock_packages, &name_counts, target);

            crates.insert(
                sid,
                CrateInfo {
                    crate_name: member.name.clone(),
                    version: member.version.clone(),
                    edition: member.edition.clone(),
                    sha256: None,
                    source: Some(SourceInfo::Local {
                        path: member.manifest_dir.clone(),
                    }),
                    dependencies,
                    build_dependencies,
                    dev_dependencies,
                    features: member.features.clone(),
                    resolved_default_features: Vec::new(), // TODO: feature resolution
                    proc_macro: member.proc_macro,
                    build: member.build_script.clone(),
                    lib_path: member.lib_path.clone(),
                    lib_name: member.lib_name.clone(),
                    crate_bin: member.bin_targets.clone(),
                    lib_crate_types: member.lib_crate_types.clone(),
                    links: member.links.clone(),
                    authors: member.authors.clone(),
                },
            );
        } else {
            // External crate — use registry index
            let source_info = resolve_pkg_source(pkg);
            let sha256 = lockfile_hashes
                .get(&(pkg.name.clone(), pkg.version.clone()))
                .cloned();

            // Determine the index URL for this source
            let index_url = match pkg.source.as_deref() {
                Some(s) if s == crates_io_source || s.contains("index.crates.io") => {
                    Some(crates_io_sparse_url.to_string())
                }
                Some(s) if s.starts_with("sparse+") || s.starts_with("registry+") => {
                    Some(s.to_string())
                }
                _ => None,
            };

            // Look up in the registry index via tame-index
            let index_version = index_url.as_deref().and_then(|url| {
                let krate = match registry::lookup_crate(cargo_home, url, &pkg.name) {
                    Ok(k) => k,
                    Err(e) => {
                        eprintln!(
                            "warning: failed to look up {} {} in index: {}",
                            pkg.name, pkg.version, e
                        );
                        return None;
                    }
                };
                registry::find_version(&krate, &pkg.version).cloned()
            });

            let (dependencies, build_dependencies, features, links) =
                if let Some(ref version) = index_version {
                    let (deps, build_deps) = resolve_index_deps(
                        version,
                        &lock_packages,
                        &name_counts,
                        &pkg.dependencies,
                        target,
                    );
                    let features = registry::features_for_version(version);
                    let links = version.links.as_deref().map(|s| s.to_string());
                    (deps, build_deps, features, links)
                } else {
                    (Vec::new(), Vec::new(), HashMap::new(), None)
                };

            let features_btree: BTreeMap<String, Vec<String>> = features.into_iter().collect();

            crates.insert(
                sid,
                CrateInfo {
                    crate_name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    // These fields will be auto-detected at build time by buildRustCrate
                    edition: String::new(),
                    sha256,
                    source: source_info,
                    dependencies,
                    build_dependencies,
                    dev_dependencies: Vec::new(), // Not needed for external crates
                    features: features_btree,
                    resolved_default_features: Vec::new(), // TODO: feature resolution
                    proc_macro: false, // Auto-detected at build time by read-crate-info
                    build: None,                           // Auto-detected at build time
                    lib_path: None,                        // Auto-detected at build time
                    lib_name: None,                        // Auto-detected at build time
                    crate_bin: Vec::new(),                 // Not needed for external crates
                    lib_crate_types: Vec::new(),           // Auto-detected at build time
                    links,
                    authors: Vec::new(),
                },
            );
        }
    }

    // --- Feature resolution ---
    // Build PackageFeatureInfo for every crate from its CrateInfo.
    let mut feature_packages: HashMap<String, PackageFeatureInfo> = HashMap::new();
    for (pkg_id, info) in &crates {
        let mut all_deps: Vec<DepFeatureInfo> = Vec::new();
        for dep in info.dependencies.iter().chain(info.build_dependencies.iter()) {
            all_deps.push(DepFeatureInfo {
                name: dep.name.clone(),
                package_id: dep.package_id.clone(),
                uses_default_features: dep.uses_default_features,
                features: dep.features.clone(),
            });
        }

        // Derive optional deps from the features map: features containing
        // "dep:X" indicate X is an optional dependency.
        let mut optional_deps = std::collections::BTreeSet::new();
        for rules in info.features.values() {
            for rule in rules {
                if let Some(dep_name) = rule.strip_prefix("dep:") {
                    optional_deps.insert(dep_name.to_string());
                }
            }
        }

        feature_packages.insert(
            pkg_id.clone(),
            PackageFeatureInfo {
                features: info.features.clone(),
                dependencies: all_deps,
                optional_deps,
            },
        );
    }

    // Roots: workspace members with requested features
    let root_packages: Vec<(String, Vec<String>)> = workspace_members
        .values()
        .map(|pkg_id| {
            let mut features: Vec<String> = root_features.to_vec();
            if !no_default_features {
                features.push("default".to_string());
            }
            (pkg_id.clone(), features)
        })
        .collect();

    let resolved_features = feature_resolve::resolve_features(&feature_packages, &root_packages);

    // Apply resolved features back to crates
    for (pkg_id, features) in &resolved_features {
        if let Some(info) = crates.get_mut(pkg_id) {
            info.resolved_default_features = features.iter().cloned().collect();
        }
    }

    // Determine root
    let root = workspace
        .root_package
        .as_ref()
        .map(|p| short_id(&p.name, &p.version));

    Ok(WorkspaceResult {
        root,
        workspace_root: workspace_root.to_string_lossy().to_string(),
        workspace_members,
        crates,
    })
}

/// Parse Cargo.lock into structured package entries.
fn parse_lock_packages(cargo_lock: &str) -> Result<Vec<LockPackage>, String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        package: Vec<RawLockPkg>,
    }

    #[derive(serde::Deserialize)]
    struct RawLockPkg {
        name: String,
        version: String,
        #[serde(default)]
        source: Option<String>,
        #[serde(default)]
        checksum: Option<String>,
        #[serde(default)]
        dependencies: Vec<String>,
    }

    let lock: Lock =
        toml::from_str(cargo_lock).map_err(|e| format!("Failed to parse Cargo.lock: {e}"))?;

    Ok(lock
        .package
        .into_iter()
        .map(|p| LockPackage {
            name: p.name,
            version: p.version,
            source: p.source,
            checksum: p.checksum,
            dependencies: p.dependencies,
        })
        .collect())
}

/// Parse the workspace root Cargo.toml and all member Cargo.toml files.
fn parse_workspace(workspace_root: &Path) -> Result<WorkspaceManifest, String> {
    let root_manifest_path = workspace_root.join("Cargo.toml");
    let root_toml_str = std::fs::read_to_string(&root_manifest_path)
        .map_err(|e| format!("Failed to read {}: {e}", root_manifest_path.display()))?;
    let root_toml: toml::Value =
        toml::from_str(&root_toml_str).map_err(|e| format!("Failed to parse Cargo.toml: {e}"))?;

    let mut members = Vec::new();
    let mut root_package = None;

    // Check if root is a package
    if let Some(pkg) = root_toml.get("package") {
        let member = parse_member_manifest(&root_toml, pkg, workspace_root)?;
        root_package = Some(member.clone());
        members.push(member);
    }

    // Check for workspace members
    if let Some(workspace) = root_toml.get("workspace") {
        if let Some(member_globs) = workspace.get("members").and_then(|m| m.as_array()) {
            for glob_val in member_globs {
                if let Some(glob_str) = glob_val.as_str() {
                    let member_dirs = expand_glob(workspace_root, glob_str);
                    for member_dir in member_dirs {
                        let member_manifest = member_dir.join("Cargo.toml");
                        if !member_manifest.exists() {
                            continue;
                        }
                        let member_toml_str =
                            std::fs::read_to_string(&member_manifest).map_err(|e| {
                                format!("Failed to read {}: {e}", member_manifest.display())
                            })?;
                        let member_toml: toml::Value =
                            toml::from_str(&member_toml_str).map_err(|e| {
                                format!("Failed to parse {}: {e}", member_manifest.display())
                            })?;
                        if let Some(pkg) = member_toml.get("package") {
                            let member = parse_member_manifest(&member_toml, pkg, &member_dir)?;
                            // Don't duplicate the root package
                            if root_package
                                .as_ref()
                                .map(|rp| rp.name != member.name)
                                .unwrap_or(true)
                            {
                                members.push(member);
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(WorkspaceManifest {
        members,
        root_package,
    })
}

/// Parse a single member's Cargo.toml.
fn parse_member_manifest(
    toml: &toml::Value,
    pkg: &toml::Value,
    manifest_dir: &Path,
) -> Result<WorkspaceMember, String> {
    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("package.name missing")?
        .to_string();
    let version = pkg
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    let edition = pkg
        .get("edition")
        .and_then(|v| v.as_str())
        .unwrap_or("2021")
        .to_string();
    let links = pkg.get("links").and_then(|v| v.as_str()).map(String::from);
    let authors: Vec<String> = pkg
        .get("authors")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Parse build script
    let build_script = match pkg.get("build") {
        Some(toml::Value::Boolean(true)) => Some("build.rs".to_string()),
        Some(toml::Value::Boolean(false)) => None,
        Some(toml::Value::String(s)) => Some(s.clone()),
        _ => {
            // Auto-detect: if build.rs exists, use it
            if manifest_dir.join("build.rs").exists() {
                Some("build.rs".to_string())
            } else {
                None
            }
        }
    };

    // Parse lib target
    let lib = toml.get("lib");
    let proc_macro = lib
        .and_then(|l| l.get("proc-macro"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let lib_path = lib
        .and_then(|l| l.get("path"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let lib_name = lib
        .and_then(|l| l.get("name"))
        .and_then(|v| v.as_str())
        .map(|n| n.replace('-', "_"));
    let lib_crate_types: Vec<String> = lib
        .and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type")))
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // Parse bin targets
    let bin_targets: Vec<BinTarget> = toml
        .get("bin")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|item| {
                    let name = item.get("name")?.as_str()?;
                    let path = item.get("path")?.as_str()?;
                    Some(BinTarget {
                        name: name.to_string(),
                        path: path.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    // Parse features
    let features: BTreeMap<String, Vec<String>> = toml
        .get("features")
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .map(|(k, v)| {
                    let vals = v
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default();
                    (k.clone(), vals)
                })
                .collect()
        })
        .unwrap_or_default();

    // Parse dependencies
    let dependencies = parse_manifest_deps(toml.get("dependencies"));
    let build_dependencies = parse_manifest_deps(toml.get("build-dependencies"));
    let dev_dependencies = parse_manifest_deps(toml.get("dev-dependencies"));

    // Also parse target-specific deps
    let mut target_deps = Vec::new();
    let mut target_build_deps = Vec::new();
    let mut target_dev_deps = Vec::new();
    if let Some(targets) = toml.get("target").and_then(|v| v.as_table()) {
        for (target_spec, target_table) in targets {
            let mut tdeps = parse_manifest_deps(target_table.get("dependencies"));
            for d in &mut tdeps {
                d.target = Some(target_spec.clone());
            }
            target_deps.extend(tdeps);

            let mut tbdeps = parse_manifest_deps(target_table.get("build-dependencies"));
            for d in &mut tbdeps {
                d.target = Some(target_spec.clone());
            }
            target_build_deps.extend(tbdeps);

            let mut tddeps = parse_manifest_deps(target_table.get("dev-dependencies"));
            for d in &mut tddeps {
                d.target = Some(target_spec.clone());
            }
            target_dev_deps.extend(tddeps);
        }
    }

    let all_deps = [dependencies, target_deps].concat();
    let all_build_deps = [build_dependencies, target_build_deps].concat();
    let all_dev_deps = [dev_dependencies, target_dev_deps].concat();

    Ok(WorkspaceMember {
        name,
        version,
        manifest_dir: manifest_dir.to_string_lossy().to_string(),
        dependencies: all_deps,
        build_dependencies: all_build_deps,
        dev_dependencies: all_dev_deps,
        features,
        edition,
        links,
        proc_macro,
        build_script,
        lib_path,
        lib_name,
        lib_crate_types,
        bin_targets,
        authors,
    })
}

/// Parse a `[dependencies]` table from Cargo.toml.
fn parse_manifest_deps(deps: Option<&toml::Value>) -> Vec<ManifestDep> {
    let Some(table) = deps.and_then(|v| v.as_table()) else {
        return Vec::new();
    };

    table
        .iter()
        .map(|(name, val)| match val {
            toml::Value::String(version) => ManifestDep {
                name: name.clone(),
                package: None,
                version_req: Some(version.clone()),
                optional: false,
                default_features: true,
                features: Vec::new(),
                target: None,
            },
            toml::Value::Table(t) => {
                let package = t.get("package").and_then(|v| v.as_str()).map(String::from);
                let version_req = t.get("version").and_then(|v| v.as_str()).map(String::from);
                let optional = t.get("optional").and_then(|v| v.as_bool()).unwrap_or(false);
                let default_features = t
                    .get("default-features")
                    .or_else(|| t.get("default_features"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let features = t
                    .get("features")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                ManifestDep {
                    name: name.clone(),
                    package,
                    version_req,
                    optional,
                    default_features,
                    features,
                    target: None,
                }
            }
            _ => ManifestDep {
                name: name.clone(),
                package: None,
                version_req: None,
                optional: false,
                default_features: true,
                features: Vec::new(),
                target: None,
            },
        })
        .collect()
}

/// Resolve dependencies for a workspace member using the lockfile.
fn resolve_member_deps(
    member: &WorkspaceMember,
    lock_packages: &[LockPackage],
    name_counts: &HashMap<String, usize>,
    target: &TargetDescription,
) -> (Vec<DepInfo>, Vec<DepInfo>, Vec<DepInfo>) {
    let short_id = |name: &str, version: &str| -> String {
        if name_counts.get(name).copied().unwrap_or(0) <= 1 {
            name.to_string()
        } else {
            format!("{name} {version}")
        }
    };

    let lock_member = lock_packages
        .iter()
        .find(|p| p.name == member.name && p.version == member.version);

    let resolve_dep_list = |manifest_deps: &[ManifestDep]| -> Vec<DepInfo> {
        manifest_deps
            .iter()
            .filter(|dep| {
                // Filter by platform
                if let Some(ref target_str) = dep.target {
                    if let Ok(platform) = cargo_platform::Platform::from_str(target_str) {
                        return matches_target(&platform, target);
                    }
                }
                true
            })
            .filter_map(|dep| {
                // Find the resolved version in the lockfile
                let pkg_name = dep.package.as_deref().unwrap_or(&dep.name);
                let resolved = find_lock_dep(lock_member?, pkg_name, lock_packages)?;
                let sid = short_id(&resolved.name, &resolved.version);

                let rename = if dep.name != pkg_name {
                    Some(dep.name.replace('-', "_"))
                } else {
                    None
                };

                Some(DepInfo {
                    name: pkg_name.to_string(),
                    package_id: sid,
                    rename,
                    uses_default_features: dep.default_features,
                    features: dep.features.clone(),
                })
            })
            .collect()
    };

    let deps = resolve_dep_list(&member.dependencies);
    let build_deps = resolve_dep_list(&member.build_dependencies);
    let dev_deps = resolve_dep_list(&member.dev_dependencies);

    (deps, build_deps, dev_deps)
}

/// Find a dependency in the lockfile's dep list for a given package.
fn find_lock_dep<'a>(
    lock_pkg: &LockPackage,
    dep_name: &str,
    all_packages: &'a [LockPackage],
) -> Option<&'a LockPackage> {
    // Lock deps are either "name" or "name version"
    for dep_ref in &lock_pkg.dependencies {
        let parts: Vec<&str> = dep_ref.splitn(2, ' ').collect();
        if parts[0] == dep_name {
            if parts.len() == 2 {
                // "name version" — exact match
                return all_packages
                    .iter()
                    .find(|p| p.name == parts[0] && p.version == parts[1]);
            } else {
                // "name" — only one version exists
                return all_packages.iter().find(|p| p.name == dep_name);
            }
        }
    }
    None
}

/// Resolve dependencies for an external crate using the registry index.
fn resolve_index_deps(
    index_version: &tame_index::IndexVersion,
    lock_packages: &[LockPackage],
    name_counts: &HashMap<String, usize>,
    lock_dep_refs: &[String],
    target: &TargetDescription,
) -> (Vec<DepInfo>, Vec<DepInfo>) {
    let short_id = |name: &str, version: &str| -> String {
        if name_counts.get(name).copied().unwrap_or(0) <= 1 {
            name.to_string()
        } else {
            format!("{name} {version}")
        }
    };

    let mut deps = Vec::new();
    let mut build_deps = Vec::new();

    for index_dep in index_version.dependencies() {
        // Skip dev dependencies for external crates
        if index_dep.kind == Some(tame_index::krate::DependencyKind::Dev) {
            continue;
        }

        // Filter by platform
        if let Some(ref target_str) = index_dep.target {
            if let Ok(platform) = cargo_platform::Platform::from_str(target_str.as_str()) {
                if !matches_target(&platform, target) {
                    continue;
                }
            }
        }

        // The actual package name on the registry
        let pkg_name = index_dep.crate_name();
        #[allow(clippy::needless_borrow)]
        let pkg_name: &str = &pkg_name;

        // Find the resolved version in the lockfile
        let resolved = find_lock_dep_by_name(pkg_name, lock_dep_refs, lock_packages);
        let Some(resolved) = resolved else {
            continue;
        };

        let sid = short_id(&resolved.name, &resolved.version);

        let rename = if index_dep.name != pkg_name {
            Some(index_dep.name.replace('-', "_"))
        } else {
            None
        };

        let dep_info = DepInfo {
            name: pkg_name.to_string(),
            package_id: sid,
            rename,
            uses_default_features: index_dep.default_features,
            features: index_dep.features().iter().map(|s| s.to_string()).collect(),
        };

        if index_dep.kind == Some(tame_index::krate::DependencyKind::Build) {
            build_deps.push(dep_info);
        } else {
            deps.push(dep_info);
        }
    }

    deps.sort_by(|a, b| a.package_id.cmp(&b.package_id));
    build_deps.sort_by(|a, b| a.package_id.cmp(&b.package_id));

    (deps, build_deps)
}

/// Find a package in the lockfile by name, using the dep refs from the parent.
fn find_lock_dep_by_name<'a>(
    name: &str,
    dep_refs: &[String],
    all_packages: &'a [LockPackage],
) -> Option<&'a LockPackage> {
    for dep_ref in dep_refs {
        let parts: Vec<&str> = dep_ref.splitn(2, ' ').collect();
        if parts[0] == name {
            if parts.len() == 2 {
                return all_packages
                    .iter()
                    .find(|p| p.name == parts[0] && p.version == parts[1]);
            } else {
                return all_packages.iter().find(|p| p.name == name);
            }
        }
    }
    None
}

/// Determine the source info for a lockfile package.
fn resolve_pkg_source(pkg: &LockPackage) -> Option<SourceInfo> {
    match pkg.source.as_deref() {
        Some(src) if src.contains("github.com/rust-lang/crates.io-index") => {
            Some(SourceInfo::CratesIo)
        }
        Some(src) if src.starts_with("git+") => {
            if let Some((url, rev)) = src.strip_prefix("git+").and_then(|s| s.rsplit_once('#')) {
                let clean_url = url.split('?').next().unwrap_or(url);
                Some(SourceInfo::Git {
                    url: clean_url.to_string(),
                    rev: rev.to_string(),
                })
            } else {
                None
            }
        }
        Some(src) if src.starts_with("sparse+") || src.starts_with("registry+") => {
            Some(SourceInfo::Registry {
                index: src.to_string(),
            })
        }
        _ => None,
    }
}

/// Simple glob expansion for workspace member paths.
/// Supports trailing `/*` patterns.
fn expand_glob(base: &Path, pattern: &str) -> Vec<std::path::PathBuf> {
    if pattern.ends_with("/*") || pattern.ends_with("\\*") {
        let prefix = &pattern[..pattern.len() - 2];
        let dir = base.join(prefix);
        if let Ok(entries) = std::fs::read_dir(&dir) {
            entries
                .flatten()
                .filter(|e| e.path().join("Cargo.toml").exists())
                .map(|e| e.path())
                .collect()
        } else {
            Vec::new()
        }
    } else if pattern.contains('*') {
        // More complex globs — for now just try the literal path
        // TODO: proper glob support
        let path = base.join(pattern);
        if path.join("Cargo.toml").exists() {
            vec![path]
        } else {
            Vec::new()
        }
    } else {
        let path = base.join(pattern);
        if path.join("Cargo.toml").exists() {
            vec![path]
        } else {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_lock_packages_basic() {
        let lock = r#"
version = 4

[[package]]
name = "serde"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "c8e3592472072e6e22e0a54d5904d9febf8508f65fb8552499a1abc7d1078c3a"
dependencies = [
 "serde_derive",
]

[[package]]
name = "serde_derive"
version = "1.0.210"
source = "registry+https://github.com/rust-lang/crates.io-index"
checksum = "abcd1234"
dependencies = [
 "proc-macro2",
 "quote",
 "syn",
]

[[package]]
name = "my-crate"
version = "0.1.0"
dependencies = [
 "serde",
]
"#;
        let pkgs = parse_lock_packages(lock).unwrap();
        assert_eq!(pkgs.len(), 3);
        assert_eq!(pkgs[0].name, "serde");
        assert_eq!(pkgs[0].dependencies, vec!["serde_derive"]);
        assert!(pkgs[2].source.is_none()); // local crate
    }

    #[test]
    fn resolve_pkg_source_crates_io() {
        let pkg = LockPackage {
            name: "serde".into(),
            version: "1.0.210".into(),
            source: Some("registry+https://github.com/rust-lang/crates.io-index".into()),
            checksum: None,
            dependencies: vec![],
        };
        assert_eq!(resolve_pkg_source(&pkg), Some(SourceInfo::CratesIo));
    }

    #[test]
    fn resolve_pkg_source_git() {
        let pkg = LockPackage {
            name: "foo".into(),
            version: "0.1.0".into(),
            source: Some("git+https://github.com/user/foo.git?branch=main#abc123".into()),
            checksum: None,
            dependencies: vec![],
        };
        assert_eq!(
            resolve_pkg_source(&pkg),
            Some(SourceInfo::Git {
                url: "https://github.com/user/foo.git".into(),
                rev: "abc123".into(),
            })
        );
    }

    #[test]
    fn expand_glob_literal() {
        let tmp = std::env::temp_dir().join("cargo-nix-test-glob");
        let member = tmp.join("foo");
        let _ = std::fs::create_dir_all(&member);
        std::fs::write(member.join("Cargo.toml"), "[package]\nname = \"foo\"\n").ok();

        let result = expand_glob(&tmp, "foo");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], member);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_manifest_deps_string_version() {
        let toml: toml::Value = toml::from_str(
            r#"
            [dependencies]
            serde = "1.0"
            "#,
        )
        .unwrap();
        let deps = parse_manifest_deps(toml.get("dependencies"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "serde");
        assert_eq!(deps[0].version_req, Some("1.0".into()));
        assert!(!deps[0].optional);
    }

    #[test]
    fn parse_manifest_deps_table_form() {
        let toml: toml::Value = toml::from_str(
            r#"
            [dependencies]
            tokio-rustls = { package = "tokio-rustls", version = "0.26", optional = true, default-features = false, features = ["ring"] }
            "#,
        )
        .unwrap();
        let deps = parse_manifest_deps(toml.get("dependencies"));
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "tokio-rustls");
        assert!(deps[0].optional);
        assert!(!deps[0].default_features);
        assert_eq!(deps[0].features, vec!["ring"]);
    }
}
