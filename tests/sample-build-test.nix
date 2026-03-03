# End-to-end build test: resolve, compile, and run a small Rust workspace
# using the nix plugin + buildRustCrate, all inside a single derivation.
# The workspace has two members (sample-lib, sample-bin) to exercise
# inter-workspace-member dependencies.
{
  pkgs,
  plugin,
  wrapperLib,
  sampleProject,
}:

pkgs.runCommand "cargo-nix-plugin-sample-build-test"
  {
    nativeBuildInputs = [
      pkgs.nixVersions.nix_2_33
      pkgs.jq
    ];
    requiredSystemFeatures = [ "recursive-nix" ];
  }
  ''
    export HOME=$(mktemp -d)

    cargoNixExpr='
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
      in import ${wrapperLib} {
        inherit pkgs;
        metadata = builtins.readFile "${sampleProject}/metadata.json";
        cargoLock = builtins.readFile "${sampleProject}/Cargo.lock";
        src = ${sampleProject};
      }
    '

    # --- Build test: compile and run the binary workspace member ---
    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "($cargoNixExpr).workspaceMembers.sample-bin.build")

    # --realize may print multiple outputs (out + lib); take the first.
    built=$(nix-store --realize "$drv" | head -1)
    out_json=$("$built"/bin/sample-bin)
    echo "Output: $out_json"

    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }

    echo "PASS: workspace built and ran successfully"

    # --- Clippy test: lint all workspace members with clippy-driver ---
    clippy_drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "($cargoNixExpr).clippy.allWorkspaceMembers")

    nix-store --realize "$clippy_drv" > /dev/null
    echo "PASS: clippy check succeeded"

    echo "$out_json" > $out
  ''
