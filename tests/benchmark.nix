# Benchmark: compare nix-instantiate time of plugin vs crate2nix's Cargo.nix
#
# Uses a small workspace (58 crates, 1 member) to keep iteration fast.
# Measures real .drv creation time, equivalent to `nix flake check`.
{
  pkgs,
  plugin,
  benchFixtures,
  nixpkgsPath,
  cargoNixFile,
}:

let
  wrapperLib = ./.. + "/lib";

  # Plugin: resolve + instantiate the workspace member derivation
  pluginExpr = pkgs.writeText "bench-plugin.nix" ''
    let
      pkgs = import <nixpkgs> { system = "x86_64-linux"; };
      cargoNix = import ${wrapperLib} {
        inherit pkgs;
        metadata = builtins.readFile "${benchFixtures}/metadata.json";
        cargoLock = builtins.readFile "${benchFixtures}/Cargo.lock";
        src = "${benchFixtures}";
      };
      members = builtins.attrValues cargoNix.workspaceMembers;
    in
      builtins.listToAttrs (builtins.map (m: {
        name = m.packageId;
        value = m.build;
      }) members)
  '';

  # Cargo.nix: instantiate the same 10 members from the torture workspace
  # Use all workspace members from the Cargo.nix
  crate2nixExpr = pkgs.writeText "bench-crate2nix.nix" ''
    let
      cargoNix = import ${cargoNixFile} {};
      members = builtins.attrValues cargoNix.workspaceMembers;
    in
      builtins.listToAttrs (builtins.map (m: {
        name = m.packageId;
        value = m.build;
      }) members)
  '';

in
pkgs.runCommand "cargo-nix-plugin-benchmark"
  {
    nativeBuildInputs = [
      pkgs.nixVersions.nix_2_33
      pkgs.coreutils
    ];
  }
  ''
    export HOME=$(mktemp -d)
    export NIX_STORE_DIR=$TMPDIR/nix/store
    export NIX_STATE_DIR=$TMPDIR/nix/var
    export NIX_LOG_DIR=$TMPDIR/nix/log
    mkdir -p $NIX_STORE_DIR $NIX_STATE_DIR $NIX_LOG_DIR

    RUNS=3
    mkdir -p $out

    run_bench() {
      local label="$1"
      shift
      local times=""
      for i in $(seq 1 $RUNS); do
        start=$(date +%s%N)
        drvs=$(nix-instantiate --read-write-mode "$@" 2>/dev/null) || {
          echo "  Run $i: FAILED"
          nix-instantiate --read-write-mode --show-trace "$@" 2>&1 | tail -30
          exit 1
        }
        end=$(date +%s%N)
        elapsed_ms=$(( (end - start) / 1000000 ))
        drv_count=$(echo "$drvs" | wc -l | tr -d ' ')
        echo "  Run $i: ''${elapsed_ms}ms  ($drv_count drv outputs)"
        times="$times $elapsed_ms"
      done
      BENCH_TIMES="$times"
      BENCH_MEDIAN=$(echo $times | tr ' ' '\n' | sort -n | sed -n "$((RUNS/2+1))p")
    }

    PLUGIN_OPT="--option plugin-files ${plugin}/lib/nix/plugins/libcargo_nix_plugin.so"
    NIXPKGS_OPT="-I nixpkgs=${nixpkgsPath}"

    echo "=== nix-instantiate: create .drv for all 224 workspace members ==="
    echo "(1798 crates, 224 members — full torture workspace, nix flake check equivalent)"
    echo ""

    echo "--- Plugin + wrapper ---"
    run_bench "plugin" $PLUGIN_OPT $NIXPKGS_OPT ${pluginExpr}
    plugin_median=$BENCH_MEDIAN
    plugin_times="$BENCH_TIMES"

    echo ""
    echo "--- Cargo.nix (99K lines) ---"
    run_bench "crate2nix" $NIXPKGS_OPT ${crate2nixExpr}
    crate2nix_median=$BENCH_MEDIAN
    crate2nix_times="$BENCH_TIMES"

    echo ""
    echo "=== Results ==="
    echo ""
    echo "nix-instantiate (1798 crates, 224 members):"
    echo "  Plugin:    ''${plugin_median}ms"
    echo "  Cargo.nix: ''${crate2nix_median}ms"
    if [ "$crate2nix_median" -gt 0 ] && [ "$plugin_median" -gt 0 ]; then
      ratio=$(awk "BEGIN { printf \"%.2f\", $plugin_median / $crate2nix_median }")
      echo "  Ratio:     ''${ratio}x (plugin / Cargo.nix)"
    fi

    cat > $out/results.txt <<EOF
Benchmark: cargo-nix-plugin vs crate2nix Cargo.nix
Crates: 1798, Workspace members: 224 (full torture workspace)
Task: nix-instantiate all members (.drv creation)
Runs: $RUNS each

Plugin + wrapper:
  Median: ''${plugin_median}ms
  Runs:  $plugin_times

Cargo.nix (99K lines, pre-computed):
  Median: ''${crate2nix_median}ms
  Runs:  $crate2nix_times

Ratio: ''${ratio:-?}x (plugin / Cargo.nix)
EOF

    cat $out/results.txt
  ''
