use std::fs;
use std::process::Command;

use super::config::BuildConfig;
use super::configure::BuildScriptOutputs;

/// Locate a dep artifact in `dir` by its metadata hash. Prefers `.rlib`
/// (Rust-to-Rust linkage), falls back to any `.so`/`.dylib` (proc-macro
/// deps are built for the build platform, so under cross-compile their
/// extension may differ from the host's). The eval-time `crateType` is
/// unreliable in lockfile-resolve mode, so a caller-supplied preference
/// would be a guess anyway.
pub fn find_by_metadata(dir: &str, metadata: &str) -> Option<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    let stem = format!("-{metadata}.");
    let mut dylib_match = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some(ext) = name.rsplit_once(&stem).map(|(_, e)| e) else {
            continue;
        };
        match ext {
            "rlib" => return Some(entry.path().to_string_lossy().to_string()),
            "so" | "dylib" => {
                dylib_match = Some(entry.path().to_string_lossy().to_string());
            }
            _ => {}
        }
    }
    dylib_match
}

/// Recover the lib name from an artifact path like
/// `target/deps/libdebug_unreachable-fb242b6c18.rlib`. The metadata hash
/// uniquely identifies the dep, so the filename is the authoritative source
/// for the `--extern` key when `[lib].name` differs from the package name
/// (sparse index can't tell us; the dep's own build named the file).
fn lib_name_from_path(path: &str, metadata: &str) -> Option<String> {
    let file = std::path::Path::new(path).file_name()?.to_str()?;
    let stem = file.strip_prefix("lib")?.rsplit_once('.')?.0;
    stem.strip_suffix(&format!("-{metadata}")).map(String::from)
}

/// Compute `--extern NAME=PATH` pairs for a set of deps whose artifacts have
/// been symlinked into `dir`. The NAME is taken from the artifact filename
/// (build-time truth) unless the dep was explicitly aliased via
/// `crateRenames`, in which case the alias wins.
pub fn dep_extern_args(deps: &[super::config::DepExtern], dir: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(deps.len() * 2);
    for dep in deps {
        let path = find_by_metadata(dir, &dep.metadata)
            .unwrap_or_else(|| format!("{dir}/lib{}-{}.rlib", dep.extern_name, dep.metadata));
        let name = if dep.is_rename {
            dep.extern_name.clone()
        } else {
            lib_name_from_path(&path, &dep.metadata)
                .unwrap_or_else(|| dep.extern_name.clone())
        };
        out.push("--extern".into());
        out.push(format!("{name}={path}"));
    }
    out
}

/// Compute base rustc flags: opt level, codegen-units, remap, linker,
/// cross-compilation target, and extra user opts. Independent of deps
/// and build script output.
pub fn base_rustc_flags(config: &BuildConfig) -> Vec<String> {
    let mut flags = Vec::new();

    if config.release {
        flags.extend_from_slice(&["-C".into(), "opt-level=3".into()]);
    } else {
        flags.extend_from_slice(&["-C".into(), "debuginfo=2".into()]);
    }
    flags.extend_from_slice(&[
        "-C".into(),
        format!("codegen-units={}", config.codegen_units),
    ]);

    if let Ok(build_top) = std::env::var("NIX_BUILD_TOP") {
        flags.push(format!("--remap-path-prefix={build_top}=/"));
    }
    if !config.rustc_path.is_empty() {
        flags.push(format!(
            "--remap-path-prefix={}=/rustc",
            config.rustc_path
        ));
    }

    if config.is_cross_compiling() {
        flags.extend_from_slice(&[
            "--target".into(),
            config.host_platform.rustc_target_spec.clone(),
        ]);
    }

    flags.extend_from_slice(&config.extra_rustc_opts);
    // Runtime hook for preBuild overrides and external schedulers; the old
    // shell builder appended $EXTRA_RUSTC_FLAGS verbatim to every rustc call.
    if let Ok(v) = std::env::var("EXTRA_RUSTC_FLAGS") {
        flags.extend(v.split_whitespace().map(String::from));
    }
    flags.extend_from_slice(&[
        "-C".into(),
        format!("linker={}", config.host_platform.linker_path),
    ]);

    flags
}

/// Pre-computed rustc flags shared across lib/bin/test builds.
pub struct RustcFlags {
    pub base: Vec<String>,
    pub meta: Vec<String>,
    pub link: Vec<String>,
    pub bso_lib: Vec<String>,
    pub bso_bins: Vec<String>,
    pub bso_cdylib: Vec<String>,
    pub out_dir: Vec<String>,
    pub cap_lints: String,
}

impl RustcFlags {
    pub fn new(config: &BuildConfig, bso: &BuildScriptOutputs) -> Self {
        let mut base = base_rustc_flags(config);

        // Dependency --extern flags. Paths and names are derived from the
        // artifacts symlinked into target/deps (build-time truth), not the
        // eval-time guesses.
        base.extend(dep_extern_args(&config.dep_externs, "target/deps"));

        // Feature --cfg flags
        for f in &config.crate_features {
            base.extend_from_slice(&["--cfg".into(), format!("feature=\"{f}\"")]);
        }

        // Proc-macro extern
        if config.crate_type.iter().any(|t| t == "proc-macro") {
            base.extend_from_slice(&["--extern".into(), "proc_macro".into()]);
        }

        let meta = vec![
            "-C".into(),
            format!("metadata={}", config.metadata),
            "-C".into(),
            format!("extra-filename=-{}", config.metadata),
        ];

        // Link flags from target/link_
        let mut link = Vec::new();
        if let Ok(content) = fs::read_to_string("target/link_") {
            link.extend(content.split_whitespace().map(String::from));
        }

        // Build script rustc flags and cfgs (link_search/link_libs are
        // already in target/link_ via persist_bso_link_flags)
        link.extend(bso.rustc_flags.split_whitespace().map(String::from));
        for cfg in &bso.cfgs {
            link.extend_from_slice(&["--cfg".into(), cfg.clone()]);
        }
        for cc in &bso.check_cfgs {
            link.extend_from_slice(&["--check-cfg".into(), cc.clone()]);
        }

        let out_dir = if !bso.build_out_dir.is_empty() {
            std::env::set_var("OUT_DIR", &bso.build_out_dir);
            vec!["-L".into(), bso.build_out_dir.clone()]
        } else {
            vec![]
        };

        RustcFlags {
            base,
            meta,
            link,
            bso_lib: bso.link_args.clone(),
            bso_bins: bso
                .link_args
                .iter()
                .chain(&bso.link_args_bins)
                .cloned()
                .collect(),
            bso_cdylib: bso.cdylib_link_args.clone(),
            out_dir,
            cap_lints: config.cap_lints.clone(),
        }
    }

    /// Build a rustc Command with all common flags.
    pub fn cmd(
        &self,
        crate_name: &str,
        source: &str,
        out_dir: &str,
        crate_types: &[&str],
        extra_flags: &[String],
        test: bool,
    ) -> Command {
        let mut cmd = Command::new("rustc");
        cmd.arg("--crate-name")
            .arg(crate_name)
            .arg(source)
            .arg("--out-dir")
            .arg(out_dir)
            .arg("-L")
            .arg("dependency=target/deps")
            .arg("--cap-lints")
            .arg(&self.cap_lints);

        for ct in crate_types {
            cmd.arg("--crate-type").arg(*ct);
        }
        if test {
            cmd.arg("--test");
        }

        cmd.args(&self.base)
            .args(&self.link)
            .args(&self.out_dir)
            .args(extra_flags)
            .arg("--color")
            .arg("auto");
        cmd
    }
}
