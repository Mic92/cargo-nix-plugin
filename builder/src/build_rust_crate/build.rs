// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::path::Path;

use super::config::{BuildConfig, CrateMetadata};
use super::configure::{BuildScriptOutputs, build_env, detect_cargo_toml_info};
use super::rustc::RustcFlags;
use super::util::{echo_colored, remove_object_files, run_cmd, set_var};

/// Load build-script outputs, export CARGO_* / rustc-env, persist link flags,
/// compute rustc flags. Caller must already be in the crate root.
fn setup_build(config: &BuildConfig) -> Result<RustcFlags, Box<dyn std::error::Error>> {
    let bso: BuildScriptOutputs = match fs::read_to_string("target/build-script-outputs.json") {
        Ok(s) => serde_json::from_str(&s)?,
        Err(_) => BuildScriptOutputs::default(),
    };

    // Export only the subset cargo's compilation.rs::fill_env sets on rustc
    // (CARGO_CFG_*/CARGO_FEATURE_*/CARGO_ENCODED_RUSTFLAGS are build-script-only).
    for (k, v) in build_env(config, "") {
        let pass = k == "CARGO"
            || k == "CARGO_CRATE_NAME"
            || k.starts_with("CARGO_PKG_")
            || k.starts_with("CARGO_MANIFEST_") && k != "CARGO_MANIFEST_LINKS";
        if pass {
            set_var(k, v);
        }
    }
    for (k, v) in &bso.envs {
        set_var(k, v);
    }

    persist_bso_link_flags(&bso, config)?;
    Ok(RustcFlags::new(config, &bso))
}

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    detect_cargo_toml_info(config);

    // Publish crate-metadata.json before rustc starts so a pipelining
    // scheduler that dispatches dependents on our rmeta (mid-build) can
    // construct `--extern` args. `install` overwrites with the scanned
    // `target/lib` artifact set. Under plain nix-build the early write is
    // unobservable (dependents only start after install).
    if !config.build_tests {
        let lib_out = config.lib_path_output().unwrap_or_else(|| config.out_path());
        CrateMetadata::provisional(config).write(lib_out)?;
    }

    let flags = setup_build(config)?;
    let crate_name = config.lib_name_normalized();
    let metadata = &config.metadata;

    let mut lib_extern: Vec<String> = Vec::new();

    // Build lib
    if let Some(lib_src) = resolve_lib_path(config) {
        echo_colored(&format!("Building {lib_src} ({})", config.lib_name));
        let crate_types: Vec<&str> = config.crate_type.iter().map(|s| s.as_str()).collect();
        let mut extra = flags.meta.clone();
        extra.extend_from_slice(&flags.bso_lib);
        if config.crate_type.iter().any(|t| t == "cdylib") {
            extra.extend_from_slice(&flags.bso_cdylib);
        }
        // Rust `dylib` deps need `-C prefer-dynamic` or downstream links fail
        // on duplicate std. Cargo gates on `!is_primary_package`; in per-crate
        // derivations a dylib is always built as a dep, so always set it.
        if config.crate_type.iter().any(|t| t == "dylib") {
            extra.extend_from_slice(&["-C".into(), "prefer-dynamic".into()]);
        }

        run_cmd(
            &mut flags.cmd(&crate_name, &lib_src, "target/lib", &crate_types, &extra, false, true),
            config.verbose,
        )?;

        // Own bins/tests link the lib only when Rust-linkable (matches
        // cargo Target::is_linkable(): lib|rlib|dylib|proc-macro).
        let linkable = config
            .crate_type
            .iter()
            .any(|t| matches!(t.as_str(), "lib" | "rlib" | "dylib" | "proc-macro"));
        if linkable {
            let lib_artifact = super::rustc::find_by_metadata("target/lib", metadata)
                .unwrap_or_else(|| format!("target/lib/lib{crate_name}-{metadata}.rlib"));
            lib_extern
                .extend_from_slice(&["--extern".into(), format!("{crate_name}={lib_artifact}")]);
        }

        if config.build_tests {
            echo_colored(&format!("Building test lib {}", config.lib_name));
            let mut cmd =
                flags.cmd(&crate_name, &lib_src, "target/lib", &crate_types, &extra, true, true);
            let tmp = fs::canonicalize({
                fs::create_dir_all("target/tmp")?;
                "target/tmp"
            })?;
            cmd.env("CARGO_TARGET_TMPDIR", tmp);
            run_cmd(&mut cmd, config.verbose)?;
        }
    }

    let bins = resolve_bins(config);
    let mut test_env: Vec<(String, String)> = Vec::new();
    if config.build_tests {
        let tmp = fs::canonicalize({
            fs::create_dir_all("target/tmp")?;
            "target/tmp"
        })?
        .to_string_lossy()
        .into_owned();
        test_env.push(("CARGO_TARGET_TMPDIR".into(), tmp));
        // Point CARGO_BIN_EXE_* at the installed $out/bin (sandbox path is
        // gone by the time tests run).
        let out = config.out_path();
        for (name, _) in &bins {
            test_env.push((format!("CARGO_BIN_EXE_{name}"), format!("{out}/bin/{name}")));
        }
    }

    let bb = BinBuilder { config, flags: &flags, lib_extern: &lib_extern, test_env: &test_env };

    // Bins are always real executables (even under buildTests) so
    // CARGO_BIN_EXE_<name> resolves; matches cargo's default test set.
    for (name, path) in &bins {
        bb.build(name, path, BinKind::Bin)?;
    }

    if config.build_tests {
        for (name, path, harness) in resolve_tests(config) {
            bb.build(&name, &path, BinKind::Test { harness })?;
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

#[derive(Clone, Copy)]
enum BinKind {
    Bin,
    Test { harness: bool },
}

struct BinBuilder<'a> {
    config: &'a BuildConfig,
    flags: &'a RustcFlags,
    lib_extern: &'a [String],
    test_env: &'a [(String, String)],
}

impl BinBuilder<'_> {
    fn build(&self, name: &str, path: &str, kind: BinKind) -> Result<(), Box<dyn std::error::Error>> {
        let test = matches!(kind, BinKind::Test { .. });
        echo_colored(&format!("Building {}{name}", if test { "test " } else { "" }));
        let out_dir = if test { "target/tests" } else { "target/bin" };
        fs::create_dir_all(out_dir)?;

        // Route build-script link-args by target kind (rustc-link-arg-bins/-bin=NAME
        // vs rustc-link-arg-tests; the universal one is folded into both).
        let mut extra = match kind {
            BinKind::Bin => {
                let mut v = self.flags.bso_bins.clone();
                if let Some(per) = self.flags.bso_bin.get(name) {
                    v.extend_from_slice(per);
                }
                v
            }
            BinKind::Test { .. } => self.flags.bso_tests.clone(),
        };
        extra.extend_from_slice(self.lib_extern);
        let crate_name_ = name.replace('-', "_");
        let harness = !matches!(kind, BinKind::Test { harness: false });
        let mut cmd = self.flags.cmd(&crate_name_, path, out_dir, &["bin"], &extra, test, harness);
        cmd.env("CARGO_BIN_NAME", name);
        if test {
            for (k, v) in self.test_env {
                cmd.env(k, v);
            }
        }
        run_cmd(&mut cmd, self.config.verbose)?;

        // Rename binary if dash vs underscore mismatch
        if crate_name_ != name {
            let wasm = format!("{out_dir}/{crate_name_}.wasm");
            let bin = format!("{out_dir}/{crate_name_}");
            if Path::new(&wasm).exists() {
                fs::rename(&wasm, format!("{out_dir}/{name}.wasm"))?;
            } else if Path::new(&bin).exists() {
                fs::rename(&bin, format!("{out_dir}/{name}"))?;
            }
        }
        Ok(())
    }
}

