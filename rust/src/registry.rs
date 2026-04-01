//! Look up crate metadata from the cargo registry index using `tame-index`.
//!
//! First tries the local cache (`$CARGO_HOME/registry/index/`). On cache miss,
//! falls back to fetching from the remote sparse HTTP index.

use base64::Engine;
use std::collections::HashMap;
use std::path::Path;

use tame_index::index::{FileLock, SparseIndex};
use tame_index::{IndexKrate, IndexVersion, KrateName};

/// Find the on-disk index directory for a given registry URL.
///
/// Cargo stores index caches under `$CARGO_HOME/registry/index/<name>-<hash>/`.
/// The hash depends on the cargo version (changed in 1.85.0). Rather than
/// reproducing the hash algorithm (which requires running `cargo --version`),
/// we scan the directory for a prefix match on the URL's hostname.
fn find_index_dir(cargo_home: &Path, url: &str) -> Option<std::path::PathBuf> {
    let hostname = host_from_url(url)?;
    let index_dir = cargo_home.join("registry").join("index");
    let entries = std::fs::read_dir(&index_dir).ok()?;

    // Directory names look like "index.crates.io-1949cf8c6b5b557f"
    entries
        .flatten()
        .find(|e| {
            e.file_name().to_string_lossy().starts_with(hostname)
                && e.file_type().is_ok_and(|t| t.is_dir())
        })
        .map(|e| e.path())
}

/// Look up a crate in the registry index cache, falling back to a remote
/// sparse HTTP fetch when the local cache doesn't have it.
///
/// `cargo_home` is the path to ~/.cargo (or $CARGO_HOME).
/// `url` is the index URL, e.g. `"sparse+https://index.crates.io/"`.
/// Returns all versions of the crate. Use [`find_version`] to pick one.
pub fn lookup_crate(cargo_home: &Path, url: &str, name: &str) -> Result<IndexKrate, String> {
    let krate_name =
        KrateName::crates_io(name).map_err(|e| format!("invalid crate name '{name}': {e}"))?;
    let lock = FileLock::unlocked();

    // Probe for a cargo-created (or user-provided) cache dir first.
    // find_index_dir scans by hostname prefix, which handles both
    // cargo's internal hash-suffixed dirs and user-provided dirs
    // (e.g. read-only nix store paths).
    if let Some(exact_path) = find_index_dir(cargo_home, url) {
        // First try tame-index's native sparse cache format.
        let location = tame_index::index::IndexLocation {
            url: tame_index::index::IndexUrl::from(url),
            root: tame_index::index::IndexPath::Exact(tame_index::PathBuf::from(
                exact_path.to_string_lossy().as_ref(),
            )),
            cargo_version: None,
        };
        if let Ok(idx) = SparseIndex::new(location) {
            if let Ok(Some(krate)) = idx.cached_krate(krate_name, &lock) {
                return Ok(krate);
            }
        }

    }

    // Fall back to tame-index's own cache layout + remote fetch.
    let sparse_index = index_for_url(cargo_home, url)?;
    if let Ok(Some(krate)) = sparse_index.cached_krate(krate_name, &lock) {
        return Ok(krate);
    }

    fetch_one(shared_agent(), &sparse_index, url, name)?
        .ok_or_else(|| format!("crate '{name}' not found in remote index '{url}'"))
}

/// Extract the hostname from a registry URL.
/// e.g. `sparse+https://index.crates.io/` → `index.crates.io`
fn host_from_url(url: &str) -> Option<&str> {
    let url = url
        .strip_prefix("sparse+")
        .or_else(|| url.strip_prefix("registry+"))
        .unwrap_or(url);
    let url = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://"))?;
    url.split('/').next()
}

