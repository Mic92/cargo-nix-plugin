// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Resolve a cargo workspace from Cargo.lock + registry index, without cargo metadata.
//!
//! This avoids downloading crate sources at eval time. Fields that require
//! reading the crate's `Cargo.toml` (edition, procMacro, libPath, etc.) are
//! left as `None`/default so that `buildRustCrate` can auto-detect them at
//! build time.

use std::collections::{hash_map::Entry, BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::cfg_eval::{matches_target, TargetDescription};
use crate::feature_resolve::{self, DepFeatureInfo, PackageFeatureInfo};
use crate::lockfile::parse_lockfile;
use crate::registry;
use crate::resolve::{CrateInfo, DepInfo, SourceInfo, WorkspaceResult};

mod manifest;
use manifest::{parse_workspace, GitCheckout, ManifestDep, WorkspaceMember};

/// A parsed Cargo.lock package entry.
#[derive(Debug, Clone, serde::Deserialize)]
struct LockPackage {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
    /// Dependency references as they appear in Cargo.lock: "name" or "name version".
    #[serde(default)]
    dependencies: Vec<String>,
}

/// Resolve a workspace using Cargo.lock + registry index (no cargo metadata).
///
/// `workspace_root` is the directory containing the workspace Cargo.toml.
/// `cargo_lock` is the contents of Cargo.lock.
/// `cargo_home` is the path to the cargo home directory (for registry index).
/// `crates_io_index` is the (already normalized) sparse index URL used for
/// crates whose lockfile source is crates.io — callers obtain it via
/// [`registry::resolve_crates_io_index`].
#[allow(clippy::too_many_arguments)]
pub fn resolve_from_lockfile(
    workspace_root: &Path,
    cargo_lock: &str,
    cargo_home: &Path,
    crates_io_index: &str,
    target: &TargetDescription,
    root_features: &[String],
    no_default_features: bool,
    git_sources: &HashMap<String, PathBuf>,
) -> Result<WorkspaceResult, String> {
    // 1. Parse Cargo.lock
    let lock_packages = parse_lock_packages(cargo_lock)?;
    let lockfile_hashes = parse_lockfile(cargo_lock);

    // 2. Parse workspace manifests
    let workspace = parse_workspace(workspace_root)?;

    // 3. Build package ID shortener: "name" if unique, else "name version".
    let short_id = ShortId::new(&lock_packages);

    // 4. Build the resolved crates
    let workspace_member_names: HashSet<String> =
        workspace.members.iter().map(|m| m.name.clone()).collect();
    // Canonical workspace root for the out-of-tree path-dep check.
    let canonical_ws_root =
        std::fs::canonicalize(workspace_root).unwrap_or_else(|_| workspace_root.to_path_buf());

    // Lockfile source string → sparse index URL (None for local/git).
    // crates.io is redirected through `crates_io_index` for mirrors (#20).
    let source_to_index_url =
        |source: Option<&str>| registry::source_to_index_url(source, crates_io_index);

    // Prefetch every (registry, name) the lockfile mentions before the
    // serial loop — cold-cache eval goes from O(n·RTT) to ~O(n/workers·RTT).
    let prefetch_jobs: Vec<registry::PrefetchJob> = lock_packages
        .iter()
        .filter(|p| !workspace_member_names.contains(&p.name))
        .filter_map(|p| {
            Some(registry::PrefetchJob {
                url: source_to_index_url(p.source.as_deref())?,
                name: p.name.clone(),
                version: p.version.clone(),
            })
        })
        .collect();
    registry::prefetch_index(cargo_home, &prefetch_jobs)?;

    // Per-checkout cache of parsed git workspace manifests, so N crates from
    // one git repo (gitoxide: 36) don't re-walk/re-parse N times.
    let mut git_checkouts: HashMap<PathBuf, GitCheckout> = HashMap::new();

    let mut crates = BTreeMap::new();
    let mut workspace_members = BTreeMap::new();

    for pkg in &lock_packages {
        let sid = short_id.get(&pkg.name, &pkg.version);
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
                resolve_member_deps(member, &pkg.dependencies, &lock_packages, &short_id, target);

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
                    resolved_default_features: Vec::new(), // filled in below
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
            let mut source_info = resolve_pkg_source(pkg);
            let sha256 = lockfile_hashes
                .get(&(pkg.name.clone(), pkg.version.clone()))
                .cloned();

            let index_url = source_to_index_url(pkg.source.as_deref());

            // Hard-fail on index lookup errors: silently continuing with
            // empty deps yields derivations that E0433 deep in the sandbox.
            let index_version = index_url
                .as_deref()
                .map(|url| {
                    registry::lookup_version(cargo_home, url, &pkg.name, &pkg.version).map_err(
                        |e| {
                            format!(
                                "failed to look up {} {} in index '{}': {e}",
                                pkg.name, pkg.version, url
                            )
                        },
                    )
                })
                .transpose()?;

            // Set when we parsed a Cargo.toml at eval time (git/path deps).
            // Registry crates stay None — buildRustCrate auto-detects those
            // fields at build time, but procMacro is needed at eval time for
            // lib/default.nix's cross-compile routing.
            let mut manifest_member: Option<WorkspaceMember> = None;
            let (dependencies, build_dependencies, features_btree, links) =
                if let Some(ref version) = index_version {
                    let (deps, build_deps) = resolve_index_deps(
                        version,
                        &lock_packages,
                        &short_id,
                        &pkg.dependencies,
                        target,
                    );
                    let features = registry::features_for_version(version);
                    let links = version.links.as_deref().map(|s| s.to_string());
                    (deps, build_deps, features.into_iter().collect(), links)
                } else if let Some(SourceInfo::Git { url, rev, .. }) = &source_info {
                    // Git dependency — read its Cargo.toml from the
                    // pre-fetched checkout the Nix wrapper handed us.
                    let key = format!("{url}#{rev}");
                    let checkout_path = git_sources.get(&key).ok_or_else(|| {
                        format!(
                            "git source for {} {} not provided: expected gitSources.\"{key}\" \
                             to point at a checkout (lib/default.nix should derive this \
                             automatically from Cargo.lock)",
                            pkg.name, pkg.version,
                        )
                    })?;
                    let checkout = match git_checkouts.entry(checkout_path.clone()) {
                        Entry::Occupied(e) => e.into_mut(),
                        Entry::Vacant(e) => e.insert(GitCheckout::scan(checkout_path)?),
                    };
                    let member = checkout.find(&pkg.name).ok_or_else(|| {
                        format!(
                            "package {} not found in git checkout {} ({key})",
                            pkg.name,
                            checkout_path.display()
                        )
                    })?;
                    let (deps, build_deps, _) = resolve_member_deps(
                        member,
                        &pkg.dependencies,
                        &lock_packages,
                        &short_id,
                        target,
                    );
                    // Re-derive source_info with the sub-path now that we
                    // know which sub-directory holds this crate.
                    let sub_path = member
                        .manifest_dir
                        .strip_prefix(&*checkout_path.to_string_lossy())
                        .map(|s| s.trim_start_matches('/'))
                        .filter(|s| !s.is_empty())
                        .map(String::from);
                    source_info = Some(SourceInfo::Git {
                        url: url.clone(),
                        rev: rev.clone(),
                        sub_path,
                    });
                    manifest_member = Some(member.clone());
                    (
                        deps,
                        build_deps,
                        member.features.clone(),
                        member.links.clone(),
                    )
                } else if pkg.source.is_none() {
                    // Local path dependency that is NOT a [workspace] member.
                    // `parse_workspace` discovered it by following `path = "..."`
                    // edges; the lockfile records no path for it.
                    let member = workspace.path_deps.get(&pkg.name).ok_or_else(|| {
                        format!(
                            "package {} {} has no `source` in Cargo.lock and is not a \
                             workspace member. It is likely a `path = \"...\"` dependency \
                             but no referring manifest under {} declares it. If it lives \
                             outside the workspace src, it cannot be built (the Nix \
                             derivation only sees `src`).",
                            pkg.name,
                            pkg.version,
                            workspace_root.display(),
                        )
                    })?;
                    // Reject paths that escape the workspace src — the drv
                    // gets `src = <workspace>`, so ../sibling would silently
                    // build against the wrong directory (or nothing).
                    let dir = std::fs::canonicalize(&member.manifest_dir)
                        .unwrap_or_else(|_| PathBuf::from(&member.manifest_dir));
                    if !dir.starts_with(&canonical_ws_root) {
                        return Err(format!(
                            "path dependency {} at {} is outside the workspace root {}. \
                             cargo-nix-plugin builds local crates from `src`; a path dep \
                             that points outside it has no source in the build sandbox. \
                             Either add it to [workspace].members and move it under the \
                             workspace, or vendor it.",
                            pkg.name,
                            dir.display(),
                            canonical_ws_root.display(),
                        ));
                    }
                    let (deps, build_deps, _) = resolve_member_deps(
                        member,
                        &pkg.dependencies,
                        &lock_packages,
                        &short_id,
                        target,
                    );
                    source_info = Some(SourceInfo::Local {
                        path: member.manifest_dir.clone(),
                    });
                    manifest_member = Some(member.clone());
                    (
                        deps,
                        build_deps,
                        member.features.clone(),
                        member.links.clone(),
                    )
                } else {
                    (Vec::new(), Vec::new(), BTreeMap::new(), None)
                };

            // For git and path deps we parsed the manifest ourselves — don't
            // leave edition/proc_macro/lib_path for build-time auto-detect.
            let path_member = manifest_member.as_ref();

            crates.insert(
                sid,
                CrateInfo {
                    crate_name: pkg.name.clone(),
                    version: pkg.version.clone(),
                    // These fields will be auto-detected at build time by buildRustCrate
                    edition: path_member.map(|m| m.edition.clone()).unwrap_or_default(),
                    sha256,
                    source: source_info,
                    dependencies,
                    build_dependencies,
                    dev_dependencies: Vec::new(), // Not needed for external crates
                    features: features_btree,
                    resolved_default_features: Vec::new(), // filled in below
                    proc_macro: path_member.map(|m| m.proc_macro).unwrap_or(false),
                    build: path_member.and_then(|m| m.build_script.clone()),
                    lib_path: path_member.and_then(|m| m.lib_path.clone()),
                    lib_name: path_member.and_then(|m| m.lib_name.clone()),
                    crate_bin: Vec::new(), // Not needed for external crates
                    lib_crate_types: path_member
                        .map(|m| m.lib_crate_types.clone())
                        .unwrap_or_default(),
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
        // Keyed by *local* dep name (raw, dash-preserved) — what `dep:X` and
        // `X/feat` rules reference. Dev-deps included: resolver v2 unifies
        // dev/normal within a unit, and a dev-only edge (e.g. ripgrep →
        // serde_derive) is otherwise unreachable.
        let deps_iter = info
            .dependencies
            .iter()
            .chain(&info.build_dependencies)
            .chain(&info.dev_dependencies);
        let optional_deps = deps_iter
            .clone()
            .filter(|d| d.optional)
            .map(|d| d.local_name().to_string())
            .collect();
        let all_deps: Vec<DepFeatureInfo> = deps_iter
            .map(|d| DepFeatureInfo {
                name: d.local_name().to_string(),
                package_id: d.package_id.clone(),
                uses_default_features: d.uses_default_features,
                features: d.features.clone(),
                optional: d.optional,
            })
            .collect();

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

    let resolution = feature_resolve::resolve_features(&feature_packages, &root_packages);

    // Apply resolved features back to crates
    for (pkg_id, features) in &resolution.features {
        if let Some(info) = crates.get_mut(pkg_id) {
            info.resolved_default_features = features.iter().cloned().collect();
        }
    }

    // Drop inactive optional deps — the lockfile lists all of them for
    // version pinning, but buildRustCrate expects only active edges.
    //
    // KNOWN LIMITATION: resolver=2 splits feature sets by host/target
    // (FeaturesFor); we unify them (same as `cargo metadata`'s `.resolve`).
    // Conservative superset, so builds succeed; cost is occasional feature
    // over-enabling on build-dep instances. buildRustCrate can't express
    // duplicated nodes today.
    for (pkg_id, info) in crates.iter_mut() {
        let keep = |dep: &DepInfo| {
            !dep.optional
                || resolution
                    .active_optional_deps
                    .contains(&(pkg_id.clone(), dep.local_name().to_string()))
        };
        info.dependencies.retain(&keep);
        info.build_dependencies.retain(&keep);
        info.dev_dependencies.retain(&keep);
    }

    // Determine root
    let root = workspace
        .root_package
        .as_ref()
        .map(|p| short_id.get(&p.name, &p.version));

    Ok(WorkspaceResult {
        api_level: crate::resolve::API_LEVEL,
        root,
        workspace_root: workspace_root.to_string_lossy().to_string(),
        workspace_members,
        crates,
    })
}

/// Compute short package IDs: "name" if unique in the lockfile, else
/// "name version". Shared between the main loop and dep resolution.
struct ShortId {
    name_counts: HashMap<String, usize>,
}

impl ShortId {
    fn new(packages: &[LockPackage]) -> Self {
        let mut name_counts: HashMap<String, usize> = HashMap::new();
        for pkg in packages {
            *name_counts.entry(pkg.name.clone()).or_default() += 1;
        }
        Self { name_counts }
    }

    fn get(&self, name: &str, version: &str) -> String {
        if self.name_counts.get(name).copied().unwrap_or(0) <= 1 {
            name.to_string()
        } else {
            format!("{name} {version}")
        }
    }
}

/// Parse Cargo.lock into structured package entries.
fn parse_lock_packages(cargo_lock: &str) -> Result<Vec<LockPackage>, String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        package: Vec<LockPackage>,
    }
    let lock: Lock =
        toml::from_str(cargo_lock).map_err(|e| format!("Failed to parse Cargo.lock: {e}"))?;
    Ok(lock.package)
}

/// Resolve dependencies for a workspace member (or git-sourced crate whose
/// manifest we parsed ourselves) using the lockfile.
fn resolve_member_deps(
    member: &WorkspaceMember,
    lock_dep_refs: &[String],
    lock_packages: &[LockPackage],
    short_id: &ShortId,
    target: &TargetDescription,
) -> (Vec<DepInfo>, Vec<DepInfo>, Vec<DepInfo>) {
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
                // Find the resolved version in the lockfile. Same
                // disambiguation need as the index path: a workspace
                // manifest can depend on two majors of one package
                // under different renames.
                let pkg_name = dep.package.as_deref().unwrap_or(&dep.name);
                let req = dep
                    .version_req
                    .as_deref()
                    .and_then(|r| semver::VersionReq::parse(r).ok())
                    .unwrap_or(semver::VersionReq::STAR);
                let resolved =
                    find_lock_dep_by_name_and_req(pkg_name, &req, lock_dep_refs, lock_packages)?;
                let sid = short_id.get(&resolved.name, &resolved.version);

                // Raw dep key; consumers normalize for --extern themselves.
                let rename = if dep.name != pkg_name {
                    Some(dep.name.clone())
                } else {
                    None
                };

                Some(DepInfo {
                    name: pkg_name.to_string(),
                    package_id: sid,
                    rename,
                    uses_default_features: dep.default_features,
                    features: dep.features.clone(),
                    optional: dep.optional,
                })
            })
            .collect()
    };

    let deps = resolve_dep_list(&member.dependencies);
    let build_deps = resolve_dep_list(&member.build_dependencies);
    let dev_deps = resolve_dep_list(&member.dev_dependencies);

    (deps, build_deps, dev_deps)
}

