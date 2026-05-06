# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# Plugin assertions against the torture fixtures. CI: nix/eval-test.nix.
# Manual: nix eval --raw --option plugin-files … --expr 'import ./tests/eval-test.nix {}'
{
  fixtures ? ../rust/tests/fixtures,
}:
let
  metadata = builtins.readFile (fixtures + "/metadata.json");
  cargoLock = builtins.readFile (fixtures + "/Cargo.lock");
  target = {
    name = "x86_64-unknown-linux-gnu";
    os = "linux";
    arch = "x86_64";
    vendor = "unknown";
    env = "gnu";
    family = [ "unix" ];
    pointer_width = "64";
    endian = "little";
    unix = true;
    windows = false;
  };

  result = builtins.resolveCargoWorkspace {
    inherit metadata cargoLock target;
    rootFeatures = [ "default" ];
  };

  crateCount = builtins.length (builtins.attrNames result.crates);
  memberCount = builtins.length (builtins.attrNames result.workspaceMembers);

  # Spot-check: serde should exist
  serde = result.crates.${"serde"} or result.crates.${"serde 1.0.228"} or null;

  # Spot-check: rav1e (external dep with bin targets) should have empty crateBin
  rav1e = result.crates.${"rav1e"} or result.crates.${"rav1e 0.7.1"} or null;

  memberIds = builtins.attrValues result.workspaceMembers;

  # Find external crates that have non-empty devDependencies
  externalWithDevDeps = builtins.filter (
    id:
    !(builtins.elem id memberIds) && builtins.length (result.crates.${id}.devDependencies or [ ]) > 0
  ) (builtins.attrNames result.crates);

  assertions = [
    {
      name = "api-level";
      ok = (result.apiLevel or 0) >= 1;
      msg = "Expected apiLevel >= 1, got ${toString (result.apiLevel or 0)}";
    }
    {
      # Primop and embedded result.apiLevel both read API_LEVEL; a
      # mismatch means the C++ shim links a stale symbol.
      name = "api-level-primop";
      ok = (builtins.cargoNixApiLevel or (-1)) == (result.apiLevel or 0);
      msg = "builtins.cargoNixApiLevel (${
        toString (builtins.cargoNixApiLevel or (-1))
      }) != result.apiLevel (${toString (result.apiLevel or 0)})";
    }
    {
      name = "crate-count";
      ok = crateCount >= 1700;
      msg = "Expected >= 1700 crates, got ${toString crateCount}";
    }
    {
      name = "member-count";
      ok = memberCount == 224;
      msg = "Expected 224 workspace members, got ${toString memberCount}";
    }
    {
      name = "serde-exists";
      ok = serde != null;
      msg = "serde not found in crates";
    }
    {
      name = "serde-has-features";
      ok = serde != null && serde.features ? default;
      msg = "serde missing 'default' feature";
    }
    {
      name = "external-crate-no-bins";
      ok = rav1e != null && rav1e.crateBin == [ ];
      msg = "rav1e (external dep) should have empty crateBin to avoid building binaries without their dependencies";
    }
    {
      name = "external-crate-no-dev-deps";
      ok = builtins.length externalWithDevDeps == 0;
      msg = "external crates should have no devDependencies, found ${toString (builtins.length externalWithDevDeps)} with dev deps";
    }
  ];

  failures = builtins.filter (a: !a.ok) assertions;
in
if failures == [ ] then
  "ALL TESTS PASSED: ${toString crateCount} crates, ${toString memberCount} workspace members"
else
  builtins.throw (
    "TEST FAILURES:\n"
    + builtins.concatStringsSep "\n" (map (a: "  FAIL: ${a.name}: ${a.msg}") failures)
  )
