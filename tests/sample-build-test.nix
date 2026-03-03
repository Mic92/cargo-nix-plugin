# End-to-end build test: resolve, compile, and run a small Rust project
# using the nix plugin + buildRustCrate, all inside a single derivation.
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

    # Use the host nix store so buildRustCrate can access nixpkgs derivations.
    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr '
        let
          pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
          cargoNix = import ${wrapperLib} {
            inherit pkgs;
            metadata = builtins.readFile "${sampleProject}/metadata.json";
            cargoLock = builtins.readFile "${sampleProject}/Cargo.lock";
            src = ${sampleProject};
          };
        in cargoNix.rootCrate.build
      ')

    # --realize may print multiple outputs (out + lib); take the first.
    built=$(nix-store --realize "$drv" | head -1)
    out_json=$("$built"/bin/sample-project)
    echo "Output: $out_json"

    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }

    echo "PASS: sample project built and ran successfully"

    # --- Clippy test: build with clippy-driver, verify it succeeds ---
    clippy_drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr '
        let
          pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
          cargoNix = import ${wrapperLib} {
            inherit pkgs;
            metadata = builtins.readFile "${sampleProject}/metadata.json";
            cargoLock = builtins.readFile "${sampleProject}/Cargo.lock";
            src = ${sampleProject};
          };
        in cargoNix.clippy.allWorkspaceMembers
      ')

    nix-store --realize "$clippy_drv" > /dev/null
    echo "PASS: clippy check succeeded"

    echo "$out_json" > $out
  ''
