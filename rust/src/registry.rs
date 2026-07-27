// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Look up crate metadata from the cargo registry index using `tame-index`.
//!
//! First tries the local cache (`$CARGO_HOME/registry/index/`). On cache miss,
//! falls back to fetching from the remote sparse HTTP index.

use base64::Engine;
use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use tame_index::index::{FileLock, SparseIndex};
use tame_index::utils::flock::LockOptions;
use tame_index::{IndexKrate, IndexVersion, KrateName};

/// `CARGO_NIX_DEBUG` set? Cached so hot paths pay an atomic load, not an
/// env lookup. `pub(crate)` only because [`debug_log!`] expands to a
/// `$crate::registry::` path so it keeps compiling if reused elsewhere.
pub(crate) fn debug_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| std::env::var_os("CARGO_NIX_DEBUG").is_some())
}

/// `eprintln!` gated on `CARGO_NIX_DEBUG`. Informational only — warnings
/// and errors stay on plain `eprintln!`.
macro_rules! debug_log {
    ($($arg:tt)*) => {
        if $crate::registry::debug_enabled() {
            eprintln!($($arg)*);
        }
    };
}

/// Canonical upstream crates.io sparse index URL.
pub const CRATES_IO_SPARSE_URL: &str = "sparse+https://index.crates.io/";

/// Cargo's own env var for `registries.crates-io.index` (config-env mapping,
/// see `cargo help environment-variables`). Reusing it means a shell that
/// already redirects `cargo` redirects us too.
pub const CRATES_IO_INDEX_ENV: &str = "CARGO_REGISTRIES_CRATES_IO_INDEX";

/// Decide which sparse index URL to use for crates.io packages.
///
/// Precedence (first match wins):
/// 1. Explicit override (`--index` on the prefetch CLI).
/// 2. `$CARGO_REGISTRIES_CRATES_IO_INDEX` — cargo's own override.
/// 3. `[source.crates-io] replace-with` chain in `.cargo/config.toml`
///    (discovered upward from `workspace_root`, then `$CARGO_HOME`).
/// 4. Upstream `sparse+https://index.crates.io/`.
///
/// Shared by the FFI entry point and `cargo-nix-prefetch` so both observe
/// the exact same mirror in a given environment.
pub fn resolve_crates_io_index(
    explicit: Option<&str>,
    workspace_root: &Path,
    cargo_home: &Path,
) -> String {
    use crate::cargo_config::{discover_crates_io_replacement, SourceReplacement};

    let env = |k: &str| std::env::var(k).ok();

    if let Some(url) = explicit {
        return normalize_index_url(url);
    }
    if let Some(url) = env(CRATES_IO_INDEX_ENV).filter(|s| !s.is_empty()) {
        return normalize_index_url(&url);
    }
    match discover_crates_io_replacement(workspace_root, cargo_home, &env) {
        SourceReplacement::Registry(url) => {
            debug_log!("cargo-nix: using crates.io replacement from .cargo/config.toml: {url}");
            normalize_index_url(&url)
        }
        SourceReplacement::Unsupported { kind } => {
            eprintln!(
                "cargo-nix: warning: .cargo/config.toml replaces crates-io with a \
                 {kind} source, which cannot serve sparse-index metadata; \
                 falling back to upstream index.crates.io"
            );
            CRATES_IO_SPARSE_URL.to_string()
        }
        SourceReplacement::None => CRATES_IO_SPARSE_URL.to_string(),
    }
}

/// Map a Cargo.lock `source` string to the sparse index URL it should be
/// fetched from, redirecting crates.io through `crates_io_index`.
///
/// Returns `None` for non-registry sources (path/git) where no index
/// lookup is needed. Shared by the lockfile resolver and the prefetch
/// CLI so both classify sources identically.
pub fn source_to_index_url(source: Option<&str>, crates_io_index: &str) -> Option<String> {
    const CRATES_IO_GIT: &str = "registry+https://github.com/rust-lang/crates.io-index";
    match source? {
        s if s == CRATES_IO_GIT || s.contains("index.crates.io") => {
            Some(crates_io_index.to_string())
        }
        s if s.starts_with("sparse+") || s.starts_with("registry+") => Some(normalize_index_url(s)),
        _ => None,
    }
}