/// Resolve dependencies for an external crate using the registry index.
fn resolve_index_deps(
    index_version: &tame_index::IndexVersion,
    lock_packages: &[LockPackage],
    short_id: &ShortId,
    lock_dep_refs: &[String],
    target: &TargetDescription,
) -> (Vec<DepInfo>, Vec<DepInfo>) {
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

        // Find the resolved version in the lockfile. Must check the
        // semver requirement, not just the name: aws-smithy-types has
        // two renamed deps on the same package at different majors
        // (http-body-0-4 → http-body@^0.4, http-body-1-0 → http-body@^1).
        // Name-only matching returns the first of the two lockfile
        // entries for both, collapsing them to one version.
        let req = index_dep.version_requirement();
        let resolved = find_lock_dep_by_name_and_req(pkg_name, &req, lock_dep_refs, lock_packages);
        let Some(resolved) = resolved else {
            continue;
        };

        let sid = short_id.get(&resolved.name, &resolved.version);

        // Raw dep key (dashes preserved). Feature rules in Cargo.toml use
        // this form (`pki-types/std`, `dep:http-body-1-0`). Consumers
        // that need a rustc identifier normalize themselves — resolve.rs:508
        // and build-rust-crate/default.nix:49 both apply `-` → `_`.
        let rename = if index_dep.name.as_str() != pkg_name {
            Some(index_dep.name.to_string())
        } else {
            None
        };

        let dep_info = DepInfo {
            name: pkg_name.to_string(),
            package_id: sid,
            rename,
            uses_default_features: index_dep.default_features,
            features: index_dep.features().iter().map(|s| s.to_string()).collect(),
            optional: index_dep.is_optional(),
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

/// Find a package in the lockfile by name AND semver requirement.
///
/// Name alone is ambiguous when a crate depends on multiple majors of one
/// package under different renames; only the req disambiguates.
fn find_lock_dep_by_name_and_req<'a>(
    name: &str,
    req: &semver::VersionReq,
    dep_refs: &[String],
    all_packages: &'a [LockPackage],
) -> Option<&'a LockPackage> {
    // A parseable version that doesn't satisfy `req` must NOT fall back to a
    // name match: it means cargo dropped this edge entirely (see test
    // `find_lock_dep_rejects_unsatisfiable_req` for the hyper-0-14 case).
    // Fallback is only for unparseable versions.
    let mut unparseable_fallback = None;
    for dep_ref in dep_refs {
        let mut parts = dep_ref.splitn(2, ' ');
        if parts.next() != Some(name) {
            continue;
        }
        let pkg = match parts.next() {
            Some(version) => all_packages
                .iter()
                .find(|p| p.name == name && p.version == version),
            None => all_packages.iter().find(|p| p.name == name),
        };
        let Some(pkg) = pkg else { continue };

        let Ok(v) = semver::Version::parse(&pkg.version) else {
            unparseable_fallback.get_or_insert(pkg);
            continue;
        };
        // semver::VersionReq won't match a pre-release unless the req names
        // one, but cargo locks to them (tokio 1.49.0+anthropic.1 vs ^1.49).
        let stripped = semver::Version::new(v.major, v.minor, v.patch);
        if req.matches(&v) || req.matches(&stripped) {
            return Some(pkg);
        }
    }
    unparseable_fallback
}

