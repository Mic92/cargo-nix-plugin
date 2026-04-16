//! Cargo.toml reading: workspace, member, and git-checkout manifests.
//!
//! Everything that turns on-disk TOML into [`WorkspaceMember`] /
//! [`ManifestDep`]. The parent module pairs the result with Cargo.lock.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::resolve::BinTarget;

/// Parsed workspace Cargo.toml — just what we need for workspace member info.
#[derive(Debug)]
pub(super) struct WorkspaceManifest {
    /// Workspace members' package names and their manifest directories.
    pub(super) members: Vec<WorkspaceMember>,
    /// The root package, if this is also a package (not a virtual workspace).
    pub(super) root_package: Option<WorkspaceMember>,
    /// Local `path = "..."` dependencies that are NOT in [workspace].members.
    /// Cargo.lock gives them `source = None` (same as members) but no path,
    /// so we must discover them by walking referrers' `[dependencies].path`
    /// fields. Keyed by package name.
    pub(super) path_deps: HashMap<String, WorkspaceMember>,
}

/// Fields from `[workspace.package]` that `foo.workspace = true` inherits.
/// Cargo also supports authors/description/license/etc. here but those don't
/// affect compilation; edition is the only one that changes rustc flags.
/// Version matters for intra-workspace dependency resolution.
#[derive(Debug, Clone, Default)]
pub(super) struct WorkspacePackage {
    pub(super) edition: Option<String>,
    pub(super) version: Option<String>,
}

#[derive(Debug, Clone)]
pub(super) struct WorkspaceMember {
    pub(super) name: String,
    pub(super) version: String,
    pub(super) manifest_dir: String,
    /// Dependencies declared in the member's Cargo.toml (for dep kind/features).
    pub(super) dependencies: Vec<ManifestDep>,
    pub(super) build_dependencies: Vec<ManifestDep>,
    pub(super) dev_dependencies: Vec<ManifestDep>,
    pub(super) features: BTreeMap<String, Vec<String>>,
    pub(super) edition: String,
    pub(super) links: Option<String>,
    pub(super) proc_macro: bool,
    pub(super) build_script: Option<String>,
    pub(super) lib_path: Option<String>,
    pub(super) lib_name: Option<String>,
    pub(super) lib_crate_types: Vec<String>,
    pub(super) bin_targets: Vec<BinTarget>,
    pub(super) authors: Vec<String>,
}

#[derive(Debug, Clone)]
pub(super) struct ManifestDep {
    pub(super) name: String,
    pub(super) package: Option<String>,
    pub(super) version_req: Option<String>,
    pub(super) optional: bool,
    pub(super) default_features: bool,
    pub(super) features: Vec<String>,
    pub(super) target: Option<String>,
    /// `path = "..."` relative to the manifest's dir. Used to discover
    /// non-member local crates (the lockfile doesn't record their path).
    pub(super) path: Option<String>,
}

/// Read and parse a TOML file with a path-contextual error.
fn read_toml(path: &Path) -> Result<toml::Value, String> {
    let s = std::fs::read_to_string(path)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    toml::from_str(&s).map_err(|e| format!("Failed to parse {}: {e}", path.display()))
}

/// Parse `[workspace.dependencies]` and `[workspace.package]`.
fn parse_workspace_tables(
    workspace_table: Option<&toml::Value>,
) -> (HashMap<String, ManifestDep>, WorkspacePackage) {
    let workspace_deps = workspace_table
        .and_then(|w| w.get("dependencies"))
        .map(|d| {
            parse_manifest_deps(Some(d), &HashMap::new())
                .into_iter()
                .map(|dep| (dep.name.clone(), dep))
                .collect()
        })
        .unwrap_or_default();
    let ws_pkg_table = workspace_table.and_then(|w| w.get("package"));
    let toml_str = |key| {
        ws_pkg_table
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_str())
            .map(String::from)
    };
    let ws_pkg = WorkspacePackage {
        edition: toml_str("edition"),
        version: toml_str("version"),
    };
    (workspace_deps, ws_pkg)
}