/// Normalize a registry index URL to the `sparse+https://.../` form
/// tame-index expects: ensure a `sparse+` prefix and a trailing slash.
/// Accepts bare `https://` (as cargo's `[source.x] registry = ...` uses)
/// and `registry+https://` (lockfile-style) and rewrites both.
pub fn normalize_index_url(url: &str) -> String {
    let stripped = url
        .strip_prefix("sparse+")
        .or_else(|| url.strip_prefix("registry+"))
        .unwrap_or(url);
    let mut out = format!("sparse+{stripped}");
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// Compute the on-disk index directory for a given registry URL.
///
/// Cargo stores index caches under
/// `$CARGO_HOME/registry/index/<host>-<hash>/`. The hash scheme was
/// stabilised in cargo 1.85; we compute that exact path via
/// `tame_index::utils::url_to_local_dir` rather than scanning by host
/// prefix. A prefix scan would conflate distinct registries that share
/// a host (e.g. an Artifactory instance serving both a crates.io mirror
/// and an internal registry under different paths), which is worse than
/// missing a pre-1.85 cache we don't care about.
fn find_index_dir(cargo_home: &Path, url: &str) -> Option<std::path::PathBuf> {
    let dir = tame_index::utils::url_to_local_dir(url, true).ok()?;
    let path = cargo_home.join("registry").join("index").join(dir.dir_name);
    path.is_dir().then_some(path)
}

/// Look up a crate in the registry index cache, falling back to a remote
/// sparse HTTP fetch when the local cache doesn't have it.
///
/// `cargo_home` is the path to ~/.cargo (or $CARGO_HOME).
/// `url` is the index URL, e.g. `"sparse+https://index.crates.io/"`.
/// Returns all versions of the crate. Use [`find_version`] to pick one.
pub fn lookup_crate(cargo_home: &Path, url: &str, name: &str) -> Result<IndexKrate, String> {
    lookup_crate_inner(cargo_home, url, name, |_| true)
}

/// Look up a specific version of a crate.
///
/// Like [`lookup_crate`], but treats a cached entry that lacks `version`
/// as a miss. Sparse-index entries are append-only — a cached file that
/// predates a release is *stale*, not authoritative — so cargo's own
/// `RemoteRegistry::load` re-fetches when the requested version isn't in
/// the cached summaries. Mirroring that here keeps a `cargo update` that
/// landed a newer version from hard-failing eval just because the user's
/// `~/.cargo` index cache hadn't caught up.
pub fn lookup_version(
    cargo_home: &Path,
    url: &str,
    name: &str,
    version: &str,
) -> Result<IndexVersion, String> {
    let krate = lookup_crate_inner(cargo_home, url, name, |k| {
        find_version(k, version).is_some()
    })?;
    find_version(&krate, version).cloned().ok_or_else(|| {
        format!("version {version} of {name} not found in index '{url}' (after re-fetch)")
    })
}

/// Shared lookup driver. `fresh` decides whether a cached `IndexKrate`
/// is good enough to return; if it isn't, the next layer is tried, with
/// a remote fetch as the last resort.
fn lookup_crate_inner(
    cargo_home: &Path,
    url: &str,
    name: &str,
    fresh: impl Fn(&IndexKrate) -> bool,
) -> Result<IndexKrate, String> {
    let krate_name =
        KrateName::crates_io(name).map_err(|e| format!("invalid crate name '{name}': {e}"))?;
    let lock = FileLock::unlocked();

    // Probe for a pre-existing cache dir first — covers both
    // cargo-populated `~/.cargo` and a read-only store-path cargoHome
    // pre-seeded by `cargo-nix-prefetch`.
    if let Some(krate) = cached_in_existing_dir(cargo_home, url, krate_name).filter(&fresh) {
        return Ok(krate);
    }

    // Fall back to tame-index's own cache layout + remote fetch.
    let sparse_index = index_for_url(cargo_home, url)?;
    if let Ok(Some(krate)) = sparse_index.cached_krate(krate_name, &lock) {
        if fresh(&krate) {
            return Ok(krate);
        }
    }

    // Cache miss (or stale) → fetch. No cross-process lock needed: the
    // write goes through `write_cache_atomic`, so the worst a concurrent
    // fetcher sees is "still absent" and does its own fetch. The package
    // lock is reserved for herd-collapse in the bulk prefetch path.
    fetch_one(shared_agent(), &sparse_index, url, name)?
        .ok_or_else(|| format!("crate '{name}' not found in remote index '{url}'"))
}

/// Extract the hostname (without port) from a registry URL.
/// e.g. `sparse+https://index.crates.io/` → `index.crates.io`,
/// `sparse+http://mirror:8081/` → `mirror`.
///
/// Only the host is needed for `.netrc` lookup.
fn host_from_url(url: &str) -> Option<&str> {
    let url = url
        .strip_prefix("sparse+")
        .or_else(|| url.strip_prefix("registry+"))
        .unwrap_or(url);
    let url = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let authority = url.split('/').next()?;
    Some(authority.split(':').next().unwrap_or(authority))
}

/// Look up credentials for a URL's host in ~/.netrc (or $NETRC).
/// Returns `Some((login, password))` if found.
fn netrc_credentials_for_url(url: &str) -> Option<(String, String)> {
    let host = host_from_url(url)?;

    let netrc_path = std::env::var("NETRC")
        .ok()
        .or_else(|| std::env::var("HOME").ok().map(|h| format!("{}/.netrc", h)))?;

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

    Some((machine.login.clone(), machine.password.clone()?))
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
        let mut cfg = ureq::Agent::config_builder()
            .max_idle_connections_per_host(PREFETCH_WORKERS)
            .timeout_global(Some(Duration::from_secs(30)))
            .http_status_as_error(false);

        match custom_root_certs(|k| std::env::var_os(k)) {
            Ok(Some(roots)) => {
                cfg = cfg.tls_config(
                    ureq::tls::TlsConfig::builder()
                        .root_certs(ureq::tls::RootCerts::from(roots))
                        .build(),
                );
            }
            Ok(None) => {}
            // Cargo treats an invalid CARGO_HTTP_CAINFO as a hard error. We
            // surface it loudly but fall back to the bundled webpki roots
            // rather than poisoning the OnceLock — the common case (public
            // crates.io) still works, and the corporate-CA case will fail
            // at TLS handshake time with a more specific error anyway.
            Err(e) => eprintln!("warning: ignoring custom CA bundle: {e}"),
        }

        cfg.build().into()
    })
}

