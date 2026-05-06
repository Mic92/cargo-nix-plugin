# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

{
  rustPlatform,
}:

rustPlatform.buildRustPackage {
  pname = "build-rust-crate";
  version = "0.1.0";
  src = ../builder;
  cargoLock.lockFile = ../builder/Cargo.lock;
  doCheck = true;
}
