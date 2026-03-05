{
  lib,
  stdenv,
  nixComponents,
  rustPlatform,
  cargo,
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
    # Bake in the cargo store path so the plugin can shell out at eval time
    CARGO_NIX_PLUGIN_CARGO_PATH = lib.getExe cargo;
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
