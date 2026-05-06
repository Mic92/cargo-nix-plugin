// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

use std::io::IsTerminal;
use std::process::Command;
use std::sync::OnceLock;

/// `std::env::set_var` wrapper. Safe: this builder is single-threaded at
/// every call site (no Rayon, jobserver init is later, Command::spawn is the
/// only fork and happens after env setup).
pub fn set_var(k: impl AsRef<std::ffi::OsStr>, v: impl AsRef<std::ffi::OsStr>) {
    unsafe { std::env::set_var(k, v) };
}

/// Process-wide jobserver sized to NIX_BUILD_CORES, passed to every rustc /
/// build-script via CARGO_MAKEFLAGS (matches cargo build_runner/mod.rs).
fn jobserver() -> Option<&'static jobserver::Client> {
    static JS: OnceLock<Option<jobserver::Client>> = OnceLock::new();
    JS.get_or_init(|| {
        let n: usize = std::env::var("NIX_BUILD_CORES")
            .ok()
            .and_then(|s| s.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1);
        let c = jobserver::Client::new(n).ok()?;
        // One token is this process; child sees implicit + (n-1) = n.
        c.acquire_raw().ok()?;
        Some(c)
    })
    .as_ref()
}

/// Set CARGO_MAKEFLAGS and mark the jobserver fds inheritable on `cmd`.
pub fn inherit_jobserver(cmd: &mut Command) {
    if let Some(c) = jobserver() {
        c.configure(cmd);
    }
}

pub fn echo_colored(msg: &str) {
    if std::io::stderr().is_terminal() {
        eprintln!("\x1b[0;1;32m{msg}\x1b[0m");
    } else {
        eprintln!("{msg}");
    }
}

pub fn echo_cmd(cmd: &Command) {
    let prog = cmd.get_program().to_string_lossy();
    let args: Vec<_> = cmd.get_args().map(|a| a.to_string_lossy()).collect();
    if std::io::stderr().is_terminal() {
        eprint!("\x1b[0;1;32mRunning\x1b[0m");
    } else {
        eprint!("Running");
    }
    eprintln!(" {prog} {}", args.join(" "));
}

/// Run a command, printing it if verbose. Exits on failure.
pub fn run_cmd(cmd: &mut Command, verbose: bool) -> Result<(), Box<dyn std::error::Error>> {
    inherit_jobserver(cmd);
    if verbose {
        echo_cmd(cmd);
    }
    let status = cmd.status()?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Remove .o files under a directory tree to avoid "wrong ELF type" errors.
pub fn remove_object_files(dir: &str) -> Result<(), Box<dyn std::error::Error>> {
    fn walk(dir: &std::path::Path) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                walk(&path)?;
            } else if path.extension().is_some_and(|e| e == "o") {
                std::fs::remove_file(&path)?;
            }
        }
        Ok(())
    }
    walk(std::path::Path::new(dir))?;
    Ok(())
}
