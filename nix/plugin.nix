{
  lib,
  stdenv,
  rustPlatform,
  cargo,
  pkg-config,
  libclang,
  nixLibs,
}:

let
  # The nix C API libraries we link against
  nix-expr-c = nixLibs.nix-expr-c;
  nix-store-c = nixLibs.nix-store-c;
  nix-util-c = nixLibs.nix-util-c;
in
rustPlatform.buildRustPackage {
  pname = "cargo-nix-plugin";
  version = "0.1.0";
  src = ../rust;
  cargoLock.lockFile = ../rust/Cargo.lock;

  nativeBuildInputs = [
    pkg-config
    libclang # needed by bindgen in the nix-bindings-*-sys crates
  ];

  buildInputs = [
    nix-expr-c
    nix-store-c
    nix-util-c
  ];

  # Bake in the cargo store path so the plugin can shell out at eval time
  CARGO_NIX_PLUGIN_CARGO_PATH = lib.getExe cargo;

  # bindgen needs libclang
  LIBCLANG_PATH = "${libclang.lib}/lib";

  postInstall = ''
    # Nix expects plugins in lib/nix/plugins/
    mkdir -p $out/lib/nix/plugins
    mv $out/lib/libcargo_nix_plugin_core.so $out/lib/nix/plugins/libcargo_nix_plugin.so 2>/dev/null || \
    mv $out/lib/libcargo_nix_plugin_core.dylib $out/lib/nix/plugins/libcargo_nix_plugin.dylib 2>/dev/null || \
    true
  '';

  meta = {
    description = "Nix plugin for resolving Cargo workspaces (pure Rust, using nix C API)";
    license = lib.licenses.mit;
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };
}
