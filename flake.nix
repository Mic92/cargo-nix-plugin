{
  description = "Nix plugin for resolving Cargo workspaces — replaces generated Cargo.nix";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crate2nix-torture = {
      url = "git+ssh://forgejo@git.ntd.one/anthropic/crate2nix-torture.git";
      flake = false;
    };
  };

  outputs =
    { self, nixpkgs, crate2nix-torture }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
        "x86_64-darwin"
      ];

      forAllSystems =
        f:
        nixpkgs.lib.genAttrs supportedSystems (system: f (import nixpkgs { inherit system; }));

      linuxSystem = "x86_64-linux";
      linuxPkgs = import nixpkgs { system = linuxSystem; };

      mkPlugin = pkgs: pkgs.callPackage ./nix/plugin.nix {
        nixComponents = pkgs.nixVersions.nixComponents_2_33;
      };
    in
    {
      packages = forAllSystems (pkgs:
        {
          default = mkPlugin pkgs;
          cargo-nix-plugin = mkPlugin pkgs;
        }
        // nixpkgs.lib.optionalAttrs (pkgs.stdenv.hostPlatform.system == linuxSystem) {
          eval-test = linuxPkgs.callPackage ./nix/eval-test.nix {
            plugin = mkPlugin linuxPkgs;
            testFixtures = ./rust/tests/fixtures;
          };

          torture-test = linuxPkgs.callPackage ./tests/torture-test.nix {
            plugin = mkPlugin linuxPkgs;
            testFixtures = ./rust/tests/fixtures;
            wrapperLib = ./lib;
          };

          benchmark = linuxPkgs.callPackage ./tests/benchmark.nix {
            plugin = mkPlugin linuxPkgs;
            benchFixtures = ./tests/bench-fixtures;
            nixpkgsPath = nixpkgs;
            cargoNixFile = "${crate2nix-torture}/Cargo.nix";
          };

          # Optional: helper for generating metadata JSON explicitly.
          # Not needed when using the automatic subprocess mode (just pass src).
          # Useful for offline/pure evaluation workflows.
          generate-metadata = linuxPkgs.writeShellApplication {
            name = "generate-metadata";
            runtimeInputs = [ linuxPkgs.cargo ];
            text = ''
              exec cargo metadata --format-version 1 --locked "$@"
            '';
          };
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.rust-analyzer
            pkgs.clippy
            pkgs.rustfmt
          ];
        };
      });

      apps.${linuxSystem} = {
        generate-metadata = {
          type = "app";
          program = "${self.packages.${linuxSystem}.generate-metadata}/bin/generate-metadata";
        };
      };

      lib = import ./lib;
    };
}
