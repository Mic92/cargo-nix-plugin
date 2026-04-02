//! Look up crate metadata from the cargo registry index using `tame-index`.
//!
//! First tries the local cache (`$CARGO_HOME/registry/index/`). On cache miss,
//! falls back to fetching from the remote sparse HTTP index.

use base64::Engine;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

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
///
/// `http_status_as_error` is disabled so 4xx/5xx responses come back as
/// `Ok(Response)` rather than `Err(Error::StatusCode)` — we need access
/// to the response headers (`Retry-After` in particular) to make retry
/// decisions, which ureq drops on the floor when it raises `StatusCode`.
fn shared_agent() -> &'static ureq::Agent {
    static AGENT: std::sync::OnceLock<ureq::Agent> = std::sync::OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .max_idle_connections_per_host(PREFETCH_WORKERS)
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false)
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

/// Internal classification of a fetch attempt's failure mode.
///
/// Drives `retry_with_backoff`: `Retryable` re-runs the closure after
/// backing off; `Permanent` aborts immediately. The `delay` on
/// `Retryable` is a server-suggested wait (parsed from `Retry-After`);
/// `None` means "use the computed exponential-backoff schedule".
enum FetchError {
    Retryable { msg: String, delay: Option<Duration> },
    Permanent(String),
}

/// Issue one sparse-index HTTP GET through the shared agent and write
/// the result to tame-index's cache. Used by both the prefetch pool and
/// the serial fallback.
///
/// Wraps [`do_fetch_one`] in [`retry_with_backoff`]: transient HTTP
/// failures (5xx, 429, connection drops, parse errors from a corrupt
/// response body) are retried with full-jitter exponential backoff up to
/// `MAX_ATTEMPTS` times. `Retry-After` (seconds form for now) is honored
/// when present on 429/503 responses. Permanent failures (404, 4xx other
/// than 429, malformed input) abort immediately.
fn fetch_one(
    agent: &ureq::Agent,
    sparse_index: &SparseIndex,
    url: &str,
    name: &str,
) -> Result<Option<IndexKrate>, String> {
    retry_with_backoff(name, || do_fetch_one(agent, sparse_index, url, name))
}

/// Single fetch attempt — no retry. The callee of [`fetch_one`].
///
/// Returns `FetchError::Retryable` for failures the retry loop should
/// re-attempt (5xx, 429, IO/timeout/TLS, corrupted body that
/// `parse_remote_response` rejects), and `FetchError::Permanent` for
/// failures that won't change on a re-fetch (404, 4xx other than 429,
/// invalid crate name, request-build errors).
fn do_fetch_one(
    agent: &ureq::Agent,
    sparse_index: &SparseIndex,
    url: &str,
    name: &str,
) -> Result<Option<IndexKrate>, FetchError> {
    let krate_name = KrateName::crates_io(name)
        .map_err(|e| FetchError::Permanent(format!("invalid crate name '{name}': {e}")))?;
    let lock = FileLock::unlocked();

    let req = sparse_index
        .make_remote_request(krate_name, None, &lock)
        .map_err(|e| FetchError::Permanent(format!("failed to build request for '{name}': {e}")))?;
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

    // shared_agent() sets http_status_as_error=false, so 4xx/5xx come back
    // as Ok(Response) — we need the headers to read Retry-After.
    let mut response = agent_req
        .call()
        .map_err(|e| classify_ureq_error(e, name, url))?;

    let status = response.status();
    if !status.is_success() {
        return Err(classify_status(status.as_u16(), response.headers(), name, url));
    }

    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| FetchError::Retryable {
            msg: format!("failed to read response body for '{name}': {e}"),
            delay: None,
        })?;

    let mut http_resp = http::Response::builder().status(status.as_u16());
    for (key, value) in response.headers().iter() {
        http_resp = http_resp.header(key.as_str(), value.as_bytes());
    }
    let http_resp = http_resp
        .body(body)
        .map_err(|e| FetchError::Permanent(format!("failed to build http response: {e}")))?;

    // A parse failure here is the asn1-rs case from #349206 — a
    // CDN/proxy returned a truncated or otherwise malformed body. Almost
    // always transient, so retry.
    sparse_index
        .parse_remote_response(krate_name, http_resp, true, &lock)
        .map_err(|e| FetchError::Retryable {
            msg: format!("failed to parse response for '{name}' from '{url}': {e}"),
            delay: None,
        })
}

