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

/// Compute `--extern NAME=PATH` pairs for a set of deps whose artifacts have
/// been symlinked into `dir`. Reads each dep's installed
/// `crate-metadata.json` for the authoritative lib name and artifact
/// filename (so `.dll`/non-standard extensions just work). NAME is the
/// alias when the dep was renamed via `crateRenames`, otherwise the dep's
/// own `lib_name`. Deps that built no linkable artifact are skipped.
pub fn dep_extern_args(deps: &[super::config::DepExtern], dir: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(deps.len() * 2);
    // `noprelude:` is gated behind -Z unstable-options; emit it once if any
    // dep is a stdlib crate (custom-std / build-std workflows).
    if deps.iter().any(|d| d.stdlib) {
        out.push("-Z".into());
        out.push("unstable-options".into());
    }
    for dep in deps {
        let m = super::config::CrateMetadata::load(&dep.lib_out).unwrap_or_else(|| {
            panic!(
                "missing {}/crate-metadata.json \u{2014} dep not built by this buildRustCrate?",
                dep.lib_out
            )
        });
        // Prefer rlib for Rust-to-Rust linkage; otherwise the first
        // artifact (proc-macro/dylib). Empty when the dep is bin-only or
        // staticlib-only — nothing to `--extern`.
        let Some(art) = m
            .artifacts
            .iter()
            .find(|a| a.ends_with(".rlib"))
            .or(m.artifacts.first())
        else {
            continue;
        };
        let name = if dep.is_rename {
            &dep.extern_name
        } else {
            &m.lib_name
        };
        let prefix = if dep.stdlib { "noprelude:" } else { "" };
        out.push("--extern".into());
        out.push(format!("{prefix}{name}={dir}/{art}"));
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
    // Omit when Nix gave us no linker (bare-metal / wasm stdenvs without a
    // CC and not using lld) — rustc's built-in default is correct there,
    // whereas `-C linker=cc` would point at a non-existent binary.
    if let Some(linker) = config.host_platform.linker_path.as_deref().filter(|s| !s.is_empty()) {
        flags.extend_from_slice(&["-C".into(), format!("linker={linker}")]);
    }

    flags
}

/// Pre-computed rustc flags shared across lib/bin/test builds.
pub struct RustcFlags {
    pub base: Vec<String>,
    pub meta: Vec<String>,
    pub link: Vec<String>,
    pub bso_lib: Vec<String>,
    pub bso_bins: Vec<String>,
    pub bso_bin: std::collections::BTreeMap<String, Vec<String>>,
    pub bso_tests: Vec<String>,
    pub bso_cdylib: Vec<String>,
    pub out_dir: Vec<String>,
    pub cap_lints: String,
    pub colors: String,
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
            bso_bin: bso.link_args_bin.clone(),
            bso_tests: bso
                .link_args
                .iter()
                .chain(&bso.link_args_tests)
                .cloned()
                .collect(),
            bso_cdylib: bso.cdylib_link_args.clone(),
            out_dir,
            cap_lints: config.cap_lints.clone(),
            colors: config.colors.clone(),
        }
    }

    /// Build a rustc Command with all common flags.
    #[allow(clippy::too_many_arguments)] // flat arg list mirrors rustc's own
    pub fn cmd(
        &self,
        crate_name: &str,
        source: &str,
        out_dir: &str,
        crate_types: &[&str],
        extra_flags: &[String],
        test: bool,
        harness: bool,
    ) -> Command {
        let mut cmd = Command::new("rustc");
        // Per-target env that cargo sets on every rustc invocation. We only
        // ever build the package itself, so CARGO_PRIMARY_PACKAGE is always 1.
        cmd.env("CARGO_CRATE_NAME", crate_name);
        cmd.env("CARGO_PRIMARY_PACKAGE", "1");
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
            // cargo build_base_args: harnessed test targets get `--test`
            // (libtest main); harness=false targets only get `--cfg test`.
            if harness {
                cmd.arg("--test");
            } else {
                cmd.arg("--cfg").arg("test");
            }
        }

        cmd.args(&self.base)
            .args(&self.link)
            .args(&self.out_dir)
            .args(extra_flags)
            .arg("--color")
            .arg(&self.colors);
        cmd
    }
}
