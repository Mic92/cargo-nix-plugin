# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Verify the plugin's HTTP retry loop recovers from transient failures.
#
# Reuses the fake sparse-index server (./fake-sparse-server.py) with its
# --fail-mode flag to simulate the three transient failure classes the
# retry loop in registry.rs is designed to handle:
#
#   503-then-ok:N           First N requests return 503, then serve.
#   429-with-retry-after:N  First N requests return 429+Retry-After, then.
#   corrupt-body:N          First N requests return truncated JSON, then.
#
# For each mode we evaluate the plugin against the same sample workspace
# as remote-sparse-test.nix, with an empty CARGO_HOME so every crate's
# metadata must be HTTP-fetched. We assert (a) the resolved deps are
# non-empty (the retry actually completed), and (b) the access log shows
# at least N+1 hits per crate (proving each fetch was retried N times).
#
# The corrupt-body case is the production scenario from #349206 — a CDN
# returned malformed JSON for asn1-rs and the resolver silently fell back
# to empty deps. Without retries this test fails the same way.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

let
  # One sub-test for a given fail-mode arg.  Spawns a fresh server with
  # that mode, runs the plugin, asserts both data and access-log shape.
  mkRetryCase =
    {
      name,
      failMode,
      # Number of times each path is expected to be hit before success.
      # Equals N+1 for the *-then-ok:N modes.
      expectedHitsPerPath,
    }:
    pkgs.runCommand "cargo-nix-plugin-retry-test-${name}"
      {
        nativeBuildInputs = [
          nix
          pkgs.python3
          pkgs.jq
        ];
        requiredSystemFeatures = [ "recursive-nix" ];
      }
      ''
        set -euo pipefail
        export HOME=$(mktemp -d)
        export CARGO_HOME=$HOME/.cargo
        mkdir -p "$CARGO_HOME"

        PORT_FILE=$(mktemp)
        ACCESS_LOG=$(mktemp)
        python3 ${./fake-sparse-server.py} \
          --fail-mode '${failMode}' \
          "$PORT_FILE" "$ACCESS_LOG" ${./fake-sparse-index} &
        SERVER_PID=$!
        trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

        while [[ ! -s $PORT_FILE ]]; do
          kill -0 $SERVER_PID
        done
        PORT=$(<"$PORT_FILE")
        echo "fake sparse index (${name}) at http://127.0.0.1:$PORT/"

        WORKSPACE=$(mktemp -d)
        cp -r ${sampleProject}/. "$WORKSPACE/"
        chmod -R u+w "$WORKSPACE"

        INDEX_URL="sparse+http://127.0.0.1:$PORT/"
        sed -i "s|registry+https://github.com/rust-lang/crates.io-index|$INDEX_URL|g" \
          "$WORKSPACE/Cargo.lock"

        nix-instantiate --eval --strict --json --read-write-mode \
          --option plugin-files ${plugin}/lib/nix/plugins \
          --expr "
            let
              pkgs = import ${pkgs.path} { system = \"${pkgs.stdenv.hostPlatform.system}\"; };
              cargoNix = import ${pluginSrc}/lib {
                inherit pkgs;
                manifestPath = \"$WORKSPACE/Cargo.toml\";
              };
            in {
              serdeDeps = map (d: d.name) cargoNix.resolved.crates.serde.dependencies;
              httpDeps  = map (d: d.name) cargoNix.resolved.crates.http.dependencies;
            }
          " | tee result.json

        echo "--- requests served ---"
        cat "$ACCESS_LOG"
        echo "-----------------------"

        # The retry loop must have actually produced real deps. If it
        # gave up, dependencies would be empty and the buildRustCrate
        # invocation downstream would have no --extern flags.
        jq -e '.serdeDeps | length > 0' result.json
        jq -e '.httpDeps  | length > 0' result.json
        jq -e '.httpDeps  | index("bytes") != null' result.json

        # Each crate's path should appear at least N+1 times: N failures
        # plus the final successful attempt. (Could be more if prefetch
        # and serial fallback both hit, but never less.)
        for path in /se/rd/serde /ht/tp/http; do
          hits=$(grep -c "^$path$" "$ACCESS_LOG" || true)
          if [ "$hits" -lt ${toString expectedHitsPerPath} ]; then
            echo "FAIL: $path was hit $hits times, expected >= ${toString expectedHitsPerPath}"
            exit 1
          fi
          echo "OK: $path hit $hits times (>= ${toString expectedHitsPerPath} expected)"
        done

        mv result.json $out
      '';
in
pkgs.linkFarmFromDrvs "cargo-nix-plugin-retry-tests" [
  (mkRetryCase {
    name = "503";
    failMode = "503-then-ok:2";
    expectedHitsPerPath = 3; # 2 fails + 1 success
  })
  (mkRetryCase {
    name = "429";
    failMode = "429-with-retry-after:2";
    expectedHitsPerPath = 3;
  })
  (mkRetryCase {
    name = "corrupt-body";
    failMode = "corrupt-body:2";
    expectedHitsPerPath = 3;
  })
]
