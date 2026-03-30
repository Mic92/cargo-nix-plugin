//! Read Cargo.toml and fix rustc flags for buildRustCrate offline mode.
//!
//! Usage: read-crate-info <Cargo.toml> <LIB_RUSTC_OPTS> <BIN_RUSTC_OPTS> <SHARED_LIB_EXT>
//!
//! - Detects edition and proc-macro from Cargo.toml
//! - Adds --edition if missing
//! - Fixes --crate-type and adds --extern proc_macro for proc-macros
//! - Rewrites --extern paths from .rlib to .so/.dylib for proc-macro deps
//!   (identified by a .proc-macro marker file in the dep's lib/ directory)
//!
//! Outputs shell variable assignments (only when changed):
//!   LIB_RUSTC_OPTS='...'
//!   BIN_RUSTC_OPTS='...'
//!   CRATE_IS_PROC_MACRO=1

use std::path::Path;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 5 {
        eprintln!("usage: read-crate-info <Cargo.toml> <LIB_RUSTC_OPTS> <BIN_RUSTC_OPTS> <SHARED_LIB_EXT>");
        std::process::exit(1);
    }

    let mut lib_opts = args[2].clone();
    let mut bin_opts = args[3].clone();
    let shared_lib_ext = &args[4];
    let mut lib_changed = false;
    let mut bin_changed = false;

    // Read Cargo.toml
    if let Ok(content) = std::fs::read_to_string(&args[1]) {
        if let Ok(doc) = toml::from_str::<toml::Value>(&content) {
            // Edition
            if let Some(edition) = doc
                .get("package")
                .and_then(|p| p.get("edition"))
                .and_then(|v| v.as_str())
            {
                if !lib_opts.contains("--edition") {
                    let flag = format!(" --edition {edition}");
                    lib_opts.push_str(&flag);
                    bin_opts.push_str(&flag);
                    lib_changed = true;
                    bin_changed = true;
                }
            }

            // Proc-macro
            let is_proc_macro = doc
                .get("lib")
                .and_then(|l| l.get("proc-macro").or_else(|| l.get("proc_macro")))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if is_proc_macro {
                lib_opts = lib_opts.replace("--crate-type lib", "--crate-type proc-macro");
                if !lib_opts.contains("--extern proc_macro") {
                    lib_opts.push_str(" --extern proc_macro");
                }
                lib_changed = true;
                println!("CRATE_IS_PROC_MACRO=1");
            }
        }
    }

    // Fix --extern paths for proc-macro dependencies.
    // When a dep's store path contains a proc-macro.marker, rewrite
    // the --extern from .rlib to the platform's shared library extension.
    lib_opts = fixup_extern_paths(&lib_opts, shared_lib_ext, &mut lib_changed);
    bin_opts = fixup_extern_paths(&bin_opts, shared_lib_ext, &mut bin_changed);

    if lib_changed {
        println!("LIB_RUSTC_OPTS='{lib_opts}'");
    }
    if bin_changed {
        println!("BIN_RUSTC_OPTS='{bin_opts}'");
    }
}

/// Check if a dep is a proc-macro by looking for a proc-macro.marker file.
/// Given an extern path like `/nix/store/xxx-foo/lib/libfoo-hash.rlib`,
/// check if `/nix/store/xxx-foo/lib/proc-macro.marker` exists.
fn is_proc_macro_dep(rlib_path: &str) -> bool {
    let path = Path::new(rlib_path);
    if let Some(dir) = path.parent() {
        dir.join("proc-macro.marker").exists()
    } else {
        false
    }
}

/// Rewrite --extern name=path.rlib to the shared library for proc-macro deps.
fn fixup_extern_paths(opts: &str, shared_lib_ext: &str, changed: &mut bool) -> String {
    let mut result = Vec::new();
    for token in opts.split_whitespace() {
        if let Some(eq_pos) = token.find('=') {
            let path = &token[eq_pos + 1..];
            if path.ends_with(".rlib") && is_proc_macro_dep(path) {
                let new_path = format!("{}{shared_lib_ext}", &path[..path.len() - 5]);
                result.push(format!("{}={new_path}", &token[..eq_pos]));
                *changed = true;
                continue;
            }
        }
        result.push(token.to_string());
    }
    result.join(" ")
}
