//! Read cargo's `[source]` replacement configuration so the lockfile
//! resolver hits the same registry mirror that `cargo` itself would.
//!
//! Cargo's source-replacement mechanism is documented at
//! <https://doc.rust-lang.org/cargo/reference/source-replacement.html>.
//! The shape we care about is:
//!
//! ```toml
//! [source.crates-io]
//! replace-with = "mirror"
//!
//! [source.mirror]
//! registry = "sparse+https://artifactory.example/api/cargo/crates/index/"
//! ```
//!
//! We follow the `replace-with` chain (cargo allows multiple hops) and
//! return the terminal source's `registry` URL. Only sparse/HTTP
//! registries are useful here — `local-registry` / `directory` / `git`
//! replacements are returned as-is and the caller decides what to do.

use std::path::Path;

/// Result of resolving the `[source.crates-io]` replacement chain.
#[derive(Debug, PartialEq, Eq)]
pub enum SourceReplacement {
    /// No replacement configured — use the upstream crates.io index.
    None,
    /// Replaced with a registry at this index URL (may or may not have
    /// the `sparse+` prefix; caller normalizes).
    Registry(String),
    /// Replaced with a non-registry source (local-registry, directory,
    /// git). We can't fetch sparse-index metadata from these, but
    /// surfacing the kind lets callers produce a useful error.
    Unsupported { kind: &'static str },
}

/// Enumerate every `.cargo/config.toml` cargo would read, in the order
/// it merges them (nearest first): walking up from `start`, then
/// `$CARGO_HOME`. Unlike cargo we don't merge — callers iterate and
/// take the first hit — but yielding *all* of them means a
/// workspace-local config without a `[source]` section doesn't shadow a
/// mirror configured in `$CARGO_HOME/config.toml`.
///
/// The legacy extensionless `.cargo/config` (deprecated since 1.39,
/// hard-warned since 1.79) is intentionally not consulted.
pub fn find_configs(start: &Path, cargo_home: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut push_if_file = |p: std::path::PathBuf| {
        if p.is_file() {
            out.push(p);
        }
    };
    let mut dir = Some(start);
    while let Some(d) = dir {
        push_if_file(d.join(".cargo").join("config.toml"));
        dir = d.parent();
    }
    push_if_file(cargo_home.join("config.toml"));
    out
}

/// Parse a cargo config TOML string and resolve where `crates-io` is
/// redirected to, following `replace-with` chains. Also honours
/// `[registries.crates-io] index = "..."`, the config-file twin of
/// `$CARGO_REGISTRIES_CRATES_IO_INDEX`.
pub fn crates_io_replacement(config: &str) -> SourceReplacement {
    let Ok(doc) = toml::from_str::<toml::Value>(config) else {
        return SourceReplacement::None;
    };

    // `[source.crates-io] replace-with` takes precedence over
    // `[registries.crates-io] index` in cargo, so resolve sources first.
    if let Some(r) = resolve_source_chain(&doc) {
        return r;
    }

    if let Some(url) = doc
        .get("registries")
        .and_then(|r| r.get("crates-io"))
        .and_then(|c| c.get("index"))
        .and_then(|v| v.as_str())
    {
        return SourceReplacement::Registry(url.to_string());
    }

    SourceReplacement::None
}

/// Resolve `[source.crates-io] replace-with` chains. Returns `None`
/// (rather than `SourceReplacement::None`) when no `[source.crates-io]`
/// is configured at all, so the caller can fall through to
/// `[registries.crates-io]`.
fn resolve_source_chain(doc: &toml::Value) -> Option<SourceReplacement> {
    let sources = doc.get("source").and_then(|s| s.as_table())?;

    // Follow the replace-with chain starting from crates-io. Cap the
    // hop count so a cycle in a broken config can't spin forever.
    let mut current = "crates-io";
    for _ in 0..16 {
        let Some(src) = sources.get(current).and_then(|s| s.as_table()) else {
            // Chain points at an undefined source. crates-io itself
            // missing means "this file says nothing about it"; a
            // dangling hop later in the chain is a misconfiguration we
            // surface as "no replacement" rather than guessing.
            return if current == "crates-io" {
                None
            } else {
                Some(SourceReplacement::None)
            };
        };
        if let Some(next) = src.get("replace-with").and_then(|v| v.as_str()) {
            current = next;
            continue;
        }
        // Terminal source. crates-io itself with no replace-with means
        // "not replaced" — but it *is* a statement, so don't fall
        // through to [registries].
        if current == "crates-io" {
            return Some(SourceReplacement::None);
        }
        if let Some(url) = src.get("registry").and_then(|v| v.as_str()) {
            return Some(SourceReplacement::Registry(url.to_string()));
        }
        for kind in ["local-registry", "directory", "git"] {
            if src.get(kind).is_some() {
                return Some(SourceReplacement::Unsupported { kind });
            }
        }
        return Some(SourceReplacement::None);
    }
    Some(SourceReplacement::None)
}

/// Convenience: search every applicable config file (nearest first) and
/// return the first one that expresses a crates.io replacement.
///
/// Stopping at the first *hit* (rather than the first *file*) is what
/// makes this match cargo's merge semantics for our purposes: a
/// workspace-local `.cargo/config.toml` that only sets `[build]` no
/// longer shadows a mirror in `$CARGO_HOME/config.toml`.
pub fn discover_crates_io_replacement(
    workspace_root: &Path,
    cargo_home: &Path,
) -> SourceReplacement {
    for path in find_configs(workspace_root, cargo_home) {
        let Ok(s) = std::fs::read_to_string(&path) else {
            continue;
        };
        match crates_io_replacement(&s) {
            SourceReplacement::None => continue,
            hit => return hit,
        }
    }
    SourceReplacement::None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_source_section() {
        assert_eq!(crates_io_replacement(""), SourceReplacement::None);
        assert_eq!(
            crates_io_replacement("[build]\njobs = 4\n"),
            SourceReplacement::None
        );
    }

    #[test]
    fn simple_mirror() {
        let cfg = r#"
            [source.crates-io]
            replace-with = "mirror"
            [source.mirror]
            registry = "sparse+https://mirror.example/index/"
        "#;
        assert_eq!(
            crates_io_replacement(cfg),
            SourceReplacement::Registry("sparse+https://mirror.example/index/".into())
        );
    }

    #[test]
    fn chained_replace_with() {
        let cfg = r#"
            [source.crates-io]
            replace-with = "a"
            [source.a]
            replace-with = "b"
            [source.b]
            registry = "https://b.example/"
        "#;
        assert_eq!(
            crates_io_replacement(cfg),
            SourceReplacement::Registry("https://b.example/".into())
        );
    }

    #[test]
    fn vendored_directory_is_unsupported() {
        let cfg = r#"
            [source.crates-io]
            replace-with = "vendored"
            [source.vendored]
            directory = "vendor"
        "#;
        assert_eq!(
            crates_io_replacement(cfg),
            SourceReplacement::Unsupported { kind: "directory" }
        );
    }

    #[test]
    fn dangling_replace_with() {
        let cfg = r#"
            [source.crates-io]
            replace-with = "missing"
        "#;
        assert_eq!(crates_io_replacement(cfg), SourceReplacement::None);
    }

    #[test]
    fn cycle_terminates() {
        let cfg = r#"
            [source.crates-io]
            replace-with = "a"
            [source.a]
            replace-with = "crates-io"
        "#;
        // Just assert it doesn't hang; result is None.
        assert_eq!(crates_io_replacement(cfg), SourceReplacement::None);
    }

    #[test]
    fn registries_crates_io_index() {
        let cfg = r#"
            [registries.crates-io]
            index = "sparse+https://mirror.example/"
        "#;
        assert_eq!(
            crates_io_replacement(cfg),
            SourceReplacement::Registry("sparse+https://mirror.example/".into())
        );
    }

    #[test]
    fn source_replace_with_beats_registries_index() {
        let cfg = r#"
            [registries.crates-io]
            index = "sparse+https://wrong.example/"
            [source.crates-io]
            replace-with = "m"
            [source.m]
            registry = "sparse+https://right.example/"
        "#;
        assert_eq!(
            crates_io_replacement(cfg),
            SourceReplacement::Registry("sparse+https://right.example/".into())
        );
    }

    #[test]
    fn find_configs_walks_up_and_includes_cargo_home() {
        let tmp = tempdir();
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".cargo")).unwrap();
        std::fs::write(tmp.join(".cargo/config.toml"), "").unwrap();

