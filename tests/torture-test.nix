# End-to-end test: evaluate the wrapper against the torture workspace.
# This runs as a Nix derivation that calls nix-instantiate with the plugin loaded.
{
  pkgs,
  plugin,
  testFixtures,
  wrapperLib,
  nix,
}:

pkgs.runCommand "cargo-nix-plugin-torture-test"
  {
    nativeBuildInputs = [ nix ];
  }
  ''
    export HOME=$(mktemp -d)
    export NIX_STORE_DIR=$TMPDIR/nix/store
    export NIX_STATE_DIR=$TMPDIR/nix/var
    export NIX_LOG_DIR=$TMPDIR/nix/log
    mkdir -p $NIX_STORE_DIR $NIX_STATE_DIR $NIX_LOG_DIR

    nix_eval() {
      nix-instantiate --eval --strict --read-write-mode \
        --option plugin-files "${plugin}/lib/nix/plugins/libcargo_nix_plugin.so" \
        --expr "$1"
    }

    wrapper_expr='
      let
        pkgs = import ${pkgs.path} { system = "x86_64-linux"; };
      in import ${wrapperLib} {
        inherit pkgs;
        metadata = builtins.readFile "${testFixtures}/metadata.json";
        cargoLock = builtins.readFile "${testFixtures}/Cargo.lock";
        src = /dev/null;
      }
    '

    # Test: wrapper produces correct structure
    result=$(nix_eval "
      let cargoNix = $wrapper_expr; in {
        members = builtins.length (builtins.attrNames cargoNix.workspaceMembers);
        crates = builtins.length (builtins.attrNames cargoNix.resolved.crates);
        hasBuild = (builtins.head (builtins.attrValues cargoNix.workspaceMembers)) ? build;
      }
    ")
    [[ "$result" == *"members = 224"* ]] || { echo "FAIL: expected 224 members: $result"; exit 1; }
    [[ "$result" == *"hasBuild = true"* ]] || { echo "FAIL: missing build attr: $result"; exit 1; }

    # Test: build dependencies are built for build platform under cross-compilation
    result=$(nix_eval '
      let
        pkgs = import ${pkgs.path} { localSystem = "x86_64-linux"; crossSystem = "aarch64-linux"; };
        cargoNix = import ${wrapperLib} {
          inherit pkgs;
          metadata = builtins.readFile "${testFixtures}/metadata.json";
          cargoLock = builtins.readFile "${testFixtures}/Cargo.lock";
          src = /dev/null;
        };
        crates = cargoNix.builtCrates.crates;
        rav1e = if crates ? rav1e then crates.rav1e else crates.${"rav1e 0.7.1"};
        buildDepSystems = map (dep: dep.stdenv.hostPlatform.system) rav1e.buildDependencies;
      in builtins.all (s: s == "x86_64-linux") buildDepSystems
    ')
    [[ "$result" == "true" ]] || { echo "FAIL: build deps should target build platform: $result"; exit 1; }

    touch $out
  ''
