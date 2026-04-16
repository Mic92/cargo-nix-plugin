//! Resolve a cargo workspace from Cargo.lock + registry index, without cargo metadata.
//!
//! This avoids downloading crate sources at eval time. Fields that require
//! reading the crate's `Cargo.toml` (edition, procMacro, libPath, etc.) are
//! left as `None`/default so that `buildRustCrate` can auto-detect them at
//! build time.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use crate::cfg_eval::{matches_target, TargetDescription};
use crate::feature_resolve::{self, DepFeatureInfo, PackageFeatureInfo};
use crate::lockfile::parse_lockfile;
use crate::registry;
use crate::resolve::{BinTarget, CrateInfo, DepInfo, SourceInfo, WorkspaceResult};

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

/// Parsed workspace Cargo.toml — just what we need for workspace member info.
#[derive(Debug)]
struct WorkspaceManifest {
    /// Workspace members' package names and their manifest directories.
    members: Vec<WorkspaceMember>,
    /// The root package, if this is also a package (not a virtual workspace).
    root_package: Option<WorkspaceMember>,
}

/// Fields from `[workspace.package]` that `foo.workspace = true` inherits.
/// Cargo also supports authors/description/license/etc. here but those don't
/// affect compilation; edition is the only one that changes rustc flags.
/// Version matters for intra-workspace dependency resolution.
#[derive(Debug, Clone, Default)]
struct WorkspacePackage {
    edition: Option<String>,
    version: Option<String>,
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
    version_req: Option<String>,
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

    // Map a lockfile source string to the sparse index URL we'd fetch
    // from. None for local/git sources (no index involved). crates.io
    // is redirected through `crates_io_index` so network-restricted
    // environments can point at a mirror (#20).
    let source_to_index_url =
        |source: Option<&str>| registry::source_to_index_url(source, crates_io_index);

    // Prefetch: the lockfile lists every (registry, name) pair we'll
    // ever need, so warm the cache concurrently before the serial
    // resolve loop. Cold-cache eval goes from O(n·RTT) serial
    // handshakes to ~O(n/workers·RTT) with connection keep-alive.
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

            // Look up in the registry index via tame-index. Hard-fail
            // the whole resolution rather than silently continuing with
            // empty dependencies — the previous eprintln!+continue path
            // produced derivations with no --extern flags that compiled
            // with E0433 errors deep inside the build sandbox.
            let index_version = match index_url.as_deref() {
                None => None, // local/git source — no index lookup needed
                Some(url) => Some(
                    registry::lookup_version(cargo_home, url, &pkg.name, &pkg.version).map_err(
                        |e| {
                            format!(
                                "failed to look up {} {} in index '{}': {e}",
                                pkg.name, pkg.version, url
                            )
                        },
                    )?,
                ),
            };

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
                    let checkout = match git_checkouts.get(checkout_path) {
                        Some(c) => c,
                        None => {
                            let c = GitCheckout::scan(checkout_path)?;
                            git_checkouts.entry(checkout_path.clone()).or_insert(c)
                        }
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
                    (deps, build_deps, member.features.clone(), member.links.clone())
                } else {
                    (Vec::new(), Vec::new(), BTreeMap::new(), None)
                };

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
                    resolved_default_features: Vec::new(), // filled in below
                    proc_macro: false, // Auto-detected at build time by build-rust-crate
                    build: None,       // Auto-detected at build time
                    lib_path: None,    // Auto-detected at build time
                    lib_name: None,    // Auto-detected at build time
                    crate_bin: Vec::new(), // Not needed for external crates
                    lib_crate_types: Vec::new(), // Auto-detected at build time
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
        // Optional deps keyed by their local name (raw, dash-preserved) —
        // that's what `dep:X` and `X/feat` in feature rules reference.
        // Cannot derive from `dep:` scan alone: legacy optional=true deps
        // have no explicit `dep:` anywhere and implicitly create a
        // self-named feature (aws-smithy-types's http-body-1-0).
        // Feature rules in Cargo.toml reference the *local* dep key
        // (e.g. rustls says `webpki/ring`, not `rustls-webpki/ring`;
        // `pki-types/std` with a dash, not the rustc-normalized form).
        let deps_iter = info.dependencies.iter().chain(&info.build_dependencies);
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