/// Determine the source info for a lockfile package.
fn resolve_pkg_source(pkg: &LockPackage) -> Option<SourceInfo> {
    let src = pkg.source.as_deref()?;
    if src.contains("github.com/rust-lang/crates.io-index") {
        Some(SourceInfo::CratesIo)
    } else if let Some(rest) = src.strip_prefix("git+") {
        let (url, rev) = rest.rsplit_once('#')?;
        let clean_url = url.split('?').next().unwrap_or(url);
        Some(SourceInfo::Git {
            url: clean_url.to_string(),
            rev: rev.to_string(),
            sub_path: None,
        })
    } else if src.starts_with("sparse+") || src.starts_with("registry+") {
        Some(SourceInfo::Registry {
            index: src.to_string(),
        })
    } else {
        None
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
            dependencies: vec![],
        };
        assert_eq!(
            resolve_pkg_source(&pkg),
            Some(SourceInfo::Git {
                url: "https://github.com/user/foo.git".into(),
                rev: "abc123".into(),
                sub_path: None,
            })
        );
    }

    /// Two renamed deps on the same package at different majors must each
    /// resolve to their own lockfile entry. aws-smithy-types does this:
    ///   http-body-0-4 = { package = "http-body", version = "^0.4.5" }
    ///   http-body-1-0 = { package = "http-body", version = "^1" }
    /// Name-only lookup collapsed both to whichever appeared first in the
    /// lockfile's dep list.
    #[test]
    fn find_lock_dep_disambiguates_by_version_req() {
        let packages = vec![
            LockPackage {
                name: "http-body".into(),
                version: "0.4.6".into(),
                source: None,
                dependencies: vec![],
            },
            LockPackage {
                name: "http-body".into(),
                version: "1.0.1".into(),
                source: None,
                dependencies: vec![],
            },
        ];
        let dep_refs = vec!["http-body 0.4.6".to_string(), "http-body 1.0.1".to_string()];

        let r04 = semver::VersionReq::parse("^0.4.5").unwrap();
        let r1 = semver::VersionReq::parse("^1").unwrap();

        let got04 = find_lock_dep_by_name_and_req("http-body", &r04, &dep_refs, &packages).unwrap();
        let got1 = find_lock_dep_by_name_and_req("http-body", &r1, &dep_refs, &packages).unwrap();

        assert_eq!(got04.version, "0.4.6");
        assert_eq!(got1.version, "1.0.1");
    }

    /// Pre-release versions in the lockfile must match their req.
    /// semver::VersionReq rejects pre-releases unless the req names one,
    /// but cargo locks to them freely. We strip pre/build for the check.
    #[test]
    fn find_lock_dep_matches_prerelease() {
        let packages = vec![LockPackage {
            name: "tokio".into(),
            version: "1.49.0+anthropic.1".into(),
            source: None,
            dependencies: vec![],
        }];
        let dep_refs = vec!["tokio 1.49.0+anthropic.1".to_string()];
        let req = semver::VersionReq::parse("^1.49").unwrap();

        let got = find_lock_dep_by_name_and_req("tokio", &req, &dep_refs, &packages);
        assert_eq!(got.unwrap().version, "1.49.0+anthropic.1");
    }

    /// An index dep whose version req doesn't match any lockfile entry
    /// must return None — cargo dropped that edge. aws-smithy-http-client
    /// has both `hyper` (^1.6) and `hyper-0-14 = {package="hyper", ^0.14}`.
    /// When only hyper@1.8.1 is in the lockfile (0.14 never activated),
    /// the ^0.14 lookup must fail so we don't emit a spurious DepInfo with
    /// rename="hyper-0-14" → `--extern hyper_0_14=.../libhyper.rlib`
    /// (which shadows the `--extern hyper=` the code actually imports).
    #[test]
    fn find_lock_dep_rejects_unsatisfiable_req() {
        let packages = vec![LockPackage {
            name: "hyper".into(),
            version: "1.8.1".into(),
            source: None,
            dependencies: vec![],
        }];
        // Bare name — only one hyper in lockfile, so no version suffix
        let dep_refs = vec!["hyper".to_string()];

        // The legacy renamed dep — must NOT match 1.8.1
        let r014 = semver::VersionReq::parse("^0.14.26").unwrap();
        assert!(
            find_lock_dep_by_name_and_req("hyper", &r014, &dep_refs, &packages).is_none(),
            "^0.14 must not fall back to hyper@1.8.1"
        );

        // The current dep — matches
        let r1 = semver::VersionReq::parse("^1.6.0").unwrap();
        assert_eq!(
            find_lock_dep_by_name_and_req("hyper", &r1, &dep_refs, &packages)
                .unwrap()
                .version,
            "1.8.1"
        );
    }

    /// `edition.workspace = true` and `version.workspace = true` inherit
    /// from [workspace.package]. Real case: the monorepo root sets
    /// edition = "2024", members like ant-jeprof inherit it and use
    /// 2024-only syntax (let chains). Without inheritance we fall
    /// through to "2021" and hit "let chains are only allowed in Rust
    /// End-to-end: a `git+` lockfile entry resolves its dependency edges,
    /// feature table, links and `sub_path` from a pre-fetched checkout.
    /// Models the gitoxide shape: one repo, virtual workspace, many member
    /// crates depending on each other via `workspace = true`.
    #[test]
    fn git_source_resolves_from_checkout() {
        let tmp = tempfile::tempdir().unwrap();

        // --- fake git checkout (as if builtins.fetchGit produced it) ---
        let checkout = tmp.path().join("checkout");
        std::fs::create_dir_all(checkout.join("crates/foo/src")).unwrap();
        std::fs::create_dir_all(checkout.join("crates/bar/src")).unwrap();
        std::fs::write(
            checkout.join("Cargo.toml"),
            r#"
[workspace]
members = ["crates/*"]
[workspace.dependencies]
bar = { path = "crates/bar", version = "0.1.0" }
"#,
        )
        .unwrap();
        std::fs::write(
            checkout.join("crates/foo/Cargo.toml"),
            r#"
[package]
name = "foo"
version = "0.1.0"
links = "foo_sys"
[dependencies]
bar = { workspace = true }
[features]
default = ["a"]
a = []
"#,
        )
        .unwrap();
        // bar is a proc-macro with a build script and non-default edition,
        // exercising eval-time manifest field forwarding (asserted below).
        std::fs::write(
            checkout.join("crates/bar/Cargo.toml"),
            r#"
[package]
name = "bar"
version = "0.1.0"
edition = "2018"
build = "build.rs"
[lib]
proc-macro = true
"#,
        )
        .unwrap();

        // --- consuming workspace ---
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            r#"
[package]
name = "consumer"
version = "0.1.0"
edition = "2021"
[dependencies]
foo = { git = "https://example.com/repo" }
"#,
        )
        .unwrap();

        let cargo_lock = r#"
