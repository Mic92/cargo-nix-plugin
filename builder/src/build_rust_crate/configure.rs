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
    #[serde(default)]
    pub check_cfgs: Vec<String>,
    pub link_args: Vec<String>,
    pub link_args_bins: Vec<String>,
    pub link_args_lib: Vec<String>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub cdylib_link_args: Vec<String>,
    pub envs: BTreeMap<String, String>,
    pub build_out_dir: String,
}

/// Change cwd into the crate's source root and patch `config` from the
/// unpacked Cargo.toml. Runs at the top of every phase: each phase is a
/// separate `build-rust-crate <phase>` process, so the configure phase's
/// chdir/detect does not survive.
pub fn enter_crate_root(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    match &config.workspace_member {
        Some(m) if m != "." && !m.is_empty() => {
            echo_colored(&format!("Changing directory to {m}"));
            std::env::set_current_dir(m)?;
        }
        None => {
            echo_colored(&format!(
                "Searching for matching Cargo.toml ({})",
                config.crate_name
            ));
            std::env::set_current_dir(find_matching_cargo_toml(&config.crate_name)?)?;
        }
        _ => {}
    }
    detect_cargo_toml_info(config);
    Ok(())
}

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    enter_crate_root(config)?;

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
    fs::write(
        "target/link_",
        link.into_iter().collect::<Vec<_>>().join(" "),
    )?;

    let build_script = match config.build.as_str() {
        "false" => None,
        "" if Path::new("build.rs").exists() => Some("build.rs".to_string()),
        "" => None,
        path => Some(path.to_string()),
    };

    if let Some(script) = build_script {
        echo_colored(&format!("Building {script} ({})", config.lib_name));

        let build_dir = format!("target/build/{}", config.crate_name);
        let out_dir = format!("target/build/{}.out", config.crate_name);
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&out_dir)?;

        let env = build_env(config, &out_dir);

        // Compile build script
        let mut cmd = Command::new("rustc");
        cmd.envs(&env);
        cmd.arg("--crate-name")
            .arg("build_script_build")
            .arg(&script)
            .arg("--crate-type")
            .arg("bin")
            .arg("--out-dir")
            .arg(&build_dir)
            .arg("--emit=dep-info,link")
            .arg("-L")
            .arg("dependency=target/buildDeps")
            .arg("--cap-lints")
            .arg(&config.cap_lints);

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
        cmd.args(super::rustc::dep_extern_args(
            &config.build_dep_externs,
            "target/buildDeps",
            &config.build_platform.lib_ext,
        ));
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
            cmd.env(
                format!("CARGO_FEATURE_{}", f.replace('-', "_").to_uppercase()),
                "1",
            );
        }

        if config.verbose {
            super::util::echo_cmd(&cmd);
        }
        // Stream stderr; only stdout carries `cargo:` directives.
        cmd.stderr(std::process::Stdio::inherit());
        cmd.stdout(std::process::Stdio::piped());
        let mut child = cmd.spawn()?;
        let mut stdout = String::new();
        std::io::Read::read_to_string(
            &mut child.stdout.take().expect("piped stdout"),
            &mut stdout,
        )?;
        let status = child.wait()?;
        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }
        print!("{stdout}");

        let abs_out_dir = fs::canonicalize(&out_dir)?.to_string_lossy().into_owned();
        let bso = parse_build_script_output(&stdout, &abs_out_dir);
        fs::write(
            "target/build-script-outputs.json",
            serde_json::to_string_pretty(&bso)?,
        )?;
        write_dep_env_file(config, &stdout)?;
    }

    Ok(())
}

