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
}:

let
  rustLib = rustPlatform.buildRustPackage {
    pname = "cargo-nix-plugin-core";
    version = "0.1.0";
    src = ../rust;
    cargoHash = "sha256-L4bbQGLZBuTb/ZshMWRGDaCCs+ZiylcynfMGx1BWdwI=";
    # Bake in the cargo store path so the plugin can shell out at eval time
    CARGO_NIX_PLUGIN_CARGO_PATH = "${cargo}/bin/cargo";
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
  ];

  meta = {
    description = "Nix plugin for resolving Cargo workspaces";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