fn resolve_lib_path(config: &BuildConfig) -> Option<String> {
    if !config.lib_path.is_empty() && Path::new(&config.lib_path).exists() {
        Some(config.lib_path.clone())
    } else if config.autolib && Path::new("src/lib.rs").exists() {
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
                    .all(|f| config.crate_features_raw.contains(f))
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
    } else if !config.has_crate_bin && config.autobins {
        // No [[bin]] and no Nix crateBin: pure inference.
        bins.extend(super::configure::inferred_bins(&config.crate_name));
    }
    bins
}

/// Merge explicit `[[test]]` with autotests-inferred targets, dedupe by
/// name/path, filter on required-features, carry `harness` through.
fn resolve_tests(config: &BuildConfig) -> Vec<(String, String, bool)> {
    let mut tests: Vec<(String, String, bool)> = Vec::new();
    for t in &config.crate_tests {
        if !t.required_features.is_empty()
            && !t
                .required_features
                .iter()
                .all(|f| config.crate_features_raw.contains(f))
        {
            eprintln!(
                "Test {name} not compiled: missing required features {:?}",
                t.required_features,
                name = t.name
            );
            continue;
        }
        let Some(path) = t.path.clone().or_else(|| {
            [format!("tests/{n}.rs", n = t.name), format!("tests/{n}/main.rs", n = t.name)]
                .into_iter()
                .find(|p| Path::new(p).exists())
        }) else {
            eprintln!(
                "\x1b[0;1;31mERROR: failed to find file for test target: {}\x1b[0m",
                t.name
            );
            std::process::exit(1);
        };
        tests.push((t.name.clone(), path, t.harness));
    }
    if config.autotests {
        for (name, path) in inferred_tests() {
            let taken = tests.iter().any(|(n, p, _)| *n == name || *p == path);
            if !taken {
                tests.push((name, path, true));
            }
        }
    }
    tests
}

/// Cargo's autotests inference: tests/*.rs → stem, tests/*/main.rs → dirname.
/// Dotfiles skipped.
fn inferred_tests() -> Vec<(String, String)> {
    let mut tests = Vec::new();
    let Ok(entries) = fs::read_dir("tests") else {
        return tests;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let fname = entry.file_name();
        if fname.to_string_lossy().starts_with('.') {
            continue;
        }
        if p.extension().is_some_and(|e| e == "rs") && (p.is_file() || p.is_symlink()) {
            let name = p.file_stem().unwrap().to_string_lossy().to_string();
            tests.push((name, p.to_string_lossy().into_owned()));
        } else if p.is_dir() && p.join("main.rs").exists() {
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            tests.push((name, p.join("main.rs").to_string_lossy().into_owned()));
        }
    }
    tests
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
