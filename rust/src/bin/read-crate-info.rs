//! Read Cargo.toml and surface fields that buildRustCrate can't learn
//! from the lockfile or sparse index (edition, proc-macro, build script
//! path, links key).
//!
//! Outputs shell variable assignments to stdout; consume with `eval "$(...)"`.
//!
//! Subcommand `configure <Cargo.toml>` (runs before build.rs compilation):
//!   CRATE_EDITION='2021'           # from [package] edition
//!   CRATE_BUILD='builder/main.rs'  # from [package] build, or build.rs default
//!   CRATE_LINKS='aws_lc_0_38_0'    # from [package] links
//!
//! Subcommand `build <Cargo.toml> <LIB_RUSTC_OPTS> <BIN_RUSTC_OPTS> <SHARED_LIB_EXT>`:
//!   LIB_RUSTC_OPTS='...'   # with --edition added, proc-macro crate-type fixed
//!   BIN_RUSTC_OPTS='...'   # with --edition added
//!   CRATE_IS_PROC_MACRO=1  # when [lib] proc-macro = true
//!
//! Both subcommands tolerate missing/unparsable Cargo.toml by emitting
//! nothing, so callers' existing values stand.

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(|s| s.as_str()) {
        Some("configure") if args.len() >= 3 => configure(&args[2]),
        Some("build") if args.len() >= 6 => build(&args[2], &args[3], &args[4], &args[5]),
        Some("fixup-deps") if args.len() >= 4 => fixup_deps(&args[2], &args[3]),
        _ => {
            eprintln!("usage:");
            eprintln!("  read-crate-info configure <Cargo.toml>");
            eprintln!("  read-crate-info build <Cargo.toml> <LIB_RUSTC_OPTS> <BIN_RUSTC_OPTS> <SHARED_LIB_EXT>");
            eprintln!("  read-crate-info fixup-deps <DEPS_OPTS> <SHARED_LIB_EXT>");
            std::process::exit(1);
        }
    }
}

fn read_manifest(path: &str) -> Option<toml::Value> {
    toml::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn shell_assign(name: &str, value: &str) {
    // Shell-safe single-quoting: close, emit '\'' , reopen.
    let escaped = value.replace('\'', r"'\''");
    println!("{name}='{escaped}'");
}

fn pkg_str<'a>(pkg: Option<&'a toml::Value>, key: &str) -> Option<&'a str> {
    pkg.and_then(|p| p.get(key)).and_then(|v| v.as_str())
}