/// Load extra root CAs the same way cargo does: `CARGO_HTTP_CAINFO` (cargo's
/// `http.cainfo` env mapping) takes precedence, then `SSL_CERT_FILE` (the
/// generic OpenSSL/libcurl knob that nixpkgs' `cacert` setup-hook exports).
///
/// ureq's default `rustls` backend ships `webpki-roots` and ignores the system
/// store entirely, so without this a corporate MITM proxy or a private
/// registry signed by an internal CA is unreachable from inside the prefetch
/// sandbox even when the surrounding nix build *has* threaded the cert bundle
/// through.
///
/// Returns `Ok(None)` when neither variable is set; the caller keeps the
/// compiled-in webpki roots in that case.
fn custom_root_certs(
    var_os: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<Option<Vec<ureq::tls::Certificate<'static>>>, String> {
    // CARGO_HTTP_CAINFO is explicit user intent — if it's set but
    // unreadable, that's an error. SSL_CERT_FILE is ambient: nixpkgs'
    // stdenv points it at the sentinel `/no-cert-file.crt` inside the
    // build sandbox precisely to defeat openssl's default lookup, so a
    // missing file there must quietly mean "no custom roots", not a
    // warning on every evaluation.
    let path = match var_os("CARGO_HTTP_CAINFO").filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => match var_os("SSL_CERT_FILE").filter(|p| !p.is_empty()) {
            Some(p) if Path::new(&p).exists() => p,
            _ => return Ok(None),
        },
    };

    let pem = std::fs::read(&path)
        .map_err(|e| format!("failed to read CA bundle {}: {e}", path.to_string_lossy()))?;

    let certs: Vec<_> = ureq::tls::parse_pem(&pem)
        .filter_map(|item| match item {
            Ok(ureq::tls::PemItem::Certificate(c)) => Some(Ok(c)),
            Ok(_) => None,
            Err(e) => Some(Err(format!(
                "failed to parse CA bundle {}: {e}",
                path.to_string_lossy()
            ))),
        })
        .collect::<Result<_, _>>()?;

    if certs.is_empty() {
        return Err(format!(
            "CA bundle {} contains no certificates",
            path.to_string_lossy()
        ));
    }

    Ok(Some(certs))
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
    Retryable {
        msg: String,
        delay: Option<Duration>,
    },
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
/// `IndexKrate::from_slice` rejects), and `FetchError::Permanent` for
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

    let req = sparse_index
        .make_remote_request(krate_name, None, &FileLock::unlocked())
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
        return Err(classify_status(
            status.as_u16(),
            response.headers(),
            name,
            url,
        ));
    }

    // ureq's read_to_vec() can surface BodyExceedsLimit, which won't change
    // on retry — give body-read errors the same Permanent/Retryable split
    // as call() errors.
    let body = response
        .body_mut()
        .read_to_vec()
        .map_err(|e| classify_ureq_error(e, name, url))?;

    // A parse failure here is the asn1-rs case from #349206 — a
    // CDN/proxy returned a truncated or otherwise malformed body. Almost
    // always transient, so retry.
    let krate = IndexKrate::from_slice(&body).map_err(|e| FetchError::Retryable {
        msg: format!("failed to parse response for '{name}' from '{url}': {e}"),
        delay: None,
    })?;

    let revision = response
        .headers()
        .get("etag")
        .and_then(|v| v.to_str().ok())
        .map(|etag| format!("etag: {etag}"))
        .or_else(|| {
            response
                .headers()
                .get("last-modified")
                .and_then(|v| v.to_str().ok())
                .map(|lm| format!("last-modified: {lm}"))
        })
        .unwrap_or_else(|| "Unknown".to_owned());
    // Persist via our own writer (raw bytes + atomic rename), not
    // `parse_remote_response(…, write_cache_entry = true, …)` — see
    // [`write_cache_atomic`] / [`write_cache_bytes`] for why both matter.
    // A write failure is unfortunate but not fatal: we already have the
    // parsed krate.
    if let Err(e) = write_cache_atomic(sparse_index, krate_name, &body, &revision) {
        eprintln!("cargo-nix: warning: failed to cache index entry for '{name}': {e}");
    }

    Ok(Some(krate))
}

