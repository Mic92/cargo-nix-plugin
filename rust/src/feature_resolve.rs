// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Feature resolution: merge, expand, and propagate features across the dependency graph.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

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
    /// Per-edge optionality (cargo `FeatureResolver::activate_pkg` checks
    /// `dep.is_optional()` per edge). NOT `optional_deps.contains(name)`:
    /// the same name can be optional in `[dependencies]` and required in
    /// `[dev-dependencies]`. Feature *rules* (`dep:foo`, `foo/feat`) still
    /// key on the name-set.
    pub optional: bool,
}

/// Result of feature resolution: resolved features and active optional deps.
pub struct FeatureResolution {
    /// package_id -> set of resolved features
    pub features: HashMap<String, BTreeSet<String>>,
    /// (package_id, local_dep_name) pairs for activated optional deps
    pub active_optional_deps: HashSet<(String, String)>,
}

/// `root_packages` are the workspace members with their initially requested features.
/// Returns resolved features and active optional deps.
pub fn resolve_features(
    packages: &HashMap<String, PackageFeatureInfo>,
    root_packages: &[(String, Vec<String>)],
) -> FeatureResolution {
    let mut resolved: HashMap<String, BTreeSet<String>> = HashMap::new();
    // (package_id, local_dep_name). Distinct from `resolved`: `dep:foo`
    // activates the dep without creating a feature named "foo".
    let mut active_optional: HashSet<(String, String)> = HashSet::new();

    // Queue of (package_id, features_to_add), seeded with roots.
    let mut queue: Vec<(String, Vec<String>)> = root_packages.to_vec();

    while let Some((pkg_id, new_features)) = queue.pop() {
        let Some(pkg) = packages.get(&pkg_id) else {
            continue;
        };

        // Expand features for this package.
        let (expanded, activated_deps) =
            expand_features(&pkg.features, &new_features, &pkg.optional_deps);

        // Merge into both state stores. `first_visit` is tracked separately
        // from `added_new`: a crate that resolves to zero features (e.g.
        // seeded with ["default"] but has none) still needs its deps
        // propagated once.
        let first_visit = !resolved.contains_key(&pkg_id);
        let entry = resolved.entry(pkg_id.clone()).or_default();
        let mut added_new = false;
        for feat in &expanded {
            added_new |= entry.insert(feat.clone());
        }
        for dep in &activated_deps {
            added_new |= active_optional.insert((pkg_id.clone(), dep.clone()));
        }

        if !added_new && !first_visit {
            continue;
        }

        // Propagate to dependencies.
        let current_features = &resolved[&pkg_id];
        for dep in &pkg.dependencies {
            // Per-edge gate (cargo `activate_pkg`): a non-optional edge with
            // the same name as an inactive optional one still propagates.
            if dep.optional && !active_optional.contains(&(pkg_id.clone(), dep.name.clone())) {
                continue;
            }

            let mut dep_features: Vec<String> = dep.features.clone();
            if dep.uses_default_features {
                dep_features.push("default".to_string());
            }

            // `dep/feat` forwarding. Weak `dep?/feat` is safe to match here:
            // the gate above already ensured the dep is enabled.
            let strong = format!("{}/", dep.name);
            let weak = format!("{}?/", dep.name);
            for rule in current_features
                .iter()
                .filter_map(|f| pkg.features.get(f))
                .flatten()
            {
                if let Some(rest) = rule
                    .strip_prefix(&strong)
                    .or_else(|| rule.strip_prefix(&weak))
                {
                    dep_features.push(rest.to_string());
                }
            }

            queue.push((dep.package_id.clone(), dep_features));
        }
    }

    FeatureResolution {
        features: resolved,
        active_optional_deps: active_optional,
    }
}

