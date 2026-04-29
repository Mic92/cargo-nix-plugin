# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Regression: a plugin prefetch must not poison `~/.cargo` for cargo itself.
#
# tame-index's IndexDependency lacks a `registry` field, so re-serializing
# a parsed IndexKrate via `write_cache_entry` drops the cross-registry
# pointer that tells cargo "this dep lives on crates.io, not on this
# alt-registry". The plugin's own resolver doesn't care (it takes sources
# from Cargo.lock), but a subsequent `cargo build` / `cargo metadata` on
# the same CARGO_HOME does: it sees `altcrate → nodep-crate` with no
# registry annotation, looks nodep-crate up on the alt-registry, and fails
# with "no matching package named `nodep-crate` found". Online doesn't
# help — the etag matches, server 304s, cache stays poisoned.
#
# This test pins the contract: after the plugin's fetch path has populated
# the index cache, `cargo metadata --locked --offline` against the same
# CARGO_HOME resolves cleanly. The fetch path is exercised via
# `cargo-nix-prefetch` (the public front-end to the same
# `registry::prefetch_index`/`do_fetch_one`/`write_cache_atomic` code the
# nix builtin uses) so the test runs as a plain sandbox build without
# recursive-nix. Metadata rather than build because it walks the identical
# resolver path without needing a working compiler for the deps.
{
  pkgs,
}:

let
  cargoNixPrefetch = pkgs.callPackage ../nix/cargo-nix-prefetch.nix { };

  # Fixtures (index + .crate tarballs) are derived, not checked in: the
  # index entry's `cksum` must match the tarball sha256, and a checked-in
  # tarball is an unreviewable binary blob.
  altRegistry = pkgs.runCommand "fake-alt-registry" { } ''
    set -euo pipefail
    mkdir -p "$out"
    mk_crate() {
      local name=$1 ver=$2 deps_toml=$3 deps_json=$4
      local stage; stage=$(mktemp -d)
      mkdir -p "$stage/$name-$ver/src"
      : > "$stage/$name-$ver/src/lib.rs"
      cat > "$stage/$name-$ver/Cargo.toml" <<EOF
    [package]
    name = "$name"
    version = "$ver"
    edition = "2021"

    [dependencies]
    $deps_toml
    EOF
      mkdir -p "$out/crates/$name/$ver"
      tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
        -czf "$out/crates/$name/$ver/download" -C "$stage" "$name-$ver"
      local cksum; cksum=$(sha256sum "$out/crates/$name/$ver/download" | cut -d' ' -f1)
      local prefix=''${name:0:2}/''${name:2:2}
      mkdir -p "$out/$prefix"
      printf '{"name":"%s","vers":"%s","deps":%s,"cksum":"%s","features":{},"yanked":false}\n' \
        "$name" "$ver" "$deps_json" "$cksum" > "$out/$prefix/$name"
    }

    # `dl` carries the download URL template; PORT is substituted at
    # serve-time by fake-sparse-server.py.
    cat > "$out/config.json" <<'EOF'
    {"dl":"http://127.0.0.1:PORT/crates/{crate}/{version}/download","api":"http://127.0.0.1:PORT"}
    EOF

    # nodep-crate has no deps so cargo metadata terminates without
    # reaching real crates.io. altcrate depends on it *from crates.io*
    # via an explicit `registry` annotation — the field tame-index drops
    # on round-trip. The .crate manifest spells it `registry-index`
    # (URL): that's what `cargo publish` rewrites to, so it resolves
    # without [registries.*] config on the consumer.
    mk_crate nodep-crate 1.0.0 "" '[]'
    mk_crate altcrate 1.0.0 \
      'nodep-crate = { version = "1.0.0", registry-index = "https://github.com/rust-lang/crates.io-index" }' \
      '[{"name":"nodep-crate","req":"^1","features":[],"optional":false,"default_features":true,"target":null,"kind":"normal","registry":"https://github.com/rust-lang/crates.io-index"}]'
  '';
