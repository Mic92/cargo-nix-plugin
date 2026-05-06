// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Evaluate cfg() target expressions against a target description.

use cargo_platform::{Cfg, Ident, Platform};
use serde::{Deserialize, Serialize};

/// Description of a target platform for cfg() evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetDescription {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub vendor: String,
    pub env: String,
    /// `target_abi` (e.g. `"eabihf"` for armv7-…-gnueabihf, `""` for most
    /// targets). rustc has emitted this since 1.78; cargo evaluates it for
    /// `[target.'cfg(target_abi = …)']`. Defaults to `""` for callers that
    /// predate the field.
    #[serde(default)]
    pub abi: String,
    pub family: Vec<String>,
    pub pointer_width: String,
    pub endian: String,
    pub unix: bool,
    pub windows: bool,
    /// Additional bare cfg names (e.g. `my_platform`) to set during
    /// `[target.'cfg(...)']` dependency resolution — equivalent to
    /// `RUSTFLAGS="--cfg foo"` at cargo-metadata time. Pair with passing the
    /// same `--cfg` via rustc opts so `#[cfg(foo)]` in source compiles too.
    #[serde(default)]
    pub extra_cfgs: Vec<String>,
}

/// Create a non-raw `Ident` from a string.
fn ident(name: &str) -> Ident {
    Ident {
        name: name.to_string(),
        raw: false,
    }
}

/// Build the list of `Cfg` values that rustc would report for this target.
pub fn target_cfgs(target: &TargetDescription) -> Vec<Cfg> {
    let mut cfgs = vec![
        Cfg::KeyPair(ident("target_os"), target.os.clone()),
        Cfg::KeyPair(ident("target_arch"), target.arch.clone()),
        Cfg::KeyPair(ident("target_vendor"), target.vendor.clone()),
        Cfg::KeyPair(ident("target_env"), target.env.clone()),
        Cfg::KeyPair(ident("target_pointer_width"), target.pointer_width.clone()),
        Cfg::KeyPair(ident("target_endian"), target.endian.clone()),
        Cfg::KeyPair(ident("target_abi"), target.abi.clone()),
    ];
    for fam in &target.family {
        cfgs.push(Cfg::KeyPair(ident("target_family"), fam.clone()));
    }
    if target.unix {
        cfgs.push(Cfg::Name(ident("unix")));
    }
    if target.windows {
        cfgs.push(Cfg::Name(ident("windows")));
    }
    for name in &target.extra_cfgs {
        cfgs.push(Cfg::Name(ident(name)));
    }
    cfgs
}

/// Evaluate whether a Platform (cfg expression or named triple) matches the target.
pub fn matches_target(platform: &Platform, target: &TargetDescription) -> bool {
    let cfgs = target_cfgs(target);
    platform.matches(&target.name, &cfgs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn linux_x86_64() -> TargetDescription {
        TargetDescription {
            name: "x86_64-unknown-linux-gnu".to_string(),
            os: "linux".to_string(),
            arch: "x86_64".to_string(),
            vendor: "unknown".to_string(),
            env: "gnu".to_string(),
            abi: "".to_string(),
            family: vec!["unix".to_string()],
            pointer_width: "64".to_string(),
            endian: "little".to_string(),
            unix: true,
            windows: false,
            extra_cfgs: vec![],
        }
    }

    #[test]
    fn cfg_target_os_linux_matches() {
        let platform = Platform::from_str("cfg(target_os = \"linux\")").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_windows_does_not_match_linux() {
        let platform = Platform::from_str("cfg(windows)").unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_unix_matches_linux() {
        let platform = Platform::from_str("cfg(unix)").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_all_matches() {
        let platform =
            Platform::from_str("cfg(all(target_os = \"linux\", target_arch = \"x86_64\"))")
                .unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_all_partial_mismatch() {
        let platform =
            Platform::from_str("cfg(all(target_os = \"linux\", target_arch = \"aarch64\"))")
                .unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_any_matches() {
        let platform =
            Platform::from_str("cfg(any(target_os = \"windows\", target_os = \"linux\"))").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_not_matches() {
        let platform = Platform::from_str("cfg(not(windows))").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_not_does_not_match() {
        let platform = Platform::from_str("cfg(not(unix))").unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn bare_target_triple_matches() {
        let platform = Platform::from_str("x86_64-unknown-linux-gnu").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn bare_target_triple_does_not_match() {
        let platform = Platform::from_str("aarch64-linux-android").unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_target_env_gnu() {
        let platform = Platform::from_str("cfg(target_env = \"gnu\")").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_target_pointer_width() {
        let platform = Platform::from_str("cfg(target_pointer_width = \"64\")").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_target_endian() {
        let platform = Platform::from_str("cfg(target_endian = \"little\")").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn cfg_target_vendor() {
        let platform = Platform::from_str("cfg(target_vendor = \"unknown\")").unwrap();
        assert!(matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn extra_cfg_matches() {
        let target = TargetDescription {
            extra_cfgs: vec!["my_platform".to_string()],
            ..linux_x86_64()
        };
        let platform = Platform::from_str("cfg(my_platform)").unwrap();
        assert!(matches_target(&platform, &target));
    }

    #[test]
    fn extra_cfg_absent_does_not_match() {
        let platform = Platform::from_str("cfg(my_platform)").unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }

    #[test]
    fn extra_cfg_composes_with_all() {
        let target = TargetDescription {
            extra_cfgs: vec!["my_platform".to_string()],
            ..linux_x86_64()
        };
        let platform = Platform::from_str("cfg(all(target_os = \"linux\", my_platform))").unwrap();
        assert!(matches_target(&platform, &target));
    }

    /// armv7-…-gnueabihf has `target_abi = "eabihf"`. If the cfg is
    /// never emitted, `cfg(target_abi = …)` deps are silently dropped.
    #[test]
    fn cfg_target_abi_eabihf() {
        let target = TargetDescription {
            name: "armv7-unknown-linux-gnueabihf".to_string(),
            arch: "arm".to_string(),
            abi: "eabihf".to_string(),
            ..linux_x86_64()
        };
        let platform = Platform::from_str("cfg(target_abi = \"eabihf\")").unwrap();
        assert!(matches_target(&platform, &target));
    }

    #[test]
    fn cfg_target_abi_does_not_match_wrong_value() {
        let platform = Platform::from_str("cfg(target_abi = \"eabihf\")").unwrap();
        assert!(!matches_target(&platform, &linux_x86_64()));
    }
}
