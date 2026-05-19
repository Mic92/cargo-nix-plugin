// Copyright 2026 Anthropic, PBC
// SPDX-License-Identifier: Apache-2.0

//! Standalone resolver CLI for the dynamic-derivations mode.
//!
//! When the Nix plugin is not loaded, `lib/default.nix` runs a planner
//! derivation that invokes this binary instead of calling
//! `builtins.resolveCargoWorkspace`. The contract is identical to the FFI
//! shim: read a `PluginInput` JSON document, write a `WorkspaceResult`
//! JSON document. Sharing the serde types keeps the two paths from
//! drifting; `apiLevel` is checked on the Nix side either way.
//!
//! Usage:
//!   cargo-nix-resolve < input.json > resolved.json
//!   cargo-nix-resolve input.json   > resolved.json

use std::io::Read;
use std::process::ExitCode;

use cargo_nix_plugin_core::ffi::{validate_and_resolve, PluginInput};
use cargo_nix_plugin_core::resolve::API_LEVEL;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let input = match args.next() {
        Some(arg) if arg == "--api-level" => {
            // Lets the Nix side hard-assert the contract before paying for
            // a full resolve, mirroring `builtins.cargoNixApiLevel`.
            println!("{API_LEVEL}");
            return ExitCode::SUCCESS;
        }
        Some(arg) if arg == "-h" || arg == "--help" => {
            eprintln!("usage: cargo-nix-resolve [INPUT.json]\n  reads PluginInput JSON from INPUT.json or stdin, writes WorkspaceResult JSON to stdout");
            return ExitCode::SUCCESS;
        }
        Some(path) => match std::fs::read_to_string(&path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cargo-nix-resolve: failed to read {path}: {e}");
                return ExitCode::FAILURE;
            }
        },
        None => {
            let mut buf = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut buf) {
                eprintln!("cargo-nix-resolve: failed to read stdin: {e}");
                return ExitCode::FAILURE;
            }
            buf
        }
    };

    let input: PluginInput = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("cargo-nix-resolve: failed to parse PluginInput JSON: {e}");
            return ExitCode::FAILURE;
        }
    };

    match validate_and_resolve(&input) {
        Ok(result) => {
            // Wrap in the same { apiLevel, result } envelope the Nix side
            // would otherwise infer from the loaded plugin.
            let envelope = serde_json::json!({
                "apiLevel": API_LEVEL,
                "result": result,
            });
            println!("{}", serde_json::to_string(&envelope).unwrap());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("cargo-nix-resolve: {e}");
            ExitCode::FAILURE
        }
    }
}
