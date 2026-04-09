use serde::Deserialize;
use std::collections::HashMap;

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
    #[serde(default)]
    pub crate_type: Vec<String>,
    #[serde(default)]
    pub crate_features: Vec<String>,
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

    /// Store path of rustc — used for --remap-path-prefix to avoid
    /// leaking rust-src into the closure.
    #[serde(default)]
    pub rustc_path: String,

    /// Flattened transitive dep lib-output store paths.
    #[serde(default)]
    pub complete_deps: Vec<String>,
    /// Flattened transitive build-dep lib-output store paths.
    #[serde(default)]
    pub complete_build_deps: Vec<String>,

    /// Pre-computed metadata hash (deterministic, from Nix).
    pub metadata: String,

    /// Per-dependency extern info computed at Nix eval time.
    /// Each entry carries the extern name (with renames applied),
    /// metadata hash, and whether it prefers .rlib or .so.
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

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct DepExtern {
    pub extern_name: String,
    pub metadata: String,
    #[serde(default)]
    pub is_rename: bool,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct PlatformInfo {
    pub rustc_target_spec: String,
    #[serde(default)]
    pub lib_ext: String,
    pub linker_path: String,
}

impl BuildConfig {
    pub fn from_json_file(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let content = std::fs::read_to_string(path)?;
        let config: Self = serde_json::from_str(&content)?;

        // Export ALL_CAPS string attrs as env vars — with __structuredAttrs
        // these end up in JSON but not the process environment, but crate
        // overrides setting e.g. `OPENSSL_DIR = …;` expect them there.
        if let Ok(raw) = serde_json::from_str::<serde_json::Value>(&content) {
            if let Some(obj) = raw.as_object() {
                for (k, v) in obj {
                    if let Some(s) = v.as_str() {
                        if !k.is_empty()
                            && k.chars()
                                .all(|c| c.is_ascii_uppercase() || c == '_' || c.is_ascii_digit())
                        {
                            std::env::set_var(k, s);
                        }
                    }
                }
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
