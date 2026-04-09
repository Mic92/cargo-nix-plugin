//! Metadata-only compilation pass for build pipelining.
//!
//! Runs `rustc --emit=metadata` to produce `.rmeta` files (type information)
//! without LLVM codegen. Dependents can start compiling against `.rmeta`
//! before the full `.rlib` is ready, shortening the critical path.
//!
//! This reuses the same flag computation as `build.rs` to ensure the
//! metadata is compatible with the eventual full build.

use std::fs;

use super::build::{resolve_lib_path, setup_build};
use super::config::BuildConfig;
use super::util::run_cmd;

/// Emit metadata (.rmeta) for the lib target into `rmeta_out_dir`.
///
/// Performs the full configure step first (symlink deps, run build.rs),
/// then invokes rustc with `--emit=metadata` instead of `--emit=link`.
pub fn run(config: &mut BuildConfig, rmeta_out_dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    // Run the configure phase to set up deps and build scripts. This already
    // calls detect_cargo_toml_info(), so config.{lib_path,lib_name,crate_type}
    // are populated by the time we read them below.
    super::configure::run(config)?;

    let flags = setup_build(config)?;
    let crate_name = config.lib_name_normalized();

    // Only emit metadata for the lib target
    let lib_path = match resolve_lib_path(config) {
        Some(p) => p,
        None => return Ok(()), // no lib target → nothing to emit
    };

    // If this crate is a proc-macro, metadata-only compilation isn't
    // useful — consumers need the full dylib. Write a marker so the
    // scheduler can detect this and require a full build for dependents.
    if config.crate_type.iter().any(|t| t == "proc-macro") {
        fs::create_dir_all(rmeta_out_dir)?;
        fs::write(format!("{rmeta_out_dir}/.proc-macro"), b"")?;
        // Fall through to emit metadata anyway (for type-checking),
        // but the scheduler will require a full build for dependents.
    }

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
