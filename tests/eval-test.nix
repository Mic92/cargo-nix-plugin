# Test the plugin by evaluating it against the torture workspace fixtures.
# Run with:
#   nix eval --raw --option plugin-files /path/to/libcargo_nix_plugin.so -f tests/eval-test.nix
let
  metadata = builtins.readFile ../rust/tests/fixtures/metadata.json;
  cargoLock = builtins.readFile ../rust/tests/fixtures/Cargo.lock;
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

  assertions = [
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