/// One unit of work for [`prefetch_index`]: fetch the index entry for
/// `name` from `url`. `version` is what the lockfile pinned — used only
/// to decide whether an existing cache entry is fresh enough to skip.
#[derive(Debug, Clone)]
pub struct PrefetchJob {
    pub url: String,
    pub name: String,
    pub version: String,
}

/// Persist a sparse-index response body to tame-index's cache path via a
/// same-directory tempfile + `rename`, so concurrent readers (other
/// nix-eval-jobs workers, `cargo` itself) never observe a torn entry.
/// tame-index's own writer is `File::create` + stream, which they can.
///
/// Second half of the cross-process safety story: the `.package-cache`
/// flock around [`prefetch_index`] serializes the bulk warm-up; atomic
/// rename keeps individual writes safe when the flock is unavailable
/// (NFS, read-only fallback) or when reached via [`lookup_crate`]'s
/// single-crate fallback.
fn write_cache_atomic(
    sparse_index: &SparseIndex,
    name: KrateName<'_>,
    body: &[u8],
    revision: &str,
) -> Result<(), std::io::Error> {
    let cache_path = sparse_index.cache().cache_path(name);
    let cache_path: &Path = cache_path.as_ref();
    let dir = cache_path
        .parent()
        .ok_or_else(|| std::io::Error::other("cache path has no parent"))?;
    std::fs::create_dir_all(dir)?;

    // Same-directory tempfile so `persist` is a rename on the same
    // filesystem. NamedTempFile cleans itself up on drop if we bail out
    // before persisting, so no `.tmp` debris on error.
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    write_cache_bytes(tmp.as_file_mut(), body, revision)?;
    // NamedTempFile defaults to mode 0600; cargo's own writer leaves
    // these umask-default. Match that so a shared CARGO_HOME stays
    // readable across UIDs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = tmp
            .as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o644));
    }
    tmp.as_file().sync_data()?;
    tmp.persist(cache_path).map_err(|e| e.error)?;
    Ok(())
}

