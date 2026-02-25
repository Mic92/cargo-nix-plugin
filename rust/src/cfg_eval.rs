//! Evaluate cfg() target expressions against a target description.

use cargo_platform::{Cfg, Platform};
use serde::{Deserialize, Serialize};

/// Description of a target platform for cfg() evaluation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetDescription {
    pub name: String,
    pub os: String,
    pub arch: String,
    pub vendor: String,
    pub env: String,
    pub family: Vec<String>,
    pub pointer_width: String,
    pub endian: String,
    pub unix: bool,
    pub windows: bool,
}

/// Build the list of `Cfg` values that rustc would report for this target.
pub fn target_cfgs(target: &TargetDescription) -> Vec<Cfg> {
    let mut cfgs = vec![
        Cfg::KeyPair("target_os".to_string(), target.os.clone()),
        Cfg::KeyPair("target_arch".to_string(), target.arch.clone()),
        Cfg::KeyPair("target_vendor".to_string(), target.vendor.clone()),
        Cfg::KeyPair("target_env".to_string(), target.env.clone()),
        Cfg::KeyPair(
            "target_pointer_width".to_string(),
            target.pointer_width.clone(),
        ),
        Cfg::KeyPair("target_endian".to_string(), target.endian.clone()),
    ];
    for fam in &target.family {
        cfgs.push(Cfg::KeyPair("target_family".to_string(), fam.clone()));
    }
    if target.unix {
        cfgs.push(Cfg::Name("unix".to_string()));
    }
    if target.windows {
        cfgs.push(Cfg::Name("windows".to_string()));
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
            family: vec!["unix".to_string()],
            pointer_width: "64".to_string(),
            endian: "little".to_string(),
            unix: true,
            windows: false,
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
            Platform::from_str("cfg(any(target_os = \"windows\", target_os = \"linux\"))")
                .unwrap();
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
}
