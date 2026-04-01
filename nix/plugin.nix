{
  lib,
  stdenv,
  nixComponents,
  rustPlatform,
  pkg-config,
  cmake,
  boost,
  nlohmann_json,
  llvmPackages ? null,
  enableSanitizers ? false,
}:

assert enableSanitizers -> llvmPackages != null;
assert enableSanitizers -> stdenv.cc.isClang;

let
  rustLib = rustPlatform.buildRustPackage {
    pname = "cargo-nix-plugin-core";
    version = "0.1.0";
    src = ../rust;
    cargoLock.lockFile = ../rust/Cargo.lock;
  };
in
stdenv.mkDerivation {
  pname = "cargo-nix-plugin";
  version = "0.1.0";

  src = ../cpp;

  nativeBuildInputs = [
    pkg-config
    cmake
  ];

  buildInputs = [
    nixComponents.nix-expr
    nixComponents.nix-store
    boost
    nlohmann_json
  ];

  cmakeFlags = [
    "-DRUST_LIB_DIR=${rustLib}/lib"
  ] ++ lib.optionals enableSanitizers [
    "-DENABLE_SANITIZERS=ON"
    "-DSANITIZER_RT_DIR=${llvmPackages.compiler-rt}/lib/linux"
  ];

  # Don't strip sanitizer-instrumented binaries — removes UBSan metadata.
  dontStrip = enableSanitizers;

  meta = {
    description = "Nix plugin for resolving Cargo workspaces";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
