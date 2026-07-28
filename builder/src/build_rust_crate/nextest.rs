// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! binaries-metadata.json for nextest's reuse-build path. It maps each
//! binary id to an installed test binary and is written at install
//! time. The paired cargo-metadata.json comes from lib/nextest.nix.
//! Package ids agree between the two files via [`FAKE_ROOT`].

use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

use super::build::{resolve_bins, resolve_tests};
use super::config::BuildConfig;

/// Must match `fakeRoot` in lib/nextest.nix.
const FAKE_ROOT: &str = "/cargo-nix-plugin-metadata/ws";

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct BinariesMetadata {
    rust_build_meta: RustBuildMeta,
    rust_binaries: BTreeMap<String, RustBinary>,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct RustBuildMeta {
    target_directory: &'static str,
    base_output_directories: [&'static str; 1],
    non_test_binaries: BTreeMap<String, String>,
    build_script_out_dirs: BTreeMap<String, String>,
    linked_paths: [&'static str; 0],
    platforms: Platforms,
    target_platforms: [Platform; 1],
    target_platform: Option<String>,
}

#[derive(Serialize)]
struct Platforms {
    host: HostPlatform,
    targets: [Platform; 0],
}

#[derive(Serialize)]
struct HostPlatform {
    platform: Platform,
    libdir: Libdir,
}

#[derive(Serialize, Clone)]
#[serde(rename_all = "kebab-case")]
struct Platform {
    triple: String,
    target_features: &'static str,
}

#[derive(Serialize)]
struct Libdir {
    status: &'static str,
    reason: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "kebab-case")]
struct RustBinary {
    binary_id: String,
    binary_name: String,
    package_id: String,
    kind: &'static str,
    binary_path: String,
    build_platform: &'static str,
}

/// Write `$out/nix-support/binaries-metadata.json` for the binaries in
/// `tests_dir`. Targets whose binary is absent were skipped for
/// missing required-features and are omitted. Targets with
/// `harness = false` are omitted too, because nextest can only run
/// binaries that implement the libtest listing protocol. Such targets
/// remain covered by runTests.
pub fn write_binaries_metadata(
    config: &BuildConfig,
    tests_dir: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let pkg = &config.crate_name;
    let member = config.workspace_member.as_deref().unwrap_or(".");
    let fake_dir = if member == "." {
        FAKE_ROOT.to_string()
    } else {
        format!("{FAKE_ROOT}/{member}")
    };
    let package_id = format!("path+file://{fake_dir}#{pkg}@{}", config.crate_version);

    // (binary-id, target name, filename, kind)
    let mut targets: Vec<(String, String, String, &'static str)> = Vec::new();
    let lib_test = format!("{}-{}", config.lib_name_normalized(), config.metadata);
    targets.push((pkg.clone(), config.lib_name_normalized(), lib_test, "lib"));
    for (name, _) in resolve_bins(config) {
        targets.push((format!("{pkg}::bin/{name}"), name.clone(), name, "bin"));
    }
    for (name, _, harness) in resolve_tests(config) {
        if harness {
            targets.push((format!("{pkg}::{name}"), name.clone(), name, "test"));
        }
    }

    let rust_binaries = targets
        .into_iter()
        .filter(|(_, _, file, _)| Path::new(tests_dir).join(file).is_file())
        .map(|(id, name, file, kind)| {
            (
                id.clone(),
                RustBinary {
                    binary_id: id,
                    binary_name: name,
                    package_id: package_id.clone(),
                    kind,
                    binary_path: format!("{tests_dir}/{file}"),
                    build_platform: "target",
                },
            )
        })
        .collect();

    let platform = Platform {
        triple: config.host_platform.rustc_target_spec.clone(),
        target_features: "unknown",
    };
    let meta = BinariesMetadata {
        rust_build_meta: RustBuildMeta {
            target_directory: "/nonexistent",
            base_output_directories: ["debug"],
            non_test_binaries: BTreeMap::new(),
            build_script_out_dirs: BTreeMap::new(),
            linked_paths: [],
            platforms: Platforms {
                host: HostPlatform {
                    platform: platform.clone(),
                    libdir: Libdir {
                        status: "unavailable",
                        reason: "not-requested",
                    },
                },
                targets: [],
            },
            target_platforms: [platform],
            target_platform: None,
        },
        rust_binaries,
    };

    let support = format!("{}/nix-support", config.out_path());
    std::fs::create_dir_all(&support)?;
    std::fs::write(
        format!("{support}/binaries-metadata.json"),
        serde_json::to_string_pretty(&meta)?,
    )?;
    Ok(())
}
