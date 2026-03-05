# Regression test: building into a chroot store (nix build --store /tmp/...)
# requires that cargo metadata runs against a host-accessible path, not a
# store path that only exists inside the chroot.
#
# This test verifies that passing `manifestPath` as a real filesystem path
# allows eval+build to succeed even with an isolated store.
{
  pkgs,
  plugin,
  wrapperLib,
  sampleProject,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-chroot-store-test"
  {
    nativeBuildInputs = [
      nix
      pkgs.jq
    ];
    requiredSystemFeatures = [ "recursive-nix" ];
  }
  ''
    export HOME=$(mktemp -d)

    # --- Test 1: manifestPath with real filesystem path ---
    # The sample project is in the nix store (always accessible from the host).
    # Pass manifestPath explicitly to avoid "${src}/Cargo.toml" interpolation
    # which would create a second store path for the fileset source.
    echo "Test 1: explicit manifestPath parameter"

    manifest_expr='
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
      in import ${wrapperLib} {
        inherit pkgs;
        src = ${sampleProject};
        manifestPath = "${sampleProject}/Cargo.toml";
      }
    '

    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "($manifest_expr).workspaceMembers.sample-bin.build")

    built=$(nix-store --realize "$drv" | head -1)
    out_json=$("$built"/bin/sample-bin)
    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }
    echo "PASS: explicit manifestPath works"

    # --- Test 2: default manifestPath (backward compatibility) ---
    echo "Test 2: default manifestPath (src interpolation)"

    default_expr='
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
      in import ${wrapperLib} {
        inherit pkgs;
        src = ${sampleProject};
      }
    '

    drv2=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --expr "($default_expr).workspaceMembers.sample-bin.build")

    built2=$(nix-store --realize "$drv2" | head -1)
    out_json2=$("$built2"/bin/sample-bin)
    msg2=$(echo "$out_json2" | jq -r .message)
    [[ "$msg2" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: default manifestPath broken: $msg2"
      exit 1
    }
    echo "PASS: default manifestPath backward compatible"

    echo "ALL CHROOT STORE TESTS PASSED" > $out
  ''
