use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

/// Top-level config deserialized from NIX_ATTRS_JSON_FILE (__structuredAttrs).
///
/// Field names use camelCase to match the Nix attribute names directly.
/// Nix passes all derivation attributes as structured JSON, so we get
/// typed access to everything without string parsing.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildConfig {
    pub crate_name: String,
    #[serde(default)]
    pub crate_version: String,
    #[serde(default)]
    pub lib_name: String,
    #[serde(default)]
    pub lib_path: String,
    #[serde(default)]
    pub build: String,
    // Nix attr is literally `workspace_member` (legacy nixpkgs spelling),
    // not the camelCase the blanket rename would expect.
    #[serde(default, rename = "workspace_member")]
    pub workspace_member: Option<String>,
    #[serde(default)]
    pub crate_bin: Vec<CrateBin>,
    #[serde(default)]
    pub has_crate_bin: bool,
    // Cargo's auto-target-discovery toggles. Not passed from Nix; learned
    // from Cargo.toml in detect_cargo_toml_info, default true (2018+).
    #[serde(skip, default = "default_true")]
    pub autobins: bool,
    #[serde(skip, default = "default_true")]
    pub autotests: bool,
    /// [[test]] targets parsed from Cargo.toml (never passed from Nix).
    #[serde(skip)]
    pub crate_tests: Vec<CrateTest>,
    #[serde(skip, default = "default_true")]
    pub autolib: bool,
    #[serde(default)]
    pub crate_type: Vec<String>,
    #[serde(default)]
    pub crate_features: Vec<String>,
    #[serde(default)]
    pub crate_features_raw: Vec<String>,
    #[serde(default = "default_true")]
    pub release: bool,
    #[serde(default = "default_true")]
    pub verbose: bool,
    #[serde(default)]
    pub build_tests: bool,
    #[serde(default = "default_codegen_units")]
    pub codegen_units: u32,
    #[serde(default)]
    pub extra_link_flags: Vec<String>,
    #[serde(default)]
    pub extra_rustc_opts: Vec<String>,
    #[serde(default)]
    pub extra_rustc_opts_for_build_rs: Vec<String>,
    #[serde(default = "default_cap_lints")]
    pub cap_lints: String,
    #[serde(default = "default_colors")]
    pub colors: String,

    /// Flattened transitive dep lib-output store paths.
    #[serde(default)]
    pub complete_deps: Vec<String>,
    /// Flattened transitive build-dep lib-output store paths.
    #[serde(default)]
    pub complete_build_deps: Vec<String>,

    /// Pre-computed metadata hash (deterministic, from Nix).
    pub metadata: String,

    /// Per-dependency extern info (extern name with renames applied,
    /// metadata hash) computed at Nix eval time.
    #[serde(default)]
    pub dep_externs: Vec<DepExtern>,
    /// Per-build-dependency extern info.
    #[serde(default)]
    pub build_dep_externs: Vec<DepExtern>,

    pub host_platform: PlatformInfo,
    pub build_platform: PlatformInfo,

    pub outputs: HashMap<String, String>,

    #[serde(default)]
    pub crate_authors: Vec<String>,
    #[serde(default)]
    pub crate_description: String,
    #[serde(default)]
    pub crate_homepage: String,
    #[serde(default)]
    pub crate_license: String,
    #[serde(default)]
    pub crate_license_file: String,
    #[serde(default)]
    pub crate_links: String,
    #[serde(default)]
    pub crate_readme: String,
    #[serde(default)]
    pub crate_repository: String,
    #[serde(default)]
    pub crate_rust_version: String,
}

