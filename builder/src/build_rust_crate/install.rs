use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use super::config::{BuildConfig, CrateMetadata};
use super::configure::enter_crate_root;

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    enter_crate_root(config)?;

    let metadata = &config.metadata;
    let out = config.out_path();

    if config.build_tests {
        return install_tests(config);
    }

    let lib_out = config.lib_path_output().unwrap_or(out);
    fs::create_dir_all(out)?;
    fs::create_dir_all(lib_out)?;

    // Legacy DEP_* env file (kept for crateOverrides that sed/read it).
    copy_if_nonempty("target/env", &format!("{lib_out}/env"))?;

    // Copy link flags for downstream crates
    copy_if_nonempty("target/link.final", &format!("{lib_out}/lib/link"))?;

    // Collect lib artifact filenames for crate-metadata.json. Anything with
    // the metadata-hash suffix that rustc emitted under target/lib.
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

    // Canonical machine-readable manifest. Dependents read this for
    // --extern name/path and DEP_* env; the legacy text/shell files above
    // are kept only for override compatibility.
    let links_vars: std::collections::BTreeMap<String, String> =
        fs::read_to_string("target/links-vars.json")
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
    let cm = CrateMetadata {
        lib_name: config.lib_name_normalized(),
        metadata: metadata.clone(),
        crate_types: config.crate_type.clone(),
        proc_macro: config.crate_type.iter().any(|t| t == "proc-macro"),
        artifacts,
        links: config.crate_links.clone(),
        links_vars,
    };
    fs::create_dir_all(lib_out)?;
    fs::write(
        format!("{lib_out}/crate-metadata.json"),
        serde_json::to_string_pretty(&cm)?,
    )?;

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
    let dst = format!("{}/tests", config.out_path());
    fs::create_dir_all(&dst)?;

    for dir in ["target/bin", "target/lib"] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap().to_string_lossy();
            // Skip non-test artifacts that share target/lib.
            let is_lib = matches!(
                p.extension().and_then(|e| e.to_str()),
                Some("rlib" | "so" | "dylib" | "d")
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
    if Path::new(src).exists()
        && fs::read_to_string(src)
            .map(|s| !s.is_empty())
            .unwrap_or(false)
    {
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
