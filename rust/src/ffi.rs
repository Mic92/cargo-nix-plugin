// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! C FFI interface for calling from the C++ Nix plugin shim.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::cfg_eval::TargetDescription;
use crate::resolve::{resolve_workspace, API_LEVEL};

/// Expose [`API_LEVEL`] to the C++ shim for `builtins.__cargoNixApiLevel`,
/// so lib/ can detect a skewed .so before calling the resolver.
#[no_mangle]
pub extern "C" fn cargo_nix_api_level() -> u32 {
    API_LEVEL
}

/// Input from the Nix side — the entire attrset serialized as JSON.
///
/// Two modes:
/// 1. Explicit: `metadata` + `cargoLock` provided (pre-generated cargo metadata JSON)
/// 2. Lockfile: `manifestPath` provided (parses Cargo.lock + registry index, no cargo subprocess)
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginInput {
    /// Explicit cargo metadata JSON (mode 1)
    metadata: Option<String>,
    /// Explicit Cargo.lock contents (required with metadata)
    cargo_lock: Option<String>,
    /// Path to Cargo.toml (mode 2 — lockfile resolve)
    manifest_path: Option<String>,
    target: TargetDescription,
    /// Features to enable on the root package.
    #[serde(default)]
    root_features: Vec<String>,
    /// Disable default features on root packages.
    #[serde(default)]
    no_default_features: bool,
    /// Path to CARGO_HOME (for registry index lookup in lockfile resolve mode).
    /// Defaults to $CARGO_HOME or ~/.cargo.
    #[serde(default)]
    cargo_home: Option<String>,
    /// Pre-fetched git checkouts, keyed by `"${url}#${rev}"` (url stripped of
    /// `git+` prefix and `?query`). Supplied by `lib/default.nix` so the
    /// resolver can read each git crate's Cargo.toml without doing IO itself.
    #[serde(default)]
    git_sources: std::collections::HashMap<String, std::path::PathBuf>,
}

/// Validate input and resolve the workspace using the appropriate mode.
fn validate_and_resolve(input: &PluginInput) -> Result<crate::resolve::WorkspaceResult, String> {
    match (&input.metadata, &input.manifest_path) {
        (Some(_), Some(_)) => {
            Err("Provide either 'metadata' or 'manifestPath', not both.".to_string())
        }
        (None, None) => Err(
            "Provide either 'metadata' (pre-generated cargo metadata JSON) or \
                 'manifestPath' (path to Cargo.toml for lockfile resolve)."
                .to_string(),
        ),
        // Mode 1: explicit cargo metadata JSON
        (Some(metadata), None) => {
            let cargo_lock = input
                .cargo_lock
                .as_deref()
                .ok_or("'cargoLock' is required when 'metadata' is provided.")?;
            resolve_workspace(metadata, cargo_lock, &input.target)
        }
        // Mode 2: lockfile resolve from Cargo.lock + registry index
        (None, Some(manifest_path)) => {
            let workspace_root = std::path::Path::new(manifest_path)
                .parent()
                .ok_or_else(|| format!("Cannot determine parent directory of {manifest_path}"))?;
            let lock_path = workspace_root.join("Cargo.lock");
            let cargo_lock_str = std::fs::read_to_string(&lock_path)
                .map_err(|e| format!("Failed to read {}: {e}", lock_path.display()))?;

            let cargo_home = input
                .cargo_home
                .as_deref()
                .map(std::path::PathBuf::from)
                .or_else(|| {
                    std::env::var("CARGO_HOME")
                        .ok()
                        .map(std::path::PathBuf::from)
                })
                .unwrap_or_else(|| dirs_home().join(".cargo"));

            let crates_io_index =
                crate::registry::resolve_crates_io_index(None, workspace_root, &cargo_home);

            crate::lockfile_resolve::resolve_from_lockfile(
                workspace_root,
                &cargo_lock_str,
                &cargo_home,
                &crates_io_index,
                &input.target,
                &input.root_features,
                input.no_default_features,
                &input.git_sources,
            )
        }
    }
}

/// Get the user's home directory.
fn dirs_home() -> std::path::PathBuf {
    std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/root"))
}

