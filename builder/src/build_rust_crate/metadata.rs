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

    // Symlink .rmeta files from pipelined deps into target/deps.
    // NIX_INC_RMETA_DIRS contains "\n"-separated "store_path\trmeta_dir" pairs
    // from deps whose full build hasn't finished but whose .rmeta is ready.
    if let Ok(dirs) = std::env::var("NIX_INC_RMETA_DIRS") {
        for line in dirs.lines() {
            if let Some((_drv, dir)) = line.split_once('\t') {
                if let Ok(entries) = fs::read_dir(dir) {
                    for entry in entries.flatten() {
                        let name = entry.file_name();
                        let name_str = name.to_string_lossy();
                        if name_str.ends_with(".rmeta") {
                            // Only symlink .rmeta if no .rlib exists for the same crate.
                            // Having both causes "colliding StableCrateId" errors.
                            let rlib_name = name_str.replace(".rmeta", ".rlib");
                            let rlib_path = Path::new("target/deps").join(&rlib_name);
                            if rlib_path.exists() {
                                continue;
                            }
                            let dest = Path::new("target/deps").join(&*name);
                            if !dest.exists() {
                                let _ = std::os::unix::fs::symlink(entry.path(), &dest);
                            }
                        }
                    }
                }
            }
        }
    }

    // For pipelining consistency, convert all .rlib deps to .rmeta.
    // rustc records the SVH of each dependency; mixing .rmeta and .rlib
    // for the same crate causes E0460 ("possibly newer version") errors.
    // Extract .rmeta from .rlib archives so all --extern flags use .rmeta.
    if let Ok(entries) = fs::read_dir("target/deps") {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.ends_with(".rlib") {
                let rmeta_name = name.replace(".rlib", ".rmeta");
                let rmeta_path = Path::new("target/deps").join(&rmeta_name);
                if !rmeta_path.exists() {
                    // Extract lib.rmeta from the .rlib archive
                    let rlib_path = entry.path();
                    let status = std::process::Command::new("ar")
                        .arg("p")
                        .arg(&rlib_path)
                        .arg("lib.rmeta")
                        .stdout(std::process::Stdio::from(
                            fs::File::create(&rmeta_path).unwrap(),
                        ))
                        .status();
                    if status.map(|s| !s.success()).unwrap_or(true) {
                        // If extraction fails, remove the partial file
                        let _ = fs::remove_file(&rmeta_path);
                    } else {
                        // Remove the .rlib so find_by_metadata picks .rmeta
                        let _ = fs::remove_file(&rlib_path);
                    }
                }
            }
        }
    }

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