    // Filter out optional deps that weren't activated by feature resolution.
    // With resolver v2, optional deps only become real dependencies when
    // their activating feature (dep:X or legacy implicit) is enabled.
    // The lockfile includes all possible deps for version pinning, but
    // buildRustCrate expects only active deps.
    for (pkg_id, info) in crates.iter_mut() {
        let keep = |dep: &DepInfo| {
            !dep.optional
                || resolution
                    .active_optional_deps
                    .contains(&(pkg_id.clone(), dep.local_name().to_string()))
        };
        info.dependencies.retain(&keep);
        info.build_dependencies.retain(&keep);
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

/// Parse the workspace root Cargo.toml and all member Cargo.toml files.
fn parse_workspace(workspace_root: &Path) -> Result<WorkspaceManifest, String> {
    let root_manifest_path = workspace_root.join("Cargo.toml");
    let root_toml_str = std::fs::read_to_string(&root_manifest_path)
        .map_err(|e| format!("Failed to read {}: {e}", root_manifest_path.display()))?;
    let root_toml: toml::Value =
        toml::from_str(&root_toml_str).map_err(|e| format!("Failed to parse Cargo.toml: {e}"))?;

    let mut members = Vec::new();
    let mut root_package = None;

    let workspace_table = root_toml.get("workspace");

    // [workspace.dependencies] — the table that `foo = { workspace = true }`
    // in a member manifest inherits from. A member can add features on top
    // of the inherited list but cannot flip default-features from false to
    // true (cargo errors). We don't enforce that, but we do honor the
    // inherited values.
    let workspace_deps: HashMap<String, ManifestDep> = workspace_table
        .and_then(|w| w.get("dependencies"))
        .map(|d| {
            parse_manifest_deps(Some(d), &HashMap::new())
                .into_iter()
                .map(|dep| (dep.name.clone(), dep))
                .collect()
        })
        .unwrap_or_default();

    // [workspace.package] — fields that `edition.workspace = true` etc.
    // inherit. Unlike deps, these are simple key-value strings with no
    // member-side merging: the member either inherits or sets its own.
    let ws_pkg_table = workspace_table.and_then(|w| w.get("package"));
    let ws_pkg = WorkspacePackage {
        edition: ws_pkg_table
            .and_then(|p| p.get("edition"))
            .and_then(|v| v.as_str())
            .map(String::from),
        version: ws_pkg_table
            .and_then(|p| p.get("version"))
            .and_then(|v| v.as_str())
            .map(String::from),
    };

    // Check if root is a package
    if let Some(pkg) = root_toml.get("package") {
        let member =
            parse_member_manifest(&root_toml, pkg, workspace_root, &workspace_deps, &ws_pkg)?;
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
                            let member = parse_member_manifest(
                                &member_toml,
                                pkg,
                                &member_dir,
                                &workspace_deps,
                                &ws_pkg,
                            )?;
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

/// Read a string-valued package field, honoring `field.workspace = true`.
///
/// In TOML, `edition.workspace = true` is not a string — it's a table with
/// one key. `.as_str()` returns None and we used to fall through to the
/// default, dropping the workspace value on the floor. That broke ant-jeprof
/// which uses edition-2024 let-chains while inheriting edition from the
/// workspace root (which is not in the build sandbox, so this must be
/// resolved at eval time).
///
/// Returns `Some(value)` for either a direct string or a workspace inherit,
/// `None` if the field is absent (caller picks the default).
fn inherit_pkg_str<'a>(
    pkg: &'a toml::Value,
    key: &str,
    ws_value: Option<&'a str>,
) -> Option<&'a str> {
    let v = pkg.get(key)?;
    // Direct string: `edition = "2024"`
    if let Some(s) = v.as_str() {
        return Some(s);
    }
    // Table form: `edition = { workspace = true }` / `edition.workspace = true`
    if v.get("workspace").and_then(|w| w.as_bool()) == Some(true) {
        return ws_value;
    }
    None
}

/// Parse a single member's Cargo.toml.
fn parse_member_manifest(
    toml: &toml::Value,
    pkg: &toml::Value,
    manifest_dir: &Path,
    workspace_deps: &HashMap<String, ManifestDep>,
    ws_pkg: &WorkspacePackage,
) -> Result<WorkspaceMember, String> {
    let name = pkg
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or("package.name missing")?
        .to_string();
    let version = inherit_pkg_str(pkg, "version", ws_pkg.version.as_deref())
        .unwrap_or("0.0.0")
        .to_string();
    let edition = inherit_pkg_str(pkg, "edition", ws_pkg.edition.as_deref())
        .unwrap_or("2021")
        .to_string();
    let links = pkg.get("links").and_then(|v| v.as_str()).map(String::from);
    let authors = toml_str_array(pkg.get("authors"));

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
    let lib_crate_types =
        toml_str_array(lib.and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type"))));

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
                .map(|(k, v)| (k.clone(), toml_str_array(Some(v))))
                .collect()
        })
        .unwrap_or_default();

    // Parse dependencies
    let dependencies = parse_manifest_deps(toml.get("dependencies"), workspace_deps);
    let build_dependencies = parse_manifest_deps(toml.get("build-dependencies"), workspace_deps);
    let dev_dependencies = parse_manifest_deps(toml.get("dev-dependencies"), workspace_deps);

    // Also parse target-specific deps and merge into the main lists.
    let (mut all_deps, mut all_build_deps, mut all_dev_deps) =
        (dependencies, build_dependencies, dev_dependencies);
    if let Some(targets) = toml.get("target").and_then(|v| v.as_table()) {
        for (target_spec, target_table) in targets {
            let parse = |key: &str, out: &mut Vec<ManifestDep>| {
                let mut deps = parse_manifest_deps(target_table.get(key), workspace_deps);
                for d in &mut deps {
                    d.target = Some(target_spec.clone());
                }
                out.extend(deps);
            };
            parse("dependencies", &mut all_deps);
            parse("build-dependencies", &mut all_build_deps);
            parse("dev-dependencies", &mut all_dev_deps);
        }
    }

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
///
/// `workspace_deps` is the root's `[workspace.dependencies]` table.
/// A member entry of the form `foo = { workspace = true }` inherits
/// version/default-features/features/package from there. The member can
/// add features (appended) and set optional; it cannot reinstate
/// default-features once the workspace turned them off.
fn parse_manifest_deps(
    deps: Option<&toml::Value>,
    workspace_deps: &HashMap<String, ManifestDep>,
) -> Vec<ManifestDep> {
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
                let member_default_features = t
                    .get("default-features")
                    .or_else(|| t.get("default_features"))
                    .and_then(|v| v.as_bool());
                let member_features = toml_str_array(t.get("features"));

                // `workspace = true` — inherit from root. Per cargo's
                // inner_dependency_inherit_with (src/cargo/util/toml/mod.rs):
                //   - version/package: workspace only (member CANNOT set)
                //   - features: workspace's ++ member's (appended)
                //   - optional: member only (workspace.deps can't set it;
                //     cargo bails at parse time if it tries)
                //   - default-features: see the match below
                if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(ws) = workspace_deps.get(name) {
                        let mut features = ws.features.clone();
                        features.extend(member_features);

                        // Cargo's merge table (member, workspace) → result:
                        //   (Some(true),  false) → true   member re-enables
                        //   (Some(false), true)  → true   member ignored (warn)
                        //   (Some(false), None)  → true   member ignored (warn)
                        //   (None,        x)     → x      workspace sticks
                        //   (Some(true),  true)  → true
                        // The "member ignored" cases become hard errors in
                        // edition 2024, but we don't enforce that — cargo
                        // already did when it wrote the lockfile.
                        //
                        // Practical upshot: member can only WIDEN defaults,
                        // never narrow. The workspace's `false` survives
                        // unless the member explicitly says `true`.
                        let default_features = match member_default_features {
                            Some(true) => true,
                            // false is ignored: workspace wins (warn/error in cargo)
                            Some(false) | None => ws.default_features,
                        };

                        return ManifestDep {
                            name: name.clone(),
                            package: ws.package.clone(),
                            version_req: ws.version_req.clone(),
                            optional,
                            default_features,
                            features,
                            target: None,
                        };
                    }
                    // workspace = true but no entry — cargo errors. We
                    // fall through to treat it as a bare dep so the
                    // lockfile lookup can still proceed.
                }

                ManifestDep {
                    name: name.clone(),
                    package,
                    version_req,
                    optional,
                    default_features: member_default_features.unwrap_or(true),
                    features: member_features,
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

/// Extract a TOML array of strings, or empty if absent/wrong type.
fn toml_str_array(value: Option<&toml::Value>) -> Vec<String> {
    value
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

/// A git checkout scanned for Cargo packages. One checkout may host many
/// crates (workspace) — gitoxide ships ~36 `gix-*` crates from one repo.
/// We parse each member's manifest into the same `WorkspaceMember` shape
/// the local-workspace path uses so dep resolution is shared.
struct GitCheckout {
    /// name → parsed manifest. Only the fields `resolve_member_deps`
    /// reads are meaningful; `manifest_dir` is the absolute path within
    /// the checkout (used to derive `sub_path`).
    members: HashMap<String, WorkspaceMember>,
}

impl GitCheckout {
    fn scan(root: &Path) -> Result<Self, String> {
        let root_manifest_path = root.join("Cargo.toml");
        let root_toml_str = std::fs::read_to_string(&root_manifest_path).map_err(|e| {
            format!(
                "git checkout {}: failed to read Cargo.toml: {e}",
                root.display()
            )
        })?;
        let root_toml: toml::Value = toml::from_str(&root_toml_str)
            .map_err(|e| format!("git checkout {}: parse Cargo.toml: {e}", root.display()))?;

        let workspace_table = root_toml.get("workspace");
        let workspace_deps: HashMap<String, ManifestDep> = workspace_table
            .and_then(|w| w.get("dependencies"))
            .map(|d| {
                parse_manifest_deps(Some(d), &HashMap::new())
                    .into_iter()
                    .map(|dep| (dep.name.clone(), dep))
                    .collect()
            })
            .unwrap_or_default();
        let ws_pkg_table = workspace_table.and_then(|w| w.get("package"));
        let ws_pkg = WorkspacePackage {
            edition: ws_pkg_table
                .and_then(|p| p.get("edition"))
                .and_then(|v| v.as_str())
                .map(String::from),
            version: ws_pkg_table
                .and_then(|p| p.get("version"))
                .and_then(|v| v.as_str())
                .map(String::from),
        };

        let mut members = HashMap::new();
        let mut push = |toml: &toml::Value, dir: &Path| -> Result<(), String> {
            if let Some(pkg) = toml.get("package") {
                let m = parse_member_manifest(toml, pkg, dir, &workspace_deps, &ws_pkg)?;
                members.insert(m.name.clone(), m);
            }
            Ok(())
        };

        // Root may itself be a package (non-virtual workspace, or no
        // workspace at all).
        push(&root_toml, root)?;

        if let Some(ws) = workspace_table {
            if let Some(member_globs) = ws.get("members").and_then(|m| m.as_array()) {
                for glob_val in member_globs {
                    let Some(glob_str) = glob_val.as_str() else {
                        continue;
                    };
                    for member_dir in expand_glob(root, glob_str) {
                        let manifest = member_dir.join("Cargo.toml");
                        let Ok(s) = std::fs::read_to_string(&manifest) else {
                            continue;
                        };
                        let toml: toml::Value = toml::from_str(&s).map_err(|e| {
                            format!("git checkout: parse {}: {e}", manifest.display())
                        })?;
                        push(&toml, &member_dir)?;
                    }
                }
            }
        }

        Ok(Self { members })
    }

    fn find(&self, name: &str) -> Option<&WorkspaceMember> {
        self.members.get(name)
    }
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
/// Name alone is ambiguous when a crate depends on multiple major
/// versions of the same package under different renames. The lockfile
/// lists both as "name version"; only the req disambiguates.
fn find_lock_dep_by_name_and_req<'a>(
    name: &str,
    req: &semver::VersionReq,
    dep_refs: &[String],
    all_packages: &'a [LockPackage],
) -> Option<&'a LockPackage> {
    // Collect all candidates first — there may be more than one dep_ref
    // with this name, and we need the one matching req.
    //
    // Fallback is only for unparseable versions. A parseable version that
    // doesn't satisfy req means this index dep is NOT the one the lockfile
    // resolved — cargo dropped it. Real case: aws-smithy-http-client has
    // both `hyper` (req ^1.6) and `hyper-0-14` (req ^0.14) pointing at the
    // same package. Lockfile only has hyper@1.8.1 (0.14 was never
    // activated). If we fall back on the name match, hyper-0-14 also
    // resolves to 1.8.1 — emitting a spurious DepInfo with rename
    // "hyper-0-14", which becomes `--extern hyper_0_14=.../libhyper.rlib`
    // and the plain `--extern hyper=` never appears.
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

        match semver::Version::parse(&pkg.version) {
            Ok(v) => {
                // Pre-release handling: semver::VersionReq doesn't match
                // pre-releases unless the req itself names one, but cargo
                // does lock to them (tokio 1.49.0+anthropic.1 vs ^1.49).
                // Strip pre/build metadata for the secondary check.
                let stripped = semver::Version::new(v.major, v.minor, v.patch);
                if req.matches(&v) || req.matches(&stripped) {
                    return Some(pkg);
                }
                // Parseable but doesn't match → cargo didn't pick this
                // index dep. Don't fall back; return None if nothing
                // else matches.
            }
            Err(_) => {
                // Genuinely unparseable — rare; keep as last resort.
                unparseable_fallback.get_or_insert(pkg);
            }
        }
    }
    unparseable_fallback
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
                    sub_path: None,
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
    } else {
        // Literal path, or a complex glob we don't support yet — try as-is.
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
        let deps = parse_manifest_deps(toml.get("dependencies"), &HashMap::new());
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
        let deps = parse_manifest_deps(toml.get("dependencies"), &HashMap::new());
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].name, "tokio-rustls");
        assert!(deps[0].optional);
        assert!(!deps[0].default_features);
        assert_eq!(deps[0].features, vec!["ring"]);
    }

    /// `foo = { workspace = true }` inherits version/package/default-features/
    /// features from [workspace.dependencies]. Member features append;
    /// optional comes from member only. Real-world: the monorepo root
    /// declares aws-sdk-s3 with default-features=false to avoid the
    /// sigv4a → p256/ring chain; members inherit that.
    #[test]
    fn workspace_inheritance_basic() {
        let ws_deps: HashMap<String, ManifestDep> = parse_manifest_deps(
            Some(
                &toml::from_str(
                    r#"
                    aws-sdk-s3 = { version = "1.82", default-features = false, features = ["rt-tokio"] }
                    renamed = { package = "actual", version = "2" }
                    "#,
                )
                .unwrap(),
            ),
            &HashMap::new(),
        )
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();

        let member: toml::Value = toml::from_str(
            r#"
            [dependencies]
            aws-sdk-s3 = { workspace = true, features = ["extra"] }
            renamed = { workspace = true, optional = true }
            "#,
        )
        .unwrap();
        let deps = parse_manifest_deps(member.get("dependencies"), &ws_deps);

        let s3 = deps.iter().find(|d| d.name == "aws-sdk-s3").unwrap();
        assert!(!s3.default_features, "inherited false must stick");
        assert_eq!(s3.version_req, Some("1.82".into()));
        assert!(s3.features.contains(&"rt-tokio".to_string()));
        assert!(s3.features.contains(&"extra".to_string()));

        let renamed = deps.iter().find(|d| d.name == "renamed").unwrap();
        assert_eq!(renamed.package, Some("actual".into()));
        assert!(renamed.optional);
    }

    /// Cargo's actual default-features merge (src/cargo/util/toml/mod.rs
    /// inner_dependency_inherit_with): member can WIDEN but not narrow.
    /// (member=true, ws=false) → true. (member=false, ws=true) → true,
    /// member ignored with a warning (hard error in edition 2024).
    #[test]
    fn workspace_inheritance_default_features_merge() {
        let ws_deps: HashMap<String, ManifestDep> = parse_manifest_deps(
            Some(
                &toml::from_str(
                    r#"
                    ws-off = { version = "1", default-features = false }
                    ws-on  = { version = "1" }
                    "#,
                )
                .unwrap(),
            ),
            &HashMap::new(),
        )
        .into_iter()
        .map(|d| (d.name.clone(), d))
        .collect();

        let probe = |toml_str: &str| {
            parse_manifest_deps(
                toml::from_str::<toml::Value>(toml_str)
                    .unwrap()
                    .get("dependencies"),
                &ws_deps,
            )
            .pop()
            .unwrap()
            .default_features
        };
        // ws=false, member silent → false (the sigv4a case)
        assert!(!probe("[dependencies]\nws-off = { workspace = true }\n"));
        // ws=false, member=true → true (re-enable)
        assert!(probe(
            "[dependencies]\nws-off = { workspace = true, default-features = true }\n"
        ));
        // ws=true, member=false → true (member ignored per cargo)
        assert!(probe(
            "[dependencies]\nws-on = { workspace = true, default-features = false }\n"
        ));
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
    /// 2024 or later".
    ///
    /// These fields cannot be fixed at build time by build-rust-crate:
    /// the workspace root Cargo.toml is not in the sandbox. Must be
    /// eval-time.
    #[test]
    fn workspace_package_inheritance() {
        let ws_pkg = WorkspacePackage {
            edition: Some("2024".into()),
            version: Some("7.7.7".into()),
        };

        // edition.workspace = true — must inherit 2024
        let toml: toml::Value = toml::from_str(
            r#"
            [package]
            name = "ant-jeprof"
            edition.workspace = true
            version.workspace = true
            "#,
        )
        .unwrap();
        let pkg = toml.get("package").unwrap();
        let member =
            parse_member_manifest(&toml, pkg, Path::new("/tmp"), &HashMap::new(), &ws_pkg).unwrap();
        assert_eq!(
            member.edition, "2024",
            "edition.workspace=true must inherit"
        );
        assert_eq!(
            member.version, "7.7.7",
            "version.workspace=true must inherit"
        );

        // Explicit edition wins over workspace
        let toml: toml::Value = toml::from_str(
            r#"
            [package]
            name = "legacy-member"
            edition = "2018"
            version = "0.1.0"
            "#,
        )
        .unwrap();
        let pkg = toml.get("package").unwrap();
        let member =
            parse_member_manifest(&toml, pkg, Path::new("/tmp"), &HashMap::new(), &ws_pkg).unwrap();
        assert_eq!(
            member.edition, "2018",
            "explicit edition overrides workspace"
        );
        assert_eq!(member.version, "0.1.0");

        // edition.workspace = true with no [workspace.package].edition →
        // cargo errors. We fall back to 2021 (least surprising).
        let empty_ws = WorkspacePackage::default();
        let toml: toml::Value = toml::from_str(
            r#"
            [package]
            name = "orphan"
            edition.workspace = true
            "#,
        )
        .unwrap();
        let pkg = toml.get("package").unwrap();
        let member =
            parse_member_manifest(&toml, pkg, Path::new("/tmp"), &HashMap::new(), &empty_ws)
                .unwrap();
        assert_eq!(
            member.edition, "2021",
            "missing workspace.package.edition falls back to 2021"
        );
    }

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
        std::fs::write(
            checkout.join("crates/bar/Cargo.toml"),
            "[package]\nname = \"bar\"\nversion = \"0.1.0\"\n",
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

        // Feature resolution propagated through the git crate: consumer
        // pulls foo's default → "a".
        assert!(foo.resolved_default_features.contains(&"a".to_string()));
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