/// Serialize a sparse-index response body into cargo's `.cache` v3 format
/// (`cargo::sources::registry::index::SummariesCache`): cache version
/// byte, LE index-format u32, NUL-terminated revision, then `(semver NUL
/// json NUL)*`.
///
/// We write each version's *raw upstream JSON line* (as cargo does), not a
/// re-serialized [`IndexVersion`] (as `IndexKrate::write_cache_entry`
/// does). [`tame_index::IndexDependency`] doesn't model `registry`, so a
/// serde round-trip drops the cross-registry pointer that tells cargo
/// "this dep is on crates.io, not this alt-registry". Our resolver takes
/// sources from Cargo.lock and doesn't care, but a later `cargo build` on
/// the same `CARGO_HOME` does — and online won't recover (etag matches →
/// 304). Lines that don't parse are skipped; the body as a whole was
/// already validated by `IndexKrate::from_slice` before we get here.
fn write_cache_bytes<W: std::io::Write>(
    writer: &mut W,
    body: &[u8],
    revision: &str,
) -> Result<(), std::io::Error> {
    use std::io::Write;
    use tame_index::index::cache::{CURRENT_CACHE_VERSION, INDEX_V_MAX};

    let mut w = std::io::BufWriter::new(writer);
    w.write_all(&[CURRENT_CACHE_VERSION])?;
    w.write_all(&INDEX_V_MAX.to_le_bytes())?;
    w.write_all(revision.as_bytes())?;
    w.write_all(&[0])?;

    for line in body.split(|&b| b == b'\n') {
        if line.is_empty() {
            continue;
        }
        let Ok(iv) = serde_json::from_slice::<IndexVersion>(line) else {
            continue;
        };
        write!(w, "{}", iv.version)?;
        w.write_all(&[0])?;
        w.write_all(line)?;
        w.write_all(&[0])?;
    }

    w.flush()
}

