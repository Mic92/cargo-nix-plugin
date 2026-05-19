# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# End-to-end test of the dynamic-derivations mode (no plugin loaded).
#
# Inside a recursive-nix sandbox, build sample-project-nodeps with a
# vanilla Nix into a chroot store. lib/default.nix detects that
# `builtins.resolveCargoWorkspace` is missing, builds a planner
# derivation (vendor -> cargo metadata -> cargo-nix-resolve ->
# nix-instantiate), and emits a buildable .drv via builtins.outputOf.
#
# Mirrors tests/chroot-store-test.nix: seed the toolchain into the
# chroot store, pin via builtins.storePath so the inner eval reuses
# the seeded paths instead of re-deriving (and rebuilding) from source.
{
  pkgs,
  pluginSrc,
  sampleProject,
  # A Nix WITHOUT the cargo-nix plugin, with experimental-features
  # available (need not default-enable them).
  nix,
}:

let
  buildRustCrateBin = pkgs.callPackage ../nix/build-rust-crate-bin.nix { };
  cargoNixResolve = pkgs.callPackage ../nix/cargo-nix-resolve.nix { };
  vendorDir = pkgs.rustPlatform.importCargoLock {
    lockFile = sampleProject + "/Cargo.lock";
    allowBuiltinFetchGit = true;
  };
in
pkgs.runCommand "cargo-nix-plugin-dyn-drv-test"
  {
    nativeBuildInputs = [ nix ];
    requiredSystemFeatures = [ "recursive-nix" ];
    seed = pkgs.linkFarm "dyn-drv-seed" {
      inherit (pkgs)
        rustc
        cargo
        mold
        jq
        stdenv
        stdenvNoCC
        ;
      inherit
        buildRustCrateBin
        cargoNixResolve
        vendorDir
        sampleProject
        pluginSrc
        nix
        ;
      nixpkgs = pkgs.path;
    };
  }
  ''
    set -euo pipefail
    export HOME=$(mktemp -d)
    CHROOT=$(mktemp -d)

    # No nix.conf in the sandbox: opt the inner nix into everything the
    # fallback path needs. recursive-nix is both an experimental feature
    # and a system feature -- the latter lets the chroot store's builder
    # accept the planner derivation.
    export NIX_CONFIG="
      experimental-features = nix-command flakes ca-derivations dynamic-derivations recursive-nix
      system-features = recursive-nix
      substituters =
    "

    # The test is meaningless if the test nix has the plugin loaded.
    if [[ "$(${nix}/bin/nix eval --expr 'builtins ? resolveCargoWorkspace')" != "false" ]]; then
      echo "FAIL: test nix has the plugin loaded; cannot exercise dyn-drv mode"; exit 1
    fi

    ${nix}/bin/nix copy --no-check-sigs --to "local?root=$CHROOT" "$seed"

    # Pin everything the planner + crate builds touch to the seeded paths
    # via builtins.storePath, so the inner eval never re-derives jq/perl/
    # autoconf/gcc from source. This shadows just enough of `pkgs` for
    # nix/dyn-drv.nix and nix/build-rust-crate to resolve their
    # tool inputs to the closure already in the chroot store.
    built=$(${nix}/bin/nix build \
      --store "$CHROOT" \
      --impure --no-link --print-out-paths -L \
      --expr '
        let
          pkgs0 = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
          sp = builtins.storePath;
          pkgs = pkgs0 // {
            cargo = sp ${pkgs.cargo};
            jq = sp ${pkgs.jq};
            nix = sp ${nix};
            rustPlatform = pkgs0.rustPlatform // { importCargoLock = _: sp ${vendorDir}; };
            callPackage =
              path: args:
              if builtins.baseNameOf (toString path) == "cargo-nix-resolve.nix" then
                sp ${cargoNixResolve}
              else
                pkgs0.callPackage path args;
          };
        in
        (import ${pluginSrc}/lib {
          inherit pkgs;
          src = ${sampleProject};
        }).workspaceMembers."nodeps-bin".build
      ')

    bin="$CHROOT$(readlink "$CHROOT$built")/bin/nodeps-bin"
    [[ -x "$bin" ]] || { echo "FAIL: nodeps-bin missing at $bin"; exit 1; }
    [[ "$("$bin")" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected output: $("$bin")"; exit 1; }
    echo "PASS: dyn-drv mode built and ran nodeps-bin"

    echo "ALL DYN-DRV FALLBACK TESTS PASSED" > $out
  ''