/// Emit shell vars for the configure phase.
///
/// These fields live only in the crate's published Cargo.toml, not in
/// Cargo.lock or the sparse registry index, so the eval-time resolver
/// returns null for them. Published manifests are normalized by cargo
/// on publish — no workspace inheritance, no `foo.workspace = true`,
/// values are always literal.
///
/// Emits (all optional):
///   CRATE_EDITION         → --edition for build.rs compilation
///   CRATE_BUILD           → build script path (tri-state: path/false/default)
///   CRATE_LINKS           → CARGO_MANIFEST_LINKS
///   CRATE_LIB_PATH        → LIB_PATH override (e.g. bindgen uses "lib.rs")
///   CRATE_LIB_NAME        → LIB_NAME / CRATE_NAME override
///   CRATE_LIB_TYPES       → space-separated crate-type list
///   CRATE_PKG_DESCRIPTION/AUTHORS/REPOSITORY/LICENSE/HOMEPAGE/README/RUST_VERSION
///                         → CARGO_PKG_* env vars
fn configure(manifest_path: &str) {
    let Some(doc) = read_manifest(manifest_path) else {
        return;
    };
    let pkg = doc.get("package");
    let lib = doc.get("lib");

    // [package] —————————————————————————————————————————————————————————

    if let Some(e) = pkg_str(pkg, "edition") {
        shell_assign("CRATE_EDITION", e);
    }
    if let Some(l) = pkg_str(pkg, "links") {
        shell_assign("CRATE_LINKS", l);
    }

    // `build` tri-state: absent → default to build.rs if present,
    // string → verbatim, false → suppress.
    let manifest_dir = Path::new(manifest_path).parent().unwrap_or(Path::new("."));
    match pkg.and_then(|p| p.get("build")) {
        Some(v) if v.as_bool() == Some(false) => {}
        Some(v) => {
            if let Some(path) = v.as_str() {
                shell_assign("CRATE_BUILD", path);
            }
        }
        None if manifest_dir.join("build.rs").exists() => {
            shell_assign("CRATE_BUILD", "build.rs");
        }
        None => {}
    }

    // CARGO_PKG_* passthrough. Most build scripts ignore these, but a
    // handful gate behaviour on rust-version (MSRV checks) or embed
    // description/repository into generated code.
    for (key, var) in [
        ("description", "CRATE_PKG_DESCRIPTION"),
        ("repository", "CRATE_PKG_REPOSITORY"),
        ("license", "CRATE_PKG_LICENSE"),
        ("homepage", "CRATE_PKG_HOMEPAGE"),
        ("readme", "CRATE_PKG_README"),
        ("rust-version", "CRATE_PKG_RUST_VERSION"),
    ] {
        if let Some(v) = pkg_str(pkg, key) {
            shell_assign(var, v);
        }
    }
    // authors is an array; CARGO_PKG_AUTHORS is colon-separated.
    if let Some(authors) = pkg
        .and_then(|p| p.get("authors"))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<&str> = authors.iter().filter_map(|a| a.as_str()).collect();
        shell_assign("CRATE_PKG_AUTHORS", &joined.join(":"));
    }

    // [lib] —————————————————————————————————————————————————————————————

    // lib.path: bindgen uses "lib.rs" (not the src/lib.rs default).
    // The shell fallback at build-crate.nix only checks src/lib.rs.
    if let Some(p) = lib.and_then(|l| l.get("path")).and_then(|v| v.as_str()) {
        shell_assign("CRATE_LIB_PATH", p);
    }

    // lib.name: cargo publish normalizes this to the package name with
    // `-` → `_`, which default.nix:338+:37 already compute. But emit it
    // anyway so the build writes the exact filename dependents expect.
    if let Some(n) = lib.and_then(|l| l.get("name")).and_then(|v| v.as_str()) {
        shell_assign("CRATE_LIB_NAME", n);
    }

    // lib.crate-type: ["lib"], ["cdylib","rlib"], ["staticlib"], etc.
    // Space-separated for the build subcommand to rewrite
    // `--crate-type lib` into the right set. Dependents always want the
    // rlib (default.nix:58), which cdylib+rlib crates produce alongside
    // the cdylib, so eval-time crateType staying ["lib"] is fine.
    if let Some(types) = lib
        .and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type")))
        .and_then(|v| v.as_array())
    {
        let joined: Vec<&str> = types.iter().filter_map(|t| t.as_str()).collect();
        if !joined.is_empty() {
            shell_assign("CRATE_LIB_TYPES", &joined.join(" "));
        }
    }
}

/// Patch rustc option strings for the build phase.
fn build(manifest_path: &str, lib_opts: &str, bin_opts: &str, shared_lib_ext: &str) {
    let mut lib_opts = lib_opts.to_owned();
    let mut bin_opts = bin_opts.to_owned();
    let mut lib_changed = false;
    let mut bin_changed = false;

    if let Some(doc) = read_manifest(manifest_path) {
        let pkg = doc.get("package");
        let lib = doc.get("lib");

        if let Some(edition) = pkg_str(pkg, "edition") {
            if !lib_opts.contains("--edition") {
                let flag = format!(" --edition {edition}");
                lib_opts.push_str(&flag);
                bin_opts.push_str(&flag);
                lib_changed = true;
                bin_changed = true;
            }
        }

        // proc-macro takes precedence over any crate-type array.
        let is_proc_macro = lib
            .and_then(|l| l.get("proc-macro").or_else(|| l.get("proc_macro")))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        if is_proc_macro {
            // str::replace is literal: "--crate-type lib" can't match inside
            // "--crate-type rlib" (no spurious substring). The eval-time
            // default is always the exact token "--crate-type lib".
            lib_opts = lib_opts.replace("--crate-type lib", "--crate-type proc-macro");
            // Exact-token match — .contains("proc_macro") would also match
            // `--extern proc_macro2=...` and skip adding the stdlib.
            if !lib_opts.split_whitespace().any(|t| t == "proc_macro") {
                lib_opts.push_str(" --extern proc_macro");
            }
            lib_changed = true;
            println!("CRATE_IS_PROC_MACRO=1");
        } else if let Some(types) = lib
            .and_then(|l| l.get("crate-type").or_else(|| l.get("crate_type")))
            .and_then(|v| v.as_array())
        {
            // ["cdylib", "rlib"] etc. Replace the lone eval-time
            // `--crate-type lib` with one flag per declared type.
            // rustc accepts repeated --crate-type and emits all artefacts.
            let flags: String = types
                .iter()
                .filter_map(|t| t.as_str())
                .map(|t| format!("--crate-type {t}"))
                .collect::<Vec<_>>()
                .join(" ");
            if !flags.is_empty() && lib_opts.contains("--crate-type lib") {
                lib_opts = lib_opts.replace("--crate-type lib", &flags);
                lib_changed = true;
            }
        }

        // LIB_PATH / CRATE_NAME are set from eval-time values in
        // build-crate.nix immediately before this runs, clobbering the
        // configure-phase exports. Re-emit here so they take effect.
        if let Some(p) = lib.and_then(|l| l.get("path")).and_then(|v| v.as_str()) {
            shell_assign("LIB_PATH", p);
        }
        if let Some(n) = lib.and_then(|l| l.get("name")).and_then(|v| v.as_str()) {
            shell_assign("CRATE_NAME", n);
        }
    }

    // Fix --extern paths for proc-macro dependencies: the resolver emits
    // .rlib paths unconditionally, but proc-macro crates produce .so/.dylib.
    lib_opts = fixup_extern_paths(&lib_opts, shared_lib_ext, &mut lib_changed);
    bin_opts = fixup_extern_paths(&bin_opts, shared_lib_ext, &mut bin_changed);

    if lib_changed {
        shell_assign("LIB_RUSTC_OPTS", &lib_opts);
    }
    if bin_changed {
        shell_assign("BIN_RUSTC_OPTS", &bin_opts);
    }
}

