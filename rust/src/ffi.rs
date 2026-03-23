//! C FFI interface for calling from the C++ Nix plugin shim.

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

use crate::cfg_eval::TargetDescription;
use crate::resolve::resolve_workspace;

/// The cargo binary path baked in at build time from the Nix store.
/// Falls back to "cargo" (from PATH) when not set, e.g. during local development.
const BUILTIN_CARGO_PATH: &str = match option_env!("CARGO_NIX_PLUGIN_CARGO_PATH") {
    Some(p) => p,
    None => "cargo",
};

/// Input from the Nix side — the entire attrset serialized as JSON.
///
/// Two modes:
/// 1. Explicit: `metadata` + `cargoLock` provided (pure, no subprocess)
/// 2. Subprocess: `manifestPath` provided (shells out to compiled-in cargo)
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginInput {
    /// Explicit cargo metadata JSON (mode 1)
    metadata: Option<String>,
    /// Explicit Cargo.lock contents (required with metadata)
    cargo_lock: Option<String>,
    /// Path to Cargo.toml (mode 2 — subprocess)
    manifest_path: Option<String>,
    /// Extra `--config` arguments for the cargo metadata subprocess.
    /// Each entry is passed as a separate `--config <value>` pair; cargo
    /// accepts both `KEY=VALUE` strings and paths to config.toml files.
    #[serde(default)]
    cargo_config: Vec<String>,
    target: TargetDescription,
    #[serde(default = "default_root_features")]
    root_features: Vec<String>,
}

fn default_root_features() -> Vec<String> {
    vec!["default".to_string()]
}

/// Validate input and resolve the workspace using the appropriate mode.
fn validate_and_resolve(input: &PluginInput) -> Result<crate::resolve::WorkspaceResult, String> {
    let (metadata_json, cargo_lock_str) = match (&input.metadata, &input.manifest_path) {
        (Some(_), Some(_)) => {
            return Err("Provide either 'metadata' or 'manifestPath', not both.".to_string());
        }
        (None, None) => {
            return Err(
                "Provide either 'metadata' (explicit JSON) or 'manifestPath' (path to Cargo.toml) \
                 to resolve a cargo workspace."
                    .to_string(),
            );
        }
        (Some(metadata), None) => {
            let cargo_lock = input
                .cargo_lock
                .as_deref()
                .ok_or("'cargoLock' is required when 'metadata' is provided.")?;
            (metadata.clone(), cargo_lock.to_string())
        }
        (None, Some(manifest_path)) => {
            let metadata_json =
                run_cargo_metadata(BUILTIN_CARGO_PATH, manifest_path, &input.cargo_config)?;
            let manifest_dir = std::path::Path::new(manifest_path)
                .parent()
                .ok_or_else(|| format!("Cannot determine parent directory of {manifest_path}"))?;
            let lock_path = manifest_dir.join("Cargo.lock");
            let cargo_lock_str = std::fs::read_to_string(&lock_path)
                .map_err(|e| format!("Failed to read {}: {e}", lock_path.display()))?;
            (metadata_json, cargo_lock_str)
        }
    };

    resolve_workspace(
        &metadata_json,
        &cargo_lock_str,
        &input.target,
        &input.root_features,
    )
}