/// Take cargo's global `.package-cache` advisory lock exclusively for the
/// duration of a cache-mutating operation, blocking until available.
///
/// This is the same flock `cargo` itself acquires (exclusive) around index
/// and source updates, so we follow its contract: hold exclusive while
/// writing, never write under shared. That serializes us correctly against
/// a concurrent `cargo fetch` as well as other plugin instances (parallel
/// `nix-eval-jobs` workers).
///
/// `exclusive(true)` enables cargo's read-only-FS → shared fallback, and
/// tame-index already short-circuits on NFS; any remaining failure is
/// exotic enough that we degrade to [`FileLock::unlocked`] and let the
/// atomic-rename writes carry correctness on their own — only herd
/// collapse is lost.
fn acquire_package_lock(cargo_home: &Path) -> FileLock {
    let root = tame_index::PathBuf::from(cargo_home.to_string_lossy().as_ref());
    LockOptions::cargo_package_lock(Some(root))
        .and_then(|o| {
            o.exclusive(true).lock(|_path| {
                eprintln!(
                    "cargo-nix: waiting for cargo package lock at {}",
                    cargo_home.join(".package-cache").display()
                );
                None
            })
        })
        .unwrap_or_else(|e| {
            eprintln!(
                "cargo-nix: warning: failed to acquire cargo package lock \
                 ({e}); continuing without cross-process serialization"
            );
            FileLock::unlocked()
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
fn classify_status(status: u16, headers: &http::HeaderMap, name: &str, url: &str) -> FetchError {
    let msg = format!("HTTP {status} fetching '{name}' from '{url}'");
    match status {
        429 | 503 => {
            let delay = parse_retry_after(headers);
            FetchError::Retryable { msg, delay }
        }
        500..=599 => FetchError::Retryable { msg, delay: None },
        404 => FetchError::Permanent(format!(
            "crate '{name}' not found in remote index '{url}' (HTTP 404)"
        )),
        // Other 4xx (400, 401, 403, ...) — permanent client/auth errors.
        400..=499 => FetchError::Permanent(msg),
        // make_remote_request() injects If-None-Match from the on-disk
        // cache; we only get here when lookup_version() already found the
        // cache useless, so a 304 is a stable "version not in registry",
        // not a transient blip.
        304 => FetchError::Permanent(format!(
            "registry has no newer index for '{name}' (HTTP 304 from '{url}'); \
             the requested version is not in '{url}'"
        )),
        // Anything else (1xx, other 3xx ureq didn't follow): conservative
        // retry, since neither side of this loop expects to see them.
        _ => FetchError::Retryable { msg, delay: None },
    }
}

/// Parse the `Retry-After` header (RFC 9110 §10.2.3) into a `Duration`.
///
/// Two valid forms:
/// - **Integer seconds** (most common from CDNs/sparse-index proxies):
///   `Retry-After: 120` → `Duration::from_secs(120)`
/// - **HTTP-date** (RFC 1123): `Retry-After: Wed, 21 Oct 2026 07:28:00 GMT`
///   → delta from `SystemTime::now()`. A date in the past returns `None`
///   so the retry loop falls back to backoff (clock skew shouldn't pin
///   the loop on a 0ms sleep).
///
/// Returns `None` if the header is missing or unparseable; the retry
/// loop falls back to its computed backoff schedule in that case.
fn parse_retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    let v = headers
        .get(http::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();

    // Form 1: integer seconds.
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }

    // Form 2: HTTP-date — compute delta from now.
    // httpdate parses all three RFC-7231/9110 date formats (IMF-fixdate,
    // obsolete RFC-850, asctime).
    let when = httpdate::parse_http_date(v).ok()?;
    when.duration_since(std::time::SystemTime::now()).ok()
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
    // The budget cap can break out before MAX_ATTEMPTS calls of f().
    let mut attempts_made: u32 = 0;

    for attempt in 0..MAX_ATTEMPTS {
        attempts_made = attempt + 1;
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

                debug_log!(
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
        "{last_msg} (gave up after {attempts_made} attempts, {}ms total)",
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
///
/// Returns `Err` if **every** fetch failed. Per-crate errors are still
/// otherwise swallowed (the serial [`lookup_crate`] path reports them
/// with proper context), but a 100% failure rate almost always means
/// the index is unreachable, and letting that degrade silently produces
/// derivations whose hashes diverge from a connected build (#20).
pub fn prefetch_index(cargo_home: &Path, jobs: &[PrefetchJob]) -> Result<(), String> {
    use std::sync::Mutex;

    // Dedup on (url, name): two locked versions of one crate share an
    // index file, so re-fetching it satisfies both. Keep one job per
    // pair, but treat it as cached only if *every* requested version is
    // present — a single stale entry forces the re-fetch.
    let by_key = {
        let mut m: std::collections::BTreeMap<(&str, &str), Vec<&str>> = Default::default();
        for j in jobs {
            m.entry((j.url.as_str(), j.name.as_str()))
                .or_default()
                .push(j.version.as_str());
        }
        m
    };
    let filter_pending = || -> Vec<(String, String)> {
        by_key
            .iter()
            .filter(|((url, name), vers)| !vers.iter().all(|v| is_cached(cargo_home, url, name, v)))
            .map(|(&(url, name), _)| (url.to_owned(), name.to_owned()))
            .collect()
    };

    // Double-checked locking against cargo's `.package-cache` flock.
    //
    // 1. Lock-free probe. If everything is cached, don't touch the lock
    //    file at all — keeps a read-only store-path `cargoHome` working
    //    and makes warm steady-state evals contention-free. Safe because
    //    cache writes are atomic renames: a racing read sees either
    //    "absent" or "complete", never torn.
    if filter_pending().is_empty() {
        return Ok(());
    }

    // 2. Acquire exclusive (blocking). With N nix-eval-jobs workers on a
    //    cold cache the first holder warms it; the rest queue here.
    let _guard = acquire_package_lock(cargo_home);

    // 3. Re-filter under the lock: whoever held it before us likely
    //    populated some or all of what we need.
    let pending = filter_pending();
    if pending.is_empty() {
        return Ok(());
    }

    // Force-create all SparseIndex objects (and their cache dirs) on the
    // main thread before workers touch them, so there's no mkdir race.
    let urls: std::collections::BTreeSet<&str> = pending.iter().map(|(u, _)| u.as_str()).collect();
    for url in &urls {
        let _ = index_for_url(cargo_home, url);
    }

    let start = std::time::Instant::now();
    let total = pending.len();
    let queue = Mutex::new(pending.into_iter());
    let agent = shared_agent();
    let ok_count = std::sync::atomic::AtomicUsize::new(0);
    let last_err: Mutex<Option<String>> = Mutex::new(None);

    std::thread::scope(|s| {
        for _ in 0..PREFETCH_WORKERS.min(total) {
            s.spawn(|| loop {
                let Some((url, name)) = queue.lock().unwrap().next() else {
                    return;
                };
                let Ok(idx) = index_for_url(cargo_home, &url) else {
                    *last_err.lock().unwrap() = Some(format!("failed to open index for '{url}'"));
                    continue;
                };
                match fetch_one(agent, &idx, &url, &name) {
                    Ok(_) => {
                        ok_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                    Err(e) => {
                        *last_err.lock().unwrap() = Some(e);
                    }
                }
            });
        }
    });

    let ok = ok_count.load(std::sync::atomic::Ordering::Relaxed);
    debug_log!(
        "cargo-nix: prefetched {ok}/{total} index entries in {:.2}s",
        start.elapsed().as_secs_f64()
    );

    if ok == 0 {
        let detail = last_err
            .into_inner()
            .unwrap()
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(format!(
            "all {total} registry index fetches failed — is the index reachable? \
             Set CARGO_REGISTRIES_CRATES_IO_INDEX or configure \
             [source.crates-io] replace-with in .cargo/config.toml to point at a mirror. \
             Last error: {detail}"
        ));
    }
    Ok(())
}

/// Is this `(crate, version)` already in the local tame-index cache?
///
/// Read-only: probes existing index dirs via [`cached_in_existing_dir`]
/// (hostname-prefix scan), which covers both cargo-populated and
/// plugin-populated layouts. Never creates directories, so `--check`
/// stays side-effect-free and a read-only store-path `cargoHome` works.
///
/// Version-aware so a stale entry (file present but missing the version
/// the lockfile pins) is reported as a miss — same staleness semantics
/// as [`lookup_version`].
pub fn is_cached(cargo_home: &Path, url: &str, name: &str, version: &str) -> bool {
    let Ok(krate_name) = KrateName::crates_io(name) else {
        return false;
    };
    cached_in_existing_dir(cargo_home, url, krate_name)
        .is_some_and(|k| find_version(&k, version).is_some())
}

/// Probe for `name` in the pre-existing index dir for `url` under
/// `cargo_home`. Unlike [`index_for_url`] this never creates
/// directories, so it is safe against a read-only `cargoHome` from the
/// nix store.
fn cached_in_existing_dir(
    cargo_home: &Path,
    url: &str,
    krate_name: KrateName<'_>,
) -> Option<IndexKrate> {
    let exact_path = find_index_dir(cargo_home, url)?;
    let location = tame_index::index::IndexLocation {
        url: tame_index::index::IndexUrl::from(url),
        root: tame_index::index::IndexPath::Exact(tame_index::PathBuf::from(
            exact_path.to_string_lossy().as_ref(),
        )),
        cargo_version: None,
    };
    SparseIndex::new(location)
        .ok()?
        .cached_krate(krate_name, &FileLock::unlocked())
        .ok()?
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

    /// The TLS path itself and the SSL_CERT_FILE sentinel handling are
    /// covered end-to-end by `tests/mirror-test.nix`; this pins the one
    /// thing that test can't see — that CARGO_HTTP_CAINFO takes precedence
    /// over SSL_CERT_FILE and, being explicit user intent, errors rather
    /// than silently falling through when unreadable.
    #[test]
    fn custom_root_certs_cainfo_precedence() {
        let err = custom_root_certs(|k| match k {
            "CARGO_HTTP_CAINFO" => Some("/nonexistent/cainfo.pem".into()),
            "SSL_CERT_FILE" => Some("/no-cert-file.crt".into()),
            _ => None,
        })
        .unwrap_err();
        assert!(err.contains("/nonexistent/cainfo.pem"), "{err}");
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
    fn source_to_index_url_classifies() {
        let mirror = "sparse+https://mirror.example/";
        // crates.io — both lockfile spellings — redirects to the mirror.
        assert_eq!(
            source_to_index_url(
                Some("registry+https://github.com/rust-lang/crates.io-index"),
                mirror
            ),
            Some(mirror.into())
        );
        assert_eq!(
            source_to_index_url(Some("sparse+https://index.crates.io/"), mirror),
            Some(mirror.into())
        );
        // Third-party registry passes through normalized.
        assert_eq!(
            source_to_index_url(Some("registry+https://other.example/index"), mirror),
            Some("sparse+https://other.example/index/".into())
        );
        // Non-registry sources → None.
        assert_eq!(
            source_to_index_url(Some("git+https://github.com/x/y#abc"), mirror),
            None
        );
        assert_eq!(source_to_index_url(None, mirror), None);
    }

    /// Regression for #20: an unreachable index must surface as an
    /// error, not silently degrade to "no features".
    #[test]
    fn prefetch_total_failure_is_an_error() {
        let tmp =
            std::env::temp_dir().join(format!("cargo-nix-prefetch-fail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        // 0.0.0.0:1 refuses connections without leaving the sandbox.
        let bad = "sparse+http://0.0.0.0:1/".to_string();
        let jobs = vec![
            PrefetchJob {
                url: bad.clone(),
                name: "serde".into(),
                version: "1.0.228".into(),
            },
            PrefetchJob {
                url: bad.clone(),
                name: "http".into(),
                version: "1.0.0".into(),
            },
        ];
        let err = prefetch_index(&tmp, &jobs).expect_err("expected total failure");
        assert!(err.contains("all 2 registry index fetches failed"), "{err}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_host_from_url() {
        assert_eq!(
            host_from_url("sparse+https://index.crates.io/"),
            Some("index.crates.io")
        );
        assert_eq!(
            host_from_url(
                "sparse+https://artifactory.example.com/artifactory/api/cargo/crates-mirror/index/"
            ),
            Some("artifactory.example.com")
        );
        assert_eq!(
            host_from_url("sparse+http://mirror.example:8081/index/"),
            Some("mirror.example")
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
        let result = lookup_crate(&tmp, "sparse+https://index.crates.io/", "serde");

        let _ = fs::remove_dir_all(&tmp);

        let krate = result.expect("should fetch serde from remote sparse index");
        assert!(!krate.versions.is_empty(), "serde should have versions");
        let v = find_version(&krate, "1.0.228");
        assert!(v.is_some(), "serde 1.0.228 should exist");
    }

    /// Regression: a cached index entry that predates the locked
    /// release (e.g. `cargo update` ran but the user's `~/.cargo` index
    /// cache is stale) must be treated as a miss and re-fetched, not
    /// taken as authoritative.
    ///
    /// We seed a synthetic cache containing only `serde 0.1.0`, then
    /// ask for `1.0.228` and verify both that `is_cached` says "no" and
    /// that `lookup_version` reaches past the stale file and finds it.
    ///
    /// Requires network access — run with `cargo test -- --ignored`.
    #[test]
    #[ignore = "requires network access"]
    fn lookup_version_refetches_stale_cache() {
        use std::fs;
        let tmp = std::env::temp_dir().join(format!(
            "cargo-nix-plugin-test-stale-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);

        // Seed via the real write path so the on-disk format is
        // whatever cargo/tame-index expects, not a hand-rolled byte layout.
        let url = CRATES_IO_SPARSE_URL;
        let idx = index_for_url(&tmp, url).expect("create index");
        write_cache_atomic(
            &idx,
            KrateName::crates_io("serde").unwrap(),
            br#"{"name":"serde","vers":"0.1.0","deps":[],"features":{},"cksum":"0000000000000000000000000000000000000000000000000000000000000000","yanked":false}"#,
            "etag: stale",
        )
        .expect("seed cache");

        // Stale entry satisfies the bare-name probe …
        assert!(is_cached(&tmp, url, "serde", "0.1.0"));
        // … but not the version we actually need.
        assert!(!is_cached(&tmp, url, "serde", "1.0.228"));

        let v = lookup_version(&tmp, url, "serde", "1.0.228").expect("re-fetch past stale cache");
        assert_eq!(v.version.as_str(), "1.0.228");

        let _ = fs::remove_dir_all(&tmp);
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

        // Truly malformed value.
        let mut h = http::HeaderMap::new();
        h.insert(http::header::RETRY_AFTER, "not a date".parse().unwrap());
        assert_eq!(parse_retry_after(&h), None);
    }

    #[test]
    fn parse_retry_after_http_date() {
        // A date well in the future — exact delta varies, just bound it.
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::RETRY_AFTER,
            // RFC 1123 / IMF-fixdate.
            "Sun, 06 Nov 2050 08:49:37 GMT".parse().unwrap(),
        );
        let d = parse_retry_after(&h).expect("future date should parse");
        assert!(d > Duration::from_secs(60 * 60 * 24)); // > 1 day

        // A date in the past returns None (clock skew, stale header).
        let mut h = http::HeaderMap::new();
        h.insert(
            http::header::RETRY_AFTER,
            "Wed, 01 Jan 1990 00:00:00 GMT".parse().unwrap(),
        );
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

    /// 304 means the conditional fetch hit a cache lookup_version already
    /// rejected — retrying can never resolve it.
    #[test]
    fn classify_status_304_is_permanent() {
        let h = http::HeaderMap::new();
        assert!(matches!(
            classify_status(304, &h, "serde", "https://example/"),
            FetchError::Permanent(_)
        ));
    }
}
