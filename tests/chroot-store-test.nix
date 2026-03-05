# Regression test: building into a chroot store (nix build --store /tmp/...)
# requires that the plugin remaps logical store paths to real filesystem paths
# so cargo metadata can find Cargo.toml on the host during eval.
#
# This test verifies that the C++ plugin's remapStorePath() correctly handles
# chroot stores, so `nix build --store <dir>` works without any special
# manifestPath parameter.
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
    CHROOT=$(mktemp -d)

    # --- Test: default manifestPath with chroot store ---
    # Uses nix build --store to exercise the remapStorePath() codepath.
    # Without the fix, this fails with:
    #   "cargo metadata failed: manifest path /nix/store/xxx/Cargo.toml does not exist"
    echo "Test: chroot store with auto-remapped manifest path"

    ${nix}/bin/nix build \
      --store "$CHROOT" \
      --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
      --impure --no-link \
      --expr '
        let
          pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
        in (import ${wrapperLib} {
          inherit pkgs;
          src = ${sampleProject};
        }).workspaceMembers.sample-bin.build
      '

    echo "PASS: chroot store build succeeded"

    # Verify the binary was actually built in the chroot store
    sample_bin=$(find "$CHROOT/nix/store" -name sample-bin -type f | head -1)
    [[ -n "$sample_bin" ]] || {
      echo "FAIL: sample-bin not found in chroot store"
      exit 1
    }

    out_json=$("$sample_bin")
    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }
    echo "PASS: binary in chroot store runs correctly"

    echo "ALL CHROOT STORE TESTS PASSED" > $out
  ''