fn default_true() -> bool {
    true
}
fn default_codegen_units() -> u32 {
    1
}
fn default_cap_lints() -> String {
    "allow".into()
}
fn default_colors() -> String {
    "always".into()
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CrateBin {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub required_features: Vec<String>,
}

/// `[[test]]` target from Cargo.toml. Unlike CrateBin this is never supplied
/// from Nix, only discovered by `detect_cargo_toml_info` and merged with the
/// autotests-inferred set in `resolve_tests`.
#[derive(Debug, Clone)]
pub struct CrateTest {
    pub name: String,
    pub path: Option<String>,
    pub required_features: Vec<String>,
    /// `harness = false` → build with `--cfg test` instead of `--test`
    /// (cargo's build_base_args: no libtest harness, target supplies its
    /// own `main`).
    pub harness: bool,
}

/// Per-crate metadata installed to `$lib/crate-metadata.json`. This is the
/// canonical machine-readable description of what a crate built; dependents
/// read it instead of scraping `$lib/lib/` filenames or parsing shell env
/// files. Legacy `$lib/lib/link` (text) and `$lib/env` (shell) are still
/// written and read alongside it so existing crateOverrides that append to /
/// sed those files keep working.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CrateMetadata {
    pub lib_name: String,
    pub metadata: String,
    pub crate_types: Vec<String>,
    pub proc_macro: bool,
    /// Installed artifact filenames under `$lib/lib/` (rlib/so/dylib/dll).
    pub artifacts: Vec<String>,
    /// `package.links` key, empty if none.
    #[serde(default)]
    pub links: String,
    /// `cargo:KEY=VAL` / `cargo::metadata=KEY=VAL` pairs exposed to
    /// dependents as `DEP_<links>_<KEY>`.
    #[serde(default)]
    pub links_vars: BTreeMap<String, String>,
}

impl CrateMetadata {
    pub fn load(dep_lib_out: &str) -> Option<Self> {
        let s = std::fs::read_to_string(format!("{dep_lib_out}/crate-metadata.json")).ok()?;
        serde_json::from_str(&s).ok()
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DepExtern {
    pub extern_name: String,
    #[serde(default)]
    pub is_rename: bool,
    /// Set for `core`/`alloc`/`std` deps in custom-std builds. Emits
    /// `--extern noprelude:NAME=…` (and `-Z unstable-options`) so the
    /// crate's own prelude resolution isn't perturbed — cargo's
    /// `compiler/mod.rs` does the same for `dep.is_std()`.
    #[serde(default)]
    pub stdlib: bool,
    /// Store path of the dep's lib output, where `crate-metadata.json` lives.
    pub lib_out: String,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlatformInfo {
    pub rustc_target_spec: String,
    /// `None` when the stdenv has no CC and isn't using lld; the builder
    /// then omits `-C linker=` entirely and lets rustc pick its default.
    #[serde(default)]
    pub linker_path: Option<String>,
}

impl BuildConfig {
    pub fn from_json_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;

        // Export ALL_CAPS scalar attrs as env vars — with __structuredAttrs
        // these end up in JSON but not the process environment, but crate
        // overrides setting e.g. `OPENSSL_NO_VENDOR = 1;` expect them there.
        // Coerce bools/ints/lists like stdenv would for non-structured attrs.
        if let Some(obj) = serde_json::from_str::<serde_json::Value>(&content)
            .ok()
            .as_ref()
            .and_then(|v| v.as_object())
        {
            for (k, v) in obj {
                if k.is_empty()
                    || !k.chars().all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                {
                    continue;
                }
                let s = match v {
                    serde_json::Value::String(s) => s.clone(),
                    serde_json::Value::Bool(b) => if *b { "1".into() } else { String::new() },
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::Array(a) => a
                        .iter()
                        .filter_map(|e| match e {
                            serde_json::Value::String(s) => Some(s.clone()),
                            serde_json::Value::Number(n) => Some(n.to_string()),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => continue,
                };
                std::env::set_var(k, s);
            }
        }

        Ok(config)
    }

    pub fn lib_name_normalized(&self) -> String {
        self.lib_name.replace('-', "_")
    }

    pub fn is_cross_compiling(&self) -> bool {
        self.host_platform.rustc_target_spec != self.build_platform.rustc_target_spec
    }

    pub fn out_path(&self) -> &str {
        self.outputs.get("out").expect("outputs must have 'out'")
    }

    pub fn lib_path_output(&self) -> Option<&str> {
        self.outputs.get("lib").map(|s| s.as_str())
    }
}
