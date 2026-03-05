{
  pkgs,
  plugin,
  testFixtures,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-eval-test"
  {
    nativeBuildInputs = [ nix ];
  }
  ''
    # Use a local temp store to avoid permission issues in the sandbox
    export HOME=$(mktemp -d)
    export NIX_STORE_DIR=$TMPDIR/nix/store
    export NIX_STATE_DIR=$TMPDIR/nix/var
    export NIX_LOG_DIR=$TMPDIR/nix/log
    mkdir -p $NIX_STORE_DIR $NIX_STATE_DIR $NIX_LOG_DIR

    result=$(nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr '
      let
        metadata = builtins.readFile "${testFixtures}/metadata.json";
        cargoLock = builtins.readFile "${testFixtures}/Cargo.lock";
        target = {
          name = "x86_64-unknown-linux-gnu";
          os = "linux"; arch = "x86_64"; vendor = "unknown";
          env = "gnu"; family = ["unix"];
          pointer_width = "64"; endian = "little";
          unix = true; windows = false;
        };
        result = builtins.resolveCargoWorkspace {
          inherit metadata cargoLock target;
          rootFeatures = ["default"];
        };
        crateCount = builtins.length (builtins.attrNames result.crates);
        memberCount = builtins.length (builtins.attrNames result.workspaceMembers);
      in
        "crates=''${toString crateCount} members=''${toString memberCount}"
    ')

    echo "Plugin eval result: $result"

    # Strip quotes from nix-instantiate output
    result=$(echo "$result" | tr -d '"')

    crates=$(echo "$result" | sed 's/crates=\([0-9]*\).*/\1/')
    members=$(echo "$result" | sed 's/.*members=\([0-9]*\).*/\1/')

    if [ "$crates" -lt 1700 ]; then
      echo "FAIL: Expected >= 1700 crates, got $crates"
      exit 1
    fi
    if [ "$members" -ne 224 ]; then
      echo "FAIL: Expected 224 members, got $members"
      exit 1
    fi

    echo "PASS: $crates crates, $members workspace members"
    echo "$result" > $out
  ''
