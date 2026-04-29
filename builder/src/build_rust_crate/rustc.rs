// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

use std::fs;
use std::process::Command;

use super::config::BuildConfig;
use super::configure::BuildScriptOutputs;

/// Locate a dep artifact in `dir` by its metadata hash. Prefers `.rlib`,
/// falls back to `.so`/`.dylib` (proc-macros may have either under cross).
pub fn find_by_metadata(dir: &str, metadata: &str) -> Option<String> {
    let stem = format!("-{metadata}.");
    let mut dylib_match = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let Some((_, ext)) = name.rsplit_once(&stem) else { continue };
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

/// `--extern NAME=PATH` pairs from deps' `crate-metadata.json`. NAME is the
/// rename alias if any, else the dep's own lib_name. Non-linkable deps skipped.
pub fn dep_extern_args(deps: &[super::config::DepExtern], dir: &str) -> Vec<String> {
    let mut out = Vec::with_capacity(deps.len() * 2);
    // `noprelude:` needs -Z unstable-options (custom-std workflows).
    if deps.iter().any(|d| d.stdlib) {
        out.push("-Z".into());
        out.push("unstable-options".into());
    }
    for dep in deps {
        let m = super::config::CrateMetadata::load(&dep.lib_out).unwrap_or_else(|| {
            panic!(
                "missing {}/crate-metadata.json — dep not built by this buildRustCrate?",
                dep.lib_out
            )
        });
        // Matches cargo Target::is_linkable(); cdylib/staticlib carry no Rust
        // metadata, so --extern would yield E0786.
        let linkable = m.proc_macro
            || m.crate_types
                .iter()
                .any(|t| matches!(t.as_str(), "lib" | "rlib" | "dylib"));
        if !linkable {
            continue;
        }
        let Some(art) = m
            .artifacts
            .iter()
            .find(|a| a.ends_with(".rlib"))
            .or_else(|| {
                m.artifacts
                    .iter()
                    .find(|a| a.ends_with(".so") || a.ends_with(".dylib") || a.ends_with(".dll"))
            })
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

fn find_rustc_prefix_on_path() -> Option<String> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .find(|dir| dir.join("rustc").is_file())
        .and_then(|bin| Some(bin.parent()?.to_string_lossy().into_owned()))
}

/// Base rustc flags: opt level, codegen-units, remap, linker, --target, user opts.
fn base_rustc_flags(config: &BuildConfig) -> Vec<String> {
    let mut flags = Vec::new();

    if config.release {
        flags.extend_from_slice(&["-C".into(), "opt-level=3".into()]);
    } else {
        flags.extend_from_slice(&["-C".into(), "debuginfo=2".into()]);
    }
    flags.extend_from_slice(&[
        "-C".into(),
        format!("codegen-units={n}", n = config.codegen_units),
    ]);

    if let Ok(build_top) = std::env::var("NIX_BUILD_TOP") {
        flags.push(format!("--remap-path-prefix={build_top}=/"));
    }
    // Locate rustc via PATH (interpolating `${rust}` in Nix breaks cross-splicing).
    if let Some(rustc_path) = find_rustc_prefix_on_path() {
        flags.push(format!("--remap-path-prefix={rustc_path}=/rustc"));
    }

    if config.is_cross_compiling() {
        flags.extend_from_slice(&[
            "--target".into(),
            config.host_platform.rustc_target_spec.clone(),
        ]);
    }

    flags.extend_from_slice(&config.extra_rustc_opts);
    // Runtime hook (preBuild overrides), like the old shell builder.
    if let Ok(v) = std::env::var("EXTRA_RUSTC_FLAGS") {
        flags.extend(v.split_whitespace().map(String::from));
    }
    // Omit when Nix supplied no linker (bare-metal/wasm); rustc's default is correct there.
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

        base.extend(dep_extern_args(&config.dep_externs, "target/deps"));

        for f in &config.crate_features {
            base.extend_from_slice(&["--cfg".into(), format!("feature=\"{f}\"")]);
        }

        if config.crate_type.iter().any(|t| t == "proc-macro") {
            base.extend_from_slice(&["--extern".into(), "proc_macro".into()]);
        }

        let m = &config.metadata;
        let meta = vec![
            "-C".into(),
            format!("metadata={m}"),
            "-C".into(),
            format!("extra-filename=-{m}"),
        ];

        let mut link = Vec::new();
        if let Ok(content) = fs::read_to_string("target/link_") {
            link.extend(content.split_whitespace().map(String::from));
        }
        // bso.link_search/link_libs are already in target/link_.
        link.extend(bso.rustc_flags.split_whitespace().map(String::from));
        for cfg in &bso.cfgs {
            link.extend_from_slice(&["--cfg".into(), cfg.clone()]);
        }
        for cc in &bso.check_cfgs {
            link.extend_from_slice(&["--check-cfg".into(), cc.clone()]);
        }

        let out_dir = if !bso.build_out_dir.is_empty() {
            super::util::set_var("OUT_DIR", &bso.build_out_dir);
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
            // cargo build_base_args: harness=false → `--cfg test` instead of `--test`.
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
