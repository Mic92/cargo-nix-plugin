# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Verify the crates.io index URL override mechanisms (#20).
#
# Unlike remote-sparse-test.nix, the workspace's Cargo.lock is left
# untouched — every package's `source` is still the upstream
# `registry+https://github.com/rust-lang/crates.io-index`. The plugin
# must redirect those lookups to the loopback mirror via:
#
#   1. cargo's `CARGO_REGISTRIES_CRATES_IO_INDEX` environment variable, and
#   2. `[source.crates-io] replace-with` in `.cargo/config.toml`.
#
# Each path is exercised against an unmodified workspace; if the override
# is not honoured, the resolver will try to reach index.crates.io,
# every fetch will fail inside the sandbox, and prefetch_index now
# returns an error — so the test fails loudly instead of silently
# producing empty feature sets.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

let
  mkCase =
    {
      name,
      # bash that arranges for the override; receives $WORKSPACE and
      # $INDEX_URL in scope.
      setup,
      # extra argv for fake-sparse-server.py
      serverArgs ? "",
      scheme ? "http",
    }:
    pkgs.runCommand "cargo-nix-plugin-mirror-test-${name}"
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
          "$PORT_FILE" "$ACCESS_LOG" ${./fake-sparse-index} ${serverArgs} &
        SERVER_PID=$!
        trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

        while [[ ! -s $PORT_FILE ]]; do
          kill -0 $SERVER_PID
        done
        PORT=$(<"$PORT_FILE")
        INDEX_URL="sparse+${scheme}://127.0.0.1:$PORT/"
        echo "mirror at $INDEX_URL"

        WORKSPACE=$(mktemp -d)
        cp -r ${sampleProject}/. "$WORKSPACE/"
        chmod -R u+w "$WORKSPACE"
        # Crucially: do NOT rewrite Cargo.lock. The override under test
        # must redirect crates.io lookups itself.

        ${setup}

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

        jq -e '.serdeDeps | length > 0' result.json
        jq -e '.httpDeps  | index("bytes") != null' result.json
        grep -q '/se/rd/serde' "$ACCESS_LOG"
        grep -q '/ht/tp/http'  "$ACCESS_LOG"

        mv result.json $out
      '';
in
pkgs.linkFarmFromDrvs "cargo-nix-plugin-mirror-tests" [
  (mkCase {
    name = "env-var";
    setup = ''
      export CARGO_REGISTRIES_CRATES_IO_INDEX="$INDEX_URL"
    '';
  })
  # HTTPS mirror signed by a private CA: exercises that the prefetch HTTP
  # client honours SSL_CERT_FILE/CARGO_HTTP_CAINFO instead of trusting only
  # the bundled webpki roots. Without that hook, rustls rejects the handshake
  # and every fetch fails inside the sandbox.
  (mkCase {
    name = "https-custom-ca";
    scheme = "https";
    serverArgs = "--tls-cert ${./fake-sparse-tls.crt} --tls-key ${./fake-sparse-tls.key}";
    setup = ''
      export CARGO_REGISTRIES_CRATES_IO_INDEX="$INDEX_URL"
      export SSL_CERT_FILE=${./fake-sparse-ca.crt}
    '';
  })
  (mkCase {
    name = "cargo-config";
    setup = ''
      mkdir -p "$WORKSPACE/.cargo"
      cat > "$WORKSPACE/.cargo/config.toml" <<EOF
      [source.crates-io]
      replace-with = "mirror"
      [source.mirror]
      registry = "$INDEX_URL"
      EOF
    '';
  })
]
