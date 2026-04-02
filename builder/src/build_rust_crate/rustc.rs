use std::fs;
use std::process::Command;

use super::config::BuildConfig;
use super::configure::BuildScriptOutputs;

/// Find a library file in `dir` by its metadata hash suffix.
/// Prefers .rlib when `prefer_rlib` is true, falls back to shared lib.
pub fn find_by_metadata(
    dir: &str,
    metadata: &str,
    prefer_rlib: bool,
    lib_ext: &str,
) -> Option<String> {
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    let rlib_suffix = format!("-{metadata}.rlib");
    let so_suffix = format!("-{metadata}{lib_ext}");
    let mut so_match = None;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(&rlib_suffix) && prefer_rlib {
            return Some(entry.path().to_string_lossy().to_string());
        }
        if name.ends_with(&so_suffix) {
            so_match = Some(entry.path().to_string_lossy().to_string());
        }
        if name.ends_with(&rlib_suffix) {
            // Non-preferred rlib is still a fallback
            if so_match.is_none() {
                so_match = Some(entry.path().to_string_lossy().to_string());
            }
        }
    }
    so_match
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
    flags.extend_from_slice(&[
        "-C".into(),
        format!("linker={}", config.host_platform.linker_path),
    ]);

    flags
}

/// Encode flags for CARGO_ENCODED_RUSTFLAGS (unit separator delimited).
pub fn encode_rustflags(config: &BuildConfig) -> String {
    base_rustc_flags(config).join("\x1f")
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

        // Dependency --extern flags from Nix eval-time info.
        for dep in &config.dep_externs {
            let path = find_by_metadata(
                "target/deps",
                &dep.metadata,
                dep.is_rlib,
                &config.host_platform.lib_ext,
            )
            .unwrap_or_else(|| {
                let ext = if dep.is_rlib {
                    "rlib"
                } else {
                    &config.host_platform.lib_ext
                };
                format!(
                    "target/deps/lib{}-{}.{ext}",
                    dep.extern_name, dep.metadata
                )
            });
            base.push("--extern".into());
            base.push(format!("{}={path}", dep.extern_name));
        }

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
            bso_lib: bso
                .link_args
                .iter()
                .chain(&bso.link_args_lib)
                .cloned()
                .collect(),
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

        for a in &self.base {
            cmd.arg(a);
        }
        for a in &self.link {
            cmd.arg(a);
        }
        for a in &self.out_dir {
            cmd.arg(a);
        }
        for a in extra_flags {
            cmd.arg(a);
        }
        cmd.arg("--color").arg("auto");
        cmd
    }
}
