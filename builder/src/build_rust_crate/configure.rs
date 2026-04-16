use std::collections::BTreeMap;
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
    #[serde(default)]
    pub link_args_lib: Vec<String>,
    pub link_args_bins: Vec<String>,
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

    // Symlink deps, collect link flags. Each entry is one *line* (may contain
    // spaces, e.g. "-L /nix/store/..."); ordering is preserved and only whole
    // duplicate lines are dropped — token-level reordering would break
    // `extraLinkFlags = [ "-L" "${zlib}/lib" ]`.
    let mut link: Vec<String> = Vec::new();
    if !config.extra_link_flags.is_empty() {
        link.push(config.extra_link_flags.join(" "));
    }
    let mut link_final = link.clone();
    let mut build_link: Vec<String> = Vec::new();

    // DEP_<links>_* env from each dep is `source`d by the stdenv shell
    // (default.nix configurePhase) before this process starts, so it's
    // already in our environment and will be inherited by build_script_build.
    for path in &config.complete_deps {
        let lib = format!("{path}/lib");
        symlink_libs(path, "target/deps")?;
        collect_link_flags(&lib, &mut link, &mut link_final)?;
    }
    for path in &config.complete_build_deps {
        let lib = format!("{path}/lib");
        symlink_libs(path, "target/buildDeps")?;
        if let Ok(c) = fs::read_to_string(format!("{lib}/link")) {
            for l in c.lines().filter(|l| !l.is_empty()) {
                if !build_link.iter().any(|e| e == l) {
                    build_link.push(l.into());
                }
            }
        }
    }

    write_flags("target/link", &link)?;
    write_flags("target/link.final", &link_final)?;
    if !build_link.is_empty() {
        write_flags("target/link.build", &build_link)?;
    }
    fs::write("target/link_", link.join(" "))?;

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
        let abs_out_dir = fs::canonicalize(&out_dir)?.to_string_lossy().into_owned();
        let env = build_env(config, &abs_out_dir);

        // Compile build script. CARGO_PKG_* / CARGO_MANIFEST_DIR are needed at
        // *compile* time too: build.rs commonly does `env!("CARGO_PKG_NAME")`.
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
        ));
        if let Ok(flags) = fs::read_to_string("target/link.build") {
            for f in flags.split_whitespace() {
                cmd.arg(f);
            }
        }
        cmd.arg("--color").arg(&config.colors);
        run_cmd(&mut cmd, config.verbose)?;

        // Run build script
        let mut cmd = Command::new(format!("{build_dir}/build_script_build"));
        cmd.env_remove("RUSTFLAGS");
        cmd.env("RUST_BACKTRACE", "1");
        cmd.envs(&env);
        cmd.envs(dep_links_env(config));
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

        let bso = parse_build_script_output(&stdout, &abs_out_dir);
        fs::write(
            "target/build-script-outputs.json",
            serde_json::to_string_pretty(&bso)?,
        )?;
        // Persist links metadata for both the JSON manifest (assembled at
        // install time) and the legacy `$lib/env` shell file (overrides like
        // ibverbs-sys still `sed` it in preFixup).
        if !config.crate_links.is_empty() {
            let vars = parse_links_metadata(&stdout);
            if !vars.is_empty() {
                fs::write("target/links-vars.json", serde_json::to_string(&vars)?)?;
                write_legacy_dep_env(config, &vars)?;
            }
        }
    }

    Ok(())
}

