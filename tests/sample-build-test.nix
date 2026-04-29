# Copyright 2026 Anthropic, PBC
# SPDX-License-Identifier: Apache-2.0

# End-to-end build test: resolve, compile, and run a small Rust workspace
# using the nix plugin + buildRustCrate, all inside a single derivation.
# The workspace has two members (sample-lib, sample-bin) to exercise
# inter-workspace-member dependencies.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-sample-build-test"
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
        pkgs = import ${pkgs.path} { system = "${pkgs.stdenv.hostPlatform.system}"; };
      in import ${pluginSrc}/lib {
        inherit pkgs;
        metadata = builtins.readFile "${sampleProject}/metadata.json";
        cargoLock = builtins.readFile "${sampleProject}/Cargo.lock";
        src = ${sampleProject};
      }
    '

    # --- Build test: compile and run the binary workspace member ---
    drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.sample-bin.build")

    # --realize may print multiple outputs (out + lib); take the first.
    built=$(nix-store --realize "$drv" | grep -v -- '-lib$' | head -1)
    out_json=$("$built"/bin/sample-bin)
    echo "Output: $out_json"

    msg=$(echo "$out_json" | jq -r .message)
    [[ "$msg" == "Hello from cargo-nix-plugin!" ]] || {
      echo "FAIL: unexpected message: $msg"
      exit 1
    }

    echo "PASS: workspace built and ran successfully"

    # --- Lib-only dep split: sample-lib has a sidecar bin (sample-tool).
    # When built as the root crate, the bin is present; when pulled in as
    # a dependency of sample-bin (cratesLibOnly), the bin is suppressed.
    lib_root_drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).workspaceMembers.sample-lib.build")
    lib_dep_drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "let c = ($cargoNixExpr); in builtins.getAttr c.resolved.workspaceMembers.sample-lib c.builtCrates.cratesLibOnly")

    [[ "$lib_root_drv" != "$lib_dep_drv" ]] || {
      echo "FAIL: lib-only dep drv should differ from with-bins root drv"
      exit 1
    }

    # --realize prints all outputs in hash order; pick the one without -lib suffix.
    lib_root=$(nix-store --realize "$lib_root_drv" | grep -v -- '-lib$')
    lib_dep=$(nix-store --realize "$lib_dep_drv" | grep -v -- '-lib$')

    [[ -x "$lib_root/bin/sample-tool" ]] || {
      echo "FAIL: workspaceMembers.sample-lib.build should include bin/sample-tool"
      exit 1
    }
    [[ ! -e "$lib_dep/bin/sample-tool" ]] || {
      echo "FAIL: lib-only dep of sample-lib should NOT include bin/sample-tool"
      exit 1
    }
    echo "PASS: lib-only dep split suppresses sidecar bins"

    # --- Clippy test: lint all workspace members with clippy-driver ---
    clippy_drv=$(nix-instantiate \
      --option plugin-files "${plugin}/lib/nix/plugins" \
      --expr "($cargoNixExpr).clippy.allWorkspaceMembers")

    nix-store --realize "$clippy_drv" > /dev/null
    echo "PASS: clippy check succeeded"

    echo "$out_json" > $out
  ''
