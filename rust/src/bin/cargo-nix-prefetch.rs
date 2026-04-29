// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Warm the sparse registry index cache for a Cargo workspace.
//!
//! Standalone front-end to the same prefetch machinery the plugin uses
//! at eval time. Intended for two use-cases (#20):
//!
//! 1. **CI / sandboxed eval**: run this once on a connected host (or
//!    against a reachable mirror), then point the plugin's `cargoHome`
//!    at the resulting directory. The plugin will find every crate
//!    cached and never touch the network.
//!
//! 2. **Vendoring into Nix**: with `--output`, write the cache into a
//!    fresh directory suitable for committing or wrapping in a
//!    fixed-output derivation, instead of mutating the user's
//!    `~/.cargo`.
//!
//! Honors the same crates.io index override precedence as the plugin
//! (`--index` → `$CARGO_REGISTRIES_CRATES_IO_INDEX` → `.cargo/config.toml`
//! source replacement → upstream).
//!
//! Usage:
//!   cargo-nix-prefetch [--manifest-path Cargo.toml]
//!                      [--lockfile Cargo.lock]
//!                      [--index URL]
//!                      [--output DIR | --cargo-home DIR]
//!                      [--check]

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use cargo_nix_plugin_core::registry;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut manifest_path: Option<PathBuf> = None;
    let mut lockfile: Option<PathBuf> = None;
    let mut index_override: Option<String> = None;
    let mut output: Option<PathBuf> = None;
    let mut cargo_home_arg: Option<PathBuf> = None;
    let mut check_only = false;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--manifest-path" => manifest_path = args.next().map(PathBuf::from),
            "--lockfile" => lockfile = args.next().map(PathBuf::from),
            "--index" => index_override = args.next(),
            "--output" => output = args.next().map(PathBuf::from),
            "--cargo-home" => cargo_home_arg = args.next().map(PathBuf::from),
            "--check" => check_only = true,
            "-h" | "--help" => {
                print_usage();
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("error: unknown argument '{other}'");
                print_usage();
                return ExitCode::from(2);
            }
        }
    }

    // Locate the workspace root and lockfile. Mirror cargo's behaviour:
    // --manifest-path points at Cargo.toml; lockfile sits next to it.
    let manifest_path = manifest_path.unwrap_or_else(|| PathBuf::from("Cargo.toml"));
    let workspace_root = manifest_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
        .to_path_buf();
    let lockfile = lockfile.unwrap_or_else(|| workspace_root.join("Cargo.lock"));

    let cargo_lock = match std::fs::read_to_string(&lockfile) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("error: failed to read {}: {e}", lockfile.display());
            return ExitCode::FAILURE;
        }
    };

    // The user's ambient CARGO_HOME — where cargo's own config.toml
    // lives. Always consulted for mirror discovery, regardless of where
    // the cache is written.
    let ambient_cargo_home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var_os("HOME")
                .map(PathBuf::from)
                .unwrap_or_default()
                .join(".cargo")
        });

    // Where to write the cache. --output takes precedence; otherwise
    // --cargo-home; otherwise the ambient $CARGO_HOME. Decoupled from
    // config discovery so `--output ./fresh` still picks up a mirror
    // from `~/.cargo/config.toml`.
    let cargo_home = output
        .or(cargo_home_arg)
        .unwrap_or_else(|| ambient_cargo_home.clone());

    let crates_io_url = registry::resolve_crates_io_index(
        index_override.as_deref(),
        &workspace_root,
        &ambient_cargo_home,
    );

    let jobs = match collect_jobs(&cargo_lock, &crates_io_url) {
        Ok(j) => j,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };

    eprintln!(
        "cargo-nix-prefetch: {} registry crates in {}",
        jobs.len(),
        lockfile.display()
    );
    eprintln!("cargo-nix-prefetch: crates.io index → {crates_io_url}");
    eprintln!(
        "cargo-nix-prefetch: cache dir       → {}",
        cargo_home.display()
    );

    if check_only {
        let missing: Vec<_> = jobs
            .iter()
            .filter(|j| !registry::is_cached(&cargo_home, &j.url, &j.name, &j.version))
            .collect();
        if missing.is_empty() {
            eprintln!("cargo-nix-prefetch: all {} crates cached", jobs.len());
            return ExitCode::SUCCESS;
        }
        eprintln!(
            "cargo-nix-prefetch: {} of {} crates missing from cache:",
            missing.len(),
            jobs.len()
        );
        for j in &missing {
            eprintln!("  {} {}", j.name, j.version);
        }
        return ExitCode::FAILURE;
    }

    if let Err(e) = std::fs::create_dir_all(&cargo_home) {
        eprintln!("error: failed to create {}: {e}", cargo_home.display());
        return ExitCode::FAILURE;
    }

    match registry::prefetch_index(&cargo_home, &jobs) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Map every registry package in Cargo.lock to a `(index_url, name)`
/// prefetch job. Non-registry sources (path, git) are skipped.
fn collect_jobs(
    cargo_lock: &str,
    crates_io_url: &str,
) -> Result<Vec<registry::PrefetchJob>, String> {
    #[derive(serde::Deserialize)]
    struct Lock {
        #[serde(default)]
        package: Vec<Pkg>,
    }
    #[derive(serde::Deserialize)]
    struct Pkg {
        name: String,
        version: String,
        #[serde(default)]
        source: Option<String>,
    }

    let lock: Lock =
        toml::from_str(cargo_lock).map_err(|e| format!("failed to parse Cargo.lock: {e}"))?;

    let mut jobs = Vec::new();
    for pkg in lock.package {
        let Some(url) = registry::source_to_index_url(pkg.source.as_deref(), crates_io_url) else {
            continue; // path/git
        };
        // No de-dup here: prefetch_index merges by (url, name) itself
        // and needs every locked version to decide cache freshness.
        jobs.push(registry::PrefetchJob {
            url,
            name: pkg.name,
            version: pkg.version,
        });
    }
    Ok(jobs)
}

fn print_usage() {
    eprintln!(
        "cargo-nix-prefetch: warm the sparse registry index cache for a Cargo.lock\n\
         \n\
         USAGE:\n\
         \x20 cargo-nix-prefetch [OPTIONS]\n\
         \n\
         OPTIONS:\n\
         \x20 --manifest-path <Cargo.toml>  workspace manifest (default: ./Cargo.toml)\n\
         \x20 --lockfile <Cargo.lock>       lockfile to read (default: next to manifest)\n\
         \x20 --index <URL>                 sparse index URL for crates.io packages\n\
         \x20                               (overrides $CARGO_REGISTRIES_CRATES_IO_INDEX\n\
         \x20                               and .cargo/config.toml source replacement)\n\
         \x20 --output <DIR>                write cache into a fresh CARGO_HOME-shaped dir\n\
         \x20 --cargo-home <DIR>            cache location (default: $CARGO_HOME or ~/.cargo)\n\
         \x20 --check                       report missing entries; do not fetch\n\
         \x20 -h, --help                    show this help"
    );
}