/// Expand a set of feature names for a single package, following feature rules.
///
/// Returns (enabled features, activated optional deps). These are distinct
/// sets — `dep:foo` activates optional dep `foo` WITHOUT creating a feature
/// named `foo`, whereas legacy `["foo"]` does both.
///
/// See https://doc.rust-lang.org/cargo/reference/features.html#optional-dependencies
fn expand_features(
    features_map: &BTreeMap<String, Vec<String>>,
    initial: &[String],
    optional_deps: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    // For optional dep X, the *feature name* "X" is valid iff either it's an
    // explicit key in `features_map`, or it's the legacy implicit feature
    // (`dep:X` appears nowhere). See cargo reference § "Optional dependencies";
    // each case has a test below.
    let dep_prefix_used: BTreeSet<&str> = features_map
        .values()
        .flatten()
        .filter_map(|r| r.strip_prefix("dep:"))
        .collect();
    let is_valid_feature = |name: &str| -> bool {
        features_map.contains_key(name)
            || (optional_deps.contains(name) && !dep_prefix_used.contains(name))
    };

    let mut enabled = BTreeSet::new();
    let mut active_deps = BTreeSet::new();
    let mut work: Vec<String> = initial.to_vec();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    while let Some(item) = work.pop() {
        if !seen.insert(item.clone()) {
            continue;
        }

        // `dep:X` activates the optional dep. Never creates a self-feature.
        if let Some(dep_name) = item.strip_prefix("dep:") {
            active_deps.insert(dep_name.to_string());
            continue;
        }

        // `X/feat` (non-weak) on an optional dep activates the dep and
        // pushes "X" to expand as a feature (sticks iff `is_valid_feature`).
        // `X?/feat` activates nothing here.
        if let Some((lhs, _)) = item.split_once('/') {
            let (dep, weak) = lhs.strip_suffix('?').map_or((lhs, false), |d| (d, true));
            if !weak && optional_deps.contains(dep) {
                active_deps.insert(dep.to_string());
                work.push(dep.to_string());
            }
            continue;
        }

        // Plain feature name. Drop dangling references to suppressed
        // implicit features.
        if !is_valid_feature(&item) {
            continue;
        }
        enabled.insert(item.clone());

        // Legacy implicit feature activates the dep. An explicit `[features]`
        // key with the same name suppresses the implicit one (cargo's
        // `build_feature_map`: skip when `features.contains_key(&dep_name)`).
        if optional_deps.contains(&item)
            && !dep_prefix_used.contains(item.as_str())
            && !features_map.contains_key(&item)
        {
            active_deps.insert(item.clone());
        }

        // Follow feature rules.
        if let Some(rules) = features_map.get(&item) {
            work.extend(rules.iter().cloned());
        }
    }

    (enabled, active_deps)
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
                    optional: optional.contains(name),
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
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let feats = result_raw.features.get("A").unwrap();
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
        let result_raw = resolve_features(
            &packages,
            &[
                ("A".to_string(), vec!["default".to_string()]),
                ("B".to_string(), vec!["default".to_string()]),
            ],
        );
        let c_feats = result_raw.features.get("C").unwrap();
        assert!(c_feats.contains("x"), "C missing feature x: {:?}", c_feats);
        assert!(c_feats.contains("y"), "C missing feature y: {:?}", c_feats);
    }

    /// `dep:foo` activates the dep but does NOT create feature "foo".
    /// Verified: cargo with `use-foo = ["dep:foo"]` emits only
    /// ["default", "use-foo"] — no "foo".
    #[test]
    fn dep_syntax_activates_without_self_feature() {
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
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let a = result_raw.features.get("A").unwrap();
        assert!(a.contains("use-foo"));
        assert!(!a.contains("foo"), "dep: leaked self-feature: {a:?}");
        // But the dep itself was reached.
        assert!(result_raw.features.contains_key("foo-pkg"));
    }

    /// Legacy optional dep (no `dep:` anywhere) DOES create an implicit
    /// self-feature when activated by name. Verified: cargo emits "foo".
    #[test]
    fn legacy_optional_creates_implicit_feature() {
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                // no dep: anywhere — "foo" is the implicit feature
                &[("default", &["use-foo"]), ("use-foo", &["foo"])],
                &[("foo", "foo-pkg", true, &[])],
                &["foo"],
            ),
        );
        packages.insert(
            "foo-pkg".to_string(),
            make_package(&[("default", &[])], &[], &[]),
        );
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let a = result_raw.features.get("A").unwrap();
        assert!(a.contains("foo"), "legacy implicit missing: {a:?}");
        assert!(result_raw.features.contains_key("foo-pkg"));
    }

    /// `dep:X` + non-weak `X/feat` + NO explicit `X = [..]` → no "X" in
    /// features (suppressed). Verified: cargo emits only ["b-std", "default"].
    #[test]
    fn dep_prefix_suppresses_implicit_even_via_non_weak() {
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                &[("default", &["b-std"]), ("b-std", &["dep:b", "b/std"])],
                &[("b", "B", false, &[])],
                &["b"],
            ),
        );
        packages.insert("B".to_string(), make_package(&[("std", &[])], &[], &[]));
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let a = result_raw.features.get("A").unwrap();
        assert!(!a.contains("b"), "suppressed implicit leaked: {a:?}");
        assert!(result_raw.features.get("B").unwrap().contains("std"));
    }

    /// Enabling a feature that shadows an optional dep must expand its
    /// rules without pulling in the dep.
    #[test]
    fn explicit_feature_shadows_implicit_optional_dep() {
        let features_map: BTreeMap<String, Vec<String>> = [
            ("foo".to_string(), vec!["bar".to_string()]),
            ("bar".to_string(), vec![]),
            // `foo/feat` keeps the dep mentionable for cargo.
            ("turbo".to_string(), vec!["foo/feat".to_string()]),
        ]
        .into_iter()
        .collect();
        let optional: BTreeSet<String> = ["foo".to_string()].into_iter().collect();
        let (enabled, active) = expand_features(&features_map, &["foo".to_string()], &optional);
        assert!(
            enabled.contains("bar"),
            "feature rules must follow: {enabled:?}"
        );
        assert!(
            !active.contains("foo"),
            "shadowed feature `foo` must not activate dep `foo`: {active:?}"
        );
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
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let b_feats = result_raw.features.get("B").unwrap();
        assert!(b_feats.contains("x"), "B missing x: {:?}", b_feats);
        let c_feats = result_raw.features.get("C").unwrap();
        assert!(c_feats.contains("y"), "C missing y: {:?}", c_feats);
    }

    /// num-rational's `num-bigint-std = ["num-bigint/std"]`.
    ///
    /// `num-bigint` is an optional dep with an EXPLICIT same-named feature
    /// `num-bigint = ["dep:num-bigint"]`. Non-weak `num-bigint/std` activates
    /// the dep AND pushes "num-bigint" to expand — which is a valid feature
    /// (explicit key in the map), so it sticks and its rule `dep:num-bigint`
    /// fires too.
    ///
    /// Verified against `cargo metadata`: num-rational with
    /// `num-bigint-std` + `std` resolves to `["num-bigint", "num-bigint-std", "std"]`.
    #[test]
    fn non_weak_enables_explicit_same_named_feature() {
        let mut packages = HashMap::new();
        packages.insert(
            "num-rational".to_string(),
            make_package(
                &[
                    ("num-bigint", &["dep:num-bigint"]),
                    ("num-bigint-std", &["num-bigint/std"]),
                    ("std", &["num-bigint?/std"]),
                ],
                &[("num-bigint", "num-bigint-pkg", false, &[])],
                &["num-bigint"],
            ),
        );
        packages.insert(
            "num-bigint-pkg".to_string(),
            make_package(&[("std", &[])], &[], &[]),
        );

        let result_raw = resolve_features(
            &packages,
            &[(
                "num-rational".to_string(),
                vec!["num-bigint-std".to_string(), "std".to_string()],
            )],
        );

        let feats = result_raw.features.get("num-rational").unwrap();
        assert!(feats.contains("num-bigint-std"));
        assert!(feats.contains("std"));
        assert!(
            feats.contains("num-bigint"),
            "non-weak dep/feat must enable explicit feature 'num-bigint': {feats:?}"
        );
        // And the dep itself got std.
        assert!(result_raw
            .features
            .get("num-bigint-pkg")
            .unwrap()
            .contains("std"));
    }

    /// Weak `dep?/feat` must NOT activate the optional dep or its
    /// self-feature — it only forwards `feat` if something else already
    /// enabled the dep.
    #[test]
    fn weak_dep_feature_does_not_enable_self_feature() {
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                &[("std", &["opt?/std"])],
                &[("opt", "opt-pkg", false, &[])],
                &["opt"],
            ),
        );
        packages.insert(
            "opt-pkg".to_string(),
            make_package(&[("std", &[])], &[], &[]),
        );

        let result_raw = resolve_features(&packages, &[("A".to_string(), vec!["std".to_string()])]);

        let feats = result_raw.features.get("A").unwrap();
        assert!(feats.contains("std"));
        assert!(
            !feats.contains("opt"),
            "weak dep?/feat leaked self-feature: {feats:?}"
        );
        // opt-pkg should not even be in the graph.
        assert!(!result_raw.features.contains_key("opt-pkg"));
    }

    /// Same dep name, two edges: optional in `[dependencies]`, required
    /// in `[dev-dependencies]`. The required edge must propagate even
    /// though the optional one is inactive. Regression: gating on the
    /// name-set `optional_deps` skipped BOTH edges.
    ///
    /// Ground truth: tests/fixtures/feature-dev-shadows-optional —
    /// `cargo metadata` resolves leaf to ["d","default","extra"].
    #[test]
    fn required_edge_not_shadowed_by_optional_same_name() {
        let mut packages = HashMap::new();
        packages.insert(
            "root".to_string(),
            PackageFeatureInfo {
                features: [("with-leaf".to_string(), vec!["dep:leaf".to_string()])]
                    .into_iter()
                    .collect(),
                dependencies: vec![
                    DepFeatureInfo {
                        name: "leaf".into(),
                        package_id: "leaf".into(),
                        uses_default_features: false,
                        features: vec![],
                        optional: true,
                    },
                    DepFeatureInfo {
                        name: "leaf".into(),
                        package_id: "leaf".into(),
                        uses_default_features: true,
                        features: vec!["extra".into()],
                        optional: false,
                    },
                ],
                optional_deps: ["leaf".to_string()].into_iter().collect(),
            },
        );
        packages.insert(
            "leaf".to_string(),
            make_package(&[("default", &["d"]), ("d", &[]), ("extra", &[])], &[], &[]),
        );

        let result = resolve_features(&packages, &[("root".into(), vec!["default".into()])]);
        let leaf = result
            .features
            .get("leaf")
            .expect("leaf reached via dev-dep");
        for f in ["default", "d", "extra"] {
            assert!(leaf.contains(f), "leaf missing {f}: {leaf:?}");
        }
        // with-leaf was NOT enabled, so the optional normal-dep edge stays
        // inactive — the build-time filter must still drop it.
        assert!(!result
            .active_optional_deps
            .contains(&("root".into(), "leaf".into())));
    }

    /// Regression: rustls `[dependencies.webpki] package = "rustls-webpki"`.
    /// Feature rules use the local dep name (`"webpki/ring"`), not the
    /// actual crate name. DepFeatureInfo.name must carry the rename so
    /// forwarding matches.
    #[test]
    fn dep_feature_forwarding_uses_rename() {
        let mut packages = HashMap::new();
        packages.insert(
            "rustls".to_string(),
            make_package(
                // rule references "webpki" (local name), not "rustls-webpki"
                &[("ring", &["webpki/ring"])],
                // dep.name must be the local rename, not the crate name
                &[("webpki", "rustls-webpki", false, &["alloc"])],
                &[],
            ),
        );
        packages.insert(
            "rustls-webpki".to_string(),
            make_package(&[("alloc", &[]), ("ring", &[])], &[], &[]),
        );

        let result_raw = resolve_features(
            &packages,
            &[("rustls".to_string(), vec!["ring".to_string()])],
        );

        let webpki = result_raw.features.get("rustls-webpki").unwrap();
        assert!(webpki.contains("alloc"));
        assert!(
            webpki.contains("ring"),
            "dep/feat forward dropped under rename: {webpki:?}"
        );
    }

    /// Weak `dep?/feat` *does* forward once the dep is independently enabled.
    /// This is num-rational's `std = ["num-bigint?/std"]`: if num-bigint-std
    /// activates the dep, then std's weak rule must forward std to it.
    #[test]
    fn weak_dep_feature_forwards_when_dep_enabled() {
        let mut packages = HashMap::new();
        packages.insert(
            "A".to_string(),
            make_package(
                &[("use-opt", &["dep:opt"]), ("std", &["opt?/std"])],
                &[("opt", "opt-pkg", false, &[])],
                &["opt"],
            ),
        );
        packages.insert(
            "opt-pkg".to_string(),
            make_package(&[("std", &[])], &[], &[]),
        );

        let result_raw = resolve_features(
            &packages,
            &[(
                "A".to_string(),
                vec!["use-opt".to_string(), "std".to_string()],
            )],
        );

        // use-opt enabled the dep; std's weak rule must now forward.
        let opt = result_raw
            .features
            .get("opt-pkg")
            .expect("opt-pkg in graph");
        assert!(opt.contains("std"), "weak forward dropped: {opt:?}");
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
        let result_raw =
            resolve_features(&packages, &[("A".to_string(), vec!["default".to_string()])]);
        let b_feats = result_raw.features.get("B").unwrap();
        assert!(
            b_feats.contains("default"),
            "B missing default: {:?}",
            b_feats
        );
        assert!(b_feats.contains("net"), "B missing net: {:?}", b_feats);
    }

    /// A featureless crate in the middle of the graph must still propagate
    /// to its deps. Regression: seeding a crate with ["default"] when it
    /// has no `default` key yields expanded={} — the skip-unchanged check
    /// was short-circuiting before the propagation loop, so its deps were
    /// never visited.
    ///
    /// Real-world case: aws-sdk-ec2 → regex-lite. regex-lite requires std
    /// (compile_error! otherwise) and gets it via default → [std, string].
    /// aws-sdk-ec2 depends with plain default-features=true.
    #[test]
    fn featureless_middle_crate_propagates_to_deps() {
        let mut packages = HashMap::new();
        // Root has features; pulls middle with default-features=true.
        packages.insert(
            "root".to_string(),
            make_package(&[("default", &[])], &[("middle", "middle", true, &[])], &[]),
        );
        // Middle has NO feature keys at all. "default" is not valid here.
        packages.insert(
            "middle".to_string(),
            make_package(&[], &[("leaf", "leaf", true, &[])], &[]),
        );
        // Leaf's std is gated on default — regex-lite's shape.
        packages.insert(
            "leaf".to_string(),
            make_package(&[("default", &["std"]), ("std", &[])], &[], &[]),
        );

        let result_raw = resolve_features(
            &packages,
            &[("root".to_string(), vec!["default".to_string()])],
        );

        // Middle was visited, resolved to empty (correct per cargo).
        assert_eq!(result_raw.features.get("middle"), Some(&BTreeSet::new()));
        // Leaf was REACHED via middle and got default → std.
        let leaf = result_raw
            .features
            .get("leaf")
            .expect("leaf must be reached via middle");
        assert!(leaf.contains("std"), "leaf missing std: {leaf:?}");
    }

    /// The other half: a crate with no features visited TWICE (from two
    /// dependers) must not re-propagate. first_visit must be false on
    /// the second pass even though the entry is still empty.
    #[test]
    fn featureless_crate_not_reprocessed_on_second_visit() {
        let mut packages = HashMap::new();
        packages.insert(
            "a".to_string(),
            make_package(&[("default", &[])], &[("m", "m", true, &[])], &[]),
        );
        packages.insert(
            "b".to_string(),
            make_package(&[("default", &[])], &[("m", "m", true, &[])], &[]),
        );
        packages.insert(
            "m".to_string(),
            make_package(&[], &[("leaf", "leaf", true, &[])], &[]),
        );
        packages.insert(
            "leaf".to_string(),
            make_package(&[("default", &["x"]), ("x", &[])], &[], &[]),
        );

        let result_raw = resolve_features(
            &packages,
            &[
                ("a".to_string(), vec!["default".to_string()]),
                ("b".to_string(), vec!["default".to_string()]),
            ],
        );
        // m reached once is enough; leaf gets x.
        assert!(result_raw.features.get("leaf").unwrap().contains("x"));
        // Weak assertion but proves we didn't blow up or loop.
        assert_eq!(result_raw.features.get("m"), Some(&BTreeSet::new()));
    }
}
