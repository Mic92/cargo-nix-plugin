use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::config::BuildConfig;
use super::util::{echo_colored, run_cmd};

/// Build script output parsed from cargo:rustc-* directives.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuildScriptOutputs {
    pub rustc_flags: String,
    pub cfgs: Vec<String>,
    pub link_args: Vec<String>,
    pub link_args_bins: Vec<String>,
    pub link_args_lib: Vec<String>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub cdylib_link_args: Vec<String>,
    pub envs: BTreeMap<String, String>,
    pub build_out_dir: String,
}

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    // cd to workspace member
    match &config.workspace_member {
        Some(m) if m != "." && !m.is_empty() => {
            echo_colored(&format!("Changing directory to {m}"));
            std::env::set_current_dir(m)?;
        }
        None => {
            echo_colored(&format!("Searching for matching Cargo.toml ({})", config.crate_name));
            std::env::set_current_dir(find_matching_cargo_toml(&config.crate_name)?)?;
        }
        _ => {}
    }

    // Auto-detect edition and proc-macro from Cargo.toml when not
    // provided via extraRustcOpts. Replaces the deleted read-crate-info.
    detect_cargo_toml_info(config);

    for dir in &["target/deps", "target/lib", "target/build", "target/buildDeps"] {
        fs::create_dir_all(dir)?;
    }

    // Symlink deps, collect link flags
    let mut link = BTreeSet::from_iter(config.extra_link_flags.iter().cloned());
    let mut link_final = link.clone();
    let mut build_link = BTreeSet::new();

    for path in &config.complete_deps {
        let lib = format!("{path}/lib");
        symlink_libs(&lib, "target/deps")?;
        source_env(path)?;
        collect_link_flags(&lib, &mut link, &mut link_final)?;
    }
    for path in &config.complete_build_deps {
        let lib = format!("{path}/lib");
        symlink_libs(&lib, "target/buildDeps")?;
        source_env(path)?;
        if let Ok(c) = fs::read_to_string(format!("{lib}/link")) {
            build_link.extend(c.lines().filter(|l| !l.is_empty()).map(String::from));
        }
    }

    write_flags("target/link", &link)?;
    write_flags("target/link.final", &link_final)?;
    if !build_link.is_empty() {
        write_flags("target/link.build", &build_link)?;
    }
    fs::write("target/link_", link.into_iter().collect::<Vec<_>>().join(" "))?;

    // Build script
    let build_script = if !config.build.is_empty() {
        Some(config.build.clone())
    } else if Path::new("build.rs").exists() {
        Some("build.rs".into())
    } else {
        None
    };

    if let Some(script) = build_script {
        echo_colored(&format!("Building {script} ({})", config.lib_name));

        let build_dir = format!("target/build/{}", config.crate_name);
        let out_dir = format!("target/build/{}.out", config.crate_name);
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&out_dir)?;

        // CARGO_PKG_* env vars needed at compile time for env!() macros
        let env = build_env(config, &out_dir);

        // Compile build script
        let mut cmd = Command::new("rustc");
        cmd.envs(&env);
        cmd.arg("--crate-name").arg("build_script_build")
            .arg(&script).arg("--crate-type").arg("bin")
            .arg("--out-dir").arg(&build_dir)
            .arg("--emit=dep-info,link")
            .arg("-L").arg("dependency=target/buildDeps")
            .arg("--cap-lints").arg(&config.cap_lints);

        // Build script rustc opts
        if config.release {
            cmd.args(["-C", "opt-level=3"]);
        } else {
            cmd.args(["-C", "debuginfo=2"]);
        }
        cmd.args(["-C", &format!("codegen-units={}", config.codegen_units)]);
        for o in &config.extra_rustc_opts_for_build_rs {
            cmd.arg(o);
        }
        for f in &config.crate_features {
            cmd.arg("--cfg").arg(format!("feature=\"{f}\""));
        }
        // Build dep --extern flags from Nix eval-time info
        for dep in &config.build_dep_externs {
            let path = super::rustc::find_by_metadata("target/buildDeps", &dep.metadata, dep.is_rlib, &config.host_platform.lib_ext)
                .unwrap_or_else(|| {
                    let ext = if dep.is_rlib { "rlib" } else { &config.host_platform.lib_ext };
                    format!("target/buildDeps/lib{}-{}.{ext}", dep.extern_name, dep.metadata)
                });
            cmd.arg("--extern").arg(format!("{}={path}", dep.extern_name));
        }
        if let Ok(flags) = fs::read_to_string("target/link.build") {
            for f in flags.split_whitespace() {
                cmd.arg(f);
            }
        }
        cmd.arg("--color").arg("auto");
        run_cmd(&mut cmd, config.verbose)?;

        // Run build script
        let abs_out_dir = fs::canonicalize(&out_dir)?.to_string_lossy().into_owned();
        let env = build_env(config, &abs_out_dir);
        let mut cmd = Command::new(format!("{build_dir}/build_script_build"));
        cmd.env("RUST_BACKTRACE", "1");
        cmd.envs(&env);
        for f in &config.crate_features {
            cmd.env(format!("CARGO_FEATURE_{}", f.replace('-', "_").to_uppercase()), "1");
        }

        if config.verbose {
            super::util::echo_cmd(&cmd);
        }
        let output = cmd.output()?;
        if !output.status.success() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
            std::process::exit(output.status.code().unwrap_or(1));
        }
        let stdout = String::from_utf8_lossy(&output.stdout);
        print!("{stdout}");
        if !output.stderr.is_empty() {
            eprint!("{}", String::from_utf8_lossy(&output.stderr));
        }

        let abs_out_dir = fs::canonicalize(&out_dir)?.to_string_lossy().into_owned();
        let bso = parse_build_script_output(&stdout, &abs_out_dir);
        fs::write("target/build-script-outputs.json", serde_json::to_string_pretty(&bso)?)?;
        write_dep_env_file(config, &stdout)?;
    }

    Ok(())
}

