# End-to-end test for offline mode: resolve from Cargo.lock + registry
# index cache (no cargo metadata), compile, and run the sample workspace.
{
  pkgs,
  plugin,
  pluginSrc,
  sampleProject,
  nix,
}:

let
  # Pre-populate a cargo home with registry index cache entries for the
  # sample project's dependencies. This is a fixed-output derivation so
  # it can access the network.
  cargoHome = pkgs.stdenv.mkDerivation {
    name = "sample-project-cargo-home";
    src = sampleProject;
    nativeBuildInputs = [ pkgs.cargo pkgs.cacert ];
    SSL_CERT_FILE = "${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt";
    outputHashAlgo = "sha256";
    outputHashMode = "recursive";
    outputHash = "sha256-2bqnTR+UyILKWwXnwFcp2H+20nmS6fKT+5OmGChFMa0=";
    buildPhase = ''
      export CARGO_HOME=$out
      # `cargo metadata` populates the sparse index cache (.cache directory)
      # which is what tame-index reads. `cargo fetch` alone doesn't create it.
      cargo metadata --manifest-path Cargo.toml --locked --format-version 1 > /dev/null
    '';
    installPhase = ''
      # Remove downloaded .crate files and extracted sources — only keep index cache
      rm -rf $out/registry/cache
      rm -rf $out/registry/src
    '';
  };
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