pub fn build_env(config: &BuildConfig, out_dir: &str) -> BTreeMap<String, String> {
    let (major, minor, patch, pre) = parse_version(&config.crate_version);
    let cwd = std::env::current_dir()
        .unwrap()
        .to_string_lossy()
        .into_owned();

    let mut env = BTreeMap::from([
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
        ("CARGO_MANIFEST_PATH".into(), format!("{cwd}/Cargo.toml")),
        ("CARGO_MANIFEST_DIR".into(), cwd),
        ("CARGO".into(), "cargo".into()),
        ("DEBUG".into(), (!config.release).to_string()),
        (
            "OPT_LEVEL".into(),
            if config.release { "3" } else { "0" }.into(),
        ),
        ("TARGET".into(), config.host_platform.rustc_target_spec.clone()),
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
        // Cargo always sets this (empty when no RUSTFLAGS). We deliberately
        // do NOT forward the builder's own opt-level/--edition/linker flags
        // here: those are per-crate, and build.rs compile-probes (anyhow,
        // proc-macro2, eyre, …) re-invoke rustc with this variable verbatim.
        ("CARGO_ENCODED_RUSTFLAGS".into(), String::new()),
        (
            "CARGO_CRATE_NAME".into(),
            config.lib_name.replace('-', "_"),
        ),
        ("CARGO_CFG_FEATURE".into(), config.crate_features.join(",")),
    ]);
    if !config.crate_links.is_empty() {
        env.insert("CARGO_MANIFEST_LINKS".into(), config.crate_links.clone());
    }
    env.extend(target_cfg_env(config));
    // rustc reports debug_assertions unconditionally; cargo overrides it from
    // the profile (custom_build.rs:368).
    if config.release {
        env.remove("CARGO_CFG_DEBUG_ASSERTIONS");
    }
    env.remove("CARGO_CFG_PROC_MACRO");
    env
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

/// `CARGO_CFG_*` env derived from `rustc --print cfg --target <host>`,
/// matching cargo: multi-valued keys (target_feature, target_has_atomic, …)
/// are comma-joined; bare keys (unix, windows, …) get an empty string. This
/// avoids second-guessing target_env/target_family from Nix's `parsed.abi`
/// (which yields "unknown" on Darwin/wasm where rustc reports "").
fn target_cfg_env(config: &BuildConfig) -> BTreeMap<String, String> {
    use std::sync::OnceLock;
    static CFG: OnceLock<String> = OnceLock::new();
    let out = CFG.get_or_init(|| {
        let o = Command::new("rustc")
            .arg("--print=cfg")
            .arg("--target")
            .arg(&config.host_platform.rustc_target_spec)
            .output()
            .expect("failed to spawn `rustc --print=cfg`");
        if !o.status.success() {
            panic!(
                "`rustc --print=cfg --target {}` failed: {}",
                config.host_platform.rustc_target_spec,
                String::from_utf8_lossy(&o.stderr)
            );
        }
        String::from_utf8_lossy(&o.stdout).into_owned()
    });
    let mut cfg: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    for line in out.lines() {
        match line.split_once('=') {
            Some((k, v)) => cfg
                .entry(k.into())
                .or_default()
                .push(v.trim_matches('"')),
            None => {
                cfg.entry(line.into()).or_default();
            }
        }
    }
    cfg.into_iter()
        .map(|(k, vs)| {
            let key = format!(
                "CARGO_CFG_{}",
                k.to_uppercase().replace(['-', '.'], "_")
            );
            (key, vs.join(","))
        })
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
        let line = line.trim();
        let (new_syntax, d) = match line.strip_prefix("cargo::") {
            Some(d) => (true, d),
            None => match line.strip_prefix("cargo:") {
                Some(d) => (false, d),
                None => continue,
            },
        };
        let d = d.trim_end();

        if let Some(v) = d.strip_prefix("rustc-flags=") {
            rustc_flags.push(v.to_string());
        } else if let Some(v) = d.strip_prefix("rustc-check-cfg=") {
            bso.check_cfgs.push(v.into());
        } else if let Some(v) = d.strip_prefix("rustc-cfg=") {
            bso.cfgs.push(v.into());
        } else if let Some(v) = d.strip_prefix("rustc-link-arg=") {
            bso.link_args
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-lib=") {
            // nixpkgs extension (not in cargo): link arg for the lib target only.
            bso.link_args_lib
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-bins=") {
            bso.link_args_bins
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
        } else if let Some(v) = d
            .strip_prefix("rustc-cdylib-link-arg=")
            .or_else(|| d.strip_prefix("rustc-link-arg-cdylib="))
        {
            bso.cdylib_link_args
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if let Some(v) = d.strip_prefix("rustc-env=") {
            if let Some((k, val)) = v.split_once('=') {
                bso.envs.insert(k.into(), val.into());
            }
        } else if let Some(msg) = d.strip_prefix("warning=") {
            eprintln!("\x1b[0;1;33mwarning\x1b[0m: {msg}");
        } else if let (true, Some(msg)) = (new_syntax, d.strip_prefix("error=")) {
            // Old-syntax `cargo:error=` is metadata, not fatal (cargo only
            // recognises `cargo::error=`).
            eprintln!("\x1b[0;1;31merror\x1b[0m: {msg}");
            std::process::exit(1);
        }
    }
    bso.rustc_flags = rustc_flags.join(" ");
    bso.link_search = link_search;
    bso
}

/// Extract `cargo:KEY=VAL` / `cargo::metadata=KEY=VAL` pairs from build-script
/// stdout. Cargo only exposes these to dependents when `package.links` is set.
fn parse_links_metadata(stdout: &str) -> BTreeMap<String, String> {
    let mut vars = BTreeMap::new();
    for line in stdout.lines() {
        let line = line.trim();
        // New-syntax: only `cargo::metadata=KEY=VALUE` is metadata; any other
        // `cargo::` directive is a (possibly unknown) instruction, not data.
        let d = if let Some(rest) = line.strip_prefix("cargo::") {
            match rest.strip_prefix("metadata=") {
                Some(kv) => kv,
                None => continue,
            }
        } else if let Some(rest) = line.strip_prefix("cargo:") {
            rest
        } else {
            continue;
        };
        if d.starts_with("rustc-")
            || d.starts_with("warning=")
            || d.starts_with("rerun-if-")
        {
            continue;
        }
        if let Some((k, v)) = d.split_once('=') {
            vars.insert(k.into(), v.into());
        }
    }
    vars
}

/// Legacy `$lib/env` writer. The builder itself no longer reads this file
/// (DEP_* comes from `crate-metadata.json`), but crateOverrides in the wild
/// `sed`/`grep` it in preFixup, so keep emitting it byte-compatibly.
fn write_legacy_dep_env(
    config: &BuildConfig,
    vars: &BTreeMap<String, String>,
) -> Result<(), Box<dyn std::error::Error>> {
    let links_upper = config.crate_links.replace('-', "_").to_uppercase();
    let lines: Vec<String> = vars
        .iter()
        .map(|(k, v)| {
            let key = k.replace('-', "_").to_uppercase();
            let q = format!("'{}'", v.replace('\'', r"'\''"));
            format!("export DEP_{links_upper}_{key}={q}")
        })
        .collect();
    fs::write("target/env", lines.join("\n"))?;
    Ok(())
}

/// Collect `DEP_<links>_<KEY>` env for this crate's build script from every
/// dep's `crate-metadata.json`. Cargo restricts this to *direct* normal deps
/// (custom_build.rs:540-560); we still read the full transitive closure for
/// parity with the old shell builder.
///
/// `$dep/env` is layered on top *only* so crateOverrides that `sed` it (e.g.
/// ibverbs-sys remapping sandbox paths) keep working until migrated to edit
/// the JSON. Once those are gone, drop the second loop and
/// `write_legacy_dep_env`.
fn dep_links_env(config: &BuildConfig) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for path in config
        .complete_deps
        .iter()
        .chain(&config.complete_build_deps)
    {
        let Some(m) = super::config::CrateMetadata::load(path) else {
            continue;
        };
        if m.links.is_empty() {
            continue;
        }
        let links = m.links.replace('-', "_").to_uppercase();
        for (k, v) in &m.links_vars {
            let key = k.replace('-', "_").to_uppercase();
            env.insert(format!("DEP_{links}_{key}"), v.clone());
        }
        // Override-authored env file, applied after JSON so its edits win.
        if let Ok(content) = fs::read_to_string(format!("{path}/env")) {
            for line in content.lines() {
                let Some(rest) = line.strip_prefix("export ") else {
                    continue;
                };
                let Some((k, v)) = rest.split_once('=') else {
                    continue;
                };
                let v = v
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
                    .map(|s| s.replace(r"'\''", "'"))
                    .unwrap_or_else(|| v.trim_matches('"').to_string());
                env.insert(k.to_string(), v);
            }
        }
    }
    env
}