pub fn build_env(config: &BuildConfig, out_dir: &str) -> BTreeMap<String, String> {
    let hp = &config.host_platform;
    let (major, minor, patch, pre) = parse_version(&config.crate_version);
    let cwd = std::env::current_dir().unwrap().to_string_lossy().into_owned();

    BTreeMap::from([
        ("CARGO_PKG_NAME".into(), config.crate_name.clone()),
        ("CARGO_PKG_VERSION".into(), config.crate_version.clone()),
        ("CARGO_PKG_AUTHORS".into(), config.crate_authors.join(":")),
        ("CARGO_PKG_DESCRIPTION".into(), config.crate_description.clone()),
        ("CARGO_PKG_HOMEPAGE".into(), config.crate_homepage.clone()),
        ("CARGO_PKG_LICENSE".into(), config.crate_license.clone()),
        ("CARGO_PKG_LICENSE_FILE".into(), config.crate_license_file.clone()),
        ("CARGO_PKG_README".into(), config.crate_readme.clone()),
        ("CARGO_PKG_REPOSITORY".into(), config.crate_repository.clone()),
        ("CARGO_PKG_RUST_VERSION".into(), config.crate_rust_version.clone()),
        ("CARGO_PKG_VERSION_MAJOR".into(), major),
        ("CARGO_PKG_VERSION_MINOR".into(), minor),
        ("CARGO_PKG_VERSION_PATCH".into(), patch),
        ("CARGO_PKG_VERSION_PRE".into(), pre),
        ("CARGO_CFG_TARGET_ARCH".into(), hp.arch.clone()),
        ("CARGO_CFG_TARGET_OS".into(), hp.os.clone()),
        ("CARGO_CFG_TARGET_FAMILY".into(), "unix".into()),
        ("CARGO_CFG_UNIX".into(), "1".into()),
        ("CARGO_CFG_TARGET_ENV".into(), hp.abi.clone()),
        ("CARGO_CFG_TARGET_ENDIAN".into(), hp.endian.clone()),
        ("CARGO_CFG_TARGET_POINTER_WIDTH".into(), hp.pointer_width.to_string()),
        ("CARGO_CFG_TARGET_VENDOR".into(), hp.vendor.clone()),
        ("CARGO_MANIFEST_DIR".into(), cwd),
        ("CARGO_MANIFEST_LINKS".into(), config.crate_links.clone()),
        ("DEBUG".into(), (!config.release).to_string()),
        ("OPT_LEVEL".into(), if config.release { "3" } else { "0" }.into()),
        ("TARGET".into(), hp.rustc_target_spec.clone()),
        ("HOST".into(), config.build_platform.rustc_target_spec.clone()),
        ("PROFILE".into(), if config.release { "release" } else { "debug" }.into()),
        ("OUT_DIR".into(), out_dir.into()),
        ("NUM_JOBS".into(), std::env::var("NIX_BUILD_CORES").unwrap_or_else(|_| "1".into())),
        ("RUSTC".into(), "rustc".into()),
        ("RUSTDOC".into(), "rustdoc".into()),
        // Cargo always sets this; build scripts like rav1e's unwrap() it.
        // buildRustCrate passes flags via CLI, so the value is empty.
        // Encode the same base rustc flags that RustcFlags uses, so build
        // scripts see consistent values via env!() / std::env::var().
        ("CARGO_ENCODED_RUSTFLAGS".into(), super::rustc::encode_rustflags(config)),
        ("CARGO_CRATE_NAME".into(), config.lib_name.replace('-', "_")),
    ])
}