version = 4
[[package]]
name = "consumer"
version = "0.1.0"
dependencies = ["foo"]
[[package]]
name = "foo"
version = "0.1.0"
source = "git+https://example.com/repo?branch=main#abc123"
dependencies = ["bar"]
[[package]]
name = "bar"
version = "0.1.0"
source = "git+https://example.com/repo?branch=main#abc123"
"#;

        let mut git_sources = HashMap::new();
        git_sources.insert(
            "https://example.com/repo#abc123".to_string(),
            checkout.clone(),
        );

        let target = TargetDescription {
            name: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            vendor: "unknown".into(),
            env: "gnu".into(),
            family: vec!["unix".into()],
            pointer_width: "64".into(),
            endian: "little".into(),
            unix: true,
            windows: false,
            extra_cfgs: vec![],
        };

        let result = resolve_from_lockfile(
            &ws,
            cargo_lock,
            tmp.path(), // cargo_home — unused, no registry crates
            "sparse+https://index.crates.io/",
            &target,
            &[],
            false,
            &git_sources,
        )
        .unwrap();

        let foo = &result.crates["foo"];
        // Dependency edge foo → bar came from the checkout's Cargo.toml,
        // resolved via [workspace.dependencies].
        assert_eq!(foo.dependencies.len(), 1, "foo → bar edge");
        assert_eq!(foo.dependencies[0].package_id, "bar");
        assert_eq!(foo.links.as_deref(), Some("foo_sys"));
        assert!(foo.features.contains_key("default"));
        match &foo.source {
            Some(SourceInfo::Git { url, rev, sub_path }) => {
                assert_eq!(url, "https://example.com/repo");
                assert_eq!(rev, "abc123");
                assert_eq!(sub_path.as_deref(), Some("crates/foo"));
            }
            other => panic!("expected Git source, got {other:?}"),
        }

        let bar = &result.crates["bar"];
        match &bar.source {
            Some(SourceInfo::Git { sub_path, .. }) => {
                assert_eq!(sub_path.as_deref(), Some("crates/bar"))
            }
            other => panic!("expected Git source, got {other:?}"),
        }
        // Manifest fields parsed from the git checkout must reach CrateInfo:
        // procMacro especially is consumed at eval time by lib/default.nix to
        // route proc-macro deps to the build platform under cross-compile.
        assert!(
            bar.proc_macro,
            "bar's [lib] proc-macro=true must be forwarded"
        );
        assert_eq!(bar.edition, "2018", "bar's edition must be forwarded");
        assert_eq!(
            bar.build.as_deref(),
            Some("build.rs"),
            "bar's build script must be forwarded"
        );

        // Feature resolution propagated through the git crate: consumer
        // pulls foo's default → "a".
        assert!(foo.resolved_default_features.contains(&"a".to_string()));
    }

    /// Dev-dependencies of workspace members participate in feature
    /// resolution. ripgrep's only edge to serde_derive is a dev-dep; if we
    /// skip dev-deps when seeding the feature resolver, serde_derive (and
    /// transitively syn/proc-macro2/quote) end up with `resolvedFeatures =
    /// []` and fail to compile with hundreds of E0432/E0433. The dev-dep
    /// itself is also dropped from the .buildTests drv because optional-dep
    /// pruning sees it as unreached.
    #[test]
    fn dev_deps_participate_in_feature_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path();
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join("devonly/src")).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            r#"
[workspace]
members = ["devonly"]
[package]
name = "root"
version = "0.1.0"
edition = "2021"
[dev-dependencies]
devonly = { path = "devonly", features = ["extra"] }
"#,
        )
        .unwrap();
        std::fs::write(
            ws.join("devonly/Cargo.toml"),
            r#"
[package]
name = "devonly"
version = "0.1.0"
[features]
default = ["a"]
a = []
extra = []
"#,
        )
        .unwrap();

        let cargo_lock = r#"