in
pkgs.runCommand "cargo-nix-plugin-cargo-compat-test"
  {
    nativeBuildInputs = [
      pkgs.python3
      pkgs.cargo
      pkgs.rustc
      cargoNixPrefetch
    ];
  }
  ''
    set -euo pipefail
    HOME=$(mktemp -d); export HOME
    export CARGO_HOME=$HOME/.cargo

    # Two loopback sparse indexes backed by the same fixture: one
    # addressed as the crates-io stand-in (cross-registry target), the
    # other as the alt-registry (where altcrate lives). Only the index
    # URL differs.
    UPSTREAM_PORT_FILE=$(mktemp)
    ALT_PORT_FILE=$(mktemp)
    python3 ${./fake-sparse-server.py} "$UPSTREAM_PORT_FILE" /dev/null ${altRegistry} &
    UPSTREAM_PID=$!
    python3 ${./fake-sparse-server.py} "$ALT_PORT_FILE" /dev/null ${altRegistry} &
    ALT_PID=$!
    trap 'kill $UPSTREAM_PID $ALT_PID 2>/dev/null || true' EXIT
    while [[ ! -s $UPSTREAM_PORT_FILE || ! -s $ALT_PORT_FILE ]]; do
      kill -0 $UPSTREAM_PID; kill -0 $ALT_PID
    done
    UPSTREAM_URL="sparse+http://127.0.0.1:$(<"$UPSTREAM_PORT_FILE")/"
    ALT_URL="sparse+http://127.0.0.1:$(<"$ALT_PORT_FILE")/"
    echo "upstream=$UPSTREAM_URL alt=$ALT_URL"

    WORKSPACE=$(mktemp -d)
    mkdir -p "$WORKSPACE/src" "$WORKSPACE/.cargo"
    : > "$WORKSPACE/src/lib.rs"
    cat > "$WORKSPACE/Cargo.toml" <<EOF
    [package]
    name = "alt-registry-project"
    version = "0.1.0"
    edition = "2021"

    [dependencies]
    altcrate = { version = "1.0.0", registry = "alt" }
    EOF
    cat > "$WORKSPACE/.cargo/config.toml" <<EOF
    [source.crates-io]
    replace-with = "upstream"
    [source.upstream]
    registry = "$UPSTREAM_URL"
    [registries.alt]
    index = "$ALT_URL"
    EOF

    # Let cargo populate the bits the plugin doesn't write (per-registry
    # config.json + .crate tarballs) and prove the rig resolves. The
    # index .cache it writes is what we'll then overwrite.
    ( cd "$WORKSPACE" && cargo fetch )

    # Wipe just the index .cache (the resolver input the plugin writes)
    # and let cargo-nix-prefetch repopulate it via prefetch_index →
    # do_fetch_one → write_cache_atomic. config.json and .crate tarballs
    # are kept — plugin neither reads nor writes those.
    rm -rf "$CARGO_HOME"/registry/index/*/.cache
    cargo-nix-prefetch \
      --manifest-path "$WORKSPACE/Cargo.toml" \
      --index "$UPSTREAM_URL" \
      --output "$CARGO_HOME"

    for d in "$CARGO_HOME"/registry/index/127.0.0.1-*; do
      [[ -f "$d/.cache/al/tc/altcrate" ]] && altcache="$d/.cache/al/tc/altcrate"
    done
    test -n "''${altcache-}"
    echo "--- plugin-written alt-index cache entry: ---"
    tr -c '[:print:]' '\n' < "$altcache"

    # Contract under test: cargo's resolver accepts what we wrote.
    # --offline so this is purely about the on-disk cache (online
    # wouldn't save us anyway: etag matches → 304).
    echo "--- cargo metadata --locked --offline against plugin-written cache ---"
    if ! ( cd "$WORKSPACE" && cargo metadata --format-version 1 --locked --offline >/dev/null ); then
      echo "FAIL: cargo cannot resolve against plugin-written index cache"
      echo "      (cross-registry dep field was dropped during cache write)"
      exit 1
    fi

    # `cargo build` walks the same resolver as metadata but additionally
    # reads the cached entry's `cksum` to verify the .crate tarball, so a
    # round-trip that mangled that field (or feature/yanked flags the
    # resolver doesn't touch but build does) would slip past metadata.
    echo "--- cargo build --locked --offline against plugin-written cache ---"
    if ! ( cd "$WORKSPACE" && cargo build --locked --offline ); then
      echo "FAIL: cargo build rejects plugin-written index cache"
      exit 1
    fi

    echo "PASS: plugin-written index cache is cargo-compatible (metadata + build)" > "$out"
  ''