fn parse_version(v: &str) -> (String, String, String, String) {
    let (ver, pre) = v.split_once('-').map(|(a, b)| (a, b.into())).unwrap_or((v, String::new()));
    let p: Vec<&str> = ver.split('.').collect();
    (
        p.first().unwrap_or(&"0").to_string(),
        p.get(1).unwrap_or(&"0").to_string(),
        p.get(2).unwrap_or(&"0").to_string(),
        pre,
    )
}

fn parse_build_script_output(stdout: &str, out_dir: &str) -> BuildScriptOutputs {
    let mut bso = BuildScriptOutputs { build_out_dir: out_dir.into(), ..Default::default() };
    let mut rustc_flags = BTreeSet::new();
    let mut link_search = BTreeSet::new();

    for line in stdout.lines() {
        let d = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"));
        let Some(d) = d else { continue };

        if let Some(v) = d.strip_prefix("rustc-flags=") { rustc_flags.insert(v.to_string()); }
        else if let Some(v) = d.strip_prefix("rustc-cfg=") { bso.cfgs.push(v.into()); }
        else if let Some(v) = d.strip_prefix("rustc-link-arg=") { bso.link_args.extend_from_slice(&["-C".into(), format!("link-arg={v}")]); }
        else if let Some(v) = d.strip_prefix("rustc-link-arg-bins=") { bso.link_args_bins.extend_from_slice(&["-C".into(), format!("link-arg={v}")]); }
        else if let Some(v) = d.strip_prefix("rustc-link-arg-lib=") { bso.link_args_lib.extend_from_slice(&["-C".into(), format!("link-arg={v}")]); }
        else if let Some(v) = d.strip_prefix("rustc-link-lib=") { bso.link_libs.push(v.into()); }
        else if let Some(v) = d.strip_prefix("rustc-link-search=") {
            // Cargo resolves relative link-search paths against the
            // package root. Make them absolute so they work regardless
            // of cwd during the build phase.
            let resolved = match v.split_once('=') {
                Some((kind, path)) if !path.starts_with('/') => {
                    let abs = std::env::current_dir().unwrap().join(path);
                    format!("{kind}={}", abs.display())
                }
                None if !v.starts_with('/') => {
                    std::env::current_dir().unwrap().join(v).to_string_lossy().into_owned()
                }
                _ => v.to_string(),
            };
            link_search.insert(resolved);
        }
        else if let Some(v) = d.strip_prefix("rustc-cdylib-link-arg=") { bso.cdylib_link_args.extend_from_slice(&["-C".into(), format!("link-arg={v}")]); }
        else if let Some(v) = d.strip_prefix("rustc-env=") {
            if let Some((k, val)) = v.split_once('=') { bso.envs.insert(k.into(), val.into()); }
        }
    }
    bso.rustc_flags = rustc_flags.into_iter().collect::<Vec<_>>().join(" ");
    bso.link_search = link_search.into_iter().collect();
    bso
}

