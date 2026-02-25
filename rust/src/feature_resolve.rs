//! Feature resolution: merge, expand, and propagate features across the dependency graph.

use std::collections::{BTreeMap, BTreeSet, HashMap};

/// A simplified view of a package for feature resolution purposes.
#[derive(Debug, Clone)]
pub struct PackageFeatureInfo {
    /// The features map: feature_name -> list of feature rules
    pub features: BTreeMap<String, Vec<String>>,
    /// Dependencies with the features they request: (dep_name, default_features, requested_features)
    pub dependencies: Vec<DepFeatureInfo>,
    /// Names of optional dependencies
    pub optional_deps: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct DepFeatureInfo {
    pub name: String,
    /// The package ID this resolves to
    pub package_id: String,
    pub uses_default_features: bool,
    pub features: Vec<String>,
}

/// Resolve features for all packages in the graph.
///
/// `root_packages` are the workspace members with their initially requested features.
/// Returns a map of package_id -> set of resolved features.
pub fn resolve_features(
    packages: &HashMap<String, PackageFeatureInfo>,
    root_packages: &[(String, Vec<String>)],
) -> HashMap<String, BTreeSet<String>> {
    let mut resolved: HashMap<String, BTreeSet<String>> = HashMap::new();

    // Queue of (package_id, features_to_add)
    let mut queue: Vec<(String, Vec<String>)> = Vec::new();

    // Seed the queue with root packages
    for (pkg_id, features) in root_packages {
        queue.push((pkg_id.clone(), features.clone()));
    }

    while let Some((pkg_id, new_features)) = queue.pop() {
        let Some(pkg) = packages.get(&pkg_id) else {
            continue;
        };

        // Expand features for this package
        let expanded = expand_features(&pkg.features, &new_features, &pkg.optional_deps);

        // Check if we're adding anything new
        let entry = resolved.entry(pkg_id.clone()).or_default();
        let mut added_new = false;
        for feat in &expanded {
            if entry.insert(feat.clone()) {
                added_new = true;
            }
        }

        if !added_new && resolved.contains_key(&pkg_id) {
            continue;
        }

        // Process dependencies: propagate features
        let current_features = resolved.get(&pkg_id).cloned().unwrap_or_default();
        for dep in &pkg.dependencies {
            // Skip optional deps that are not enabled
            if pkg.optional_deps.contains(&dep.name) && !current_features.contains(&dep.name) {
                continue;
            }

            let mut dep_features: Vec<String> = dep.features.clone();
            if dep.uses_default_features {
                dep_features.push("default".to_string());
            }

            // Check for dep/feat forwarding: if this package has a feature
            // that forwards to a dep feature (e.g. "x" enables "dep_name/feat")
            for feat in &current_features {
                if let Some(rules) = pkg.features.get(feat.as_str()) {
                    for rule in rules {
                        if let Some(rest) = rule.strip_prefix(&format!("{}/", dep.name)) {
                            dep_features.push(rest.to_string());
                        }
                    }
                }
            }

            queue.push((dep.package_id.clone(), dep_features));
        }
    }

    resolved
}

/// Expand a set of feature names for a single package, following feature rules.
/// Returns the full set of enabled features (including transitively enabled ones).
fn expand_features(
    features_map: &BTreeMap<String, Vec<String>>,
    initial: &[String],
    optional_deps: &BTreeSet<String>,
) -> BTreeSet<String> {
    let mut enabled = BTreeSet::new();
    let mut work: Vec<String> = initial.to_vec();

    while let Some(feat) = work.pop() {
        if !enabled.insert(feat.clone()) {
            continue; // already processed
        }

        // Handle "dep:name" syntax — activates the optional dep as a feature
        if let Some(dep_name) = feat.strip_prefix("dep:") {
            if !enabled.insert(dep_name.to_string()) {
                continue;
            }
            // Also expand the dep name if it's in the features map
            if let Some(rules) = features_map.get(dep_name) {
                work.extend(rules.iter().cloned());
            }
            continue;
        }

        // If this feature name matches an optional dep, it implicitly enables it
        if optional_deps.contains(&feat) {
            // The dep is enabled; no further expansion needed for the dep name itself
            // unless there are feature rules for it
        }

        // Follow feature rules
        if let Some(rules) = features_map.get(&feat) {
            for rule in rules {
                // Skip dep/feat forwarding here — handled during dependency propagation
                if rule.contains('/') && !rule.starts_with("dep:") {
                    continue;
                }
                work.push(rule.clone());
            }
        }
    }

    enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_package(
        features: &[(&str, &[&str])],
        deps: &[(&str, &str, bool, &[&str])],
        optional: &[&str],
    ) -> PackageFeatureInfo {
        PackageFeatureInfo {
            features: features
                .iter()
                .map(|(k, v)| (k.to_string(), v.iter().map(|s| s.to_string()).collect()))
                .collect(),
            dependencies: deps
                .iter()
                .map(|(name, pkg_id, default_feats, feats)| DepFeatureInfo {
                    name: name.to_string(),
                    package_id: pkg_id.to_string(),
                    uses_default_features: *default_feats,
                    features: feats.iter().map(|s| s.to_string()).collect(),
                })
                .collect(),
            optional_deps: optional.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn feature_enables_other_features() {
        // "default" enables "foo", "foo" enables "bar"
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                &[("default", &["foo"]), ("foo", &["bar"]), ("bar", &[])],
                &[],
                &[],
            ),
        );
        let result = resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let feats = result.get("A").unwrap();
        assert!(feats.contains("default"));
        assert!(feats.contains("foo"));
        assert!(feats.contains("bar"));
    }

    #[test]
    fn feature_unification_across_dependents() {
        // A depends on C with feature "x", B depends on C with feature "y".
        // Both A and B are roots. C should have both "x" and "y".
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(&[("default", &[])], &[("c", "C", false, &["x"])], &[]),
        );
        packages.insert(
            "B".to_string(),
            make_package(&[("default", &[])], &[("c", "C", false, &["y"])], &[]),
        );
        packages.insert(
            "C".to_string(),
            make_package(&[("default", &[]), ("x", &[]), ("y", &[])], &[], &[]),
        );
        let result = resolve_features(
            &packages,
            &[
                ("A".to_string(), vec!["default".to_string()]),
                ("B".to_string(), vec!["default".to_string()]),
            ],
        );
        let c_feats = result.get("C").unwrap();
        assert!(c_feats.contains("x"), "C missing feature x: {:?}", c_feats);
        assert!(c_feats.contains("y"), "C missing feature y: {:?}", c_feats);
    }