/// Map a `ureq::Error` (from a transport-level call failure, before any
/// HTTP status is seen) into [`FetchError`]. With `http_status_as_error`
/// disabled, this never sees `StatusCode` — those are surfaced via
/// [`classify_status`] from the response path.
fn classify_ureq_error(e: ureq::Error, name: &str, url: &str) -> FetchError {
    use ureq::Error::*;
    let msg = format!("failed to fetch '{name}' from '{url}': {e}");
    match e {
        // Transport-level — almost always transient.
        Io(_) | Timeout(_) | Protocol(_) | ConnectionFailed | HostNotFound | Tls(_)
        | TooManyRedirects => FetchError::Retryable { msg, delay: None },
        // Local programming errors. Won't change on retry.
        BadUri(_) | Http(_) | InvalidProxyUrl | RedirectFailed | BodyExceedsLimit(_) => {
            FetchError::Permanent(msg)
        }
        // shared_agent() disables http_status_as_error, so this branch
        // never fires from the response path. Treat conservatively as
        // retryable in case a future ureq config drift re-enables it.
        StatusCode(_) => FetchError::Retryable { msg, delay: None },
        // ureq::Error is non_exhaustive — default to retryable rather
        // than turning a future variant into a hard failure.
        _ => FetchError::Retryable { msg, delay: None },
    }
}

/// Map an HTTP response status into [`FetchError`].
///
/// 5xx and 429 are retryable; on 429/503 we look for `Retry-After` and
/// pass it to the backoff loop. Other 4xx are permanent.
fn classify_status(
    status: u16,
    headers: &http::HeaderMap,
    name: &str,
    url: &str,
) -> FetchError {
    let msg = format!("HTTP {status} fetching '{name}' from '{url}'");
    match status {
        429 | 503 => {
            let delay = parse_retry_after(headers);
            FetchError::Retryable { msg, delay }
        }
        500..=599 => FetchError::Retryable { msg, delay: None },
        404 => FetchError::Permanent(format!("crate '{name}' not found in remote index '{url}' (HTTP 404)")),
        // Other 4xx (400, 401, 403, ...) — permanent client/auth errors.
        400..=499 => FetchError::Permanent(msg),
        // Anything else (1xx, 3xx that ureq didn't follow): conservative
        // retry, since neither side of this loop expects to see them.
        _ => FetchError::Retryable { msg, delay: None },
    }
}

/// Parse the `Retry-After` header (RFC 9110 §10.2.3) into a `Duration`.
///
/// Currently parses **integer seconds** only — the form CDNs and
/// sparse-index proxies (Cloudflare/Fastly) emit. HTTP-date support is
/// added in a follow-up commit.
///
/// Returns `None` if the header is missing or unparseable; the retry
/// loop falls back to its computed backoff schedule in that case.
fn parse_retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    let v = headers.get(http::header::RETRY_AFTER)?.to_str().ok()?.trim();
    let secs = v.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

const MAX_ATTEMPTS: u32 = 5; // initial + 4 retries
const BASE_DELAY_MS: u64 = 100;
const MAX_DELAY_MS: u64 = 30_000; // cap each individual sleep at 30s
const MAX_TOTAL_MS: u64 = 60_000; // cap cumulative wait at 60s