fn write_dep_env_file(config: &BuildConfig, stdout: &str) -> Result<(), Box<dyn std::error::Error>> {
    let crate_upper = config.crate_name.replace("-sys", "").replace('-', "_").to_uppercase();
    let ver_safe = config.crate_version.replace(['.', '+', '-'], "_");
    let mut lines = Vec::new();

    for line in stdout.lines() {
        if let Some(rest) = line.strip_prefix("cargo::metadata=") {
            if let Some((k, v)) = rest.split_once('=') {
                lines.push(format!("export DEP_{crate_upper}_{}=\"{v}\"", k.replace('-', "_").to_uppercase()));
            }
            continue;
        }
        let d = line.strip_prefix("cargo::").or_else(|| line.strip_prefix("cargo:"));
        let Some(d) = d else { continue };
        if d.starts_with("rustc-") || d.starts_with("warning=") || d.starts_with("rerun-if-") { continue; }
        if let Some((k, v)) = d.split_once('=') {
            let key = k.replace('-', "_").to_uppercase();
            lines.push(format!("export DEP_{crate_upper}_{key}=\"{v}\""));
            lines.push(format!("export DEP_{crate_upper}_{ver_safe}_{key}=\"{v}\""));
        }
    }
    if !lines.is_empty() {
        fs::write("target/env", lines.join("\n"))?;
    }
    Ok(())
}

fn symlink_libs(lib_dir: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(lib_dir);
    if !dir.exists() { return Ok(()); }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name.ends_with(".rlib") || name.ends_with(".so") || name.ends_with(".dylib") {
            let dst = Path::new(target).join(path.file_name().unwrap());
            let _ = fs::remove_file(&dst);
            symlink(&path, &dst)?;
        }
    }
    Ok(())
}

fn source_env(dep_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let env_path = PathBuf::from(dep_path).join("env");
    if let Ok(content) = fs::read_to_string(&env_path) {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("export ") {
                if let Some((k, v)) = rest.split_once('=') {
                    std::env::set_var(k, v.trim_matches('"'));
                }
            }
        }
    }
    Ok(())
}

fn collect_link_flags(lib_dir: &str, link: &mut BTreeSet<String>, link_final: &mut BTreeSet<String>) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(content) = fs::read_to_string(format!("{lib_dir}/link")) {
        for line in content.lines().filter(|l| !l.is_empty()) {
            link.insert(line.into());
            link_final.insert(line.into());
        }
    }
    Ok(())
}

fn write_flags(path: &str, flags: &BTreeSet<String>) -> Result<(), Box<dyn std::error::Error>> {
    let mut content = flags.iter().cloned().collect::<Vec<_>>().join("\n");
    if !content.is_empty() {
        content.push('\n');
    }
    fs::write(path, content)?;
    Ok(())
}

