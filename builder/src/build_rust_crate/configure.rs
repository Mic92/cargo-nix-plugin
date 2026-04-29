// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

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
    #[serde(default)]
    pub link_args_bin: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub link_args_tests: Vec<String>,
    pub link_libs: Vec<String>,
    pub link_search: Vec<String>,
    pub cdylib_link_args: Vec<String>,
    pub envs: BTreeMap<String, String>,
    pub build_out_dir: String,
}

/// Resolve the crate's source root from `workspace_member` (or by scanning
/// for a matching Cargo.toml when unset) and print its absolute path. The
/// stdenv shell cd's there once before `runHook preConfigure`; genericBuild
/// runs all phases in one shell, so every later `build-rust-crate` invocation
/// starts in the crate root.
pub fn locate(config: &BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    let dir = match &config.workspace_member {
        Some(m) => PathBuf::from(m),
        None => {
            echo_colored(&format!(
                "Searching for matching Cargo.toml ({})",
                config.crate_name
            ));
            find_matching_cargo_toml(&config.crate_name)?.into()
        }
    };
    println!("{}", fs::canonicalize(dir)?.display());
    Ok(())
}

pub fn run(config: &mut BuildConfig) -> Result<(), Box<dyn std::error::Error>> {
    detect_cargo_toml_info(config);

    for dir in &["target/deps", "target/lib", "target/build", "target/buildDeps"] {
        fs::create_dir_all(dir)?;
    }

    // Link flags: one entry = one line (may contain spaces); preserve order,
    // dedupe whole lines only.
    let mut link: Vec<String> = Vec::new();
    if !config.extra_link_flags.is_empty() {
        link.push(config.extra_link_flags.join(" "));
    }
    let mut link_final = link.clone();
    let mut build_link: Vec<String> = Vec::new();

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

    let mut hook_out_dir = String::new();
    let mut hook_bso_envs: BTreeMap<String, String> = BTreeMap::new();

    let base_env = build_env(config, "");

    if let Some(script) = build_script {
        echo_colored(&format!("Building {script} ({})", config.lib_name));

        let build_dir = format!("target/build/{n}", n = config.crate_name);
        let out_dir = format!("target/build/{n}.out", n = config.crate_name);
        fs::create_dir_all(&build_dir)?;
        fs::create_dir_all(&out_dir)?;
        let abs_out_dir = fs::canonicalize(&out_dir)?.to_string_lossy().into_owned();
        let mut env = base_env.clone();
        env.insert("OUT_DIR".into(), abs_out_dir.clone());

        // Compile build script (CARGO_PKG_* needed at compile time too for env!()).
        let mut cmd = Command::new("rustc");
        cmd.envs(&env);
        cmd.env("CARGO_CRATE_NAME", "build_script_build");
        cmd.env("CARGO_PRIMARY_PACKAGE", "1");
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
        cmd.args(["-C", &format!("codegen-units={n}", n = config.codegen_units)]);
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

        // Run build script. Match cargo custom_build.rs: scrub RUSTFLAGS/wrappers
        // so compile-probes (autocfg etc.) invoke a bare rustc.
        let mut cmd = Command::new(format!("{build_dir}/build_script_build"));
        cmd.env_remove("RUSTFLAGS");
        cmd.env_remove("RUSTC_WRAPPER");
        cmd.env_remove("RUSTC_WORKSPACE_WRAPPER");
        super::util::inherit_jobserver(&mut cmd);
        cmd.env("RUST_BACKTRACE", "1");
        cmd.envs(&env);
        // RUSTC_LINKER for cc-rs / cross probes (custom_build.rs:338).
        if let Some(l) = &config.host_platform.linker_path {
            cmd.env("RUSTC_LINKER", l);
        }
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
        hook_out_dir = abs_out_dir.clone();
        hook_bso_envs = bso.envs.clone();
        fs::write(
            "target/build-script-outputs.json",
            serde_json::to_string_pretty(&bso)?,
        )?;
        // install.rs remaps these to store paths and writes crate-metadata.json + legacy env.
        if !config.crate_links.is_empty() {
            let vars = parse_links_metadata(&stdout);
            if !vars.is_empty() {
                fs::write("target/links-vars.json", serde_json::to_string(&vars)?)?;
            }
        }
    }

    // Snapshot cargo env so stdenv hooks can `source target/hook-env`
    // (the binary's env is process-local).
    let mut hook_env = base_env;
    if hook_out_dir.is_empty() {
        hook_env.remove("OUT_DIR");
    } else {
        hook_env.insert("OUT_DIR".into(), hook_out_dir);
    }
    write_hook_env("target/hook-env", &hook_env, &hook_bso_envs)?;

    Ok(())
}

fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str(r"'\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

fn is_shell_ident(k: &str) -> bool {
    let mut it = k.chars();
    matches!(it.next(), Some(c) if c == '_' || c.is_ascii_alphabetic())
        && it.all(|c| c == '_' || c.is_ascii_alphanumeric())
}

fn write_hook_env(
    path: &str,
    base: &BTreeMap<String, String>,
    bso_envs: &BTreeMap<String, String>,
) -> std::io::Result<()> {
    let mut s = String::new();
    for (k, v) in base.iter().chain(bso_envs) {
        if is_shell_ident(k) {
            s.push_str(&format!("export {k}={}\n", shell_quote(v)));
        }
    }
    fs::write(path, s)
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
        // Always set, always empty: per-crate flags must not leak into build.rs probes.
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

/// `CARGO_CFG_*` env from `rustc --print cfg --target <host>` (multi-valued
/// keys comma-joined, bare keys empty), avoiding Nix's `parsed.abi` guesswork.
fn target_cfg_env(config: &BuildConfig) -> BTreeMap<String, String> {
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
    let out = String::from_utf8_lossy(&o.stdout).into_owned();
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
        let (new_syntax, d) = if let Some(d) = line.strip_prefix("cargo::") {
            (true, d)
        } else if let Some(d) = line.strip_prefix("cargo:") {
            (false, d)
        } else {
            continue;
        };
        let d = d.trim_end();

        if let Some(v) = d.strip_prefix("rustc-flags=") {
            // cargo parse_rustc_flags: only -L/-l accepted, routed to link
            // fields; anything else kept in rustc_flags for back-compat.
            let mut iter = v.split_whitespace();
            while let Some(tok) = iter.next() {
                let (flag, val) = if tok == "-L" || tok == "-l" {
                    let Some(v) = iter.next() else {
                        rustc_flags.push(tok.to_string());
                        continue;
                    };
                    (tok, v.to_string())
                } else if let Some(v) = tok.strip_prefix("-L") {
                    ("-L", v.to_string())
                } else if let Some(v) = tok.strip_prefix("-l") {
                    ("-l", v.to_string())
                } else {
                    rustc_flags.push(tok.to_string());
                    continue;
                };
                if flag == "-L" {
                    if !link_search.contains(&val) {
                        link_search.push(val);
                    }
                } else {
                    bso.link_libs.push(val);
                }
            }
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
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-bin=") {
            if let Some((bin, arg)) = v.split_once('=') {
                bso.link_args_bin
                    .entry(bin.into())
                    .or_default()
                    .extend_from_slice(&["-C".into(), format!("link-arg={arg}")]);
            }
        } else if let Some(v) = d.strip_prefix("rustc-link-arg-tests=") {
            bso.link_args_tests
                .extend_from_slice(&["-C".into(), format!("link-arg={v}")]);
        } else if d.starts_with("rustc-link-arg-examples=")
            || d.starts_with("rustc-link-arg-benches=")
            || d.starts_with("rerun-if-")
        {
            // Accepted but inert (no example/bench targets; rerun-if-* meaningless in sandbox).
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
        } else if new_syntax && let Some(msg) = d.strip_prefix("error=") {
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
        // cargo:: → only `metadata=` is data; cargo: → anything not in RESERVED_PREFIXES.
        let d = if let Some(rest) = line.strip_prefix("cargo::") {
            let Some(kv) = rest.strip_prefix("metadata=") else { continue };
            kv
        } else if let Some(rest) = line.strip_prefix("cargo:") {
            if rest.starts_with("rustc-")
                || rest.starts_with("warning=")
                || rest.starts_with("rerun-if-")
            {
                continue;
            }
            rest
        } else {
            continue;
        };
        if let Some((k, v)) = d.split_once('=') {
            vars.insert(k.into(), v.into());
        }
    }
    vars
}

/// `DEP_<links>_<KEY>` env from each dep's `crate-metadata.json` (full
/// transitive closure, like the old shell builder; cargo only does direct).
/// `$dep/env` overlaid afterwards so crateOverrides that sed it still win.
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
    // crate-metadata.json is the authoritative artifact list (extension-agnostic).
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
        let Some(name_val) = doc.get("package").and_then(|p| p.get("name")) else {
            continue;
        };
        if name_val.as_str() == Some(crate_name)
            || (name_val.is_table() && find_workspace_name(&entry).as_deref() == Some(crate_name))
        {
            return Ok(entry.parent().unwrap().to_string_lossy().into_owned());
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

/// Fill in `libPath`/`libName`/`crateType`/edition/etc from Cargo.toml when
/// the resolver couldn't supply them (lockfile mode has no `[lib]` table).
/// Runs at the top of every phase; cwd is already the crate root.
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

    // Resolve `{ workspace = true }` against the nearest [workspace.package]
    // (only matters for path/git deps; registry tarballs are normalised).
    let ws_pkg: Option<toml::Value> = pkg
        .map(|p| {
            p.as_table()
                .map(|t| t.values().any(is_ws_inherit))
                .unwrap_or(false)
        })
        .filter(|&any| any)
        .and_then(|_| find_workspace_package(Path::new("Cargo.toml")));
    let pkg_str = |key: &str| -> Option<String> {
        let v = pkg?.get(key)?;
        if let Some(s) = v.as_str() {
            return Some(s.to_string());
        }
        if is_ws_inherit(v) {
            return Some(ws_pkg.as_ref()?.get(key)?.as_str()?.to_string());
        }
        None
    };

    let pkg_bool = |key: &str| pkg.and_then(|p| p.get(key)).and_then(|v| v.as_bool());
    if let Some(b) = pkg_bool("autobins") {
        config.autobins = b;
    }
    if let Some(b) = pkg_bool("autotests") {
        config.autotests = b;
    }
    if let Some(b) = pkg_bool("autolib") {
        config.autolib = b;
    }

    let has_edition =
        |opts: &[String]| opts.iter().any(|o| o == "--edition" || o.starts_with("--edition="));
    if let Some(ed) = pkg.and_then(|p| p.get("edition")).and_then(|v| {
        v.as_str()
            .map(String::from)
            .or_else(|| v.as_integer().map(|i| i.to_string()))
            .or_else(|| {
                is_ws_inherit(v)
                    .then_some(())
                    .and_then(|_| ws_pkg.as_ref()?.get("edition")?.as_str().map(String::from))
            })
    }) {
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

    // CARGO_PKG_* recovery for lockfile-resolve mode (resolver emits "").
    let fill = |dst: &mut String, key: &str| {
        if dst.is_empty() && let Some(v) = pkg_str(key) {
            *dst = v;
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
        let a = pkg.and_then(|p| p.get("authors")).and_then(|v| {
            v.as_array().cloned().or_else(|| {
                is_ws_inherit(v)
                    .then_some(())
                    .and_then(|_| ws_pkg.as_ref()?.get("authors")?.as_array().cloned())
            })
        });
        if let Some(a) = a {
            config.crate_authors =
                a.iter().filter_map(|v| v.as_str().map(String::from)).collect();
        }
    }

    if config.build.is_empty() {
        match pkg.and_then(|p| p.get("build")) {
            Some(v) if v.as_bool() == Some(false) => config.build = "false".into(),
            Some(v) if v.as_str().is_some_and(|p| Path::new(p).exists()) => {
                config.build = v.as_str().unwrap().to_string();
            }
            _ => {}
        }
    }

    fill(&mut config.crate_links, "links");

    let lib = doc.get("lib");

    // lib.path (fnv etc. use `path = "lib.rs"`; resolve_lib_path only falls back to src/lib.rs).
    if config.lib_path.is_empty()
        && let Some(p) = lib.and_then(|l| l.get("path")).and_then(|v| v.as_str())
    {
        config.lib_path = p.to_string();
    }

    // lib.name: drv defaults libName=crateName, so treat that as unset.
    if config.lib_name.is_empty() || config.lib_name == config.crate_name {
        config.lib_name = lib
            .and_then(|l| l.get("name"))
            .and_then(|v| v.as_str())
            .map(String::from)
            .unwrap_or_else(|| config.crate_name.replace('-', "_"));
    }

    // lib.crate-type / proc-macro: only rewrite the eval-time default ["lib"].
    if config.crate_type == ["lib"] {
        let is_proc_macro = lib
            .and_then(|l| l.get("proc-macro").or_else(|| l.get("proc_macro")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if is_proc_macro {
            config.crate_type = vec!["proc-macro".into()];
        } else if let Some(types) = lib
            .and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type")))
            .and_then(|v| v.as_array())
        {
            let mut ts: Vec<String> = types
                .iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect();
            // Promote to rlib if not Rust-linkable: in lockfile mode we can't
            // tell at eval time which deps need --extern (cargo just omits it).
            if !ts.iter().any(|t| t == "lib" || t == "rlib" || t == "proc-macro") {
                ts.push("rlib".into());
            }
            if !ts.is_empty() {
                config.crate_type = ts;
            }
        }
    }

    // [[bin]] entries. Skip when the drv set crateBin (an explicit `[]` is how
    // lib/default.nix suppresses bins on the lib-only dep variant).
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
        }
        // Merge with inferred set, dedupe by name *or* path (cargo behaviour).
        if config.autobins {
            for (name, path) in inferred_bins(&config.crate_name) {
                let taken = config.crate_bin.iter().any(|b| {
                    b.name.as_deref() == Some(name.as_str())
                        || b.path.as_deref() == Some(path.as_str())
                });
                if !taken {
                    config.crate_bin.push(super::config::CrateBin {
                        name: Some(name),
                        path: Some(path),
                        required_features: Vec::new(),
                    });
                }
            }
        }
        if !config.crate_bin.is_empty() {
            config.has_crate_bin = true;
        }
    }

    if let Some(tests) = doc.get("test").and_then(|v| v.as_array()) {
        for t in tests {
            let Some(name) = t.get("name").and_then(|v| v.as_str()).map(String::from) else {
                continue;
            };
            let path = t.get("path").and_then(|v| v.as_str()).map(String::from);
            let required_features = t
                .get("required-features")
                .and_then(|v| v.as_array())
                .map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            let harness = t.get("harness").and_then(|v| v.as_bool()).unwrap_or(true);
            config.crate_tests.push(super::config::CrateTest {
                name,
                path,
                required_features,
                harness,
            });
        }
    }
}

fn is_ws_inherit(v: &toml::Value) -> bool {
    v.get("workspace").and_then(|w| w.as_bool()) == Some(true)
}

/// Cargo's autobins inference: src/main.rs → crate_name, src/bin/*.rs → stem,
/// src/bin/*/main.rs → dirname. Dotfiles skipped.
pub fn inferred_bins(crate_name: &str) -> Vec<(String, String)> {
    let mut bins = Vec::new();
    if Path::new("src/main.rs").exists() {
        bins.push((crate_name.to_string(), "src/main.rs".into()));
    }
    if let Ok(entries) = fs::read_dir("src/bin") {
        for entry in entries.flatten() {
            let path = entry.path();
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            if fname.starts_with('.') {
                continue;
            }
            if path.extension().is_some_and(|e| e == "rs") {
                let name = path.file_stem().unwrap().to_string_lossy().to_string();
                bins.push((name, path.to_string_lossy().into_owned()));
            } else if path.is_dir() && path.join("main.rs").exists() {
                bins.push((fname.into_owned(), path.join("main.rs").to_string_lossy().into_owned()));
            }
        }
    }
    bins
}

/// Walk up from `cargo_toml` to find the workspace root and return its
/// `[workspace.package]` table for `*.workspace = true` inheritance.
fn find_workspace_package(cargo_toml: &Path) -> Option<toml::Value> {
    // The member manifest may itself be the workspace root.
    let mut dir = std::env::current_dir()
        .ok()?
        .join(cargo_toml)
        .parent()?
        .to_path_buf();
    loop {
        let ws = dir.join("Cargo.toml");
        if ws.exists()
            && let Ok(doc) = toml::from_str::<toml::Value>(&fs::read_to_string(&ws).ok()?)
            && let Some(wp) = doc.get("workspace").and_then(|w| w.get("package"))
        {
            return Some(wp.clone());
        }
        dir = dir.parent()?.to_path_buf();
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
        assert_eq!(bso.rustc_flags, "");
        assert_eq!(bso.link_libs, vec!["foo", "bar"]);
        assert_eq!(bso.check_cfgs, vec!["cfg(has_foo)"]);
        assert_eq!(bso.link_search, vec!["/a", "native=/out"]); // de-duped, order kept
    }

    #[test]
    fn build_script_output_rustc_flags_routes_to_link_fields() {
        let bso =
            parse_build_script_output("cargo:rustc-flags=-L /a -lfoo -L native=/b\n", "/out");
        assert_eq!(bso.link_search, vec!["/a", "native=/b"]);
        assert_eq!(bso.link_libs, vec!["foo"]);
        assert_eq!(bso.rustc_flags, "");
    }

    #[test]
    fn build_script_output_per_target_link_args() {
        let stdout = "\
cargo:rustc-link-arg=-all\n\
cargo:rustc-link-arg-lib=-libonly\n\
cargo:rustc-link-arg-bins=-allbins\n\
cargo:rustc-link-arg-bin=foo=-Tlink.x\n\
cargo:rustc-link-arg-bin=foo=-Map=foo.map\n\
cargo::rustc-link-arg-bin=bar=-barflag\n\
cargo:rustc-link-arg-tests=-testflag\n";
        let bso = parse_build_script_output(stdout, "/out");
        assert_eq!(bso.link_args, vec!["-C", "link-arg=-all"]);
        assert_eq!(bso.link_args_lib, vec!["-C", "link-arg=-libonly"]);
        assert_eq!(bso.link_args_bins, vec!["-C", "link-arg=-allbins"]);
        assert_eq!(
            bso.link_args_bin.get("foo").unwrap(),
            &vec![
                "-C".to_string(),
                "link-arg=-Tlink.x".into(),
                "-C".into(),
                "link-arg=-Map=foo.map".into()
            ]
        );
        assert_eq!(
            bso.link_args_bin.get("bar").unwrap(),
            &vec!["-C".to_string(), "link-arg=-barflag".into()]
        );
        assert_eq!(bso.link_args_tests, vec!["-C", "link-arg=-testflag"]);
    }

    #[test]
    fn build_script_output_link_arg_prefix_disambiguation() {
        // `rustc-link-arg-bins=` must not be eaten by the `rustc-link-arg-bin=` branch.
        let bso = parse_build_script_output("cargo:rustc-link-arg-bins=-x\n", "/out");
        assert_eq!(bso.link_args_bins, vec!["-C", "link-arg=-x"]);
        assert!(bso.link_args_bin.is_empty());
        // `rustc-link-arg=` must not be eaten by `rustc-link-arg-lib=`.
        let bso = parse_build_script_output("cargo:rustc-link-arg=-x\n", "/out");
        assert_eq!(bso.link_args, vec!["-C", "link-arg=-x"]);
        assert!(bso.link_args_lib.is_empty());
    }

    #[test]
    fn build_script_output_noop_directives_are_ignored() {
        // None of these may leak into rustc_flags or link fields.
        let stdout = "\
cargo:rustc-link-arg-examples=-ex\n\
cargo::rustc-link-arg-benches=-bn\n\
cargo:rerun-if-changed=build.rs\n\
cargo::rerun-if-env-changed=CC\n\
cargo:error=this is metadata, not fatal\n\
cargo::metadata=include=/out/inc\n";
        let bso = parse_build_script_output(stdout, "/out");
        assert_eq!(bso.rustc_flags, "");
        assert!(bso.link_args.is_empty());
        assert!(bso.link_args_bins.is_empty());
        assert!(bso.link_args_bin.is_empty());
        assert!(bso.envs.is_empty());
    }

    #[test]
    fn build_script_output_cdylib_aliases() {
        let bso = parse_build_script_output(
            "cargo:rustc-cdylib-link-arg=-a\ncargo::rustc-link-arg-cdylib=-b\n",
            "/out",
        );
        assert_eq!(
            bso.cdylib_link_args,
            vec!["-C", "link-arg=-a", "-C", "link-arg=-b"]
        );
    }

    #[test]
    fn links_metadata_parsing() {
        let stdout = "\
cargo:include=/a=/b\n\
cargo::metadata=root=/out\n\
cargo::metadata=rustc-foo=bar\n\
cargo::rustc-cfg=x\n\
cargo::warning=w\n\
cargo:warning=w2\n\
cargo:rerun-if-changed=build.rs\n\
cargo:error=legacy\n\
cargo::error=fatal\n";
        let vars = parse_links_metadata(stdout);
        // `=` in value preserved; new-syntax routed via metadata=; reserved
        // keys and cargo:: instructions excluded; old-syntax cargo:error= is
        // plain metadata (not in cargo's RESERVED_PREFIXES).
        assert_eq!(
            vars,
            BTreeMap::from([
                ("include".into(), "/a=/b".into()),
                ("root".into(), "/out".into()),
                ("rustc-foo".into(), "bar".into()),
                ("error".into(), "legacy".into()),
            ])
        );
    }

    #[test]
    fn hook_env_is_shell_sourceable() {
        let mut base = BTreeMap::new();
        base.insert("CARGO_PKG_NAME".into(), "it's a name".into());
        base.insert("OUT_DIR".into(), "/build/out".into());
        let mut extra = BTreeMap::new();
        extra.insert("MY_VAR".into(), "a\nb".into());
        extra.insert("bad key".into(), "x".into());
        let tmp = std::env::temp_dir().join("hook-env-test");
        write_hook_env(tmp.to_str().unwrap(), &base, &extra).unwrap();
        let s = fs::read_to_string(&tmp).unwrap();
        assert!(s.contains("export CARGO_PKG_NAME='it'\\''s a name'\n"));
        assert!(s.contains("export OUT_DIR='/build/out'\n"));
        assert!(s.contains("export MY_VAR='a\nb'\n"));
        assert!(!s.contains("bad key"));
    }
}
