# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# End-to-end: a workspace depending on a multi-crate git repo.
#
# Exercises the gitSources path added for `git+` lockfile entries: the
# resolver must read each crate's Cargo.toml from the supplied checkout
# (not the registry index), emit dep edges + features, and report the
# per-crate sub-directory so buildRustCrate cd's into the right member.
#
# We override `gitSources` so no actual fetchGit happens — keeps the test
# hermetic and lets the rev be a placeholder.
{
  pkgs,
  plugin,
  pluginSrc,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-git-source-test"
  {
    nativeBuildInputs = [ nix ];
    requiredSystemFeatures = [ "recursive-nix" ];
  }
  ''
    export HOME=$(mktemp -d)

    cargoNixExpr='
      let
        pkgs = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
      in import ${pluginSrc}/lib {
        inherit pkgs;
        src = ${./git-consumer};
        gitSources = {
          "file:///fake-git-repo#0000000000000000000000000000000000000000" =
            ${./fake-git-repo};
        };
      }
    '

    # Resolver-level checks: deps/features/subPath came from the checkout.
    nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "
        let c = ($cargoNixExpr); foo = c.resolved.crates.foo; in
        assert (builtins.head foo.dependencies).packageId == \"bar\";
        assert foo.source.subPath == \"crates/foo\";
        assert builtins.elem \"loud\" foo.resolvedDefaultFeatures;
        \"resolver-ok\"
      "

    # Build & run: foo→bar edge compiles, feature 'loud' is set, and
    # build-rust-crate found crates/foo via workspace_member.
    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.git-consumer.build")
    built=$(nix-store --realize "$drv" | grep -v -- '-lib$' | head -1)
    got=$("$built"/bin/git-consumer)
    echo "output: $got"
    [[ "$got" == "hello!" ]] || { echo "FAIL: expected 'hello!', got '$got'"; exit 1; }

    echo "PASS: git-source workspace built and ran" > $out
  ''
