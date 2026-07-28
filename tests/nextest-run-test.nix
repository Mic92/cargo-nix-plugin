# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# End-to-end test for workspaceMembers.<name>.nextestRun. nodeps-lib
# covers the three test-binary shapes. Those are the lib test with its
# hash-suffixed name, the single-file tests/integration.rs, and the
# multi-file tests/multi/main.rs.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-nextest-run-test"
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
        src = ${sampleProject};
      }
    '

    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.nodeps-lib.nextestRun")
    report=$(nix-store --realize "$drv")

    # A manifest bug that drops a binary would still exit 0. Check
    # that the report in $out names both integration binaries.
    for id in "nodeps-lib::integration" "nodeps-lib::multi"; do
      grep -q "$id" "$report" || { echo "FAIL: $id not in report"; exit 1; }
    done
    echo "PASS: nextestRun ran all integration test binaries"

    # A failing test must fail the derivation. pipefail makes
    # nextest's exit code survive the tee into $out.
    faildrv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.nodeps-lib.nextestRun.overrideAttrs (_: { NEXTEST_TEST_MUST_FAIL = \"1\"; })")
    if nix-store --realize "$faildrv" 2>/dev/null; then
      echo "FAIL: derivation succeeded despite failing test"
      exit 1
    fi
    echo "PASS: failing test fails the derivation"
    touch $out
  ''