/// Look up credentials for a URL's host in ~/.netrc (or $NETRC).
/// Returns `Some((login, password))` if found.
fn netrc_credentials_for_url(url: &str) -> Option<(String, String)> {
    let host = host_from_url(url)?;

    let netrc_path = std::env::var("NETRC")
        .ok()
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| format!("{}/.netrc", h))
        })?;

    let file = std::fs::File::open(&netrc_path).ok()?;
    let reader = std::io::BufReader::new(file);
    let netrc = netrc::Netrc::parse(reader).ok()?;

    // Look for exact machine match, then fall back to default.
    let machine = netrc
        .hosts
        .iter()
        .find(|(name, _)| name == host)
        .map(|(_, m)| m)
        .or(netrc.default.as_ref())?;

    Some((
        machine.login.clone(),
        machine.password.clone()?,
    ))
}

/// Shared HTTP agent: ureq's Agent is Arc-backed, so clones share the
/// connection pool. Sized for the prefetch thread pool — one pooled
/// connection per worker lets every thread keep-alive between requests.
fn shared_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .max_idle_connections_per_host(PREFETCH_WORKERS)
            .timeout_global(Some(std::time::Duration::from_secs(30)))
            .build()
            .into()
    })
}

const PREFETCH_WORKERS: usize = 32;

/// Per-URL SparseIndex cache. Creating one hashes the URL and mkdirs the
/// cache path — cheap, but doing it once per fetched crate adds up when
/// the prefetch pool is hammering the same registry.
fn index_for_url(cargo_home: &Path, url: &str) -> Result<std::sync::Arc<SparseIndex>, String> {
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex, OnceLock};

    type IndexCache = Mutex<HashMap<(String, String), Arc<SparseIndex>>>;
    static CACHE: OnceLock<IndexCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);

    let key = (cargo_home.to_string_lossy().into_owned(), url.to_string());
    if let Some(idx) = cache.lock().unwrap().get(&key) {
        return Ok(idx.clone());
    }

    // Cargo-compatible path layout so the cache is reusable by cargo itself.
    // Pin a cargo version so tame-index doesn't exec `cargo -V` (not available
    // in the nix sandbox). 1.85+ uses the stable dir hash scheme.
    let mk_location = || tame_index::index::IndexLocation {
        url: tame_index::index::IndexUrl::from(url),
        root: tame_index::index::IndexPath::UserSpecified(tame_index::PathBuf::from(
            cargo_home.to_string_lossy().as_ref(),
        )),
        cargo_version: Some(tame_index::Version::new(1, 85, 0)),
    };

    // tame-index doesn't create the index cache dir itself.
    let (cache_dir, _url) = mk_location()
        .into_parts()
        .map_err(|e| format!("failed to compute index path for '{url}': {e}"))?;
    std::fs::create_dir_all(cache_dir.as_std_path())
        .map_err(|e| format!("failed to create index cache dir: {e}"))?;

    let idx = Arc::new(
        SparseIndex::new(mk_location())
            .map_err(|e| format!("failed to create sparse index for '{url}': {e}"))?,
    );
    cache.lock().unwrap().insert(key, idx.clone());
    Ok(idx)
}

/// Issue one sparse-index HTTP GET through the shared agent and write
/// the result to tame-index's cache. Used by both the prefetch pool and
/// the serial fallback.
fn fetch_one(
    agent: &ureq::Agent,
    sparse_index: &SparseIndex,
    url: &str,
    name: &str,
) -> Result<Option<IndexKrate>, String> {
    let krate_name =
        KrateName::crates_io(name).map_err(|e| format!("invalid crate name '{name}': {e}"))?;
    let lock = FileLock::unlocked();

    let req = sparse_index
        .make_remote_request(krate_name, None, &lock)
        .map_err(|e| format!("failed to build request for '{name}': {e}"))?;
    let (parts, _) = req.into_parts();
    let uri = parts.uri.to_string();

    let mut agent_req = agent.get(&uri);
    for (key, value) in parts.headers.iter() {
        if let Ok(v) = value.to_str() {
            agent_req = agent_req.header(key.as_str(), v);
        }
    }
    if let Some((user, password)) = netrc_credentials_for_url(url) {
        let credentials =
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"));
        agent_req = agent_req.header("authorization", &format!("Basic {credentials}"));
    }

    let mut response = agent_req
        .call()
        .map_err(|e| format!("failed to fetch '{name}' from '{url}': {e}"))?;

    let status = response.status();
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| format!("failed to read response body for '{name}': {e}"))?;

    let mut http_resp = http::Response::builder().status(status.as_u16());
    for (key, value) in response.headers().iter() {
        http_resp = http_resp.header(key.as_str(), value.as_bytes());
    }
    let http_resp = http_resp
        .body(body)
        .map_err(|e| format!("failed to build http response: {e}"))?;

    sparse_index
        .parse_remote_response(krate_name, http_resp, true, &lock)
        .map_err(|e| format!("failed to parse response for '{name}' from '{url}': {e}"))
}