/// Resolve a cargo workspace. Input and output are JSON strings.
///
/// # Safety
/// `input_json` must be a valid null-terminated C string.
/// The returned strings must be freed with `free_string`.
#[no_mangle]
pub unsafe extern "C" fn resolve_cargo_workspace(
    input_json: *const c_char,
    out: *mut *mut c_char,
    err_out: *mut *mut c_char,
) -> i32 {
    let input_str = match unsafe { CStr::from_ptr(input_json) }.to_str() {
        Ok(s) => s,
        Err(e) => {
            let msg = CString::new(format!("Invalid UTF-8 in input: {e}")).unwrap();
            unsafe { *err_out = msg.into_raw() };
            return 1;
        }
    };

    let input: PluginInput = match serde_json::from_str(input_str) {
        Ok(v) => v,
        Err(e) => {
            let msg = CString::new(format!("Failed to parse plugin input: {e}")).unwrap();
            unsafe { *err_out = msg.into_raw() };
            return 1;
        }
    };

    match validate_and_resolve(&input) {
        Ok(result) => {
            let json = serde_json::to_string(&result).unwrap();
            let cstr = CString::new(json).unwrap();
            unsafe { *out = cstr.into_raw() };
            0
        }
        Err(e) => {
            let msg = CString::new(e).unwrap();
            unsafe { *err_out = msg.into_raw() };
            1
        }
    }
}

/// Free a string returned by `resolve_cargo_workspace`.
///
/// # Safety
/// The pointer must have been returned by `resolve_cargo_workspace`.
#[no_mangle]
pub unsafe extern "C" fn free_string(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_api_level_matches_constant() {
        assert_eq!(cargo_nix_api_level(), API_LEVEL);
    }

    fn linux_x86_64() -> TargetDescription {
        TargetDescription {
            name: "x86_64-unknown-linux-gnu".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            vendor: "unknown".to_string(),
            env: "gnu".to_string(),
            abi: "".to_string(),
            family: vec!["unix".to_string()],
            pointer_width: "64".to_string(),
            endian: "little".to_string(),
            unix: true,
            windows: false,
            extra_cfgs: vec![],
        }
    }

    /// Lockfile resolve: resolve this crate's workspace from Cargo.lock +
    /// registry index, without running cargo metadata or downloading crates.
    /// Requires that the registry index is cached (run `cargo update` first).
    /// Ignored in sandboxed builds where ~/.cargo is unavailable.
    #[test]
    #[ignore]
    fn lockfile_resolves_own_workspace() {
        let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

        let input = PluginInput {
            metadata: None,
            cargo_lock: None,
            manifest_path: Some(manifest_path.to_string()),
            target: linux_x86_64(),
            root_features: vec![],
            no_default_features: false,
            cargo_home: None,
            git_sources: Default::default(),
        };

        let result = validate_and_resolve(&input).expect("lockfile resolution failed");

        // This crate itself should be a workspace member
        assert!(
            !result.workspace_members.is_empty(),
            "expected at least one workspace member"
        );
        assert!(
            result
                .workspace_members
                .contains_key("cargo-nix-plugin-core"),
            "expected cargo-nix-plugin-core in workspace members"
        );

        // Should have all lockfile crates
        assert!(
            result.crates.len() > 20,
            "expected >20 crates, got {}",
            result.crates.len()
        );

        // External crates should have features from the registry index
        let serde = result.crates.values().find(|c| c.crate_name == "serde");
        assert!(serde.is_some(), "serde not found in resolved crates");
        let serde = serde.unwrap();
        assert!(
            serde.features.contains_key("default"),
            "serde should have 'default' feature from index"
        );
        assert!(
            serde.sha256.is_some(),
            "serde should have sha256 from lockfile"
        );
        assert_eq!(serde.source, Some(crate::resolve::SourceInfo::CratesIo));

        // Workspace member should have edition from Cargo.toml
        let root_id = result
            .workspace_members
            .get("cargo-nix-plugin-core")
            .unwrap();
        let root_crate = &result.crates[root_id];
        assert_eq!(root_crate.edition, "2021");

        // External crates leave edition empty (auto-detected at build time)
        assert_eq!(
            serde.edition, "",
            "external crate edition should be empty for build-time detection"
        );

        // Feature resolution: serde should have "default" and "std" resolved
        assert!(
            serde
                .resolved_default_features
                .contains(&"default".to_string()),
            "serde should have resolved 'default' feature, got: {:?}",
            serde.resolved_default_features
        );

        // proc-macro2 should have "proc-macro" feature (needed by serde_derive)
        let pm2 = result
            .crates
            .values()
            .find(|c| c.crate_name == "proc-macro2");
        assert!(pm2.is_some(), "proc-macro2 not found");
        let pm2 = pm2.unwrap();
        assert!(
            pm2.resolved_default_features
                .contains(&"proc-macro".to_string()),
            "proc-macro2 should have 'proc-macro' feature, got: {:?}",
            pm2.resolved_default_features
        );
    }
}
