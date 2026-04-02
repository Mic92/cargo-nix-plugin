//! Metadata-only compilation pass for build pipelining.
//!
//! Runs `rustc --emit=metadata` to produce `.rmeta` files (type information)
//! without LLVM codegen. Dependents can start compiling against `.rmeta`
//! before the full `.rlib` is ready, shortening the critical path.
//!
//! This reuses the same flag computation as `build.rs` to ensure the
//! metadata is compatible with the eventual full build.

use std::fs;
use std::path::Path;

use super::build::resolve_lib_path;
use super::config::BuildConfig;
use super::configure::{BuildScriptOutputs, build_env, detect_cargo_toml_info};
use super::rustc::RustcFlags;
use super::util::run_cmd;

/// Emit metadata (.rmeta) for the lib target into `rmeta_out_dir`.
///
/// Performs the full configure step first (symlink deps, run build.rs),
/// then invokes rustc with `--emit=metadata` instead of `--emit=link`.
pub fn run(config: &mut BuildConfig, rmeta_out_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Run the configure phase to set up deps and build scripts
    super::configure::run(config)?;

    detect_cargo_toml_info(config);

    let bso = if Path::new("target/build-script-outputs.json").exists() {
        serde_json::from_str(&fs::read_to_string("target/build-script-outputs.json")?)?
    } else {
        BuildScriptOutputs::default()
    };

    // Set CARGO_PKG_* env vars
    let env = build_env(config, "");
    for (k, v) in &env {
        if k.starts_with("CARGO_") {
            std::env::set_var(k, v);
        }
    }

    // Export build script env vars
    let cwd = std::env::current_dir()?;
    for (k, v) in &bso.envs {
        let val = if Path::new(v).is_relative() && Path::new(v).exists() {
            cwd.join(v).to_string_lossy().to_string()
        } else {
            v.clone()
        };
        std::env::set_var(k, &val);
    }

    // Persist build script link flags (needed for link_ flags)
    super::build::persist_bso_link_flags(&bso, config)?;

    let flags = RustcFlags::new(config, &bso);
    let crate_name = config.lib_name_normalized();

    // Only emit metadata for the lib target
    let lib_path = match resolve_lib_path(config) {
        Some(p) => p,
        None => return Ok(()), // no lib target → nothing to emit
    };

    fs::create_dir_all(rmeta_out_dir)?;

    let crate_types: Vec<&str> = config.crate_type.iter().map(|s| s.as_str()).collect();
    let mut extra = flags.meta.clone();
    extra.extend_from_slice(&flags.bso_lib);

    // Build the command with --emit=metadata instead of default
    let mut cmd = flags.cmd(&crate_name, &lib_path, rmeta_out_dir, &crate_types, &extra, false);
    cmd.arg("--emit=metadata");

    run_cmd(&mut cmd, config.verbose)?;

    Ok(())
}