/// Concurrently warm the local index cache for a set of crates.
///
/// The lockfile gives us every (registry, crate-name) pair upfront — no
/// need to discover dependencies iteratively — so we can fire all
/// requests before the resolve loop starts. Each thread pulls from a
/// shared work queue and writes cache entries; errors are swallowed
/// because the serial [`lookup_crate`] path surfaces them cleanly later.
///
/// No-ops for crates already in the local cache.
pub fn prefetch_index(cargo_home: &Path, jobs: &[(String, String)]) {
    use std::sync::Mutex;

    // Skip anything already cached. Cheap: just a file stat per entry.
    let pending: Vec<(String, String)> = jobs
        .iter()
        .filter(|(url, name)| !is_cached(cargo_home, url, name))
        .cloned()
        .collect();

    if pending.is_empty() {
        return;
    }

    // Force-create all SparseIndex objects (and their cache dirs) on the
    // main thread before workers touch them, so there's no mkdir race.
    let urls: std::collections::BTreeSet<&str> =
        pending.iter().map(|(u, _)| u.as_str()).collect();
    for url in &urls {
        let _ = index_for_url(cargo_home, url);
    }

    let start = std::time::Instant::now();
    let total = pending.len();
    let queue = Mutex::new(pending.into_iter());
    let agent = shared_agent();

    std::thread::scope(|s| {
        for _ in 0..PREFETCH_WORKERS.min(total) {
            s.spawn(|| loop {
                let Some((url, name)) = queue.lock().unwrap().next() else {
                    return;
                };
                let Ok(idx) = index_for_url(cargo_home, &url) else {
                    continue;
                };
                let _ = fetch_one(agent, &idx, &url, &name);
            });
        }
    });

    eprintln!(
        "cargo-nix: prefetched {total} index entries in {:.2}s",
        start.elapsed().as_secs_f64()
    );
}

/// Is this crate already in the local tame-index cache?
fn is_cached(cargo_home: &Path, url: &str, name: &str) -> bool {
    let Ok(idx) = index_for_url(cargo_home, url) else {
        return false;
    };
    let Ok(krate_name) = KrateName::crates_io(name) else {
        return false;
    };
    let lock = FileLock::unlocked();
    matches!(idx.cached_krate(krate_name, &lock), Ok(Some(_)))
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



    #[test]
    fn test_host_from_url() {
        assert_eq!(
            host_from_url("sparse+https://index.crates.io/"),
            Some("index.crates.io")
        );
        assert_eq!(
            host_from_url("sparse+https://artifactory.infra.ant.dev/artifactory/api/cargo/crates-internal/index/"),
            Some("artifactory.infra.ant.dev")
        );
    }

    /// Integration test: fetch a crate from the remote sparse index into
    /// an empty cargo home (no pre-populated cache). Verifies that
    /// lookup_crate falls back to HTTP when the local cache misses.
    ///
    /// Requires network access — run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn fetch_from_remote_sparse_index_cold_cache() {
        use std::fs;
        let tmp = std::env::temp_dir().join("cargo-nix-plugin-test-cold-cache");
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).expect("create tempdir");

        // Empty cargo home — no local index cache at all.
        let result = lookup_crate(
            &tmp,
            "sparse+https://index.crates.io/",
            "serde",
        );

        let _ = fs::remove_dir_all(&tmp);

        let krate = result.expect("should fetch serde from remote sparse index");
        assert!(!krate.versions.is_empty(), "serde should have versions");
        let v = find_version(&krate, "1.0.228");
        assert!(v.is_some(), "serde 1.0.228 should exist");
    }
}
