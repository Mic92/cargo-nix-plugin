// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use super::config::{BuildConfig, CrateMetadata};
use super::configure::detect_cargo_toml_info;

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    detect_cargo_toml_info(config);

    let metadata = &config.metadata;
    let out = config.out_path();

    if config.build_tests {
        return install_tests(config);
    }

    let lib_out = config.lib_path_output().unwrap_or(out);
    fs::create_dir_all(out)?;
    fs::create_dir_all(lib_out)?;

    // Copy link flags for downstream crates
    copy_if_nonempty("target/link.final", &format!("{lib_out}/lib/link"))?;

    // Lib artifact filenames for crate-metadata.json (anything with `-{metadata}.`).
    let mut artifacts = Vec::new();
    if let Ok(entries) = fs::read_dir("target/lib") {
        let stem = format!("-{metadata}.");
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.contains(&stem) && !name.ends_with(".d") {
                artifacts.push(name);
            }
        }
        artifacts.sort();
    }

    // links_vars often point under the sandbox OUT_DIR. We copy target/build/*
    // → $lib_out/lib/* below, so remap that prefix in DEP_<LINKS>_* values to
    // the installed location (bso.envs are NOT remapped: they're consumed by
    // this crate's own compile while the sandbox path is still live).
    let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
    let sandbox_build = format!("{cwd}/target/build/");
    let installed_build = format!("{lib_out}/lib/");
    let links_vars: std::collections::BTreeMap<String, String> =
        fs::read_to_string("target/links-vars.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let links_vars: std::collections::BTreeMap<String, String> = links_vars
        .into_iter()
        .map(|(k, v)| (k, v.replace(&sandbox_build, &installed_build)))
        .collect();

    // Legacy DEP_* env file (for crateOverrides that sed it); regenerated from remapped vars.
    if !config.crate_links.is_empty() && !links_vars.is_empty() {
        let links_upper = config.crate_links.replace('-', "_").to_uppercase();
        let lines: Vec<String> = links_vars
            .iter()
            .map(|(k, v)| {
                let key = k.replace('-', "_").to_uppercase();
                let q = format!("'{}'", v.replace('\'', r"'\''"));
                format!("export DEP_{links_upper}_{key}={q}")
            })
            .collect();
        fs::write(format!("{lib_out}/env"), lines.join("\n"))?;
    }

    // Overwrite the provisional copy from `build` with the scanned artifact
    // set and remapped links_vars.
    CrateMetadata {
        artifacts,
        links_vars,
        ..CrateMetadata::provisional(config)
    }
    .write(lib_out)?;

    // Copy lib artifacts + create un-hashed symlinks for .so/.dylib
    if dir_has_files("target/lib") {
        let dst = format!("{lib_out}/lib");
        fs::create_dir_all(&dst)?;
        copy_tree("target/lib", &dst)?;
        for entry in fs::read_dir(&dst)?.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if (name.ends_with(".so") || name.ends_with(".dylib"))
                && name.contains(&format!("-{metadata}"))
            {
                let unhashed = name.replace(&format!("-{metadata}"), "");
                let link = format!("{dst}/{unhashed}");
                let _ = fs::remove_file(&link);
                symlink(entry.path(), &link)?;
            }
        }
    }

    // Copy build script outputs
    if dir_has_files("target/build") {
        let dst = format!("{lib_out}/lib");
        fs::create_dir_all(&dst)?;
        copy_tree("target/build", &dst)?;
    }

    // Copy binaries
    if dir_has_files("target/bin") {
        let dst = format!("{out}/bin");
        fs::create_dir_all(&dst)?;
        copy_tree("target/bin", &dst)?;
    }

    Ok(())
}

fn install_tests(config: &BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    let out = config.out_path();
    let tests_dst = format!("{out}/tests");
    let bin_dst = format!("{out}/bin");
    fs::create_dir_all(&tests_dst)?;
    fs::create_dir_all(&bin_dst)?;

    // Tests → $out/tests; real bins → $out/bin (so CARGO_BIN_EXE_* resolves
    // and runTests doesn't execute them).
    for (dir, dst) in [
        ("target/tests", tests_dst.as_str()),
        ("target/lib", tests_dst.as_str()),
        ("target/bin", bin_dst.as_str()),
    ] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap().to_string_lossy();
            let is_lib = matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("rlib" | "so" | "dylib" | "a" | "d")
            );
            if !p.is_file() || !is_executable(&p) || (dir == "target/lib" && is_lib) {
                continue;
            }
            fs::copy(&p, format!("{dst}/{name}"))?;
        }
    }
    Ok(())
}

fn copy_if_nonempty(src: &str, dst: &str) -> Result<(), Box<dyn std::error::Error>> {
    if fs::read_to_string(src).is_ok_and(|s| !s.is_empty()) {
        if let Some(parent) = Path::new(dst).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::copy(src, dst)?;
    }
    Ok(())
}

fn dir_has_files(dir: &str) -> bool {
    Path::new(dir).is_dir() && fs::read_dir(dir).map(|mut d| d.next().is_some()).unwrap_or(false)
}

fn copy_tree(src: &str, dst: &str) -> Result<(), Box<dyn std::error::Error>> {
    for entry in fs::read_dir(src)?.flatten() {
        let target = format!("{dst}/{}", entry.file_name().to_string_lossy());
        let p = entry.path();
        if p.is_dir() {
            fs::create_dir_all(&target)?;
            copy_tree(&p.to_string_lossy(), &target)?;
        } else if p.is_symlink() {
            let _ = fs::remove_file(&target);
            symlink(fs::read_link(&p)?, &target)?;
        } else {
            fs::copy(&p, &target)?;
        }
    }
    Ok(())
}

fn is_executable(path: &Path) -> bool {
    fs::metadata(path)
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
