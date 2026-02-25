//! Filter dependencies based on cfg() conditions, optional status, and features.

use cargo_metadata::Dependency;

use crate::cfg_eval::{matches_target, TargetDescription};

/// Filter a list of dependencies, keeping only those that match the target
/// and are either non-optional or enabled by the given features.
pub fn filter_dependencies(
    deps: &[Dependency],
    enabled_features: &[String],
    target: &TargetDescription,
) -> Vec<usize> {
    deps.iter()
        .enumerate()
        .filter(|(_, dep)| dep_matches(dep, enabled_features, target))
        .map(|(i, _)| i)
        .collect()
}

/// Check if a dependency should be included based on target and optional status.
fn dep_matches(dep: &Dependency, enabled_features: &[String], target: &TargetDescription) -> bool {
    // Check platform condition
    if let Some(ref platform) = dep.target {
        if !matches_target(platform, target) {
            return false;
        }
    }

    // Check optional: only include if a feature enables this dep
    if dep.optional && !enabled_features.contains(&dep.name) {
        return false;
    }

    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use cargo_metadata::Dependency;
    use cargo_platform::Platform;
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

    fn make_dep(name: &str, optional: bool, target: Option<&str>) -> Dependency {
        let mut dep: Dependency = serde_json::from_value(serde_json::json!({
            "name": name,
            "req": "*",
            "kind": null,
            "optional": optional,
            "uses_default_features": true,
            "features": [],
            "rename": null,
            "registry": null,
            "path": null,
            "source": null
        }))
        .unwrap();
        if let Some(t) = target {
            dep.target = Some(Platform::from_str(t).unwrap());
        }
        dep
    }

    #[test]
    fn cfg_matching_dep_included() {
        let deps = [make_dep("libc", false, Some("cfg(unix)"))];
        let indices = filter_dependencies(&deps, &[], &linux_x86_64());
        assert_eq!(indices.len(), 1);
        assert_eq!(deps[indices[0]].name, "libc");
    }

    #[test]
    fn cfg_non_matching_dep_excluded() {
        let deps = [make_dep("winapi", false, Some("cfg(windows)"))];
        let indices = filter_dependencies(&deps, &[], &linux_x86_64());
        assert_eq!(indices.len(), 0);
    }

    #[test]
    fn unconditional_dep_always_included() {
        let deps = [make_dep("serde", false, None)];
        let indices = filter_dependencies(&deps, &[], &linux_x86_64());
        assert_eq!(indices.len(), 1);
    }

    #[test]
    fn optional_dep_excluded_without_feature() {
        let deps = [make_dep("tokio", true, None)];
        let indices = filter_dependencies(&deps, &[], &linux_x86_64());
        assert_eq!(indices.len(), 0);
    }

    #[test]
    fn optional_dep_included_when_feature_enables_it() {
        let deps = [make_dep("tokio", true, None)];
        let features = vec!["tokio".to_string()];
        let indices = filter_dependencies(&deps, &features, &linux_x86_64());
        assert_eq!(indices.len(), 1);
    }

    #[test]
    fn renamed_dep_filtered_correctly() {
        let mut dep = make_dep("hyper", false, None);
        dep.rename = Some("hyper_v0_14".to_string());
        let deps = [dep];
        let indices = filter_dependencies(&deps, &[], &linux_x86_64());
        assert_eq!(indices.len(), 1);
        assert_eq!(deps[indices[0]].name, "hyper");
        assert_eq!(deps[indices[0]].rename, Some("hyper_v0_14".to_string()));
    }
}
