# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

{
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "cargo-nix-prefetch";
  version = "0.1.0";
  src = ../rust;
  cargoLock.lockFile = ../rust/Cargo.lock;
  cargoBuildFlags = [
    "--bin"
    "cargo-nix-prefetch"
  ];
  doCheck = false;

  meta.description = "Warm the sparse registry index cache for a Cargo workspace";
  meta.mainProgram = "cargo-nix-prefetch";
}