/// Run `cargo metadata` as a subprocess and return its stdout.
///
/// Runs with CWD set to the manifest's directory so cargo discovers
/// `.cargo/config.toml` relative to the workspace (cargo's config search
/// walks up from CWD, not from `--manifest-path`). Without this, the
/// result depends on where the nix evaluator happens to be running.
fn run_cargo_metadata(
    cargo_path: &str,
    manifest_path: &str,
    cargo_config: &[String],
) -> Result<String, String> {
    let manifest_dir = std::path::Path::new(manifest_path)
        .parent()
        .ok_or_else(|| format!("Cannot determine parent directory of {manifest_path}"))?;
    let mut cmd = std::process::Command::new(cargo_path);
    cmd.current_dir(manifest_dir).args([
        "metadata",
        "--format-version",
        "1",
        "--locked",
        "--manifest-path",
        manifest_path,
    ]);
    for cfg in cargo_config {
        cmd.args(["--config", cfg]);
    }
    let output = cmd
        .output()
        .map_err(|e| format!("Failed to run '{cargo_path} metadata': {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "cargo metadata failed (exit {}):\n{stderr}\n\n\
             Hint: pass 'metadata' explicitly for offline/pure usage.",
            output.status
        ));
    }

    String::from_utf8(output.stdout)
        .map_err(|e| format!("cargo metadata produced invalid UTF-8: {e}"))
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

    /// Verify run_cargo_metadata sets CWD to the manifest dir so cargo
    /// discovers .cargo/config.toml relative to the workspace, not the
    /// evaluator's CWD.
    ///
    /// The fixture references a fake registry via [patch.crates-io]. Cargo
    /// needs the registry *defined* (via .cargo/config.toml) to parse the
    /// manifest; without it, the error is "registry index was not found".
    /// With it, cargo proceeds to contact the (unreachable) registry and
    /// fails on DNS — a different error that proves config discovery worked.
    #[test]
    fn subprocess_discovers_cargo_config_via_manifest_dir() {
        let fixture = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample-project-registry/Cargo.toml"
        );

        // `cargo test` runs from CARGO_MANIFEST_DIR (rust/), which has no
        // .cargo/config.toml in its ancestry. Without .current_dir(manifest_dir)
        // the subprocess would search from there and miss the fixture's config.
        let err = run_cargo_metadata(BUILTIN_CARGO_PATH, fixture, &[])
            .expect_err("fixture registry is unreachable; metadata should fail");

        assert!(
            !err.contains("registry index was not found"),
            "cargo did not discover .cargo/config.toml — current_dir not applied?\n{err}"
        );
        assert!(
            err.contains("registry.invalid") || err.contains("test-registry"),
            "expected registry-contact error, got:\n{err}"
        );
    }

    /// Verify cargoConfig entries are passed through as --config args.
    /// Same fixture, but we move .cargo/config.toml out of reach by
    /// pointing at a manifest copy in a tempdir, then supply the registry
    /// definition via cargoConfig instead.
    #[test]
    fn cargo_config_param_defines_registry() {
        let fixture_dir = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/sample-project-registry"
        );
        let tmp = mk_tempdir("cargo-nix-plugin-config-test");
        for f in ["Cargo.toml", "Cargo.lock"] {
            std::fs::copy(format!("{fixture_dir}/{f}"), tmp.join(f)).expect("copy fixture file");
        }
        std::fs::create_dir(tmp.join("src")).expect("mkdir src");
        std::fs::write(tmp.join("src/lib.rs"), "").expect("write lib.rs");

        let manifest = tmp.join("Cargo.toml");
        let manifest = manifest.to_str().expect("utf8 path");

        // No .cargo/config.toml here — registry must come from cargoConfig.
        let cfg = vec![
            r#"registries.test-registry.index="sparse+https://registry.invalid/index/""#
                .to_string(),
        ];
        let err = run_cargo_metadata(BUILTIN_CARGO_PATH, manifest, &cfg)
            .expect_err("fixture registry is unreachable; metadata should fail");

        assert!(
            !err.contains("registry index was not found"),
            "cargoConfig --config arg not applied?\n{err}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn mk_tempdir(prefix: &str) -> std::path::PathBuf {
        loop {
            let dir = std::env::temp_dir().join(format!(
                "{prefix}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            match std::fs::create_dir(&dir) {
                Ok(()) => return dir,
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("create tempdir: {e}"),
            }
        }
    }

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

    /// End-to-end test: run cargo metadata on this crate's own workspace
    /// and verify the subprocess path produces valid output.
    /// Requires cargo on PATH and network access.
    #[test]
    #[ignore]
    fn subprocess_resolves_own_workspace() {
        let manifest_path = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");

        let input = PluginInput {
            metadata: None,
            cargo_lock: None,
            manifest_path: Some(manifest_path.to_string()),
            cargo_config: vec![],
            target: linux_x86_64(),
            root_features: vec!["default".to_string()],
        };

        let result = validate_and_resolve(&input).expect("subprocess resolution failed");
        // This crate itself should be a workspace member
        assert!(
            !result.workspace_members.is_empty(),
            "expected at least one workspace member"
        );
        assert!(!result.crates.is_empty(), "expected at least one crate");
        // Spot-check: serde should be in deps (we depend on it)
        let has_serde = result.crates.values().any(|c| c.crate_name == "serde");
        assert!(has_serde, "expected serde in resolved crates");
    }
}