pub fn build_env(config: &BuildConfig, out_dir: &str) -> BTreeMap<String, String> {
    let hp = &config.host_platform;
    let (major, minor, patch, pre) = parse_version(&config.crate_version);
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    BTreeMap::from([
        ("CARGO_PKG_NAME".into(), config.crate_name.clone()),
        ("CARGO_PKG_VERSION".into(), config.crate_version.clone()),
        (
            "CARGO_PKG_AUTHORS".into(),
            config.crate_authors.join(":"),
        ),
        (
            "CARGO_PKG_DESCRIPTION".into(),
            config.crate_description.clone(),
        ),
        ("CARGO_PKG_HOMEPAGE".into(), config.crate_homepage.clone()),
        ("CARGO_PKG_LICENSE".into(), config.crate_license.clone()),
        (
            "CARGO_PKG_LICENSE_FILE".into(),
            config.crate_license_file.clone(),
        ),
        ("CARGO_PKG_README".into(), config.crate_readme.clone()),
        (
            "CARGO_PKG_REPOSITORY".into(),
            config.crate_repository.clone(),
        ),
        (
            "CARGO_PKG_RUST_VERSION".into(),
            config.crate_rust_version.clone(),
        ),
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
        (
            "CARGO_CFG_TARGET_POINTER_WIDTH".into(),
            hp.pointer_width.to_string(),
        ),
        ("CARGO_CFG_TARGET_VENDOR".into(), hp.vendor.clone()),
        (
            "CARGO_CFG_TARGET_FEATURE".into(),
            target_cfg_values(config, "target_feature").join(","),
        ),
        (
            "CARGO_CFG_TARGET_HAS_ATOMIC".into(),
            target_cfg_values(config, "target_has_atomic").join(","),
        ),
        ("CARGO_MANIFEST_DIR".into(), cwd),
        ("CARGO_MANIFEST_LINKS".into(), config.crate_links.clone()),
        ("DEBUG".into(), (!config.release).to_string()),
        (
            "OPT_LEVEL".into(),
            if config.release { "3" } else { "0" }.into(),
        ),
        ("TARGET".into(), hp.rustc_target_spec.clone()),
        (
            "HOST".into(),
            config.build_platform.rustc_target_spec.clone(),
        ),
        (
            "PROFILE".into(),
            if config.release { "release" } else { "debug" }.into(),
        ),
        ("OUT_DIR".into(), out_dir.into()),
        (
            "NUM_JOBS".into(),
            std::env::var("NIX_BUILD_CORES").unwrap_or_else(|_| "1".into()),
        ),
        ("RUSTC".into(), "rustc".into()),
        ("RUSTDOC".into(), "rustdoc".into()),
        (
            "CARGO_ENCODED_RUSTFLAGS".into(),
            super::rustc::encode_rustflags(config),
        ),
        (
            "CARGO_CRATE_NAME".into(),
            config.lib_name.replace('-', "_"),
        ),
    ])
}

fn parse_version(v: &str) -> (String, String, String, String) {
    let v = v.split_once('+').map_or(v, |(a, _)| a);
    let (ver, pre) = v.split_once('-').unwrap_or((v, ""));
    let mut p = ver.splitn(3, '.');
    (
        p.next().unwrap_or("0").into(),
        p.next().unwrap_or("0").into(),
        p.next().unwrap_or("0").into(),
        pre.into(),
    )
}

/// All values of one `name="value"` cfg from `rustc --print cfg`, memoized.
fn target_cfg_values(config: &BuildConfig, name: &str) -> Vec<String> {
    use std::sync::OnceLock;
    static CFG: OnceLock<String> = OnceLock::new();
    let out = CFG.get_or_init(|| {
        Command::new("rustc")
            .arg("--print=cfg")
            .arg("--target")
            .arg(&config.host_platform.rustc_target_spec)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    });
    let prefix = format!("{name}=\"");
    out.lines()
        .filter_map(|l| l.strip_prefix(&prefix)?.strip_suffix('"'))
        .map(String::from)
        .collect()
}