/// Run `f` with full-jitter exponential backoff retries on
/// [`FetchError::Retryable`]; abort immediately on
/// [`FetchError::Permanent`].
///
/// Schedule: per-attempt cap = `BASE_DELAY_MS * 2^attempt` clamped to
/// `MAX_DELAY_MS`; actual sleep = uniform random in `[0, cap)` (full
/// jitter — see AWS Architecture Blog "Exponential Backoff And Jitter").
/// Server-supplied `Retry-After` overrides the computed cap when present
/// (clamped to `MAX_DELAY_MS`). The loop also gives up if cumulative
/// sleep would exceed `MAX_TOTAL_MS`, returning the last error.
fn retry_with_backoff<T>(
    name: &str,
    mut f: impl FnMut() -> Result<T, FetchError>,
) -> Result<T, String> {
    let mut total_slept = Duration::ZERO;
    let mut last_msg = String::new();

    for attempt in 0..MAX_ATTEMPTS {
        match f() {
            Ok(v) => return Ok(v),
            Err(FetchError::Permanent(msg)) => return Err(msg),
            Err(FetchError::Retryable { msg, delay }) => {
                last_msg = msg;
                if attempt + 1 == MAX_ATTEMPTS {
                    break; // exhausted, fall through to error
                }

                // Server-suggested delay overrides backoff (but stays
                // capped — a CDN can't pin us for an hour).
                let sleep = match delay {
                    Some(d) => d.min(Duration::from_millis(MAX_DELAY_MS)),
                    None => {
                        let cap_ms = BASE_DELAY_MS
                            .saturating_mul(1u64 << attempt)
                            .min(MAX_DELAY_MS);
                        Duration::from_millis(jitter_ms(cap_ms))
                    }
                };

                if total_slept + sleep > Duration::from_millis(MAX_TOTAL_MS) {
                    break;
                }

                eprintln!(
                    "cargo-nix: retrying '{name}' (attempt {}/{}) after {}ms: {}",
                    attempt + 2,
                    MAX_ATTEMPTS,
                    sleep.as_millis(),
                    last_msg,
                );

                std::thread::sleep(sleep);
                total_slept += sleep;
            }
        }
    }

    Err(format!(
        "{last_msg} (gave up after {MAX_ATTEMPTS} attempts, {}ms total)",
        total_slept.as_millis()
    ))
}

/// Full-jitter helper: uniform random `u64` in `[0, max_ms)`.
///
/// `getrandom` is infallible on Linux/macOS in practice; if it ever
/// fails, return `max_ms / 2` (deterministic, never panics, avoids
/// thundering herds slightly worse than true random).
fn jitter_ms(max_ms: u64) -> u64 {
    if max_ms <= 1 {
        return 0;
    }
    let mut buf = [0u8; 8];
    if getrandom::fill(&mut buf).is_err() {
        return max_ms / 2;
    }
    u64::from_ne_bytes(buf) % max_ms
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

    #[test]
    fn parse_retry_after_seconds() {
        let mut h = http::HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "120".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(120)));

        // Whitespace is trimmed.
        let mut h = http::HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "  5  ".parse().unwrap());
        assert_eq!(parse_retry_after(&h), Some(Duration::from_secs(5)));
    }

    #[test]
    fn parse_retry_after_missing_or_garbage() {
        // Missing.
        assert_eq!(parse_retry_after(&http::HeaderMap::new()), None);

        // Garbage (HTTP-date is parsed in a follow-up; for now this is None).
        let mut h = http::HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "not a number".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn jitter_is_bounded() {
        for max in [1u64, 2, 100, 30_000] {
            for _ in 0..100 {
                let j = jitter_ms(max);
                assert!(j < max.max(1), "jitter {j} >= max {max}");
            }
        }
        // Edge cases.
        assert_eq!(jitter_ms(0), 0);
        assert_eq!(jitter_ms(1), 0);
    }

    #[test]
    fn retry_succeeds_immediately() {
        let mut calls = 0;
        let result: Result<i32, String> = retry_with_backoff("test", || {
            calls += 1;
            Ok(42)
        });
        assert_eq!(result, Ok(42));
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_aborts_on_permanent() {
        let mut calls = 0;
        let result: Result<i32, String> = retry_with_backoff("test", || {
            calls += 1;
            Err(FetchError::Permanent("nope".into()))
        });
        assert_eq!(result, Err("nope".into()));
        assert_eq!(calls, 1);
    }

    #[test]
    fn retry_succeeds_after_transient_failures() {
        let mut calls = 0;
        let result: Result<i32, String> = retry_with_backoff("test", || {
            calls += 1;
            if calls < 3 {
                Err(FetchError::Retryable {
                    msg: format!("attempt {calls} failed"),
                    // Force min jitter so the test stays fast.
                    delay: Some(Duration::from_millis(1)),
                })
            } else {
                Ok(42)
            }
        });
        assert_eq!(result, Ok(42));
        assert_eq!(calls, 3);
    }

    #[test]
    fn retry_gives_up_after_max_attempts() {
        let mut calls = 0;
        let result: Result<i32, String> = retry_with_backoff("test", || {
            calls += 1;
            Err(FetchError::Retryable {
                msg: "always fails".into(),
                delay: Some(Duration::from_millis(1)),
            })
        });
        assert_eq!(calls, MAX_ATTEMPTS as usize);
        let err = result.unwrap_err();
        assert!(err.contains("always fails"));
        assert!(err.contains("gave up"));
    }
}
