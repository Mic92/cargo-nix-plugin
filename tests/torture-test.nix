# End-to-end test: evaluate the wrapper against the torture workspace.
# This runs as a Nix derivation that calls nix-instantiate with the plugin loaded.
{
  pkgs,
  plugin,
  testFixtures,
  wrapperLib,
}:

pkgs.runCommand "cargo-nix-plugin-torture-test"
  {
    nativeBuildInputs = [ pkgs.nixVersions.nix_2_33 ];
  }
  ''
    export HOME=$(mktemp -d)
    export NIX_STORE_DIR=$TMPDIR/nix/store
    export NIX_STATE_DIR=$TMPDIR/nix/var
    export NIX_LOG_DIR=$TMPDIR/nix/log
    mkdir -p $NIX_STORE_DIR $NIX_STATE_DIR $NIX_LOG_DIR

    # Test: verify the wrapper produces derivation attrset structure
    result=$(nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr '
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
        cargoNix = import ${wrapperLib} {
          inherit pkgs;
          metadata = builtins.readFile "${testFixtures}/metadata.json";
          cargoLock = builtins.readFile "${testFixtures}/Cargo.lock";
          src = /dev/null;  # placeholder - we only test evaluation, not building
        };
        memberCount = builtins.length (builtins.attrNames cargoNix.workspaceMembers);
        crateCount = builtins.length (builtins.attrNames cargoNix.resolved.crates);
        # Check that workspace members have build attributes
        firstMember = builtins.head (builtins.attrValues cargoNix.workspaceMembers);
        hasBuild = firstMember ? build;
        hasPackageId = firstMember ? packageId;
      in
        "members=''${toString memberCount} crates=''${toString crateCount} hasBuild=''${if hasBuild then "yes" else "no"} hasPackageId=''${if hasPackageId then "yes" else "no"}"
    ')

    echo "Torture test result: $result"

    # Strip quotes
    result=$(echo "$result" | tr -d '"')

    members=$(echo "$result" | sed 's/.*members=\([0-9]*\).*/\1/')
    hasBuild=$(echo "$result" | sed 's/.*hasBuild=\([a-z]*\).*/\1/')

    if [ "$members" -ne 224 ]; then
      echo "FAIL: Expected 224 members, got $members"
      exit 1
    fi
    if [ "$hasBuild" != "yes" ]; then
      echo "FAIL: workspace members should have 'build' attribute"
      exit 1
    fi

    echo "PASS: wrapper produces correct structure for $members workspace members"
    echo "$result" > $out
  ''