/// Parse the workspace root Cargo.toml and all member Cargo.toml files.
pub(super) fn parse_workspace(workspace_root: &Path) -> Result<WorkspaceManifest, String> {
    let root_toml = read_toml(&workspace_root.join("Cargo.toml"))?;

    let mut members = Vec::new();
    let mut root_package = None;

    let workspace_table = root_toml.get("workspace");

    // [workspace.dependencies] / [workspace.package] — inheritance sources
    // for `foo = { workspace = true }` and `edition.workspace = true`.
    let (workspace_deps, ws_pkg) = parse_workspace_tables(workspace_table);

    // Check if root is a package
    if let Some(pkg) = root_toml.get("package") {
        let member =
            parse_member_manifest(&root_toml, pkg, workspace_root, &workspace_deps, &ws_pkg)?;
        root_package = Some(member.clone());
        members.push(member);
    }

    // Workspace members
    let member_globs = workspace_table
        .and_then(|w| w.get("members"))
        .and_then(|m| m.as_array());
    for glob_str in member_globs
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
    {
        for member_dir in expand_glob(workspace_root, glob_str) {
            let member_manifest = member_dir.join("Cargo.toml");
            if !member_manifest.exists() {
                continue;
            }
            let member_toml = read_toml(&member_manifest)?;
            let Some(pkg) = member_toml.get("package") else {
                continue;
            };
            let member =
                parse_member_manifest(&member_toml, pkg, &member_dir, &workspace_deps, &ws_pkg)?;
            // Don't duplicate the root package.
            if root_package
                .as_ref()
                .is_none_or(|rp| rp.name != member.name)
            {
                members.push(member);
            }
        }
    }

    // Discover non-member path dependencies by walking `path = "..."` edges
    // outward from members; Cargo.lock records no path for them.
    let mut path_deps: HashMap<String, WorkspaceMember> = HashMap::new();
    let member_names: HashSet<&str> = members.iter().map(|m| m.name.as_str()).collect();
    // Queue of (referrer_dir, dep_path) edges to visit.
    let mut queue: Vec<(PathBuf, String)> = Vec::new();
    let enqueue = |q: &mut Vec<_>, m: &WorkspaceMember| {
        for d in m
            .dependencies
            .iter()
            .chain(&m.build_dependencies)
            .chain(&m.dev_dependencies)
        {
            if let Some(p) = &d.path {
                q.push((PathBuf::from(&m.manifest_dir), p.clone()));
            }
        }
    };
    for m in &members {
        enqueue(&mut queue, m);
    }
    while let Some((referrer_dir, rel)) = queue.pop() {
        // Lexical normalize (not canonicalize): `manifest_dir` must share a
        // textual prefix with `workspace_root` for lib/default.nix's
        // removePrefix. The out-of-tree check canonicalizes separately.
        let dir = normalize_path(&referrer_dir.join(&rel));
        let manifest = dir.join("Cargo.toml");
        // Missing manifest: leave for the main loop to error if referenced.
        let Ok(toml_str) = std::fs::read_to_string(&manifest) else {
            continue;
        };
        let toml: toml::Value = toml::from_str(&toml_str)
            .map_err(|e| format!("Failed to parse {}: {e}", manifest.display()))?;
        let Some(pkg) = toml.get("package") else {
            continue;
        };
        let m = parse_member_manifest(&toml, pkg, &dir, &workspace_deps, &ws_pkg)?;
        if member_names.contains(m.name.as_str()) || path_deps.contains_key(&m.name) {
            continue;
        }
        enqueue(&mut queue, &m);
        path_deps.insert(m.name.clone(), m);
    }

    Ok(WorkspaceManifest {
        members,
        root_package,
        path_deps,
    })
}

