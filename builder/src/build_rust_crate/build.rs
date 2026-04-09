use std::fs;
use std::path::Path;

use super::config::BuildConfig;
use super::configure::{BuildScriptOutputs, build_env, enter_crate_root};
use super::rustc::RustcFlags;
use super::util::{echo_colored, remove_object_files, run_cmd};

/// Shared preamble for the build and metadata phases: load build-script
/// outputs, export CARGO_* / rustc-env, persist link flags, compute rustc
/// flags. Caller must already be in the crate root.
pub fn setup_build(config: &BuildConfig) -> Result<RustcFlags, Box<dyn std::error::Error>> {
    let bso: BuildScriptOutputs = match fs::read_to_string("target/build-script-outputs.json") {
        Ok(s) => serde_json::from_str(&s)?,
        Err(_) => BuildScriptOutputs::default(),
    };

    for (k, v) in build_env(config, "") {
        if k.starts_with("CARGO_") {
            std::env::set_var(k, v);
        }
    }
    // rustc-env from build script, verbatim: scripts that need absolute paths
    // join OUT_DIR/CARGO_MANIFEST_DIR themselves.
    for (k, v) in &bso.envs {
        std::env::set_var(k, v);
    }

    persist_bso_link_flags(&bso, config)?;
    Ok(RustcFlags::new(config, &bso))
}

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    enter_crate_root(config)?;

    let flags = setup_build(config)?;
    let crate_name = config.lib_name_normalized();
    let metadata = &config.metadata;

    let mut lib_extern: Vec<String> = Vec::new();

    // Build lib
    if let Some(lib_path) = resolve_lib_path(config) {
        echo_colored(&format!("Building {lib_path} ({})", config.lib_name));
        let crate_types: Vec<&str> = config.crate_type.iter().map(|s| s.as_str()).collect();
        let mut extra = flags.meta.clone();
        extra.extend_from_slice(&flags.bso_lib);
        if config.crate_type.iter().any(|t| t == "cdylib") {
            extra.extend_from_slice(&flags.bso_cdylib);
        }

        run_cmd(
            &mut flags.cmd(&crate_name, &lib_path, "target/lib", &crate_types, &extra, false),
            config.verbose,
        )?;

        // Own bins/tests link against the lib we just built. Look it up by
        // metadata so proc-macro / dylib-only crates (no .rlib) still work.
        let lib_path = super::rustc::find_by_metadata("target/lib", metadata)
            .unwrap_or_else(|| format!("target/lib/lib{crate_name}-{metadata}.rlib"));
        lib_extern.extend_from_slice(&["--extern".into(), format!("{crate_name}={lib_path}")]);

        // Proc-macro marker for downstream crates
        if config.crate_type.iter().any(|t| t == "proc-macro") {
            fs::write("target/lib/proc-macro.marker", b"")?;
        }

        if config.build_tests {
            echo_colored(&format!("Building test lib {}", config.lib_name));
            run_cmd(
                &mut flags.cmd(&crate_name, &lib_path, "target/lib", &crate_types, &extra, true),
                config.verbose,
            )?;
        }
    }

    // Build binaries
    for (name, path) in resolve_bins(config) {
        build_bin(config, &flags, &lib_extern, &name, &path, false)?;
        if config.build_tests {
            build_bin(config, &flags, &lib_extern, &name, &path, true)?;
        }
    }

    // Build integration tests from tests/
    if config.build_tests && Path::new("tests").is_dir() {
        for entry in fs::read_dir("tests")?.flatten() {
            let p = entry.path();
            if p.extension().map(|e| e == "rs").unwrap_or(false) && (p.is_file() || p.is_symlink())
            {
                let name = p.file_stem().unwrap().to_string_lossy().to_string();
                build_bin(config, &flags, &lib_extern, &name, &p.to_string_lossy(), true)?;
            } else if p.is_dir() && p.join("main.rs").exists() {
                let name = p.file_name().unwrap().to_string_lossy().to_string();
                build_bin(
                    config,
                    &flags,
                    &lib_extern,
                    &name,
                    &p.join("main.rs").to_string_lossy(),
                    true,
                )?;
            }
        }
    }

    remove_object_files("target")?;
    Ok(())
}

