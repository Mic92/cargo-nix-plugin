//! Look up crate metadata from the local cargo registry index cache
//! using `tame-index`.

use std::collections::HashMap;
use std::path::Path;

pub use tame_index::index::{ComboIndexCache, FileLock};
pub use tame_index::{IndexKrate, IndexVersion, KrateName};

/// Find the on-disk index directory for a given registry URL.
///
/// Cargo stores index caches under `$CARGO_HOME/registry/index/<name>-<hash>/`.
/// The hash depends on the cargo version (changed in 1.85.0). Rather than
/// reproducing the hash algorithm (which requires running `cargo --version`),
/// we scan the directory for a prefix match on the URL's hostname.
fn find_index_dir(cargo_home: &Path, url: &str) -> Result<std::path::PathBuf, String> {
    // Extract hostname from URL for matching.
    // e.g. "sparse+https://index.crates.io/" -> "index.crates.io"
    let hostname = url
        .trim_start_matches("sparse+")
        .trim_start_matches("registry+")
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .split('/')
        .next()
        .unwrap_or(url);

    let index_dir = cargo_home.join("registry").join("index");
    let entries = std::fs::read_dir(&index_dir).map_err(|e| {
        format!(
            "failed to open index cache for '{url}': {} ({})",
            e,
            index_dir.display()
        )
    })?;

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        // Directory names look like "index.crates.io-1949cf8c6b5b557f"
        if name_str.starts_with(hostname) && entry.file_type().is_ok_and(|t| t.is_dir()) {
            return Ok(entry.path());
        }
    }

    Err(format!(
        "no index directory matching '{hostname}' found in {}",
        index_dir.display()
    ))
}

/// Look up a crate in the registry index cache.
///
/// `cargo_home` is the path to ~/.cargo (or $CARGO_HOME).
/// `url` is the index URL, e.g. `"sparse+https://index.crates.io/"`.
/// Returns all versions of the crate. Use [`find_version`] to pick one.
pub fn lookup_crate(cargo_home: &Path, url: &str, name: &str) -> Result<IndexKrate, String> {
    let krate_name =
        KrateName::crates_io(name).map_err(|e| format!("invalid crate name '{name}': {e}"))?;

    let exact_path = find_index_dir(cargo_home, url)?;

    let index_url = tame_index::index::IndexUrl::from(url);
    let location = tame_index::index::IndexLocation {
        url: index_url,
        root: tame_index::index::IndexPath::Exact(tame_index::PathBuf::from(
            exact_path.to_string_lossy().as_ref(),
        )),
        cargo_version: None,
    };

    let cache = ComboIndexCache::new(location)
        .map_err(|e| format!("failed to open index cache for '{url}': {e}"))?;

    let lock = FileLock::unlocked();
    cache
        .cached_krate(krate_name, &lock)
        .map_err(|e| format!("failed to read index cache for '{name}': {e}"))?
        .ok_or_else(|| format!("crate '{name}' not found in index cache for '{url}'"))
}

/// Find a specific version in an `IndexKrate`.
pub fn find_version<'a>(krate: &'a IndexKrate, version: &str) -> Option<&'a IndexVersion> {
    krate.versions.iter().find(|v| v.version == version)
}

/// Convenience: get the merged features map from an `IndexVersion`.
pub fn features_for_version(version: &IndexVersion) -> HashMap<String, Vec<String>> {
    version
        .features()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cargo_home() -> std::path::PathBuf {
        std::path::PathBuf::from(std::env::var("CARGO_HOME").unwrap_or_else(|_| {
            format!(
                "{}/.cargo",
                std::env::var("HOME").unwrap_or_else(|_| "/root".to_string())
            )
        }))
    }

    #[test]
    fn lookup_serde_from_cache() {
        let home = cargo_home();
        let krate = lookup_crate(&home, "sparse+https://index.crates.io/", "serde");
        let Ok(krate) = krate else {
            eprintln!("skipping: {}", krate.unwrap_err());
            return;
        };

        assert!(!krate.versions.is_empty());
        let v = find_version(&krate, "1.0.228");
        assert!(v.is_some(), "serde 1.0.228 not found in index");
        let v = v.unwrap();
        let features = features_for_version(v);
        assert!(
            features.contains_key("default"),
            "serde should have 'default' feature"
        );
        assert!(
            features.contains_key("derive"),
            "serde should have 'derive' feature"
        );
    }

    #[test]
    fn lookup_nonexistent_crate() {
        let home = cargo_home();
        let result = lookup_crate(
            &home,
            "sparse+https://index.crates.io/",
            "this-crate-definitely-does-not-exist-xyz-123",
        );
        assert!(result.is_err());
    }
}
