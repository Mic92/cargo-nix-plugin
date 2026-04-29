# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

{
  pkgs,
  plugin,
  testFixtures,
  nix,
}:

# Run tests/eval-test.nix under the built plugin; it throws on failure.
pkgs.runCommand "cargo-nix-plugin-eval-test"
  {
    nativeBuildInputs = [ nix ];
  }
  ''
    export HOME=$(mktemp -d)
    export NIX_STORE_DIR=$TMPDIR/nix/store
    export NIX_STATE_DIR=$TMPDIR/nix/var
    export NIX_LOG_DIR=$TMPDIR/nix/log
    mkdir -p $NIX_STORE_DIR $NIX_STATE_DIR $NIX_LOG_DIR

    nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr 'import ${../tests/eval-test.nix} { fixtures = ${testFixtures}; }' \
      | tee $out
  ''