/// Rewrite --extern paths in a pre-baked deps string.
/// Fixes .rlib → .so for proc-macro deps and [lib] name mismatches.
/// Prints a shell assignment for BUILD_DEPS if anything changed.
fn fixup_deps(deps_opts: &str, shared_lib_ext: &str) {
    let mut changed = false;
    let fixed = fixup_extern_paths(deps_opts, shared_lib_ext, &mut changed);
    if changed {
        shell_assign("BUILD_DEPS", &fixed);
    }
}

/// Check if a dep is a proc-macro by looking for a proc-macro.marker file.
/// Given an extern path like `/nix/store/xxx-foo/lib/libfoo-hash.rlib`,
/// check if `/nix/store/xxx-foo/lib/proc-macro.marker` exists.
fn is_proc_macro_dep(rlib_path: &str) -> bool {
    Path::new(rlib_path)
        .parent()
        .map(|d| d.join("proc-macro.marker").exists())
        .unwrap_or(false)
}

/// If the dep's Cargo.toml had a `[lib] name` differing from the crate name,
/// install-crate.nix wrote the real name here. We need it to fix the
/// `--extern NAME=` key — `use utf8::` compiles against `--extern utf8=`,
/// not `--extern utf_8=`.
fn real_lib_name(rlib_path: &str) -> Option<String> {
    let marker = Path::new(rlib_path).parent()?.join("lib-name");
    std::fs::read_to_string(marker)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Rewrite `--extern name=path` tokens:
///   - proc-macro deps: .rlib → .so/.dylib (eval-time can't know)
///   - [lib] name override: fix both the name key AND the filename
///     (rustls-webpki ships libwebpki.rlib; utf-8 ships libutf8.rlib;
///     sparse index doesn't carry lib.name)
///
/// Skips tokens where the name contains `:` (e.g. `noprelude:core`) —
/// those are stdlib preludes, not disk-backed rlibs.
fn fixup_extern_paths(opts: &str, shared_lib_ext: &str, changed: &mut bool) -> String {
    let mut result = Vec::new();
    for token in opts.split_whitespace() {
        let Some((name, path)) = token.split_once('=') else {
            result.push(token.to_string());
            continue;
        };
        // noprelude:std and friends — leave alone
        if name.contains(':') {
            result.push(token.to_string());
            continue;
        }

        let mut out_name = name.to_string();
        let mut out_path = path.to_string();
        let mut touched = false;

        // [lib] name override — marker file written by install-crate.nix
        // when CRATE_NAME (build-time truth) ≠ normalized crate name
        // (eval-time guess). Rewrite both key and filename.
        if let Some(real_name) = real_lib_name(path) {
            let (dir, file) = path.rsplit_once('/').unwrap_or((".", path));
            // file is lib{WRONG}-{hash}.{ext}; keep -{hash}.{ext}
            let suffix = file
                .strip_prefix("lib")
                .and_then(|rest| rest.find('-').map(|i| &rest[i..]))
                .unwrap_or(file);
            out_path = format!("{dir}/lib{real_name}{suffix}");
            out_name = real_name;
            touched = true;
        }

        // proc-macro: .rlib → .so/.dylib (applied after name fixup so a
        // proc-macro crate with a custom lib name gets both corrections)
        if out_path.ends_with(".rlib") && is_proc_macro_dep(&out_path) {
            out_path = format!("{}{shared_lib_ext}", &out_path[..out_path.len() - 5]);
            touched = true;
        }

        if touched {
            result.push(format!("{out_name}={out_path}"));
            *changed = true;
        } else {
            result.push(token.to_string());
        }
    }
    result.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Create a temp directory for the duration of the test. Std has no
    /// `tempdir`; this is the minimal version without pulling in tempfile.
    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn new() -> Self {
            // rand-ish via time-ns; CI runs these in isolation so collisions
            // within a single process are the only concern.
            let p = std::env::temp_dir().join(format!(
                "rci-test-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .subsec_nanos()
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// The [lib] name rewrite (tungstenite/utf-8): tungstenite says `use utf8::`,
    /// but eval-time emits `--extern utf_8=.../libutf_8-HASH.rlib` because the
    /// sparse index has no lib.name. install-crate.nix writes a `lib-name`
    /// marker; this fn reads it and fixes both halves of --extern NAME=PATH.
    #[test]
    fn fixup_extern_rewrites_lib_name_and_path() {
        let tmp = Tmp::new();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();

        // utf-8 crate: [lib] name = "utf8"
        std::fs::write(lib.join("lib-name"), "utf8\n").unwrap();
        let rlib = lib.join("libutf_8-abc123.rlib"); // eval-time guess
        std::fs::write(&rlib, "").unwrap();

        // Plain dep with no marker — must pass through untouched
        let plain_dir = tmp.path().join("plain/lib");
        std::fs::create_dir_all(&plain_dir).unwrap();
        let plain = plain_dir.join("libbytes-def456.rlib");
        std::fs::write(&plain, "").unwrap();

        let opts = format!(
            "--extern utf_8={} --extern bytes={} --edition 2021",
            rlib.display(),
            plain.display()
        );

        let mut changed = false;
        let out = fixup_extern_paths(&opts, ".so", &mut changed);

        assert!(changed);
        assert!(
            out.contains(&format!("utf8={}/libutf8-abc123.rlib", lib.display())),
            "expected utf8=.../libutf8-abc123.rlib in: {out}"
        );
        assert!(
            out.contains(&format!("bytes={}", plain.display())),
            "unmarked crate stays verbatim: {out}"
        );
        assert!(
            out.contains("--edition 2021"),
            "non-extern tokens preserved"
        );
        assert!(!out.contains("utf_8"), "old name gone: {out}");
    }

    /// A proc-macro crate with a [lib] name override needs BOTH rewrites.
    /// Name fixup runs first, then proc-macro .rlib→.so on the rewritten
    /// path. If someone reorders the blocks, this fails.
    #[test]
    fn fixup_extern_composes_lib_name_with_proc_macro() {
        let tmp = Tmp::new();
        let lib = tmp.path().join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        std::fs::write(lib.join("lib-name"), "realderive").unwrap();
        std::fs::write(lib.join("proc-macro.marker"), "").unwrap();
        let rlib = lib.join("libmy_derive-abc.rlib");
        std::fs::write(&rlib, "").unwrap();

        let mut changed = false;
        let out = fixup_extern_paths(
            &format!("--extern my_derive={}", rlib.display()),
            ".so",
            &mut changed,
        );

        assert!(changed);
        // name: my_derive → realderive; ext: .rlib → .so
        assert!(
            out.contains(&format!(
                "realderive={}/librealderive-abc.so",
                lib.display()
            )),
            "got: {out}"
        );
    }

    /// The `touched` flag must be per-token. An earlier rewrite shouldn't
    /// cause later unchanged tokens to be spuriously rebuilt from parts
    /// (which would work today but is a latent shape-change hazard).
    #[test]
    fn fixup_extern_changed_flag_is_per_token() {
        let tmp = Tmp::new();
        let marked = tmp.path().join("marked/lib");
        std::fs::create_dir_all(&marked).unwrap();
        std::fs::write(marked.join("lib-name"), "real").unwrap();

        let plain = tmp.path().join("plain/lib");
        std::fs::create_dir_all(&plain).unwrap();

        // Marked dep FIRST, then plain dep. If `changed` persists across
        // iterations, the plain token gets rebuilt as "plain=.../libplain..."
        // instead of kept as the original string.
        let opts = format!(
            "--extern wrong={}/libwrong-xxx.rlib --extern plain={}/libplain-yyy.rlib",
            marked.display(),
            plain.display()
        );

        let mut changed = false;
        let out = fixup_extern_paths(&opts, ".so", &mut changed);

        assert!(changed);
        // Plain token preserved byte-for-byte
        assert!(out.contains(&format!("plain={}/libplain-yyy.rlib", plain.display())));
    }
}
