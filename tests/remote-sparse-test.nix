# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Verify the plugin's remote sparse index HTTP fallback works when
# there is no local cargo registry cache.
#
# A static sparse index fixture (./fake-sparse-index/) is served over
# loopback by `python3 -m http.server`. The plugin is pointed at this
# server with an empty CARGO_HOME, forcing it to HTTP-fetch every crate's
# metadata. We then assert that the resolved dependencies are non-empty.
#
# The fixture contains only the exact versions pinned in
# sample-project/Cargo.lock, one JSONL line per crate file — reproducible
# and checked into git. No FOD, no network.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-remote-sparse-test"
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

    # Serve the fixture over loopback on a kernel-allocated port.
    # The server writes the bound port and an access log to files.
    PORT_FILE=$(mktemp)
    ACCESS_LOG=$(mktemp)
    python3 ${./fake-sparse-server.py} \
      "$PORT_FILE" "$ACCESS_LOG" ${./fake-sparse-index} &
    SERVER_PID=$!
    trap 'kill $SERVER_PID 2>/dev/null || true' EXIT

    # Block until the server publishes its port.
    while [[ ! -s $PORT_FILE ]]; do
      kill -0 $SERVER_PID  # abort if server died before binding
    done
    PORT=$(<"$PORT_FILE")
    echo "sparse index at http://127.0.0.1:$PORT/"

    # Rewrite the sample project's lockfile and cargo config to point at
    # our fake registry instead of crates.io.
    WORKSPACE=$(mktemp -d)
    cp -r ${sampleProject}/. "$WORKSPACE/"
    chmod -R u+w "$WORKSPACE"

    INDEX_URL="sparse+http://127.0.0.1:$PORT/"

    # Cargo.lock pins `source = "registry+https://github.com/rust-lang/crates.io-index"`.
    # The lockfile resolver keys its index lookup off that string, so rewrite it.
    sed -i "s|registry+https://github.com/rust-lang/crates.io-index|$INDEX_URL|g" \
      "$WORKSPACE/Cargo.lock"

    # Evaluate: the plugin should hit the local server for every crate
    # (empty CARGO_HOME → no local cache) and return real dependencies.
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

    # serde depends on serde_core + serde_derive; http depends on bytes, itoa.
    jq -e '.serdeDeps | length > 0' result.json
    jq -e '.httpDeps  | length > 0' result.json
    jq -e '.httpDeps  | index("bytes") != null' result.json

    # And prove the server was actually hit — not a silent local cache read.
    grep -q '/se/rd/serde' "$ACCESS_LOG"
    grep -q '/ht/tp/http'  "$ACCESS_LOG"

    # No path fetched twice: the prefetch pool and the serial resolve
    # loop share one cache path. A duplicate means they diverged.
    dupes=$(sort "$ACCESS_LOG" | uniq -d)
    if [ -n "$dupes" ]; then
      echo "FAIL: duplicate index fetches (prefetch cache miss):"
      echo "$dupes"
      exit 1
    fi

    mv result.json $out
  ''
