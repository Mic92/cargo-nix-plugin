use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use super::config::BuildConfig;

pub fn run(config: &BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    let metadata = &config.metadata;
    let out = config.out_path();

    if config.build_tests {
        return install_tests(config);
    }

    let lib_out = config.lib_path_output().unwrap_or(out);
    fs::create_dir_all(out)?;

    // Copy env
    copy_if_nonempty("target/env", &format!("{lib_out}/env"))?;

    // Copy link flags for downstream crates
    copy_if_nonempty("target/link.final", &format!("{lib_out}/lib/link"))?;

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
    let lib_ext = &config.host_platform.lib_ext;

    for dir in ["target/bin", "target/lib"] {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            let name = p.file_name().unwrap().to_string_lossy().to_string();
            if !p.is_file() || !is_executable(&p) {
                continue;
            }
            if dir == "target/lib"
                && (name.ends_with(".rlib") || name.ends_with(lib_ext) || name.ends_with(".d"))
            {
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
