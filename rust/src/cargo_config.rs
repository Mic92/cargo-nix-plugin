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

/// Look for a `.cargo/config.toml` (or legacy `.cargo/config`) by
/// walking up from `start` and in `$CARGO_HOME`, mirroring cargo's own
/// search order. Returns the first one found.
pub fn find_config(start: &Path, cargo_home: &Path) -> Option<std::path::PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        for name in ["config.toml", "config"] {
            let p = d.join(".cargo").join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        dir = d.parent();
    }
    for name in ["config.toml", "config"] {
        let p = cargo_home.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Parse a cargo config TOML string and resolve where `crates-io` is
/// redirected to, following `replace-with` chains.
pub fn crates_io_replacement(config: &str) -> SourceReplacement {
    let Ok(doc) = toml::from_str::<toml::Value>(config) else {
        return SourceReplacement::None;
    };
    let Some(sources) = doc.get("source").and_then(|s| s.as_table()) else {
        return SourceReplacement::None;
    };

    // Follow the replace-with chain starting from crates-io. Cap the
    // hop count so a cycle in a broken config can't spin forever.
    let mut current = "crates-io";
    for _ in 0..16 {
        let Some(src) = sources.get(current).and_then(|s| s.as_table()) else {
            // Chain points at an undefined source — treat as no
            // replacement rather than guessing.
            return SourceReplacement::None;
        };
        if let Some(next) = src.get("replace-with").and_then(|v| v.as_str()) {
            current = next;
            continue;
        }
        // Terminal source. crates-io itself with no replace-with means
        // "not replaced".
        if current == "crates-io" {
            return SourceReplacement::None;
        }
        if let Some(url) = src.get("registry").and_then(|v| v.as_str()) {
            return SourceReplacement::Registry(url.to_string());
        }
        for kind in ["local-registry", "directory", "git"] {
            if src.get(kind).is_some() {
                return SourceReplacement::Unsupported { kind };
            }
        }
        return SourceReplacement::None;
    }
    SourceReplacement::None
}

/// Convenience: search for a config file and resolve the crates.io
/// replacement in one step.
pub fn discover_crates_io_replacement(
    workspace_root: &Path,
    cargo_home: &Path,
) -> SourceReplacement {
    let Some(path) = find_config(workspace_root, cargo_home) else {
        return SourceReplacement::None;
    };
    match std::fs::read_to_string(&path) {
        Ok(s) => crates_io_replacement(&s),
        Err(_) => SourceReplacement::None,
    }
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
    fn find_config_walks_up() {
        let tmp = tempdir();
        let nested = tmp.join("a/b/c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(tmp.join(".cargo")).unwrap();
        std::fs::write(tmp.join(".cargo/config.toml"), "").unwrap();

        let found = find_config(&nested, Path::new("/nonexistent")).unwrap();
        assert_eq!(found, tmp.join(".cargo/config.toml"));
    }

    #[test]
    fn find_config_falls_back_to_cargo_home() {
        let home = tempdir();
        std::fs::write(home.join("config.toml"), "").unwrap();
        // Start from a path with no real ancestors so the walk-up phase
        // cannot accidentally find a .cargo/config.toml that the build
        // sandbox (or developer machine) happens to have lying around.
        let found = find_config(Path::new("/nonexistent-workspace-root"), &home).unwrap();
        assert_eq!(found, home.join("config.toml"));
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