version = 4
[[package]]
name = "root"
version = "0.1.0"
dependencies = ["devonly"]
[[package]]
name = "devonly"
version = "0.1.0"
"#;

        let target = TargetDescription {
            name: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            vendor: "unknown".into(),
            env: "gnu".into(),
            family: vec!["unix".into()],
            pointer_width: "64".into(),
            endian: "little".into(),
            unix: true,
            windows: false,
            extra_cfgs: vec![],
        };

        let result = resolve_from_lockfile(
            ws,
            cargo_lock,
            tmp.path(),
            "sparse+https://index.crates.io/",
            &target,
            &[],
            false,
            &HashMap::new(),
        )
        .unwrap();

        let root = &result.crates["root"];
        assert_eq!(
            root.dev_dependencies.len(),
            1,
            "root → devonly dev-dep edge"
        );
        assert_eq!(root.dev_dependencies[0].package_id, "devonly");

        // Reached via dev-dep edge: root requested features=["extra"] plus
        // default-features=true → default → a. devonly is a workspace
        // member too so it's also seeded with ["default"], but "extra"
        // can ONLY arrive via root's dev-dep edge.
        let devonly = &result.crates["devonly"];
        for f in ["default", "a", "extra"] {
            assert!(
                devonly.resolved_default_features.contains(&f.to_string()),
                "devonly missing feature {f}: {:?}",
                devonly.resolved_default_features
            );
        }
    }

    fn linux_target() -> TargetDescription {
        TargetDescription {
            name: "x86_64-unknown-linux-gnu".into(),
            os: "linux".into(),
            arch: "x86_64".into(),
            vendor: "unknown".into(),
            env: "gnu".into(),
            family: vec!["unix".into()],
            pointer_width: "64".into(),
            endian: "little".into(),
            unix: true,
            windows: false,
            extra_cfgs: vec![],
        }
    }

    /// A `path = "..."` dependency that is NOT in [workspace].members must
    /// still get a Local source pointing at its subdir, with edition and
    /// transitive deps from its own Cargo.toml. Cargo.lock gives it
    /// `source = None` and no path; previously this fell through to
    /// source=None and built against the workspace root with artifacts:[].
    ///
    /// Fixture: sample-path-dep [dev-dep]→ devdep (path ./devdep)
    ///                                     └→ inner (path ../inner)
    /// Neither devdep nor inner is a [workspace] member.
    #[test]
    fn path_dep_non_member_gets_local_source() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample-path-dep");
        let cargo_lock = std::fs::read_to_string(ws.join("Cargo.lock")).unwrap();
        let result = resolve_from_lockfile(
            &ws,
            &cargo_lock,
            &ws, // cargo_home — unused, no registry crates
            "sparse+https://index.crates.io/",
            &linux_target(),
            &[],
            false,
            &HashMap::new(),
        )
        .unwrap();

        let devdep = &result.crates["devdep"];
        match &devdep.source {
            Some(SourceInfo::Local { path }) => {
                assert_eq!(Path::new(path), ws.join("devdep"))
            }
            other => panic!("devdep: expected Local source, got {other:?}"),
        }
        assert_eq!(
            devdep.edition, "2021",
            "edition read from devdep/Cargo.toml"
        );
        assert_eq!(
            devdep.dependencies.len(),
            1,
            "devdep → inner edge from devdep/Cargo.toml"
        );
        assert_eq!(devdep.dependencies[0].package_id, "inner");

        // Transitive: inner was reached via devdep's `path = "../inner"`,
        // joined onto devdep's dir and canonicalized back under ws.
        let inner = &result.crates["inner"];
        match &inner.source {
            Some(SourceInfo::Local { path }) => {
                assert_eq!(Path::new(path), ws.join("inner"))
            }
            other => panic!("inner: expected Local source, got {other:?}"),
        }

        // Not promoted to workspace members — they're deps, not roots.
        assert!(!result.workspace_members.contains_key("devdep"));
        assert!(!result.workspace_members.contains_key("inner"));
    }

    /// `leaf` is optional in [dependencies] AND required in
    /// [dev-dependencies]. The dev-dep edge must reach leaf with its own
    /// features even though the optional normal-dep edge is inactive.
    /// Ground truth: `cargo metadata` on this fixture resolves leaf to
    /// ["d","default","extra"] (see fixture Cargo.toml comment).
    #[test]
    fn feature_dev_dep_shadows_optional_normal_dep() {
        let ws = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/feature-dev-shadows-optional");
        let cargo_lock = std::fs::read_to_string(ws.join("Cargo.lock")).unwrap();
        let result = resolve_from_lockfile(
            &ws,
            &cargo_lock,
            &ws,
            "sparse+https://index.crates.io/",
            &linux_target(),
            &[],
            false,
            &HashMap::new(),
        )
        .unwrap();

        let leaf = &result.crates["leaf"];
        let mut got = leaf.resolved_default_features.clone();
        got.sort();
        assert_eq!(got, vec!["d", "default", "extra"], "leaf features");

        // The optional normal-dep edge stays inactive (with-leaf not set):
        // root.dependencies must NOT contain leaf, but dev_dependencies must.
        let root = &result.crates["root"];
        assert!(
            root.dependencies.iter().all(|d| d.name != "leaf"),
            "optional normal-dep edge leaked into dependencies"
        );
        assert!(
            root.dev_dependencies.iter().any(|d| d.name == "leaf"),
            "dev-dep edge dropped"
        );
    }

    /// A path dep pointing OUTSIDE the workspace src must fail loudly at
    /// resolve time. The drv only has `src` in its sandbox; ../sibling
    /// would otherwise build against nothing and emit artifacts:[].
    #[test]
    fn path_dep_outside_workspace_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        let sibling = tmp.path().join("sibling");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(sibling.join("src")).unwrap();
        std::fs::write(
            ws.join("Cargo.toml"),
            r#"
[package]
name = "root"
version = "0.1.0"
edition = "2021"
[dependencies]
sibling = { path = "../sibling" }
"#,
        )
        .unwrap();
        std::fs::write(
            sibling.join("Cargo.toml"),
            "[package]\nname = \"sibling\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        let cargo_lock = r#"
version = 4
[[package]]
name = "root"
version = "0.1.0"
dependencies = ["sibling"]
[[package]]
name = "sibling"
version = "0.1.0"
"#;

        let err = resolve_from_lockfile(
            &ws,
            cargo_lock,
            tmp.path(),
            "sparse+https://index.crates.io/",
            &linux_target(),
            &[],
            false,
            &HashMap::new(),
        )
        .unwrap_err();
        assert!(
            err.contains("sibling") && err.contains("outside the workspace root"),
            "error should name the crate and the reason: {err}"
        );
    }

    /// Missing gitSources entry surfaces a clear error naming the key.
    #[test]
    fn git_source_missing_checkout_errors() {
        let key = "https://example.com/repo#abc123";
        let pkg = LockPackage {
            name: "foo".into(),
            version: "0.1.0".into(),
            source: Some("git+https://example.com/repo#abc123".into()),
            dependencies: vec![],
        };
        // Just exercise the source parse + key format, since the full
        // resolve needs a workspace on disk.
        match resolve_pkg_source(&pkg) {
            Some(SourceInfo::Git { url, rev, .. }) => {
                assert_eq!(format!("{url}#{rev}"), key)
            }
            other => panic!("{other:?}"),
        }
    }
}
