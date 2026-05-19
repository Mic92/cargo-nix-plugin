# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

{
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "cargo-nix-resolve";
  version = "0.1.0";
  src = ../rust;
  cargoLock.lockFile = ../rust/Cargo.lock;
  cargoBuildFlags = [
    "--bin"
    "cargo-nix-resolve"
  ];
  doCheck = false;

  meta.description = "Standalone Cargo workspace resolver (PluginInput JSON in, WorkspaceResult JSON out)";
  meta.mainProgram = "cargo-nix-resolve";
}