fn parse_build_script_output(stdout: &str, out_dir: &str) -> BuildScriptOutputs {
    let mut bso = BuildScriptOutputs {
        build_out_dir: out_dir.into(),
        ..Default::default()
    };
    // Linker flags are position-sensitive; preserve emission order.
    let mut rustc_flags: Vec<String> = Vec::new();
    let mut link_search: Vec<String> = Vec::new();

    for line in stdout.lines() {
        let d = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"));
        let Some(d) = d else { continue };

        if let Some(v) = d.strip_prefix("rustc-flags=") {
            rustc_flags.push(v.to_string());
        } else if let Some(v) = d.strip_prefix("rustc-check-cfg=") {
            bso.check_cfgs.push(v.into());
        } else if let Some(v) = d.strip_prefix("rustc-cfg=") {
            bso.cfgs.push(v.into());
        } else if let Some(v) = d.strip_prefix("rustc-link-arg=") {
            bso.link_args
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-bins=") {
            bso.link_args_bins
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-lib=") {
            bso.link_args_lib
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-link-lib=") {
            bso.link_libs.push(v.into());
        } else if let Some(v) = d.strip_prefix("rustc-link-search=") {
            // Resolve relative paths against the package root
            let resolved = match v.split_once('=') {
                Some((kind, path)) if !path.starts_with('/') => {
                    let abs = std::env::current_dir().unwrap().join(path);
                    format!("{kind}={}", abs.display())
                }
                None if !v.starts_with('/') => std::env::current_dir()
                    .unwrap()
                    .join(v)
                    .to_string_lossy()
                    .into_owned(),
                _ => v.to_string(),
            };
            if !link_search.contains(&resolved) {
                link_search.push(resolved);
            }
        } else if let Some(v) = d.strip_prefix("rustc-cdylib-link-arg=") {
            bso.cdylib_link_args
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-env=") {
            if let Some((k, val)) = v.split_once('=') {
                bso.envs.insert(k.into(), val.into());
            }
        } else if let Some(msg) = d.strip_prefix("warning=") {
            eprintln!("\x1b[0;1;33mwarning\x1b[0m: {msg}");
        } else if let Some(msg) = d.strip_prefix("error=") {
            eprintln!("\x1b[0;1;31merror\x1b[0m: {msg}");
            std::process::exit(1);
        }
    }
    bso.rustc_flags = rustc_flags.join(" ");
    bso.link_search = link_search;
    bso
}

fn write_dep_env_file(
    config: &BuildConfig,
    stdout: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // Cargo only emits DEP_* when the crate declares `links`.
    if config.crate_links.is_empty() {
        return Ok(());
    }
    let links_upper = config.crate_links.replace('-', "_").to_uppercase();
    let ver_safe = config.crate_version.replace(['.', '+', '-'], "_");
    let mut lines = Vec::new();

    for line in stdout.lines() {
        // Handle cargo::metadata= (new-style)
        if let Some(rest) = line.strip_prefix("cargo::metadata=") {
            if let Some((k, v)) = rest.split_once('=') {
                lines.push(format!(
                    "export DEP_{links_upper}_{}=\"{v}\"",
                    k.replace('-', "_").to_uppercase()
                ));
            }
            continue;
        }
        let d = line
            .strip_prefix("cargo::")
            .or_else(|| line.strip_prefix("cargo:"));
        let Some(d) = d else { continue };
        if d.starts_with("rustc-")
            || d.starts_with("warning=")
            || d.starts_with("rerun-if-")
        {
            continue;
        }
        if let Some((k, v)) = d.split_once('=') {
            let key = k.replace('-', "_").to_uppercase();
            lines.push(format!("export DEP_{links_upper}_{key}=\"{v}\""));
            lines.push(format!(
                "export DEP_{links_upper}_{ver_safe}_{key}=\"{v}\""
            ));
        }
    }
    if !lines.is_empty() {
        fs::write("target/env", lines.join("\n"))?;
    }
    Ok(())
}

