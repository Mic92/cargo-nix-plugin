# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# End-to-end test for offline mode: resolve from Cargo.lock + registry
# index cache (no cargo metadata), compile, and run the sample workspace.
#
# Instead of fetching from the network, we construct a CARGO_HOME from
# the fake-sparse-index fixtures already in the repo. This makes the
# test fully reproducible — no FOD, no network.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

let
  cargoNixPrefetch = pkgs.callPackage ../nix/cargo-nix-prefetch.nix { };

  # Build a CARGO_HOME by dogfooding `cargo-nix-prefetch` against the
  # checked-in fixture served over loopback. This is the exact workflow
  # users in network-restricted environments are expected to use
  # (prefetch → ship cache dir → point cargoHome at it), so the offline
  # build is gated on the public tool rather than a bespoke test helper.
  cargoHome =
    pkgs.runCommand "sample-project-cargo-home"
      {
        nativeBuildInputs = [
          cargoNixPrefetch
          pkgs.python3
        ];
      }
      ''
        set -euo pipefail
        PORT_FILE=$(mktemp)
        ACCESS_LOG=$(mktemp)
        python3 ${./fake-sparse-server.py} \
          "$PORT_FILE" "$ACCESS_LOG" ${./fake-sparse-index} &
        SERVER_PID=$!
        trap 'kill $SERVER_PID 2>/dev/null || true' EXIT
        while [[ ! -s $PORT_FILE ]]; do kill -0 $SERVER_PID; done
        PORT=$(<"$PORT_FILE")

        cargo-nix-prefetch \
          --manifest-path ${sampleProject}/Cargo.toml \
          --index "sparse+http://127.0.0.1:$PORT/" \
          --output "$out"

        cargo-nix-prefetch \
          --manifest-path ${sampleProject}/Cargo.toml \
          --index "sparse+http://127.0.0.1:$PORT/" \
          --output "$out" --check

        # Normalize the dir name so the eval (which sees Cargo.lock's
        # crates.io source URLs) finds it: the prefetch wrote it under
        # 127.0.0.1-<hash>, but find_index_dir computes the exact 1.85+
        # stable hash for sparse+https://index.crates.io/. Rename rather
        # than re-prefetch so the cache contents are exactly what the tool
        # produced. The hash is stable by construction — cargo froze it
        # in 1.85 (rust-lang/cargo#13684) and tame-index reproduces it.
        mv "$out"/registry/index/127.0.0.1-* \
           "$out"/registry/index/index.crates.io-1949cf8c6b5b557f
      '';
in
pkgs.runCommand "cargo-nix-plugin-offline-build-test"
  {
    nativeBuildInputs = [
      nix
      pkgs.jq
    ];
    requiredSystemFeatures = [ "recursive-nix" ];
  }
  ''
    export HOME=$(mktemp -d)

    cargoNixExpr='
      let
        pkgs = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
      in import ${pluginSrc}/lib {
        inherit pkgs;
        src = ${sampleProject};
        cargoHome = "${cargoHome}";
      }
    '

    # --- Eval test: offline resolution produces workspace members ---
    result=$(nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "builtins.attrNames ($cargoNixExpr).workspaceMembers")
    echo "Workspace members: $result"
    [[ "$result" == *"sample-bin"* ]] || { echo "FAIL: missing sample-bin"; exit 1; }
    [[ "$result" == *"sample-lib"* ]] || { echo "FAIL: missing sample-lib"; exit 1; }
    echo "PASS: offline eval produces workspace members"

    # --- Build test: compile and run the binary ---
    drv=$(nix-instantiate --show-trace \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.sample-bin.build")

    built=$(nix-store --realize "$drv" | grep -v -- '-lib$' | head -1)
    out_json=$("$built"/bin/sample-bin)
    echo "Output: $out_json"

    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }

    echo "PASS: offline build succeeded"

    # --- Clippy all-features: with clippyAllFeatures = true, sample-lib's
    # all-features-probe feature must be active in the clippy drv only.
    instantiate() {
      nix-instantiate \
        --option plugin-files "${plugin}/lib/nix/plugins" \
        --expr "$1"
    }

    allFeaturesExpr='
      let
        pkgs = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
      in import ${pluginSrc}/lib {
        inherit pkgs;
        src = ${sampleProject};
        cargoHome = "${cargoHome}";
        clippyAllFeatures = true;
      }
    '
    clippy_lib_drv=$(instantiate "($allFeaturesExpr).clippy.workspaceMembers.sample-lib.build")
    grep -q 'all-features-probe' "$clippy_lib_drv" || {
      echo "FAIL: all-features clippy drv lacks all-features-probe"
      exit 1
    }

    lib_drv=$(instantiate "($allFeaturesExpr).workspaceMembers.sample-lib.build")
    if grep -q 'all-features-probe' "$lib_drv"; then
      echo "FAIL: normal build drv enables all-features-probe"
      exit 1
    fi

    default_clippy_drv=$(instantiate "($cargoNixExpr).clippy.workspaceMembers.sample-lib.build")
    if grep -q 'all-features-probe' "$default_clippy_drv"; then
      echo "FAIL: default clippy drv enables all-features-probe"
      exit 1
    fi

    # Realize so the feature-gated code is actually compiled by clippy-driver.
    nix-store --realize "$clippy_lib_drv" > /dev/null
    echo "PASS: clippyAllFeatures lints feature-gated code"

    echo "$out_json" > $out
  ''
