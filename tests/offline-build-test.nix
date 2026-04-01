# End-to-end test for offline mode: resolve from Cargo.lock + registry
# index cache (no cargo metadata), compile, and run the sample workspace.
#
# Instead of fetching from the network, we construct a CARGO_HOME from
# the fake-sparse-index fixtures already in the repo. This makes the
# test fully reproducible — no FOD, no network.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

let
  readCrateInfo = pkgs.callPackage ../nix/read-crate-info.nix { };

  # Build a synthetic CARGO_HOME with binary index cache entries from
  # the fake-sparse-index fixtures.  `read-crate-info populate-cache`
  # converts the raw JSON-lines files into tame-index's binary cache
  # format so SparseIndex::cached_krate can read them directly.
  cargoHome = pkgs.runCommand "sample-project-cargo-home"
    { nativeBuildInputs = [ readCrateInfo ]; }
    ''
      read-crate-info populate-cache \
        ${./fake-sparse-index} $out \
        index.crates.io-offline
    '';
in
pkgs.runCommand "cargo-nix-plugin-offline-build-test"
  {
    nativeBuildInputs = [
      nix
      pkgs.jq
    ];
    requiredSystemFeatures = [ "recursive-nix" ];
  }
  ''
    export HOME=$(mktemp -d)

    cargoNixExpr='
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
      in import ${pluginSrc}/lib {
        inherit pkgs;
        src = ${sampleProject};
        cargoHome = "${cargoHome}";
      }
    '

    # --- Eval test: offline resolution produces workspace members ---
    result=$(nix-instantiate --eval --strict --read-write-mode \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "builtins.attrNames ($cargoNixExpr).workspaceMembers")
    echo "Workspace members: $result"
    [[ "$result" == *"sample-bin"* ]] || { echo "FAIL: missing sample-bin"; exit 1; }
    [[ "$result" == *"sample-lib"* ]] || { echo "FAIL: missing sample-lib"; exit 1; }
    echo "PASS: offline eval produces workspace members"

    # --- Build test: compile and run the binary ---
    drv=$(nix-instantiate --show-trace \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "($cargoNixExpr).workspaceMembers.sample-bin.build")

    built=$(nix-store --realize "$drv" | grep -v -- '-lib$' | head -1)
    out_json=$("$built"/bin/sample-bin)
    echo "Output: $out_json"

    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }

    echo "PASS: offline build succeeded"
    echo "$out_json" > $out
  ''