fn symlink_libs(lib_dir: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let dir = Path::new(lib_dir);
    if !dir.exists() {
        return Ok(());
    }
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

fn collect_link_flags(
    lib_dir: &str,
    link: &mut BTreeSet<String>,
    link_final: &mut BTreeSet<String>,
) -> Result<(), Box<dyn std::error::Error>> {
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
        if entry.file_name() != Some(std::ffi::OsStr::new("Cargo.toml")) {
            continue;
        }
        let Ok(content) = fs::read_to_string(&entry) else {
            continue;
        };
        let Ok(doc) = toml::from_str::<toml::Value>(&content) else {
            continue;
        };
        if let Some(name) = doc
            .get("package")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
        {
            if name == crate_name {
                return Ok(entry.parent().unwrap().to_string_lossy().into_owned());
            }
        }
        // workspace inheritance: name = { workspace = true }
        if let Some(name_val) = doc.get("package").and_then(|p| p.get("name")) {
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
    if !dir.is_dir() {
        return Ok(out);
    }
    for entry in fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            out.extend(walk_files(&path)?);
        } else {
            out.push(path);
        }
    }
    Ok(out)
}

/// Auto-detect crate metadata from Cargo.toml when the resolver could not
/// supply it at eval time. Replaces the old read-crate-info binary.
///
/// In lockfile-resolve mode the sparse index carries no `[lib]` table, so for
/// registry crates `libPath` / `libName` / `crateType` / `procMacro` arrive as
/// their derivation defaults. Those defaults are wrong for crates like fnv
/// (`path = "lib.rs"`), new_debug_unreachable (`name = "debug_unreachable"`)
/// or anything with `crate-type = ["cdylib","rlib"]`. We learn the truth here
/// from the unpacked source and overwrite the config in place so every phase
/// sees consistent values.
pub fn detect_cargo_toml_info(config: &mut BuildConfig) {
    if !Path::new("Cargo.toml").exists() {
        return;
    }

    let Ok(content) = fs::read_to_string("Cargo.toml") else {
        return;
    };
    let Ok(doc) = toml::from_str::<toml::Value>(&content) else {
        return;
    };

    // Auto-detect edition
    let has_edition =
        |opts: &[String]| opts.iter().any(|o| o == "--edition" || o.starts_with("--edition="));
    if !has_edition(&config.extra_rustc_opts) {
        let edition = doc.get("package").and_then(|p| p.get("edition")).and_then(|v| {
            v.as_str()
                .map(String::from)
                .or_else(|| v.as_integer().map(|i| i.to_string()))
        });
        if let Some(ed) = edition {
            config
                .extra_rustc_opts
                .extend_from_slice(&["--edition".into(), ed.clone()]);
            config
                .extra_rustc_opts_for_build_rs
                .extend_from_slice(&["--edition".into(), ed]);
        }
    }

    if config.build.is_empty() {
        match doc.get("package").and_then(|p| p.get("build")) {
            Some(v) if v.as_bool() == Some(false) => config.build = "false".into(),
            Some(v) => {
                if let Some(p) = v.as_str() {
                    if Path::new(p).exists() {
                        config.build = p.to_string();
                    }
                }
            }
            None => {}
        }
    }

    // Fallback for path/git deps; registry crates get this from the index.
    if config.crate_links.is_empty() {
        if let Some(l) = doc
            .get("package")
            .and_then(|p| p.get("links"))
            .and_then(|v| v.as_str())
        {
            config.crate_links = l.to_string();
        }
    }

    // ---- [lib] table -----------------------------------------------------
    let lib = doc.get("lib");

    // lib.path: fnv/fxhash/serde_derive_internals/document-features all use
    // a non-default `path = "lib.rs"`. resolve_lib_path() only falls back to
    // src/lib.rs, so without this the build phase silently emits no rlib and
    // every dependent dies with "extern location for X does not exist".
    if config.lib_path.is_empty() {
        if let Some(p) = lib.and_then(|l| l.get("path")).and_then(|v| v.as_str()) {
            config.lib_path = p.to_string();
        }
    }

    // lib.name: new_debug_unreachable→debug_unreachable, rustls-webpki→webpki,
    // utf-8→utf8. The drv defaults libName to crateName (hyphens kept), so we
    // treat that as "unset" and consult Cargo.toml. The dependent recovers the
    // real `--extern` key from the artifact filename (see dep_extern_args).
    if config.lib_name.is_empty() || config.lib_name == config.crate_name {
        config.lib_name = lib
            .and_then(|l| l.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| config.crate_name.replace('-', "_"));
    }

    // lib.crate-type / proc-macro: only rewrite the eval-time default ["lib"];
    // an explicit value from the resolver or a crateOverride wins.
    if config.crate_type == ["lib"] {
        let is_proc_macro = lib
            .and_then(|l| l.get("proc-macro").or_else(|| l.get("proc_macro")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_proc_macro {
            // RustcFlags::new adds `--extern proc_macro` based on crate_type;
            // no need to also push it into extra_rustc_opts.
            config.crate_type = vec!["proc-macro".into()];
        } else if let Some(types) = lib
            .and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type")))
            .and_then(|v| v.as_array())
        {
            let mut ts: Vec<String> = types
                .iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect();
            // Registry crates are only ever built here as dependencies, so the
            // dependent needs an rlib regardless of what the manifest declares
            // (cargo does the same implicit promotion). Without this a
            // `crate-type = ["cdylib"]` crate would emit only a .so and the
            // consumer's `--extern` would point at a non-rmeta artifact.
            if !ts.iter().any(|t| t == "lib" || t == "rlib" || t == "proc-macro") {
                ts.push("rlib".into());
            }
            if !ts.is_empty() {
                config.crate_type = ts;
            }
        }
    }

    // Populate crate_bin from [[bin]] entries for required-features filtering.
    // Only when the drv didn't set crateBin at all: an explicit `crateBin = []`
    // (has_crate_bin = true) is how lib/default.nix suppresses bins on the
    // lib-only dep variant, and that must not be undone here.
    if !config.has_crate_bin && config.crate_bin.is_empty() {
        if let Some(bins) = doc.get("bin").and_then(|v| v.as_array()) {
            for bin in bins {
                let name = bin.get("name").and_then(|v| v.as_str()).map(String::from);
                let path = bin.get("path").and_then(|v| v.as_str()).map(String::from);
                let required_features = bin
                    .get("required-features")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
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
            return doc
                .get("workspace")?
                .get("package")?
                .get("name")?
                .as_str()
                .map(String::from);
        }
        if dir == Path::new("/") || dir == Path::new("") {
            break;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_components_strip_build_metadata() {
        assert_eq!(
            parse_version("1.2.3"),
            ("1".into(), "2".into(), "3".into(), "".into())
        );
        assert_eq!(
            parse_version("1.2.3-alpha.1+git.abc"),
            ("1".into(), "2".into(), "3".into(), "alpha.1".into())
        );
        assert_eq!(
            parse_version("0.39.1+e3ba2a3"),
            ("0".into(), "39".into(), "1".into(), "".into())
        );
    }

    #[test]
    fn build_script_output_preserves_flag_order_and_handles_check_cfg() {
        let stdout = "\
cargo:rustc-flags=-l foo\n\
cargo:rustc-flags=-L /a\n\
cargo:rustc-flags=-l bar\n\
cargo::rustc-check-cfg=cfg(has_foo)\n\
cargo:rustc-link-search=native=/out\n\
cargo:rustc-link-search=native=/out\n\
cargo:warning=heads up\n";
        let bso = parse_build_script_output(stdout, "/out");
        assert_eq!(bso.rustc_flags, "-l foo -L /a -l bar");
        assert_eq!(bso.check_cfgs, vec!["cfg(has_foo)"]);
        assert_eq!(bso.link_search, vec!["native=/out"]); // de-duped, order kept
    }
}