fn symlink_libs(dep_out: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    // crate-metadata.json lists every rustc artifact the dep installed
    // (filtered by `-{metadata}.` at install time), so this is
    // extension-agnostic: .rlib/.so/.dylib/.dll and anything rustc invents
    // next all link. Every dep is built by this same builder, so the file
    // is authoritative — no need to scan `$lib/lib`.
    let Some(m) = super::config::CrateMetadata::load(dep_out) else {
        return Ok(());
    };
    for art in &m.artifacts {
        let dst = Path::new(target).join(art);
        let _ = fs::remove_file(&dst);
        symlink(Path::new(dep_out).join("lib").join(art), &dst)?;
    }
    Ok(())
}

fn collect_link_flags(
    lib_dir: &str,
    link: &mut Vec<String>,
    link_final: &mut Vec<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Ok(content) = fs::read_to_string(format!("{lib_dir}/link")) {
        for line in content.lines().filter(|l| !l.is_empty()) {
            if !link.iter().any(|e| e == line) {
                link.push(line.into());
            }
            if !link_final.iter().any(|e| e == line) {
                link_final.push(line.into());
            }
        }
    }
    Ok(())
}

fn write_flags(path: &str, flags: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let mut content = flags.join("\n");
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

    let pkg = doc.get("package");
    let pkg_str = |key: &str| pkg.and_then(|p| p.get(key)).and_then(|v| v.as_str());

    // Auto-detect edition
    let has_edition =
        |opts: &[String]| opts.iter().any(|o| o == "--edition" || o.starts_with("--edition="));
    if let Some(ed) = pkg
        .and_then(|p| p.get("edition"))
        .and_then(|v| v.as_str().map(String::from).or_else(|| v.as_integer().map(|i| i.to_string())))
    {
        if !has_edition(&config.extra_rustc_opts) {
            config
                .extra_rustc_opts
                .extend_from_slice(&["--edition".into(), ed.clone()]);
        }
        if !has_edition(&config.extra_rustc_opts_for_build_rs) {
            config
                .extra_rustc_opts_for_build_rs
                .extend_from_slice(&["--edition".into(), ed]);
        }
    }

    // CARGO_PKG_* recovery for lockfile-resolve mode, where the resolver
    // emits empty strings for everything but name/version.
    let fill = |dst: &mut String, key: &str| {
        if dst.is_empty() {
            if let Some(v) = pkg_str(key) {
                *dst = v.to_string();
            }
        }
    };
    fill(&mut config.crate_description, "description");
    fill(&mut config.crate_homepage, "homepage");
    fill(&mut config.crate_license, "license");
    fill(&mut config.crate_license_file, "license-file");
    fill(&mut config.crate_readme, "readme");
    fill(&mut config.crate_repository, "repository");
    fill(&mut config.crate_rust_version, "rust-version");
    if config.crate_authors.is_empty() {
        if let Some(a) = pkg.and_then(|p| p.get("authors")).and_then(|v| v.as_array()) {
            config.crate_authors =
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
    }

    if config.build.is_empty() {
        match pkg.and_then(|p| p.get("build")) {
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
    fill(&mut config.crate_links, "links");

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
            // dependent needs an rlib regardless of what the manifest declares.
            // Cargo does *not* do this (it omits --extern for non-linkable
            // crate-types and lets the dependent fail to resolve); we promote
            // because in lockfile mode we cannot tell at eval time which deps
            // are actually linkable.
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