        let home = tempdir();
        std::fs::write(home.join("config.toml"), "").unwrap();

        let found = find_configs(&nested, &home);
        // The walk-up may also pick up `.cargo/config.toml` files from
        // ancestors outside our tempdir (e.g. the cargo vendoring config
        // that nixpkgs' cargoSetupHook writes at /build/.cargo). Assert
        // only on what we control: both expected entries present, in
        // order, with the workspace-local one first.
        let ws_cfg = tmp.join(".cargo/config.toml");
        let home_cfg = home.join("config.toml");
        let pos = |p: &std::path::Path| found.iter().position(|f| f == p).unwrap();
        assert_eq!(found.first(), Some(&ws_cfg), "found = {found:?}");
        assert!(pos(&ws_cfg) < pos(&home_cfg), "found = {found:?}");
    }

    /// Regression: a workspace-local config without a [source] section
    /// must not shadow a mirror configured further out.
    ///
    /// We can't rely on `$CARGO_HOME/config.toml` here because the
    /// walk-up to `/` may encounter unrelated configs first (the nix
    /// build sandbox places one at `/build/.cargo`). Instead we put the
    /// mirror one level *above* the workspace — still found by walk-up,
    /// still proves the "skip files that say nothing" behaviour.
    #[test]
    fn discover_skips_configs_without_source() {
        let tmp = tempdir();
        let ws = tmp.join("outer/ws");
        std::fs::create_dir_all(ws.join(".cargo")).unwrap();
        std::fs::write(ws.join(".cargo/config.toml"), "[build]\njobs = 4\n").unwrap();

        let outer = tmp.join("outer");
        std::fs::create_dir_all(outer.join(".cargo")).unwrap();
        std::fs::write(
            outer.join(".cargo/config.toml"),
            r#"
                [source.crates-io]
                replace-with = "m"
                [source.m]
                registry = "sparse+https://mirror.example/"
            "#,
        )
        .unwrap();

        assert_eq!(
            discover_crates_io_replacement(&ws, Path::new("/nonexistent-cargo-home")),
            SourceReplacement::Registry("sparse+https://mirror.example/".into())
        );
    }

    fn tempdir() -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!(
            "cargo-nix-cfg-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
