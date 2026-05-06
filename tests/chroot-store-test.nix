# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Regression test: building into a chroot store (nix build --store /tmp/...)
# requires that the plugin remaps logical store paths to real filesystem paths
# so cargo metadata can find Cargo.toml on the host during eval.
#
# This test verifies that the C++ plugin's remapStorePath() correctly handles
# chroot stores, so `nix build --store <dir>` works without any special
# manifestPath parameter.
#
# Uses sample-project-nodeps (no crates.io dependencies) so cargo metadata
# completes inside the network-less build sandbox. Crates.io resolution is
# orthogonal to the store path remapping under test.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-chroot-store-test"
  {
    nativeBuildInputs = [ nix ];
    requiredSystemFeatures = [ "recursive-nix" ];
    # Pre-seed the chroot store via `nix copy`. Wrap in linkFarm so we can
    # copy the whole toolchain closure in one shot. Can't pre-build the
    # actual crate here — the wrapper lib calls builtins.resolveCargoWorkspace
    # unconditionally, and the flake evaluator doesn't have the plugin loaded.
    buildClosureSeed = pkgs.linkFarm "chroot-store-seed" {
      rustc = pkgs.rustc;
      cargo = pkgs.cargo;
      stdenv = pkgs.stdenv;
      mold = pkgs.mold;
      buildRustCrateBin = pkgs.callPackage ../nix/build-rust-crate-bin.nix { };
      sampleProject = sampleProject;
      pluginSrc = pluginSrc;
      nixpkgs = pkgs.path;
    };
  }
  ''
    export HOME=$(mktemp -d)
    # The build sandbox has no nix.conf, so the new CLI refuses `nix build`
    # with "experimental feature nix-command is disabled".
    export NIX_CONFIG="experimental-features = nix-command"
    CHROOT=$(mktemp -d)

    # --- Test: default manifestPath with chroot store ---
    # Uses nix build --store to exercise the remapStorePath() codepath.
    # Without the fix, this fails with:
    #   "cargo metadata failed: manifest path /nix/store/xxx/Cargo.toml does not exist"
    echo "Test: chroot store with auto-remapped manifest path"

    # Seed the chroot store. `nix copy` reads from the outer /nix/store
    # (visible in the recursive-nix sandbox) and registers the closure in
    # the chroot store's db. The linkFarm bundles everything the inner
    # build needs so a single copy covers it.
    ${nix}/bin/nix copy --no-check-sigs --to "local?root=$CHROOT" "$buildClosureSeed"

    # Pin the inner buildRustCrate to the exact store paths copied above so
    # the derivations match what's in the chroot store — `import pkgs.path`
    # and the flake's pkgs can derive different output paths for the same
    # package, and a near-miss means rebuilding jq (and perl, and autoconf)
    # from source. Empty substituters so we never attempt cache.nixos.org.
    inner_build() {
      local attr=$1; shift
      ${nix}/bin/nix build \
        --store "$CHROOT" \
        --substituters "" \
        --option plugin-files "${plugin}/lib/nix/plugins" \
        --impure --no-link "$@" \
        --expr '
          let
            pkgs = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
            pinnedBuildRustCrate = pkgs.callPackage (builtins.storePath ${pluginSrc}/nix/build-rust-crate) {
              rustc = builtins.storePath ${pkgs.rustc};
              cargo = builtins.storePath ${pkgs.cargo};
              mold = builtins.storePath ${pkgs.mold};
              buildRustCrateBin = builtins.storePath ${
                pkgs.callPackage ../nix/build-rust-crate-bin.nix { }
              };
            };
          in (import ${pluginSrc}/lib {
            inherit pkgs;
            src = ${sampleProject};
            buildRustCrateForPkgs = _: _: pinnedBuildRustCrate;
          }).workspaceMembers.'"$attr"
    }

    inner_build 'nodeps-bin.build'
    echo "PASS: chroot store build succeeded"

    # Verify the binary was actually placed in the chroot store and runs.
    built_bin=$(find "$CHROOT/nix/store" -name nodeps-bin -type f -executable | head -1)
    [[ -n "$built_bin" ]] || {
      echo "FAIL: nodeps-bin not found in chroot store"
      exit 1
    }

    bin_output=$("$built_bin")
    [[ "$bin_output" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected output: $bin_output"
      exit 1
    }
    echo "PASS: binary in chroot store runs correctly"

    multifile=$(find "$CHROOT/nix/store" -name multifile -type f -executable | head -1)
    [[ -n "$multifile" && "$($multifile)" == "multifile ok" ]] || {
      echo "FAIL: src/bin/<name>/main.rs autodiscovery did not produce a working bin"
      exit 1
    }
    echo "PASS: src/bin/<name>/main.rs autodiscovered"

    # Target-discovery parity: nodeps-mixed has one explicit [[bin]] plus an
    # inferred src/main.rs, edition.workspace=true, and a dotfile in src/bin/.
    inner_build 'nodeps-mixed.build' --print-out-paths > mixed-out
    mixed_out=$(cat mixed-out)
    [[ "$($CHROOT$mixed_out/bin/nodeps-mixed)" == "mixed-main ok" ]] || {
      echo "FAIL: inferred src/main.rs lost when [[bin]] present"; exit 1;
    }
    [[ "$($CHROOT$mixed_out/bin/explicit)" == "explicit ok (ws-inherited)" ]] || {
      echo "FAIL: explicit [[bin]] or workspace-inherited description broken"; exit 1;
    }
    echo "PASS: [[bin]]+autobins merge, edition.workspace=true, dotfile skipped"

    # Regression for the lib_path-shadowing bug: buildTests=true compiles the
    # lib with --test (unit tests) and an integration test under tests/, both
    # linked against the just-built rlib.
    inner_build 'nodeps-lib.build.override { buildTests = true; }' --print-out-paths > tests-out
    tests_out=$(cat tests-out)
    ran=0
    for t in "$CHROOT$tests_out"/tests/*; do
      [[ -x "$t" ]] || continue
      "$t" 2>&1 | grep -q 'test result: ok' || {
        echo "FAIL: $t did not pass"; exit 1;
      }
      ran=$((ran+1))
    done
    [[ $ran -ge 2 ]] || { echo "FAIL: expected lib unit + integration test, ran $ran"; exit 1; }
    echo "PASS: buildTests=true produces runnable lib + integration tests"

    echo "ALL CHROOT STORE TESTS PASSED" > $out
  ''
