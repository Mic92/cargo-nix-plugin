# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Shared scaffolding for chroot-store tests. Sets up an isolated local
# Nix store inside a recursive-nix sandbox, pre-seeds it with a closure
# that the inner build needs, then hands a `$CHROOT` shell variable to
# the test script.
#
#   mkChrootStoreTest {
#     name = "my-test";
#     seed = { rustc = pkgs.rustc; … };   # linkFarm contents to copy in
#     nixConfig = ''…'';                  # appended to NIX_CONFIG
#     script = ''…'';                     # runs with $CHROOT seeded
#   }
{
  pkgs,
  nix,
}:

{
  name,
  seed,
  nixConfig ? "",
  script,
}:
pkgs.runCommand name
  {
    nativeBuildInputs = [ nix ];
    requiredSystemFeatures = [ "recursive-nix" ];
    seed = pkgs.linkFarm "${name}-seed" seed;
  }
  ''
    export HOME=$(mktemp -d)
    CHROOT=$(mktemp -d)

    # The build sandbox has no nix.conf; the new CLI refuses to run
    # without nix-command. Tests append what else they need.
    export NIX_CONFIG="
      experimental-features = nix-command
      substituters =
      ${nixConfig}
    "

    # Pre-seed the chroot store. `nix copy` reads from the outer
    # /nix/store (visible in the recursive-nix sandbox) and registers
    # the closure in the chroot store's db, so the inner build finds
    # its toolchain instead of rebuilding from source.
    ${nix}/bin/nix copy --no-check-sigs --to "local?root=$CHROOT" "$seed"

    ${script}
  ''