    #[test]
    fn optional_dep_activation_via_dep_syntax() {
        // Feature "use-foo" enables "dep:foo" (optional dep)
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                &[("default", &["use-foo"]), ("use-foo", &["dep:foo"])],
                &[("foo", "foo-pkg", true, &[])],
                &["foo"],
            ),
        );
        packages.insert(
            "foo-pkg".to_string(),
            make_package(&[("default", &[])], &[], &[]),
        );
        let result = resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let a_feats = result.get("A").unwrap();
        assert!(
            a_feats.contains("use-foo"),
            "A missing use-foo: {:?}",
            a_feats
        );
        // The optional dep "foo" should be activated
        assert!(a_feats.contains("foo"), "A missing foo: {:?}", a_feats);
        // foo-pkg should have been reached and resolved
        assert!(result.contains_key("foo-pkg"));
    }

    #[test]
    fn transitive_feature_propagation() {
        // A depends on B with feature "x", B's feature "x" enables feature "y" on C
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(&[("default", &[])], &[("b", "B", false, &["x"])], &[]),
        );
        packages.insert(
            "B".to_string(),
            make_package(&[("x", &["c/y"])], &[("c", "C", false, &[])], &[]),
        );
        packages.insert("C".to_string(), make_package(&[("y", &[])], &[], &[]));
        let result = resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let b_feats = result.get("B").unwrap();
        assert!(b_feats.contains("x"), "B missing x: {:?}", b_feats);
        let c_feats = result.get("C").unwrap();
        assert!(c_feats.contains("y"), "C missing y: {:?}", c_feats);
    }

    #[test]
    fn default_features_propagated_when_uses_default() {
        // A depends on B with uses_default_features=true
        // B has default = ["net"]
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(&[("default", &[])], &[("b", "B", true, &[])], &[]),
        );
        packages.insert(
            "B".to_string(),
            make_package(&[("default", &["net"]), ("net", &[])], &[], &[]),
        );
        let result = resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let b_feats = result.get("B").unwrap();
        assert!(
            b_feats.contains("default"),
            "B missing default: {:?}",
            b_feats
        );
        assert!(b_feats.contains("net"), "B missing net: {:?}", b_feats);
    }
}