/// Append build-script link search/lib flags to target/link and
/// target/link.final, mirroring what the bash setup_link_paths did.
fn persist_bso_link_flags(
    bso: &BuildScriptOutputs,
    config: &BuildConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    if bso.link_search.is_empty() && bso.link_libs.is_empty() {
        return Ok(());
    }

    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let lib_out = config.lib_path_output().unwrap_or_else(|| config.out_path());

    let mut link_append = String::new();
    let mut link_final_append = String::new();

    for search in &bso.link_search {
        link_append.push_str(&format!("-L {search}\n"));
        // Remap build sandbox paths to installed output paths
        let remapped = search.replace(
            &format!("{cwd}/target/build"),
            &format!("{lib_out}/lib"),
        );
        link_final_append.push_str(&format!("-L {remapped}\n"));
    }
    for lib in &bso.link_libs {
        link_append.push_str(&format!("-l {lib}\n"));
    }

    use std::io::Write;
    if !link_append.is_empty() {
        let mut f = fs::OpenOptions::new().append(true).open("target/link")?;
        f.write_all(link_append.as_bytes())?;
    }
    if !link_final_append.is_empty() {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open("target/link.final")?;
        f.write_all(link_final_append.as_bytes())?;
    }

    // Regenerate target/link_ (space-separated)
    let link_content = fs::read_to_string("target/link")?;
    fs::write(
        "target/link_",
        link_content.lines().collect::<Vec<_>>().join(" "),
    )?;

    Ok(())
}

fn build_bin(
    config: &BuildConfig,
    flags: &RustcFlags,
    lib_extern: &[String],
    name: &str,
    path: &str,
    test: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    echo_colored(&format!(
        "Building {}{name}",
        if test { "test " } else { "" }
    ));
    fs::create_dir_all("target/bin")?;

    let mut extra = flags.bso_bins.clone();
    extra.extend_from_slice(lib_extern);
    let crate_name_ = name.replace('-', "_");
    run_cmd(
        &mut flags.cmd(&crate_name_, path, "target/bin", &["bin"], &extra, test),
        config.verbose,
    )?;

    // Rename binary if dash vs underscore mismatch
    if !test && crate_name_ != name {
        let wasm = format!("target/bin/{crate_name_}.wasm");
        let bin = format!("target/bin/{crate_name_}");
        if Path::new(&wasm).exists() {
            fs::rename(&wasm, format!("target/bin/{name}.wasm"))?;
        } else if Path::new(&bin).exists() {
            fs::rename(&bin, format!("target/bin/{name}"))?;
        }
    }
    Ok(())
}

fn resolve_lib_path(config: &BuildConfig) -> Option<String> {
    if !config.lib_path.is_empty() && Path::new(&config.lib_path).exists() {
        Some(config.lib_path.clone())
    } else if Path::new("src/lib.rs").exists() {
        Some("src/lib.rs".into())
    } else {
        None
    }
}

fn resolve_bins(config: &BuildConfig) -> Vec<(String, String)> {
    let mut bins = Vec::new();

    if !config.crate_bin.is_empty() {
        for bin in &config.crate_bin {
            let name = bin
                .name
                .clone()
                .unwrap_or_else(|| config.crate_name.clone());

            // Skip binaries missing required features
            if !bin.required_features.is_empty()
                && !bin
                    .required_features
                    .iter()
                    .all(|f| config.crate_features.contains(f))
            {
                eprintln!(
                    "Binary {name} not compiled: missing required features {:?}",
                    bin.required_features
                );
                continue;
            }

            if let Some(ref path) = bin.path {
                bins.push((name, path.clone()));
            } else if let Some(path) =
                search_bin_path(&name, &config.lib_path, &config.lib_name)
            {
                bins.push((name, path));
            } else {
                eprintln!(
                    "\x1b[0;1;31mERROR: failed to find file for binary target: {name}\x1b[0m"
                );
                std::process::exit(1);
            }
        }
    } else if !config.has_crate_bin {
        if Path::new("src/main.rs").exists() {
            bins.push((config.crate_name.clone(), "src/main.rs".into()));
        }
        if let Ok(entries) = fs::read_dir("src/bin") {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "rs").unwrap_or(false) {
                    let name = path.file_stem().unwrap().to_string_lossy().to_string();
                    bins.push((name, path.to_string_lossy().into_owned()));
                } else if path.is_dir() && path.join("main.rs").exists() {
                    let name = path.file_name().unwrap().to_string_lossy().to_string();
                    bins.push((name, path.join("main.rs").to_string_lossy().into_owned()));
                }
            }
        }
    }
    bins
}

fn search_bin_path(bin_name: &str, lib_path: &str, lib_name: &str) -> Option<String> {
    let bin_name_ = bin_name.replace('-', "_");
    let has_lib = (!lib_path.is_empty() && Path::new(lib_path).exists())
        || Path::new("src/lib.rs").exists()
        || Path::new(&format!("src/{lib_name}.rs")).exists();

    let mut candidates = Vec::new();
    if !has_lib {
        candidates.push(format!("src/{bin_name}.rs"));
        candidates.push(format!("src/{bin_name_}.rs"));
    }
    candidates.extend([
        format!("src/bin/{bin_name}.rs"),
        format!("src/bin/{bin_name}/main.rs"),
        format!("src/bin/{bin_name_}.rs"),
        format!("src/bin/{bin_name_}/main.rs"),
        "src/bin/main.rs".into(),
        "src/main.rs".into(),
    ]);
    candidates.into_iter().find(|c| Path::new(c).exists())
}