/// Read a string-valued package field, honoring `field.workspace = true`.
///
/// `edition.workspace = true` is a table in TOML, not a string. Must be
/// resolved at eval time — the workspace root Cargo.toml is not in the
/// build sandbox (broke ant-jeprof's edition-2024 let-chains).
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
                path: None,
            },
            toml::Value::Table(t) => {
                let path = t.get("path").and_then(|v| v.as_str()).map(String::from);
                let package = t.get("package").and_then(|v| v.as_str()).map(String::from);
                let version_req = t.get("version").and_then(|v| v.as_str()).map(String::from);
                let optional = t.get("optional").and_then(|v| v.as_bool()).unwrap_or(false);
                let member_default_features = t
                    .get("default-features")
                    .or_else(|| t.get("default_features"))
                    .and_then(|v| v.as_bool());
                let member_features = toml_str_array(t.get("features"));

                // `workspace = true` — inherit from root. Per cargo's
                // `inner_dependency_inherit_with` (src/cargo/util/toml/mod.rs):
                // version/package from workspace only; features appended;
                // optional from member only; default-features merged below.
                if t.get("workspace").and_then(|v| v.as_bool()) == Some(true) {
                    if let Some(ws) = workspace_deps.get(name) {
                        let mut features = ws.features.clone();
                        features.extend(member_features);

                        // Member can only WIDEN defaults, never narrow
                        // (`inner_dependency_inherit_with`): member=false is
                        // ignored (warn; hard error in edition 2024).
                        let default_features = match member_default_features {
                            Some(true) => true,
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
                            // [workspace.dependencies] paths are relative to
                            // the workspace root, but we resolve path deps
                            // relative to the *member* dir. Members that
                            // inherit a path dep are normally also workspace
                            // members themselves, so leave this unset rather
                            // than carry a wrongly-based path.
                            path: None,
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
                    path,
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
                path: None,
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
pub(super) struct GitCheckout {
    /// name → parsed manifest. Only the fields `resolve_member_deps`
    /// reads are meaningful; `manifest_dir` is the absolute path within
    /// the checkout (used to derive `sub_path`).
    members: HashMap<String, WorkspaceMember>,
}

impl GitCheckout {
    pub(super) fn scan(root: &Path) -> Result<Self, String> {
        let root_toml = read_toml(&root.join("Cargo.toml"))
            .map_err(|e| format!("git checkout {}: {e}", root.display()))?;
        let workspace_table = root_toml.get("workspace");
        let (workspace_deps, ws_pkg) = parse_workspace_tables(workspace_table);

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

        let member_globs = workspace_table
            .and_then(|w| w.get("members"))
            .and_then(|m| m.as_array());
        for glob_str in member_globs
            .into_iter()
            .flatten()
            .filter_map(|v| v.as_str())
        {
            for member_dir in expand_glob(root, glob_str) {
                let manifest = member_dir.join("Cargo.toml");
                let Ok(s) = std::fs::read_to_string(&manifest) else {
                    continue;
                };
                let toml: toml::Value = toml::from_str(&s)
                    .map_err(|e| format!("git checkout: parse {}: {e}", manifest.display()))?;
                push(&toml, &member_dir)?;
            }
        }

        Ok(Self { members })
    }

    pub(super) fn find(&self, name: &str) -> Option<&WorkspaceMember> {
        self.members.get(name)
    }
}

/// Lexically normalize a path: collapse `.` and `..` without touching the
/// filesystem. Like Go's `path.Clean`. We need `ws/devdep/../inner` →
/// `ws/inner` so it textually prefixes `workspace_root` for relPath
/// extraction, but `canonicalize()` would also resolve symlinks and may
/// diverge from the (non-canonical) `workspace_root` string the Nix side
/// prefix-strips against.
fn normalize_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                if !out.pop() {
                    out.push(c);
                }
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Simple glob expansion for workspace member paths.
/// Supports trailing `/*` patterns.
fn expand_glob(base: &Path, pattern: &str) -> Vec<PathBuf> {
    if let Some(prefix) = pattern
        .strip_suffix("/*")
        .or_else(|| pattern.strip_suffix("\\*"))
    {
        std::fs::read_dir(base.join(prefix))
            .into_iter()
            .flatten()
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.join("Cargo.toml").exists())
            .collect()
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
}