fn find_matching_cargo_toml(crate_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    for entry in walk_files(Path::new("."))? {
        if entry.file_name() != Some(std::ffi::OsStr::new("Cargo.toml")) { continue; }
        let Ok(content) = fs::read_to_string(&entry) else { continue };
        let Ok(doc) = toml::from_str::<toml::Value>(&content) else { continue };
        let pkg = doc.get("package");
        if let Some(name) = pkg.and_then(|p| p.get("name")).and_then(|n| n.as_str()) {
            if name == crate_name {
                return Ok(entry.parent().unwrap().to_string_lossy().into_owned());
            }
        }
        // workspace inheritance: name = { workspace = true }
        if let Some(name_val) = pkg.and_then(|p| p.get("name")) {
            if name_val.is_table() {
                if let Some(ws_name) = find_workspace_name(&entry) {
                    if ws_name == crate_name {
                        return Ok(entry.parent().unwrap().to_string_lossy().into_owned());
                    }
                }
            }
        }
    }
    Err(format!("No matching Cargo.toml found for {crate_name}").into())
}

fn walk_files(dir: &Path) -> Result<Vec<PathBuf>, std::io::Error> {
    let mut out = Vec::new();
    if !dir.is_dir() { return Ok(out); }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() { out.extend(walk_files(&path)?); } else { out.push(path); }
    }
    Ok(out)
}

/// Auto-detect edition and proc-macro from Cargo.toml when not already
/// provided via extraRustcOpts. This replaces the deleted read-crate-info
/// binary that the offline mode relied on.
pub fn detect_cargo_toml_info(config: &mut BuildConfig) {
    if !Path::new("Cargo.toml").exists() {
        return;
    }

    let Ok(content) = fs::read_to_string("Cargo.toml") else { return };
    let Ok(doc) = toml::from_str::<toml::Value>(&content) else { return };

    // Auto-detect edition if not already in extraRustcOpts
    if !config.extra_rustc_opts.iter().any(|o| o == "--edition") {
        let edition = doc
            .get("package")
            .and_then(|p| p.get("edition"))
            .and_then(|v| {
                v.as_str()
                    .map(String::from)
                    .or_else(|| v.as_integer().map(|i| i.to_string()))
            });
        if let Some(ed) = edition {
            config.extra_rustc_opts.extend_from_slice(&["--edition".into(), ed.clone()]);
            config.extra_rustc_opts_for_build_rs.extend_from_slice(&["--edition".into(), ed]);
        }
    }

    // Auto-detect proc-macro
    let is_proc_macro = doc
        .get("lib")
        .and_then(|l| l.get("proc-macro").or_else(|| l.get("proc_macro")))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if is_proc_macro {
        if let Some(pos) = config.crate_type.iter().position(|t| t == "lib") {
            config.crate_type[pos] = "proc-macro".into();
        }
        if !config.extra_rustc_opts.iter().any(|o| o == "proc_macro") {
            config.extra_rustc_opts.extend_from_slice(&["--extern".into(), "proc_macro".into()]);
        }
    }

    // Populate crate_bin from [[bin]] entries so required-features filtering
    // works. Without this, resolve_bins() auto-discovers src/bin/*.rs and
    // skips required-features checks.
    if config.crate_bin.is_empty() {
        if let Some(bins) = doc.get("bin").and_then(|v| v.as_array()) {
            for bin in bins {
                let name = bin.get("name").and_then(|v| v.as_str()).map(String::from);
                let path = bin.get("path").and_then(|v| v.as_str()).map(String::from);
                let required_features = bin
                    .get("required-features")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                    .unwrap_or_default();
                config.crate_bin.push(super::config::CrateBin {
                    name,
                    path,
                    required_features,
                });
            }
            if !config.crate_bin.is_empty() {
                config.has_crate_bin = true;
            }
        }
    }
}

fn find_workspace_name(cargo_toml: &Path) -> Option<String> {
    let mut dir = cargo_toml.parent()?;
    loop {
        dir = dir.parent()?;
        let ws = dir.join("Cargo.toml");
        if ws.exists() {
            let doc: toml::Value = toml::from_str(&fs::read_to_string(&ws).ok()?).ok()?;
            return doc.get("workspace")?.get("package")?.get("name")?.as_str().map(String::from);
        }
        if dir == Path::new("/") || dir == Path::new("") { break; }
    }
    None
}
